// SPDX-License-Identifier: MPL-2.0
//! Notification mirroring (docs/protocol/notifications.md).
//!
//! On the phone, [`Feed`] holds the notifications currently showing (as the
//! app reports them) and keeps every allowed PC in sync: a snapshot when a
//! session starts or the user changes what that PC may see, then each post
//! and removal. On the PC, incoming events become [`NodeEvent`]s, and the
//! user's dismissals, replies and actions go back as requests.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use nectarlink_protocol::{
    DeviceId, Envelope, ErrorCode,
    messages::{Notification, NotifyAction, NotifyKey, NotifySnapshot, notify_limits::SNAPSHOT_ITEMS, types},
};

use crate::{
    Result,
    events::NodeEvent,
    features::{Role, role_of},
    node::Shared,
    session::Session,
};

/// The device toggle that allows notifications for a device.
pub(crate) const TOGGLE: &str = "notifications";
/// Offered by phones and tablets that show a paired device's notifications.
pub const SHOW: &str = "notify.show";

/// A snapshot stays well under the 1 MiB frame limit (v0 §3).
const SNAPSHOT_BUDGET: usize = 768 * 1024;

/// Why the phone couldn't dismiss a notification or run its action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationError {
    /// The notification or action no longer exists.
    NotFound,
    /// This device can't do it (e.g. no notification access).
    Unsupported,
    /// Anything else; the string is for logs and never holds content.
    Failed(String),
}

/// Original notification action authority, retained until the platform executes it.
/// Contains no notification content, reply text, or wire-visible identifier.
#[derive(Clone)]
pub struct NotificationActionContext {
    shared: Arc<Shared>,
    session: Arc<Session>,
    active: Arc<AtomicBool>,
}

impl std::fmt::Debug for NotificationActionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NotificationActionContext")
    }
}

impl NotificationActionContext {
    fn new(shared: &Arc<Shared>, session: &Arc<Session>) -> Option<Self> {
        // Admission and the actual permission writes use the same short lock.
        // A queued action cannot capture a fresh token during an OFF/ON update.
        let mut authority = shared.notifications.action_authority();
        if !Self::owner_current(shared, session) {
            return None;
        }
        let active = authority.for_peer(session.peer, session);
        Some(Self { shared: shared.clone(), session: session.clone(), active })
    }

    /// Check immediately before the OS action, including after any platform queue.
    /// A replacement Node or connection can never acquire this action's authority.
    pub fn is_current(&self) -> bool {
        Self::owner_current(&self.shared, &self.session)
            // Read last: removal followed by regrant still retires the captured token.
            && self.active.load(Ordering::Acquire)
    }

    fn owner_current(shared: &Shared, session: &Arc<Session>) -> bool {
        !shared.cancel.is_cancelled()
            && !session.cancel.is_cancelled()
            && shared.store.is_paired(&session.peer).unwrap_or(false)
            && shared.session(&session.peer).is_some_and(|live| Arc::ptr_eq(&live, session))
            && allowed(shared, &session.peer).is_ok()
    }
}

/// One current token per peer with an admitted action. The token is bound to
/// the session that admitted it. A different session retires that token.
/// Revocation removes it. Retained contexts keep the retired token, and a
/// later admission gets a new one.
#[derive(Default)]
pub(crate) struct ActionAuthority {
    peers: HashMap<DeviceId, PeerToken>,
}

struct PeerToken {
    active: Arc<AtomicBool>,
    session_ptr: usize,
}

impl ActionAuthority {
    fn for_peer(&mut self, peer: DeviceId, session: &Arc<Session>) -> Arc<AtomicBool> {
        self.token_for(peer, Arc::as_ptr(session) as usize)
    }

    /// Returns the live token for this session pointer. A different pointer
    /// releases the previous token before the new one is stored.
    fn token_for(&mut self, peer: DeviceId, session_ptr: usize) -> Arc<AtomicBool> {
        if let Some(existing) = self.peers.get(&peer) {
            if existing.session_ptr == session_ptr {
                return existing.active.clone();
            }
            existing.active.store(false, Ordering::Release);
        }
        let active = Arc::new(AtomicBool::new(true));
        self.peers.insert(peer, PeerToken { active: active.clone(), session_ptr });
        active
    }

    /// Retires the token only when it still belongs to this session. A newer
    /// admission for the same peer is left in place.
    pub(crate) fn revoke_session(&mut self, peer: &DeviceId, session: &Arc<Session>) {
        self.retire_if_session(peer, Arc::as_ptr(session) as usize);
    }

    fn retire_if_session(&mut self, peer: &DeviceId, session_ptr: usize) {
        let matches = self.peers.get(peer).is_some_and(|token| token.session_ptr == session_ptr);
        if matches && let Some(token) = self.peers.remove(peer) {
            token.active.store(false, Ordering::Release);
        }
    }

    pub(crate) fn revoke_peer(&mut self, peer: &DeviceId) {
        if let Some(token) = self.peers.remove(peer) {
            token.active.store(false, Ordering::Release);
        }
    }

    pub(crate) fn revoke_all(&mut self) {
        for (_, token) in self.peers.drain() {
            token.active.store(false, Ordering::Release);
        }
    }
}

/// The phone's current notifications, plus app icons for sending once per
/// session.
#[derive(Default)]
pub(crate) struct Feed {
    inner: Mutex<FeedState>,
    action_authority: Mutex<ActionAuthority>,
}

#[derive(Default)]
struct FeedState {
    /// Set once the app reports notifications: only a device that mirrors
    /// its notifications sends snapshots.
    active: bool,
    items: HashMap<String, Notification>,
    icons: HashMap<String, Vec<u8>>,
    peer_icons: HashMap<(DeviceId, String), Vec<u8>>,
}

impl Feed {
    pub(crate) fn action_authority(&self) -> std::sync::MutexGuard<'_, ActionAuthority> {
        self.action_authority.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FeedState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_active(&self) -> bool {
        self.lock().active
    }

    /// Records an app icon received from a peer and returns the cached icon
    /// for `(peer, app)` when `incoming` is `None`.
    fn peer_icon(&self, peer: DeviceId, app: &str, incoming: Option<&[u8]>) -> Option<Vec<u8>> {
        let mut state = self.lock();
        if let Some(icon) = incoming {
            state.peer_icons.insert((peer, app.to_owned()), icon.to_vec());
            Some(icon.to_vec())
        } else {
            state.peer_icons.get(&(peer, app.to_owned())).cloned()
        }
    }

    /// Stores a notification (its icon separately); returns it without the
    /// icon, sanitized, or `None` if it can't be shown.
    fn post(&self, notification: Notification) -> Option<Notification> {
        let mut n = notification.sanitized()?;
        let mut state = self.lock();
        state.active = true;
        if let Some(icon) = n.icon.take() {
            state.icons.insert(n.app.clone(), icon);
        }
        state.items.insert(n.key.clone(), n.clone());
        Some(n)
    }

    fn remove(&self, key: &str) -> bool {
        let mut state = self.lock();
        state.active = true;
        state.items.remove(key).is_some()
    }

    fn reset(&self, items: Vec<Notification>) {
        let mut state = self.lock();
        state.active = true;
        state.items.clear();
        for n in items.into_iter().filter_map(Notification::sanitized) {
            let mut n = n;
            if let Some(icon) = n.icon.take() {
                state.icons.insert(n.app.clone(), icon);
            }
            state.items.insert(n.key.clone(), n);
        }
    }

    /// The newest notifications that fit a frame, each app's icon attached
    /// once unless `session` already got it.
    fn snapshot(&self, session: &Session) -> NotifySnapshot {
        let state = self.lock();
        let mut items: Vec<&Notification> = state.items.values().collect();
        items.sort_by(|a, b| {
            b.live
                .is_some()
                .cmp(&a.live.is_some())
                .then_with(|| b.when.cmp(&a.when))
                .then_with(|| a.key.cmp(&b.key))
        });
        let mut sent = session.sent_icons();
        let mut budget = SNAPSHOT_BUDGET;
        let mut out = Vec::new();
        for n in items.into_iter().take(SNAPSHOT_ITEMS) {
            let mut n = n.clone();
            // Pictures go while there's room; the rest go without.
            if n.image.is_some() && approx_size(&n) > budget {
                n.image = None;
            }
            let size = approx_size(&n);
            if size > budget {
                break;
            }
            budget -= size;
            if !sent.contains(&n.app)
                && let Some(icon) = state.icons.get(&n.app)
                && icon.len() <= budget
            {
                budget -= icon.len();
                sent.insert(n.app.clone());
                n.icon = Some(icon.clone());
            }
            out.push(n);
        }
        NotifySnapshot { items: out }
    }

    /// `n` with its app icon attached if `session` hasn't had it yet.
    fn for_session(&self, n: &Notification, session: &Session) -> Notification {
        let mut n = n.clone();
        // Same lock order as `snapshot`: the feed, then the session.
        let icon = self.lock().icons.get(&n.app).cloned();
        if let Some(icon) = icon
            && session.sent_icons().insert(n.app.clone())
        {
            n.icon = Some(icon);
        }
        n
    }
}

/// Encoded size, roughly (text plus a little CBOR overhead per field).
fn approx_size(n: &Notification) -> usize {
    let text = [&n.title, &n.text, &n.sub].iter().map(|s| s.as_ref().map_or(0, String::len)).sum::<usize>();
    let actions = n.actions.iter().map(|a| a.id.len() + a.title.len() + 16).sum::<usize>();
    let live = n.live.as_ref().map_or(0, |l| {
        l.chip.as_ref().map_or(0, String::len) + (l.segments.len() + l.points.len()) * 16 + 32
    });
    let conv = n.conversation.as_ref().map_or(0, |c| {
        c.title.len()
            + c.avatar.as_ref().map_or(0, Vec::len)
            + c.messages
                .iter()
                .map(|m| {
                    m.sender.as_ref().map_or(0, String::len)
                        + m.text.len()
                        + m.avatar.as_ref().map_or(0, Vec::len)
                        + 32
                })
                .sum::<usize>()
            + 32
    });
    n.key.len()
        + n.app.len()
        + n.app_name.len()
        + text
        + actions
        + live
        + conv
        + n.image.as_ref().map_or(0, Vec::len)
        + 64
}

impl Shared {
    /// Whether the user allows notifications for `peer` on this device.
    pub(crate) fn notifications_allowed(&self, peer: &DeviceId) -> bool {
        self.toggle_on(peer, TOGGLE)
    }

    /// Sessions that should get this phone's notifications: connected
    /// devices that show them (PCs, and phones or tablets that offer
    /// `notify.show`) and the user allows.
    fn notification_targets(&self) -> Vec<Arc<Session>> {
        self.live_sessions()
            .into_iter()
            .filter(|s| self.shows_notifications(&s.peer) && self.notifications_allowed(&s.peer))
            .collect()
    }

    pub(crate) fn is_desktop(&self, peer: &DeviceId) -> bool {
        self.store.get_peer(peer).ok().flatten().is_some_and(|p| role_of(&p.info) == Some(Role::Desktop))
    }

    /// Whether `peer` shows this device's notifications: a PC does, and a
    /// phone or tablet that offers `notify.show`.
    pub(crate) fn shows_notifications(&self, peer: &DeviceId) -> bool {
        self.store
            .get_peer(peer)
            .ok()
            .flatten()
            .is_some_and(|p| role_of(&p.info) == Some(Role::Desktop) || p.caps.contains(SHOW))
    }

    /// Sends this phone's notifications to one PC: everything when the
    /// user allows it, an empty list (which clears the PC) when not.
    pub(crate) async fn send_notification_snapshot(&self, session: &Arc<Session>) {
        if !self.notifications.is_active() || !self.shows_notifications(&session.peer) {
            return;
        }
        let snapshot = if self.notifications_allowed(&session.peer) {
            self.notifications.snapshot(session)
        } else {
            NotifySnapshot { items: Vec::new() }
        };
        if let Ok(env) = Envelope::new(types::NOTIFY_SNAPSHOT, &snapshot) {
            let _ = session.send(env).await;
        }
    }

    pub(crate) async fn notification_posted(&self, notification: Notification) {
        let Some(n) = self.notifications.post(notification) else { return };
        for session in self.notification_targets() {
            if let Ok(env) =
                Envelope::new(types::NOTIFY_POSTED, &self.notifications.for_session(&n, &session))
            {
                let _ = session.send(env).await;
            }
        }
    }

    pub(crate) async fn notification_removed(&self, key: String) {
        if !self.notifications.remove(&key) {
            return;
        }
        let Ok(env) = Envelope::new(types::NOTIFY_REMOVED, &NotifyKey { key }) else { return };
        for session in self.notification_targets() {
            let _ = session.send(env.clone()).await;
        }
    }

    pub(crate) async fn notifications_reset(&self, items: Vec<Notification>) {
        self.notifications.reset(items);
        for session in self.live_sessions() {
            self.send_notification_snapshot(&session).await;
        }
    }
}

/// Handles a `notify.*` message on a session. Returns false for other types.
pub(crate) async fn handle(shared: &Arc<Shared>, session: &Arc<Session>, env: &Envelope) -> Result<bool> {
    let peer = session.peer;
    match env.t.as_str() {
        // ---- On the PC ----
        types::NOTIFY_SNAPSHOT => {
            if shared.notifications_allowed(&peer) {
                let snapshot: NotifySnapshot = env.body()?;
                let items: Vec<Notification> = snapshot
                    .items
                    .into_iter()
                    .filter_map(Notification::sanitized)
                    .take(SNAPSHOT_ITEMS)
                    .collect();
                let mut changed_thread = None;
                for n in &items {
                    let cached_icon = shared.notifications.peer_icon(peer, &n.app, n.icon.as_deref());
                    if let Ok(Some(tid)) =
                        shared.store.upsert_chat_notification(&peer, n, cached_icon.as_deref(), true)
                    {
                        changed_thread = Some(tid);
                    }
                }
                if let Some(thread) = changed_thread {
                    shared.emit(NodeEvent::SmsChanged { device: peer, thread: Some(thread) });
                }
                shared.emit(NodeEvent::NotificationsReset { device: peer, items });
            }
        }
        types::NOTIFY_POSTED => {
            if shared.notifications_allowed(&peer)
                && let Some(notification) = env.body::<Notification>()?.sanitized()
            {
                let cached_icon =
                    shared.notifications.peer_icon(peer, &notification.app, notification.icon.as_deref());
                if let Ok(Some(thread)) =
                    shared.store.upsert_chat_notification(&peer, &notification, cached_icon.as_deref(), false)
                {
                    shared.emit(NodeEvent::SmsChanged { device: peer, thread: Some(thread) });
                }
                shared.emit(NodeEvent::NotificationPosted { device: peer, notification });
            }
        }
        types::NOTIFY_REMOVED => {
            let NotifyKey { key } = env.body()?;
            if let Ok(Some(thread)) = shared.store.mark_chat_notification_removed(&peer, &key) {
                shared.emit(NodeEvent::SmsChanged { device: peer, thread: Some(thread) });
            }
            shared.emit(NodeEvent::NotificationRemoved { device: peer, key });
        }
        // ---- On the phone ----
        types::NOTIFY_SYNC => shared.send_notification_snapshot(session).await,
        types::NOTIFY_DISMISS => {
            let NotifyKey { key } = env.body()?;
            let reply = match allowed(shared, &peer) {
                Err(reply) => reply,
                Ok(()) => {
                    let platform = shared.platform.clone();
                    match run_blocking(move || platform.dismiss_notification(&key)).await {
                        // Already gone is what the PC wanted.
                        Ok(()) | Err(NotificationError::NotFound) => Envelope::empty(types::OK),
                        Err(e) => error_reply(e),
                    }
                }
            };
            session.send(reply.reply_to(env.id)).await?;
        }
        types::NOTIFY_ACTION => {
            let NotifyAction { key, action, reply } = env.body()?;
            let reply = reply.map(|mut text| {
                if let Some((cut, _)) =
                    text.char_indices().nth(nectarlink_protocol::messages::notify_limits::TEXT_CHARS)
                {
                    text.truncate(cut);
                }
                text
            });
            let answer = match allowed(shared, &peer) {
                Err(answer) => answer,
                Ok(()) => match NotificationActionContext::new(shared, session) {
                    None => {
                        Envelope::error(ErrorCode::Denied, "notification action owner is no longer current")
                    }
                    Some(context) => {
                        let platform = shared.platform.clone();
                        match run_blocking(move || {
                            if !context.is_current() {
                                return Err(NotificationError::NotFound);
                            }
                            platform.run_notification_action_owned(context, &key, &action, reply.as_deref())
                        })
                        .await
                        {
                            Ok(()) => Envelope::empty(types::OK),
                            Err(e) => error_reply(e),
                        }
                    }
                },
            };
            session.send(answer.reply_to(env.id)).await?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Whether a PC may act on this phone's notifications; the error reply if not.
fn allowed(shared: &Shared, peer: &DeviceId) -> std::result::Result<(), Envelope> {
    if !shared.local_capabilities().iter().any(|c| c == "notify.reply") {
        return Err(Envelope::error(ErrorCode::Unsupported, "notification actions are not available"));
    }
    if !shared.notifications_allowed(peer) {
        return Err(Envelope::error(ErrorCode::Denied, "notifications are off for this device"));
    }
    Ok(())
}

fn error_reply(e: NotificationError) -> Envelope {
    match e {
        NotificationError::NotFound => Envelope::error(ErrorCode::NotFound, "no longer exists"),
        NotificationError::Unsupported => Envelope::error(ErrorCode::Unsupported, "not available"),
        NotificationError::Failed(reason) => {
            tracing::warn!(reason, "notification action failed");
            Envelope::error(ErrorCode::Internal, "failed")
        }
    }
}

/// Platform calls may touch the OS; keep them off the async workers.
async fn run_blocking(
    f: impl FnOnce() -> std::result::Result<(), NotificationError> + Send + 'static,
) -> std::result::Result<(), NotificationError> {
    tokio::task::spawn_blocking(f).await.unwrap_or_else(|e| Err(NotificationError::Failed(e.to_string())))
}

/// Per-session record of which apps' icons were sent.
pub(crate) type SentIcons = Mutex<HashSet<String>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> DeviceId {
        DeviceId([7; 32])
    }

    #[test]
    fn same_session_reuses_the_action_token() {
        let feed = Feed::default();
        let mut authority = feed.action_authority();
        let session = 0xA11usize;
        let first = authority.token_for(peer(), session);
        let second = authority.token_for(peer(), session);
        assert!(Arc::ptr_eq(&first, &second));
        assert!(first.load(Ordering::Acquire));
    }

    #[test]
    fn a_different_session_retires_the_previous_action_token() {
        let feed = Feed::default();
        let mut authority = feed.action_authority();
        let id = peer();
        let old = authority.token_for(id, 0x01);
        let current = authority.token_for(id, 0x02);
        assert!(!Arc::ptr_eq(&old, &current));
        assert!(!old.load(Ordering::Acquire));
        assert!(current.load(Ordering::Acquire));
        authority.retire_if_session(&id, 0x01);
        assert!(current.load(Ordering::Acquire));
        authority.retire_if_session(&id, 0x02);
        assert!(!current.load(Ordering::Acquire));
        let later = authority.token_for(id, 0x03);
        assert!(later.load(Ordering::Acquire));
        assert!(!Arc::ptr_eq(&later, &current));
    }
}
