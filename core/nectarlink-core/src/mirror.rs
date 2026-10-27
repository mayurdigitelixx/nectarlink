// SPDX-License-Identifier: MPL-2.0
//! Screen mirroring (docs/protocol/mirror.md): a PC asks a phone for its
//! screen; once the phone's user agrees, the phone streams encoded video
//! on a stream of its own, which the PC decodes and shows.
//!
//! The phone side keeps latency low over throughput: when the network
//! can't keep up, frames are dropped (never queued for long) and the
//! encoder is asked for a keyframe to start clean from.
//!
//! When the PC asks for it and the phone can, the phone's sound comes on a
//! second stream (PCM), dropped rather than delayed in the same way.
//!
//! Each mirroring is a session the PC numbers: 0 for the phone's screen,
//! others for apps the phone runs on displays of their own (Elevated), each
//! in a window of its own on the PC.

use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use iroh::endpoint::{RecvStream, SendStream, VarInt};
use nectarlink_protocol::{
    DeviceId, Envelope, ErrorCode, PacketKind,
    messages::{
        MirrorAudioConfig, MirrorConfig, MirrorInput, MirrorPower, MirrorResize, MirrorSession, MirrorStart,
        PhoneApp, PhoneApps, StreamHeader, mirror, types,
    },
    read_video_packet, video_packet_header, write_frame,
};
use tokio::sync::mpsc;

use crate::{Error, Result, events::NodeEvent, node::Shared, session::Session};

/// The device toggle that allows phone-to-PC mirroring for a device.
pub(crate) const TOGGLE: &str = "mirroring";
/// The per-device toggle that allows PC-to-phone reverse mirroring (off by default).
pub const PC_MIRROR_TOGGLE: &str = "pc_mirror";
/// A local phone-owner grant for exactly one trusted PC; viewing stays separate.
pub const PHONE_CONTROL_TOGGLE: &str = "phone_control";

/// The existing accepted-request map also owns its live video stream.
#[derive(Debug)]
pub(crate) struct IncomingMirror {
    request: u64,
    connection: u64,
    capture: u64,
    video: Option<Weak<AtomicBool>>,
    stop: Option<Weak<tokio::sync::Notify>>,
}

/// Rechecked by the phone platform immediately before injection, after posting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MirrorControlContext {
    pub session: u32,
    /// Local app admission identity; zero for the unchanged screen contract.
    pub capture: u64,
    pub request: u64,
    pub generation: u64,
    pub connection: u64,
}

/// An input queue entry owns the original Node, connection, request and grant
/// epoch. It never reconnects or reads a replacement request when dequeued.
#[derive(Clone)]
pub struct MirrorControl {
    shared: Arc<Shared>,
    session: Arc<Session>,
    request: u64,
    mirroring: u32,
    epoch: u64,
}

impl std::fmt::Debug for MirrorControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirrorControl")
            .field("peer", &self.session.peer)
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

impl MirrorControl {
    pub(crate) fn new(
        shared: Arc<Shared>,
        session: Arc<Session>,
        mirroring: u32,
        request: u64,
    ) -> Result<Self> {
        let control = Self {
            shared,
            epoch: session.mirror_input_epoch.load(Ordering::Acquire),
            session,
            request,
            mirroring,
        };
        if !control.is_current() {
            return Err(Error::Denied);
        }
        Ok(control)
    }

    pub fn is_current(&self) -> bool {
        self.epoch != 0
            && self.session.mirror_input_epoch.load(Ordering::Acquire) == self.epoch
            && self.shared.session(&self.session.peer).is_some_and(|live| Arc::ptr_eq(&live, &self.session))
            && self.shared.mirror_control_reason(&self.session.peer, self.mirroring, self.request)
                == "available"
    }

    /// Check again after outbox backpressure, then enqueue on that exact connection.
    pub async fn send(&self, input: MirrorInput) -> Result<()> {
        self.send_if_current(input, || true).await
    }

    /// The viewer rechecks its Showing window after outbox backpressure too.
    pub async fn send_if_current(&self, input: MirrorInput, showing: impl FnOnce() -> bool) -> Result<()> {
        if !phone_control_input_valid(&input, self.mirroring) {
            return Err(Error::Protocol("invalid phone control input".into()));
        }
        let env = nectarlink_protocol::messages::mirror_control_input_envelope(
            &input,
            self.mirroring,
            self.request,
            self.epoch,
        )?;
        let permit = self.session.reserve().await?;
        if !showing() || !self.is_current() {
            return Err(Error::Denied);
        }
        permit.send(env);
        Ok(())
    }
}

/// An app view retains its original Node, connection and durable request.
/// Capture operations neither require nor grant phone input permission.
#[derive(Clone)]
pub struct MirrorView {
    shared: Arc<Shared>,
    session: Arc<Session>,
    mirroring: u32,
    request: u64,
}

impl std::fmt::Debug for MirrorView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirrorView")
            .field("peer", &self.session.peer)
            .field("session", &self.mirroring)
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

impl MirrorView {
    pub(crate) fn new(
        shared: Arc<Shared>,
        session: Arc<Session>,
        mirroring: u32,
        request: u64,
    ) -> Result<Self> {
        if mirroring == mirror::SCREEN
            || !is_local_desktop(&shared)
            || !shared.mirror_correlated(&session.peer)
            || !shared.peer_supports(&session.peer, mirror::VIRTUAL_DISPLAY)
            || !shared.peer_supports(&session.peer, mirror::APP_WINDOWS_OWNED_V1)
        {
            return Err(Error::Unsupported);
        }
        let target = Self { shared, session, mirroring, request };
        if !target.is_current() {
            return Err(Error::Denied);
        }
        Ok(target)
    }

    pub fn is_current(&self) -> bool {
        self.request != 0
            && !self.shared.cancel.is_cancelled()
            && self.shared.toggle_on(&self.session.peer, TOGGLE)
            && self.shared.store.is_paired(&self.session.peer).unwrap_or(false)
            && self.shared.session(&self.session.peer).is_some_and(|live| Arc::ptr_eq(&live, &self.session))
            && self.shared.peer_supports(&self.session.peer, mirror::VIRTUAL_DISPLAY)
            && self.shared.peer_supports(&self.session.peer, mirror::APP_WINDOWS_OWNED_V1)
            && self.shared.mirror_correlated(&self.session.peer)
            && self
                .shared
                .mirror_outgoing
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&(self.session.peer, self.mirroring))
                == Some(&self.request)
    }

    /// Sender-local durable request, echoed by this accepted capture's stream.
    pub fn request(&self) -> u64 {
        self.request
    }

    /// Retire only this view locally before sending its request-bound Stop.
    /// A replaced Node, Session or outgoing request is never selected.
    pub async fn stop_if_current(&self) -> Result<bool> {
        let retired = self.shared.retire_mirror_view(&self.session, self.mirroring, self.request);
        if !retired {
            return Ok(false);
        }
        stop(&self.session, self.mirroring, Some(self.request)).await?;
        Ok(true)
    }

    /// Reserve on the original outbox, then repeat both viewer and Core admission.
    pub async fn send_if_current(
        &self,
        width: u32,
        height: u32,
        showing: impl FnOnce() -> bool,
    ) -> Result<()> {
        let msg = MirrorResize { session: self.mirroring, width, height, request: Some(self.request) };
        if !msg.is_valid() {
            return Err(Error::Protocol("invalid resize".into()));
        }
        let env = Envelope::new(types::MIRROR_RESIZE, &msg)?;
        let permit = self.session.reserve().await?;
        if !showing() || !self.is_current() {
            return Err(Error::Denied);
        }
        permit.send(env);
        Ok(())
    }
}

fn phone_control_input_valid(input: &MirrorInput, mirroring: u32) -> bool {
    input.is_valid()
        && !matches!(input, MirrorInput::Gamepad { .. })
        && !matches!(input, MirrorInput::Key { key } if key == "keyboard_on" || key == "keyboard_off")
        && (mirroring == mirror::SCREEN
            || !matches!(input, MirrorInput::Key { key } if matches!(key.as_str(), "home" | "recents" | "notifications")))
}

pub(crate) fn is_local_phone(shared: &Shared) -> bool {
    let local = shared.local.read().unwrap_or_else(|e| e.into_inner());
    matches!(local.device.kind, crate::DeviceKind::Phone | crate::DeviceKind::Tablet)
}

fn is_local_desktop(shared: &Shared) -> bool {
    let local = shared.local.read().unwrap_or_else(|e| e.into_inner());
    matches!(crate::features::role_of(&local.device), Some(crate::features::Role::Desktop))
}

fn pc_mirror_allowed(shared: &Shared, peer: &DeviceId) -> bool {
    shared.toggle_on(peer, PC_MIRROR_TOGGLE)
        || shared.pc_mirror_sessions.lock().unwrap_or_else(|e| e.into_inner()).contains(peer)
}
/// Packets waiting to go out, at most. Beyond this the network is behind:
/// sending more would only add delay.
const QUEUE: usize = 4;
/// Sound packets waiting to go out, at most (each is a few milliseconds).
const AUDIO_QUEUE: usize = 24;
/// Icons in a `mirror.apps` answer, at most, in bytes: the rest go without
/// (the answer stays well inside a frame).
const ICONS_BUDGET: usize = 768 * 1024;

/// What the PC's app gets of a mirroring stream. Called from the core's
/// threads; must return quickly (hand the work to a thread of its own).
pub trait MirrorSink: Send + Sync {
    /// The stream's format: first, and again when it changes.
    fn config(&self, config: MirrorConfig);
    /// Encoded video (Annex B), `keyframe` when it starts fresh.
    fn packet(&self, keyframe: bool, time_us: u64, data: Vec<u8>);
    /// The stream ended (the phone stopped, or the connection dropped).
    fn ended(&self);
    /// The sound's format: first on the sound stream, and when it changes.
    fn audio_config(&self, _config: MirrorAudioConfig) {}
    /// Sound, in that format.
    fn audio(&self, _time_us: u64, _data: Vec<u8>) {}
    /// The sound stream ended.
    fn audio_ended(&self) {}
}

impl std::fmt::Debug for dyn MirrorSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MirrorSink")
    }
}

// ---- The PC ----

fn prepare_start(
    shared: &Shared,
    session: &Session,
    options: MirrorStart,
) -> Result<(MirrorStart, Envelope)> {
    let allowed = if is_local_desktop(shared) {
        shared.toggle_on(&session.peer, TOGGLE)
    } else {
        shared.store.toggle(&session.peer, PC_MIRROR_TOGGLE)?.unwrap_or(true)
    };
    if !allowed {
        return Err(Error::Denied);
    }
    if !options.is_valid() {
        return Err(Error::Protocol("invalid mirroring options".into()));
    }
    // This Mac must not launch capture in a phone's old unsafe app pipeline.
    if shared.local.read().unwrap_or_else(|e| e.into_inner()).device.os == "macos"
        && is_local_desktop(shared)
        && options.app.is_some()
        && !shared.peer_supports(&session.peer, mirror::APP_WINDOWS_OWNED_V1)
    {
        return Err(Error::Unsupported);
    }
    // This request's generation: the phone echoes it in the stream's config
    // packets (and honors it in `mirror.stop`), so a delayed stream from an
    // older request can't pass for this one. Optional on the wire: peers
    // that predate correlation ignore it.
    let request = shared.store.next_mirror_request()?;
    let options = MirrorStart { request: Some(request), ..options };
    let env = Envelope::new(types::MIRROR_START, &options)?;
    // A peer can open its media stream before the control reply arrives.
    shared.note_mirror_outgoing(&session.peer, options.session, request);
    Ok((options, env))
}

async fn finish_start(shared: &Shared, session: &Session, options: MirrorStart, env: Envelope) -> Result<()> {
    let request = options.request.expect("prepare_start allocated the request");
    let result = session
        .request(env, crate::session::REQUEST_TIMEOUT)
        .await
        .and_then(|reply| reply.expect(types::OK).map_err(Error::from));
    if result.is_err() {
        let mut outgoing = shared.mirror_outgoing.lock().unwrap_or_else(|e| e.into_inner());
        if outgoing.get(&(session.peer, options.session)) == Some(&request) {
            outgoing.remove(&(session.peer, options.session));
        }
    }
    result
}

pub(crate) async fn start(shared: &Shared, session: &Session, options: MirrorStart) -> Result<()> {
    let (options, env) = prepare_start(shared, session, options)?;
    finish_start(shared, session, options, env).await
}

/// Publish the exact app view before the start request can wait for consent/reply.
pub(crate) async fn start_owned(
    shared: &Arc<Shared>,
    session: &Arc<Session>,
    options: MirrorStart,
    ready: impl FnOnce(MirrorView) -> bool,
) -> Result<()> {
    if options.app.is_none()
        || options.watch.is_some()
        || options.session == mirror::SCREEN
        || !is_local_desktop(shared)
        || !shared.mirror_correlated(&session.peer)
        || !shared.peer_supports(&session.peer, mirror::VIRTUAL_DISPLAY)
        || !shared.peer_supports(&session.peer, mirror::APP_WINDOWS_OWNED_V1)
    {
        return Err(Error::Unsupported);
    }
    let (options, env) = prepare_start(shared, session, options)?;
    let request = options.request.expect("prepare_start allocated the request");
    let view =
        MirrorView { shared: shared.clone(), session: session.clone(), mirroring: options.session, request };
    if !view.is_current() || !ready(view.clone()) {
        let mut outgoing = shared.mirror_outgoing.lock().unwrap_or_else(|e| e.into_inner());
        if outgoing.get(&(session.peer, options.session)) == Some(&request) {
            outgoing.remove(&(session.peer, options.session));
        }
        return Err(Error::Denied);
    }
    if let Err(error) = finish_start(shared, session, options, env).await {
        // A timed-out start may still be completing at the source. Release
        // only the original wire request on its original connection.
        let _ = stop(session, view.mirroring, Some(request)).await;
        return Err(error);
    }
    if !view.is_current() {
        // Close may have queued Stop before this Start reached the source.
        let _ = stop(session, view.mirroring, Some(request)).await;
        return Err(Error::Denied);
    }
    Ok(())
}

pub(crate) async fn stop(session: &Session, mirroring: u32, request: Option<u64>) -> Result<()> {
    let env = Envelope::new(types::MIRROR_STOP, &MirrorSession { session: mirroring, request })?;
    session.request(env, crate::session::REQUEST_TIMEOUT).await?.expect(types::OK)?;
    Ok(())
}

/// Mouse and keyboard on a phone's mirrored screen or app window (not
/// answered: input is only worth it right away).
pub(crate) async fn input(
    shared: &Shared,
    session: &Session,
    mirroring: u32,
    input: MirrorInput,
) -> Result<()> {
    let allowed = if is_local_desktop(shared) {
        shared.toggle_on(&session.peer, TOGGLE)
    } else {
        shared.store.toggle(&session.peer, PC_MIRROR_TOGGLE)?.unwrap_or(true)
            && shared.toggle_on(&session.peer, crate::remote::TOGGLE)
    };
    if !allowed {
        return Err(Error::Denied);
    }
    if !input.is_valid() {
        return Err(Error::Protocol("invalid input".into()));
    }
    session.send(input_envelope(&input, mirroring)?).await
}

fn input_envelope(input: &MirrorInput, mirroring: u32) -> Result<Envelope> {
    Ok(nectarlink_protocol::messages::mirror_input_envelope(input, mirroring)?)
}

/// The phone owner's Stop on a PC's typing (screen input, session 0), per
/// peer. Every admitted screen input carries the generation it was admitted
/// under to the platform, which checks it again right before the OS sees it
/// ([`Shared::mirror_typing_current`]). Generations never repeat across a
/// stored node lifetime: node start, Stop, owner rearm and unpair each move
/// them, so input admitted before any of those never lands after it. Only the
/// phone owner changes this; nothing a peer sends does.
#[derive(Debug, Clone)]
pub(crate) struct TypingFence {
    next: u64,
    /// The generation of every peer without an entry; moves on Node start and unpair.
    floor: u64,
    /// Peers the owner stopped (and maybe rearmed since), until unpaired.
    peers: std::collections::HashMap<DeviceId, Typing>,
    /// A durable marker that this phone Node may still be admitting input.
    node_running: bool,
    /// Exact Store value observed or last committed by this Node; protects a newer Node's row.
    persisted_value: Option<String>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
struct Typing {
    generation: u64,
    stopped: bool,
    /// The epoch the phone owner's last mirroring "off" was tapped at
    /// ([`Shared::mirror_typing_epoch`]). Only an off tapped after the Stop
    /// carries the Stop's own generation; a Stop clears it.
    owner_off: Option<u64>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredTypingFence {
    version: u8,
    next: u64,
    floor: u64,
    peers: std::collections::BTreeMap<String, Typing>,
    #[serde(default)]
    node_running: Option<bool>,
}

impl Default for TypingFence {
    fn default() -> Self {
        Self {
            next: 2,
            floor: 1,
            peers: std::collections::HashMap::new(),
            node_running: false,
            persisted_value: None,
        }
    }
}

impl TypingFence {
    fn advance(&mut self) -> u64 {
        let generation = self.next;
        self.next = self.next.saturating_add(1);
        generation
    }

    fn current(&self, peer: &DeviceId) -> Typing {
        self.peers.get(peer).copied().unwrap_or(Typing {
            generation: self.floor,
            stopped: false,
            owner_off: None,
        })
    }

    pub(crate) fn load(store: &crate::store::Store) -> Result<Self> {
        let Some(value) = store.mirror_typing_fence()? else { return Ok(Self::default()) };
        let stored: StoredTypingFence = serde_json::from_str(&value)
            .map_err(|error| Error::Internal(format!("invalid persisted mirror typing fence: {error}")))?;
        let node_running = match (stored.version, stored.node_running) {
            // Legacy state has no clean-shutdown marker, so recover it as unclean.
            (1, None) => true,
            (2, Some(node_running)) => node_running,
            _ => return Err(Error::Internal("invalid persisted mirror typing fence state".into())),
        };
        if stored.floor == 0
            || stored.next <= stored.floor
            || stored.peers.values().any(|typing| typing.generation == 0 || typing.generation >= stored.next)
        {
            return Err(Error::Internal("invalid persisted mirror typing fence state".into()));
        }
        let peers = stored
            .peers
            .into_iter()
            .map(|(id, typing)| {
                id.parse::<DeviceId>()
                    .map(|id| (id, typing))
                    .map_err(|_| Error::Internal("invalid peer in persisted mirror typing fence".into()))
            })
            .collect::<Result<std::collections::HashMap<_, _>>>()?;
        Ok(Self { next: stored.next, floor: stored.floor, peers, node_running, persisted_value: Some(value) })
    }

    pub(crate) fn node_was_running(&self) -> bool {
        self.node_running
    }

    /// Whether this fence was absent from Store. With existing paired peers,
    /// an absent fence cannot prove the prior phone lifetime ended cleanly.
    pub(crate) fn has_no_durable_state(&self) -> bool {
        self.persisted_value.is_none()
    }

    /// On an unclean restart, deny every still-paired peer and clear orphaned
    /// entries. Persist the deny before the caller continues Node startup; the
    /// stopped entries remain until the phone owner explicitly rearms them.
    pub(crate) fn recover_unclean_node_lifetime(
        &mut self,
        store: &crate::store::Store,
        paired_peers: &[DeviceId],
    ) -> Result<()> {
        let paired: std::collections::HashSet<_> = paired_peers.iter().copied().collect();
        self.floor = self.advance();
        self.peers.retain(|peer, _| paired.contains(peer));
        for peer in paired_peers {
            let generation = self.advance();
            self.peers.insert(*peer, Typing { generation, stopped: true, owner_off: None });
        }
        self.node_running = false;
        self.persist(store)
    }

    /// Moves admissions to this new Node lifetime and discards any
    /// half-completed owner off tap. A fresh off → on is required afterward.
    pub(crate) fn begin_node_lifetime(&mut self) {
        self.floor = self.advance();
        let peers: Vec<_> = self.peers.keys().copied().collect();
        for peer in peers {
            let generation = self.advance();
            if let Some(typing) = self.peers.get_mut(&peer) {
                typing.generation = generation;
                typing.owner_off = None;
            }
        }
        self.node_running = true;
    }

    pub(crate) fn end_node_lifetime(&mut self, store: &crate::store::Store) -> Result<()> {
        if self.persisted_value.is_none() {
            return Ok(());
        }
        let mut stopped = self.clone();
        stopped.node_running = false;
        stopped.persist(store)?;
        *self = stopped;
        Ok(())
    }

    pub(crate) fn persist(&mut self, store: &crate::store::Store) -> Result<()> {
        let stored = StoredTypingFence {
            version: 2,
            next: self.next,
            floor: self.floor,
            peers: self.peers.iter().map(|(id, typing)| (id.to_string(), *typing)).collect(),
            node_running: Some(self.node_running),
        };
        let value = serde_json::to_string(&stored)
            .map_err(|error| Error::Internal(format!("can't encode mirror typing fence: {error}")))?;
        store.compare_and_set_mirror_typing_fence(self.persisted_value.as_deref(), &value)?;
        self.persisted_value = Some(value);
        Ok(())
    }
}

impl Shared {
    fn typing_fence(&self) -> std::sync::MutexGuard<'_, TypingFence> {
        self.mirror_typing.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The generation `peer`'s screen input is admitted under, or `None`
    /// while the phone owner has it stopped.
    pub(crate) fn mirror_typing_admit(&self, peer: &DeviceId) -> Option<u64> {
        let typing = self.typing_fence().current(peer);
        (!typing.stopped).then_some(typing.generation)
    }

    pub(crate) fn mirror_typing_stopped(&self, peer: &DeviceId) -> bool {
        self.typing_fence().current(peer).stopped
    }

    /// Whether screen input admitted under `generation` may still reach the
    /// OS: not stopped since, and no rearm or unpair in between.
    pub(crate) fn mirror_typing_current(&self, peer: &DeviceId, generation: u64) -> bool {
        let typing = self.typing_fence().current(peer);
        !typing.stopped && typing.generation == generation
    }

    /// `peer`'s typing epoch now. The phone owner's mirroring switch reads it
    /// when tapped; a later Stop moves it, which voids that tap's resume.
    pub(crate) fn mirror_typing_epoch(&self, peer: &DeviceId) -> u64 {
        self.typing_fence().current(peer).generation
    }

    /// The phone owner's Stop: drops `peer`'s screen input from now on,
    /// including input already admitted and on its way to the OS. Every
    /// owner switch tap made before it can no longer end it.
    pub(crate) fn stop_mirror_typing(&self, peer: &DeviceId) -> Result<u64> {
        let mut fence = self.typing_fence();
        let generation = fence.advance();
        fence.peers.insert(*peer, Typing { generation, stopped: true, owner_off: None });
        // Keep the in-memory denial even when persistence fails. The durable
        // running marker makes the next Node start recover all paired peers
        // fail-closed instead of trusting the old allow-on value.
        fence.persist(&self.store)?;
        Ok(generation)
    }

    /// The phone owner turned mirroring for `peer` off, tapped at epoch
    /// `tapped_at`.
    pub(crate) fn note_owner_mirroring_off(&self, peer: &DeviceId, tapped_at: u64) -> Result<()> {
        let mut fence = self.typing_fence();
        if fence.peers.contains_key(peer) {
            let mut noted = fence.clone();
            let typing = noted.peers.get_mut(peer).expect("peer checked above");
            typing.owner_off = Some(tapped_at);
            noted.persist(&self.store)?;
            *fence = noted;
        }
        Ok(())
    }

    /// The phone owner turned mirroring for `peer` back on, tapped at epoch
    /// `tapped_at`: a Stop ends, under a fresh generation, only when both
    /// that on and the off before it were tapped after the Stop (no Stop
    /// since either). Checked and changed under one lock, so a Stop can't
    /// slip in between. True when it ended one.
    pub(crate) fn rearm_mirror_typing(&self, peer: &DeviceId, tapped_at: u64) -> Result<bool> {
        let mut fence = self.typing_fence();
        let typing = fence.current(peer);
        if !typing.stopped || typing.owner_off != Some(typing.generation) || tapped_at != typing.generation {
            return Ok(false);
        }
        let mut resumed = fence.clone();
        let generation = resumed.advance();
        resumed.peers.insert(*peer, Typing { generation, stopped: false, owner_off: None });
        resumed.persist(&self.store)?;
        *fence = resumed;
        Ok(true)
    }

    /// `peer` is unpaired: its entry goes, and the floor moves, so nothing
    /// admitted before (stopped or not) reaches the OS after a re-pairing.
    pub(crate) fn forget_mirror_typing(&self, peer: &DeviceId) -> Result<()> {
        let mut fence = self.typing_fence();
        // Unpair has already removed the peer and cannot be rolled back. Drop
        // its in-memory authority before persisting; the running marker keeps
        // recovery fail-closed if this write also fails.
        fence.peers.remove(peer);
        let floor = fence.advance();
        fence.floor = floor;
        fence.persist(&self.store)
    }
}

pub(crate) async fn request_keyframe(session: &Session, mirroring: u32) {
    if let Ok(env) =
        Envelope::new(types::MIRROR_KEYFRAME, &MirrorSession { session: mirroring, request: None })
    {
        let _ = session.send(env).await;
    }
}

/// Resize an app window's display (`mirroring` != 0) on a phone.
pub(crate) async fn resize(
    shared: &Shared,
    session: &Session,
    mirroring: u32,
    width: u32,
    height: u32,
) -> Result<()> {
    if !shared.toggle_on(&session.peer, TOGGLE) {
        return Err(Error::Denied);
    }
    let msg = MirrorResize { session: mirroring, width, height, request: None };
    if !msg.is_valid() {
        return Err(Error::Protocol("invalid resize".into()));
    }
    // Owned apps must retain their original view; old peers keep their existing API.
    if shared.mirror_correlated(&session.peer)
        && shared.peer_supports(&session.peer, mirror::APP_WINDOWS_OWNED_V1)
    {
        return Err(Error::Denied);
    }
    session.send(Envelope::new(types::MIRROR_RESIZE, &msg)?).await
}

/// Update `stay_awake` and `screen_off` on a phone while mirroring its screen (`session == 0`).
pub(crate) async fn power(
    shared: &Shared,
    session: &Session,
    stay_awake: bool,
    screen_off: bool,
) -> Result<()> {
    if !shared.toggle_on(&session.peer, TOGGLE) {
        return Err(Error::Denied);
    }
    let msg = MirrorPower { stay_awake, screen_off };
    session.send(Envelope::new(types::MIRROR_POWER, &msg)?).await
}

/// The apps a phone can open in windows of their own, by name.
pub(crate) async fn apps(shared: &Shared, session: &Session) -> Result<Vec<PhoneApp>> {
    if !shared.toggle_on(&session.peer, TOGGLE) {
        return Err(Error::Denied);
    }
    let env = Envelope::empty(types::MIRROR_APPS);
    let PhoneApps { mut apps } =
        session.request(env, crate::session::REQUEST_TIMEOUT).await?.expect_body(types::MIRROR_APPS)?;
    apps.retain(|a| nectarlink_protocol::messages::is_package_name(&a.pkg));
    apps.truncate(mirror::MAX_APPS);
    for app in &mut apps {
        app.label = app.label.chars().take(100).collect();
        if app.icon.as_ref().is_some_and(|i| i.len() > mirror::MAX_ICON_BYTES) {
            app.icon = None;
        }
    }
    apps.sort_by_cached_key(|a| a.label.to_lowercase());
    Ok(apps)
}

/// A phone opened a video stream (its header already read). Its first
/// packet says which mirroring it is.
pub(crate) async fn receive(
    shared: Arc<Shared>,
    session: Arc<Session>,
    mut send: SendStream,
    mut recv: RecvStream,
) {
    let peer = session.peer;
    let refuse = |mut send: SendStream, mut recv: RecvStream| {
        let _ = send.reset(VarInt::from_u32(mirror::STOPPED));
        let _ = recv.stop(VarInt::from_u32(mirror::STOPPED));
    };
    let shows = shared.we_offer(mirror::VIEW);
    let allowed = if is_local_desktop(&shared) {
        shared.toggle_on(&peer, TOGGLE)
    } else {
        shared.store.toggle(&peer, PC_MIRROR_TOGGLE).ok().flatten().unwrap_or(true)
    };
    if !shows || !allowed || !shared.session(&peer).is_some_and(|live| Arc::ptr_eq(&live, &session)) {
        return refuse(send, recv);
    }
    let first = match read_video_packet(&mut recv).await {
        Ok(Some(p)) if p.kind == PacketKind::Config => {
            let config = MirrorConfig::from_cbor(&p.data).ok().filter(|c| c.is_valid());
            if config.is_none() {
                tracing::warn!("a phone sent a video format this PC can't show");
            }
            config
        }
        Ok(Some(_)) => {
            tracing::warn!("a phone's video stream started without its format");
            None
        }
        Ok(None) | Err(_) => {
            tracing::warn!("a phone's video stream ended before its first frame");
            None
        }
    };
    let Some(config) = first else {
        return refuse(send, recv);
    };
    let mirroring = config.session;
    // A delayed stream from an older request must not pass for the live
    // one. Only where both peers negotiate correlation; otherwise (old
    // phones, unstamped streams) behavior is unchanged.
    let expected =
        shared.mirror_outgoing.lock().unwrap_or_else(|e| e.into_inner()).get(&(peer, mirroring)).copied();
    let correlated = shared.mirror_correlated(&peer)
        || (mirroring != mirror::SCREEN && shared.peer_supports(&peer, mirror::APP_WINDOWS_OWNED_V1));
    if !shared.session(&peer).is_some_and(|live| Arc::ptr_eq(&live, &session))
        || !mirror_token_accepted(expected, config.request, correlated)
    {
        tracing::warn!("a phone's video stream answered an older mirroring request");
        return refuse(send, recv);
    };
    let Some(sink) = shared.platform.mirror_sink(&peer, mirroring) else {
        return refuse(send, recv);
    };
    // Ready to be stopped before anyone hears it's showing: a stop sent the
    // moment it appears must not be lost.
    let request = config.request;
    let current = || {
        !shared.cancel.is_cancelled()
            && shared.session(&peer).is_some_and(|live| Arc::ptr_eq(&live, &session))
            && (!correlated
                || shared
                    .mirror_outgoing
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&(peer, mirroring))
                    .copied()
                    == request)
    };
    let Some(stopped) = shared.register_mirror_view_reader(&session, mirroring, request, correlated) else {
        return refuse(send, recv);
    };
    let started_at = std::time::Instant::now();
    let title = if mirroring == mirror::SCREEN {
        "Screen mirroring".to_owned()
    } else {
        format!("App window #{mirroring}")
    };
    let tl_row = shared.record_timeline(crate::timeline::NewTimelineEntry {
        kind: crate::TimelineKind::Session,
        device_id: peer,
        device_name: shared.peer_name(&peer),
        incoming: true,
        timestamp: crate::now_unix(),
        title,
        detail: format!("{}×{}", config.width, config.height),
        target: "mirror".into(),
        size_bytes: 0,
        duration_secs: 0,
        ref_id: None,
    });
    shared.emit(NodeEvent::Mirroring { device: peer, session: mirroring, on: true });
    sink.config(config);
    loop {
        let packet = tokio::select! {
            _ = stopped.notified() => break,
            packet = read_video_packet(&mut recv) => packet,
        };
        if !current() {
            break;
        }
        match packet {
            Ok(Some(p)) if p.kind == PacketKind::Config => match MirrorConfig::from_cbor(&p.data) {
                Ok(config)
                    if config.is_valid()
                        && config.session == mirroring
                        && (!correlated || config.request == request) =>
                {
                    sink.config(config)
                }
                _ => {
                    tracing::warn!("a phone sent a video format this PC can't show");
                    break;
                }
            },
            Ok(Some(p)) => sink.packet(p.kind == PacketKind::Keyframe, p.time_us, p.data),
            Ok(None) => break,
            Err(e) => {
                tracing::debug!(error = %e, "mirroring stream ended");
                break;
            }
        }
    }
    let _ = recv.stop(VarInt::from_u32(mirror::STOPPED));
    let _ = send.finish();
    sink.ended();
    if let Some(row_id) = tl_row {
        shared.finish_timeline_session(row_id, started_at.elapsed().as_secs().max(1));
    }
    if current() {
        shared.emit(NodeEvent::Mirroring { device: peer, session: mirroring, on: false });
    }
}

/// A phone opened a sound stream (its header already read).
pub(crate) async fn receive_audio(
    shared: Arc<Shared>,
    peer: DeviceId,
    mut send: SendStream,
    mut recv: RecvStream,
) {
    // Sound comes with the screen only.
    let listens = shared.we_offer(mirror::LISTEN);
    let sink = listens.then(|| shared.platform.mirror_sink(&peer, mirror::SCREEN)).flatten();
    let Some(sink) = sink.filter(|_| shared.toggle_on(&peer, TOGGLE)) else {
        let _ = send.reset(VarInt::from_u32(mirror::STOPPED));
        let _ = recv.stop(VarInt::from_u32(mirror::STOPPED));
        return;
    };
    // Sound has no token of its own: when correlation is negotiated, it
    // only answers a live screen request.
    if shared.mirror_correlated(&peer)
        && !shared
            .mirror_outgoing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&(peer, mirror::SCREEN))
    {
        let _ = send.reset(VarInt::from_u32(mirror::STOPPED));
        let _ = recv.stop(VarInt::from_u32(mirror::STOPPED));
        return;
    }
    let stopped = shared.new_mirror_stop(&peer, mirror::SCREEN, mirror::OP_AUDIO);
    loop {
        let packet = tokio::select! {
            _ = stopped.notified() => break,
            packet = read_video_packet(&mut recv) => packet,
        };
        match packet {
            Ok(Some(p)) if p.kind == PacketKind::Config => match MirrorAudioConfig::from_cbor(&p.data) {
                Ok(config) if config.is_valid() => sink.audio_config(config),
                _ => {
                    tracing::warn!("a phone sent a sound format this PC can't play");
                    break;
                }
            },
            Ok(Some(p)) => sink.audio(p.time_us, p.data),
            Ok(None) => break,
            Err(e) => {
                tracing::debug!(error = %e, "mirrored sound ended");
                break;
            }
        }
    }
    let _ = recv.stop(VarInt::from_u32(mirror::STOPPED));
    let _ = send.finish();
    sink.audio_ended();
}

// Ownership-aware peers cannot enter the separate upstream legacy app path.
fn legacy_app_protocol(correlated: bool, input_request: bool, owned_apps: bool) -> bool {
    !correlated && !input_request && !owned_apps
}

impl Shared {
    pub(crate) fn legacy_app_peer(&self, peer: &DeviceId) -> bool {
        legacy_app_protocol(
            self.mirror_correlated(peer), self.peer_supports(peer, mirror::INPUT_REQUEST),
            self.peer_supports(peer, mirror::APP_WINDOWS_OWNED_V1),
        )
    }

    pub(crate) fn legacy_app_current(&self, peer: &DeviceId, mirroring: u32, connection: u64) -> bool {
        mirroring != mirror::SCREEN && connection != 0 && !self.cancel.is_cancelled()
            && is_local_phone(self) && self.legacy_app_peer(peer) && self.toggle_on(peer, TOGGLE)
            && self.we_offer(mirror::VIRTUAL_DISPLAY)
            && self.session(peer).is_some_and(|live| live.mirror_control_id == connection)
            && self.mirror_legacy_apps.lock().unwrap_or_else(|e| e.into_inner())
                .get(&(*peer, mirroring)) == Some(&connection)
    }

    fn take_legacy_app(&self, peer: &DeviceId, mirroring: u32, connection: u64) -> bool {
        let mut legacy = self.mirror_legacy_apps.lock().unwrap_or_else(|e| e.into_inner());
        if legacy.get(&(*peer, mirroring)) != Some(&connection) { return false; }
        legacy.remove(&(*peer, mirroring));
        true
    }
}

// ---- The phone ----

/// Handles `mirror.*` on a session. Returns false for other types.
pub(crate) async fn handle(shared: &Arc<Shared>, session: &Arc<Session>, env: &Envelope) -> Result<bool> {
    let peer = session.peer;
    let platform = shared.platform.clone();
    match env.t.as_str() {
        types::MIRROR_START => {
            let admission_epoch = shared.mirror_epoch(&peer);
            if !shared.session(&peer).is_some_and(|live| Arc::ptr_eq(&live, session)) {
                return Ok(true);
            }
            let options: MirrorStart = env.body()?;
            let desktop = is_local_desktop(shared);
            let legacy_app = options.app.is_some() && is_local_phone(shared) && shared.legacy_app_peer(&peer);
            let needs = if options.app.is_some() {
                mirror::VIRTUAL_DISPLAY
            } else if desktop && options.watch.is_some() {
                nectarlink_protocol::messages::media_watch::STREAM
            } else {
                mirror::CAPTURE
            };
            let allowed = match &options.watch {
                // Play on phone: only what this PC offered that phone.
                Some(player) if desktop => crate::media::watch_allowed(shared, &peer, player),
                Some(_) => false,
                None if desktop => pc_mirror_allowed(shared, &peer),
                None => shared.toggle_on(&peer, TOGGLE),
            };
            let reply = if !options.is_valid() {
                Envelope::error(ErrorCode::BadMessage, "invalid mirroring options")
            } else if (shared.mirror_correlated(&peer) || (options.app.is_some() && !legacy_app)) && options.request.is_none()
            {
                Envelope::error(ErrorCode::BadMessage, "missing mirror request generation")
            } else if !allowed {
                if desktop && options.app.is_none() && options.watch.is_none() {
                    shared.emit(NodeEvent::PcMirrorRequested { device: peer, options: options.clone() });
                }
                Envelope::error(ErrorCode::Denied, "mirroring is off for this device")
            } else if !shared.we_offer(needs) {
                Envelope::error(ErrorCode::Unsupported, "this device doesn't share that")
            } else {
                // Copied out: the consent call below moves `options`.
                let accepted_session = options.session;
                let accepted_request = options.request;
                let registered = if legacy_app {
                    shared.admit_legacy_app(session, accepted_session, admission_epoch).then_some(0)
                } else { match accepted_request {
                    Some(request) if options.app.is_some() => {
                        shared.admit_mirror_app(session, accepted_session, request, admission_epoch)
                    }
                    Some(request) => shared.note_mirror_incoming(
                        &peer,
                        accepted_session,
                        request,
                        session.mirror_control_id,
                    ),
                    None => Some(0),
                } };
                if let Some(capture) = registered {
                    let connection = session.mirror_control_id;
                    match tokio::task::spawn_blocking(move || {
                        if legacy_app {
                            platform.mirror_app_requested_legacy(&peer, &options, connection)
                        } else {
                            platform.mirror_requested_owned(&peer, &options, connection, capture)
                        }
                    })
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()))
                    {
                        Ok(()) => {
                            // This request's generation is live: the stream that
                            // answers must echo it, and a stop for any other one
                            // is stale.
                            if !shared.session(&peer).is_some_and(|live| Arc::ptr_eq(&live, session)) {
                                Envelope::error(ErrorCode::Denied, "the mirror connection was replaced")
                            } else {
                                Envelope::empty(types::OK)
                            }
                        }
                        Err(reason) => {
                            if legacy_app { shared.take_legacy_app(&peer, accepted_session, connection); }
                            if let Some(request) = accepted_request.filter(|_| !legacy_app) {
                                shared.take_mirror_incoming_request(
                                    &peer,
                                    accepted_session,
                                    request,
                                    connection,
                                    capture,
                                );
                            }
                            tracing::warn!(reason, "can't ask to share the screen");
                            if desktop {
                                Envelope::error(ErrorCode::Denied, reason)
                            } else {
                                Envelope::error(ErrorCode::Internal, "the phone couldn't ask")
                            }
                        }
                    }
                } else {
                    Envelope::error(ErrorCode::Denied, "the app request was revoked or replaced")
                }
            };
            session.send(reply.reply_to(env.id)).await?;
        }
        types::MIRROR_STOP => {
            let MirrorSession { session: mirroring, request } = env.body()?;
            let source: nectarlink_protocol::messages::MirrorSourceStop = env.body()?;
            if source.source_stopped {
                // A source decline applies only to our outgoing request. It must
                // never invoke this PC's capture-stop callback or affect a newer request.
                if shared.mirror_correlated(&peer)
                    && shared.peer_supports(&peer, mirror::INPUT_REQUEST)
                    && shared.session(&peer).is_some_and(|live| Arc::ptr_eq(&live, session))
                    && let Some(request) = request
                    && shared.retire_mirror_view(session, mirroring, request)
                {
                    shared.emit(NodeEvent::Mirroring { device: peer, session: mirroring, on: false });
                }
                session.send(Envelope::empty(types::OK).reply_to(env.id)).await?;
                return Ok(true);
            }
            if mirroring != mirror::SCREEN && shared.legacy_app_peer(&peer)
                && shared.take_legacy_app(&peer, mirroring, session.mirror_control_id)
            {
                let connection = session.mirror_control_id;
                tokio::task::spawn_blocking(move || platform.mirror_app_stop_requested_legacy(&peer, mirroring, connection));
                session.send(Envelope::empty(types::OK).reply_to(env.id)).await?;
                return Ok(true);
            }
            if is_local_phone(shared)
                && (mirroring != mirror::SCREEN
                    || (shared.local_capabilities().iter().any(|cap| cap == mirror::INPUT_REQUEST)
                        && shared.peer_supports(&peer, mirror::INPUT_REQUEST)))
                && (request.is_none()
                    || !shared.session(&peer).is_some_and(|live| Arc::ptr_eq(&live, session)))
            {
                session.send(Envelope::empty(types::OK).reply_to(env.id)).await?;
                return Ok(true);
            }
            // A stop for a superseded request is stale (a replacement start
            // already superseded it): ack it, change nothing.
            if let Some(capture) =
                shared.mirror_stop_applies(&peer, mirroring, request, session.mirror_control_id)
            {
                // The PC's side: stop showing; the phone's: stop sharing.
                shared.stop_showing(&peer, mirroring);
                if mirroring == mirror::SCREEN {
                    shared.pc_mirror_sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(&peer);
                    // Watching stopped: the phone may come back to it for a while
                    // (the screen rotated, the app went to the background).
                    crate::media::renew_watch(shared, &peer);
                }
                shared.emit(NodeEvent::Mirroring { device: peer, session: mirroring, on: false });
                let connection = session.mirror_control_id;
                tokio::task::spawn_blocking(move || {
                    platform.mirror_stop_requested_owned(&peer, mirroring, request, connection, capture)
                });
            }
            session.send(Envelope::empty(types::OK).reply_to(env.id)).await?;
        }
        types::MIRROR_KEYFRAME => {
            let MirrorSession { session: mirroring, .. } = env.body()?;
            let original = session.clone();
            let shared = shared.clone();
            tokio::task::spawn_blocking(move || {
                if mirroring == mirror::SCREEN || !shared.legacy_app_peer(&peer)
                    || shared.legacy_app_current(&peer, mirroring, original.mirror_control_id)
                { platform.mirror_keyframe_requested(&peer, mirroring); }
            });
        }
        types::MIRROR_RESIZE => {
            let resize: MirrorResize = env.body()?;
            let allowed = shared.toggle_on(&peer, TOGGLE)
                && shared.local_capabilities().iter().any(|c| c == mirror::VIRTUAL_DISPLAY);
            if allowed && resize.is_valid()
                && shared.legacy_app_current(&peer, resize.session, session.mirror_control_id)
            {
                let connection = session.mirror_control_id;
                let shared = shared.clone();
                tokio::task::spawn_blocking(move || {
                    if shared.legacy_app_current(&peer, resize.session, connection) {
                        platform.mirror_resize_requested(&peer, resize.session, resize.width, resize.height);
                    }
                });
                return Ok(true);
            }
            // App resizes are capture operations. Mint authority from the local
            // accepted request and authenticated Session, never from sender IDs.
            if allowed
                && resize.is_valid()
                && shared.session(&peer).is_some_and(|live| Arc::ptr_eq(&live, session))
                && let Some(request) = resize.request.filter(|request| *request != 0)
                && let Some(context) = shared.mirror_capture_context(&peer, resize.session, request)
                && context.connection == session.mirror_control_id
            {
                let shared = shared.clone();
                tokio::task::spawn_blocking(move || {
                    if shared.mirror_capture_current(&peer, context) {
                        platform.mirror_resize_requested_owned(&peer, resize.width, resize.height, context);
                    }
                });
            }
        }
        types::MIRROR_POWER => {
            let power: MirrorPower = env.body()?;
            let allowed = shared.toggle_on(&peer, TOGGLE) && shared.we_offer(mirror::CAPTURE);
            if allowed {
                tokio::task::spawn_blocking(move || {
                    platform.mirror_power_requested(&peer, power.stay_awake, power.screen_off);
                });
            }
        }
        types::MIRROR_INPUT => {
            let input: MirrorInput = env.body()?;
            let MirrorSession { session: mirroring, .. } = env.body()?;
            if mirroring != mirror::SCREEN && shared.legacy_app_peer(&peer) {
                let requires_owner_grant = shared.local_capabilities().iter().any(|cap| cap == mirror::INPUT_REQUEST);
                if input.is_valid() && shared.legacy_app_current(&peer, mirroring, session.mirror_control_id)
                    && (!requires_owner_grant || shared.phone_control_granted(&peer))
                    && let Some(generation) = shared.mirror_typing_admit(&peer)
                {
                    platform.mirror_app_input_legacy(&peer, mirroring, input, session.mirror_control_id, generation);
                }
                return Ok(true);
            }
            if !is_local_desktop(shared)
                && shared.local_capabilities().iter().any(|cap| cap == mirror::INPUT_REQUEST)
            {
                let owner: nectarlink_protocol::messages::MirrorInputOwner = env.body()?;
                if let (Some(request), Some(generation)) = (owner.request, owner.input_epoch)
                    && let Some(owner) = shared.mirror_capture_context(&peer, mirroring, request)
                {
                    let context =
                        MirrorControlContext { generation, connection: session.mirror_control_id, ..owner };
                    if phone_control_input_valid(&input, mirroring)
                        && shared.phone_control_current(&peer, context)
                    {
                        platform.mirror_control_input(&peer, input, context);
                    }
                }
                return Ok(true);
            }
            // Legacy screen input only. App sessions always took the owned branch above.
            let needs = if mirroring == mirror::SCREEN { mirror::INPUT } else { mirror::VIRTUAL_DISPLAY };
            let desktop = is_local_desktop(shared);
            let mirror_on =
                if desktop { pc_mirror_allowed(shared, &peer) } else { shared.toggle_on(&peer, TOGGLE) };
            let remote_input_on = !desktop || shared.toggle_on(&peer, crate::remote::TOGGLE);
            let has_cap = shared.we_offer(needs);
            let allowed = mirror_on && remote_input_on && has_cap;
            if mirroring == mirror::SCREEN && allowed && input.is_valid() {
                // Stopped by the phone owner: dropped here. The platform checks
                // the generation again right before the OS sees the input.
                if let Some(generation) = shared.mirror_typing_admit(&peer) {
                    platform.mirror_screen_input(&peer, input, generation);
                }
            } else if allowed && input.is_valid() {
                platform.mirror_input(&peer, mirroring, input);
            } else if mirroring == mirror::SCREEN
                && mirror_on
                && input.is_valid()
                && !shared.mirror_typing_stopped(&peer)
            {
                if desktop {
                    if !remote_input_on {
                        let first_time =
                            shared.remote.lock().unwrap_or_else(|e| e.into_inner()).mark_prompted(peer);
                        if first_time {
                            shared.emit(NodeEvent::RemoteInputRequested { device: peer });
                        }
                    }
                } else if !has_cap {
                    shared.emit(NodeEvent::RemoteInputRequested { device: peer });
                }
            }
        }
        types::MIRROR_APPS => {
            let reply = if !shared.toggle_on(&peer, TOGGLE) {
                Envelope::error(ErrorCode::Denied, "mirroring is off for this device")
            } else if !shared.we_offer(mirror::VIRTUAL_DISPLAY) {
                Envelope::error(ErrorCode::Unsupported, "this device doesn't open apps in windows")
            } else {
                match tokio::task::spawn_blocking(move || platform.phone_apps())
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()))
                {
                    Ok(apps) => Envelope::new(types::MIRROR_APPS, &PhoneApps { apps: fit_apps(apps) })?,
                    Err(reason) => {
                        tracing::warn!(reason, "can't list the apps");
                        Envelope::error(ErrorCode::Internal, "the phone couldn't list its apps")
                    }
                }
            };
            session.send(reply.reply_to(env.id)).await?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Apps within the limits: at most [`mirror::MAX_APPS`], icons small and
/// within [`ICONS_BUDGET`] all together.
fn fit_apps(mut apps: Vec<PhoneApp>) -> Vec<PhoneApp> {
    apps.retain(|a| nectarlink_protocol::messages::is_package_name(&a.pkg));
    apps.truncate(mirror::MAX_APPS);
    let mut budget = ICONS_BUDGET;
    for app in &mut apps {
        app.label = app.label.chars().take(100).collect();
        match app.icon.as_ref().map(Vec::len) {
            Some(len) if len <= mirror::MAX_ICON_BYTES && len <= budget => budget -= len,
            _ => app.icon = None,
        }
    }
    apps
}

/// What became of a packet handed to [`MirrorStream::send`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorSend {
    Queued,
    /// Sound dropped to keep the delay low (the next packet may go).
    Dropped,
    /// Dropped to keep the delay low: send a keyframe next (frames are
    /// dropped until one comes).
    NeedKeyframe,
    /// The PC stopped watching, or the connection is gone: stop sharing.
    Closed,
}

/// A phone's video (or sound) stream to one PC.
pub struct MirrorStream {
    queue: mpsc::Sender<(PacketKind, u64, Vec<u8>)>,
    closed: Arc<AtomicBool>,
    /// Wakes the sending task when the stream is closed, so it finishes the
    /// stream even if no more packets come.
    stop: Arc<tokio::sync::Notify>,
    /// Frames are being dropped until a keyframe.
    resyncing: Mutex<bool>,
    /// Sound: each packet stands alone, so a dropped one needs no keyframe.
    audio: bool,
    /// Only phone video streams can establish a control target.
    control_owner: Option<(Arc<Shared>, DeviceId, u64, Option<MirrorControlContext>)>,
}

impl std::fmt::Debug for MirrorStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirrorStream")
            .field("audio", &self.audio)
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl MirrorStream {
    /// Queues a packet without waiting. A config packet waits for room:
    /// the PC can't decode without it.
    pub fn send(&self, kind: PacketKind, time_us: u64, data: Vec<u8>) -> MirrorSend {
        if self.closed.load(Ordering::Acquire) {
            return MirrorSend::Closed;
        }
        if let Some((shared, peer, _, Some(owner))) = &self.control_owner
            && !shared.mirror_capture_current(peer, *owner)
        {
            self.close();
            return MirrorSend::Closed;
        }
        let mut resyncing = self.resyncing.lock().unwrap_or_else(|e| e.into_inner());
        if *resyncing && kind == PacketKind::Frame && !self.audio {
            return MirrorSend::NeedKeyframe;
        }
        let config = (kind == PacketKind::Config && !self.audio)
            .then(|| MirrorConfig::from_cbor(&data).ok())
            .flatten();
        let sent = if kind == PacketKind::Config {
            self.queue.blocking_send((kind, time_us, data)).map_err(|_| ())
        } else {
            self.queue.try_send((kind, time_us, data)).map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => (),
                mpsc::error::TrySendError::Closed(_) => self.closed.store(true, Ordering::Release),
            })
        };
        match sent {
            Ok(()) => {
                if let (Some(config), Some((shared, peer, connection, _))) = (config, &self.control_owner)
                    && config.is_valid()
                    && shared.session(peer).is_some_and(|live| live.mirror_control_id == *connection)
                    && let Some(accepted) = shared
                        .mirror_incoming
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get_mut(&(*peer, config.session))
                    && config.request == Some(accepted.request)
                    && accepted.connection == *connection
                    && self
                        .control_owner
                        .as_ref()
                        .and_then(|(_, _, _, owner)| *owner)
                        .is_none_or(|owner| owner.capture == accepted.capture)
                {
                    accepted.video = Some(Arc::downgrade(&self.closed));
                    accepted.stop = Some(Arc::downgrade(&self.stop));
                }
                if kind == PacketKind::Keyframe {
                    *resyncing = false;
                }
                MirrorSend::Queued
            }
            Err(()) if self.closed.load(Ordering::Acquire) => MirrorSend::Closed,
            Err(()) if self.audio => MirrorSend::Dropped,
            Err(()) => {
                *resyncing = true;
                MirrorSend::NeedKeyframe
            }
        }
    }

    /// Ends the stream (the phone stopped sharing).
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.stop.notify_one();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire) || self.queue.is_closed()
    }
}

impl MirrorStream {
    /// A live outgoing stream with no network behind it, for tests: the
    /// queue stays open while the receiver lives, so `send` reports
    /// `Queued` until the stream is closed.
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn synthetic_for_test() -> (Self, mpsc::Receiver<(PacketKind, u64, Vec<u8>)>) {
        let (queue, rx) = mpsc::channel(2);
        let stream = MirrorStream {
            queue,
            closed: Arc::default(),
            stop: Arc::default(),
            resyncing: Mutex::new(false),
            audio: false,
            control_owner: None,
        };
        (stream, rx)
    }
}

/// Whether this device may currently open an outgoing mirror stream to
/// `peer`: the same permission `open` checks before opening and re-checks
/// after registering (the toggle may flip while the stream is being
/// opened). Input, start and generation checks live elsewhere and are
/// unchanged.
fn mirror_open_allowed(shared: &Shared, peer: &DeviceId) -> bool {
    if is_local_desktop(shared) {
        pc_mirror_allowed(shared, peer) || crate::media::watch_open(shared, peer)
    } else {
        shared.toggle_on(peer, TOGGLE)
    }
}

/// Opens a video stream to a PC that asked for the screen, or (`audio`) a
/// sound stream to one that asked for the sound too.
pub(crate) async fn open(
    shared: &Arc<Shared>,
    session: &Session,
    audio: bool,
    owner: Option<MirrorControlContext>,
) -> Result<MirrorStream> {
    // Sampled first: any revocation after this point must be observed by
    // the re-check below, even across an off/on race.
    let epoch = shared.mirror_epoch(&session.peer);
    if !mirror_open_allowed(shared, &session.peer)
        || owner.is_some_and(|owner| !shared.mirror_capture_current(&session.peer, owner))
    {
        return Err(Error::Denied);
    }
    let mut pending = PendingMirrorOpen::new(shared, session.peer, owner);
    if shared.mirror_epoch(&session.peer) != epoch
        || pending.live.closed.load(Ordering::Acquire)
        || !mirror_open_allowed(shared, &session.peer)
        || owner.is_some_and(|owner| !shared.mirror_capture_current(&session.peer, owner))
    {
        return Err(Error::Denied);
    }
    let (mut send, recv) = tokio::select! {
        biased;
        _ = pending.live.stop.notified() => return Err(Error::Denied),
        opened = session.conn.open_bi() => opened.map_err(crate::error::net)?,
    };
    let op = if audio { mirror::OP_AUDIO } else { mirror::OP_VIDEO };
    let header = StreamHeader { svc: mirror::SERVICE.into(), op: op.into(), v: mirror::VERSION };
    let prelude = Envelope::new(types::STREAM, &header)?.to_cbor();
    let written: Result<()> = tokio::select! {
        biased;
        _ = pending.live.stop.notified() => Err(Error::Denied),
        result = write_frame(&mut send, &prelude) => result.map_err(Error::from),
    };
    if let Err(error) = written {
        let _ = send.finish();
        return Err(error);
    }
    let on_end = if !audio {
        shared
            .record_timeline(crate::timeline::NewTimelineEntry {
                kind: crate::TimelineKind::Session,
                device_id: session.peer,
                device_name: shared.peer_name(&session.peer),
                incoming: false,
                timestamp: crate::now_unix(),
                title: "Screen mirroring".into(),
                detail: String::new(),
                target: "mirror".into(),
                size_bytes: 0,
                duration_secs: 0,
                ref_id: None,
            })
            .map(|id| (shared.clone(), id, std::time::Instant::now()))
    } else {
        None
    };
    let mut stream = MirrorStream::spawn_stream_with_lifetime(
        send,
        recv,
        audio,
        on_end,
        pending.live.closed.clone(),
        pending.live.stop.clone(),
    );
    if !audio && !is_local_desktop(shared) {
        stream.control_owner = Some((shared.clone(), session.peer, session.mirror_control_id, owner));
    }
    // Phone-side lifetime: revoking per-peer mirroring must finish even
    // app-window output when the peer keeps reading (see
    // `Shared::clear_mirror_peer`). The next `send` then reports
    // `Closed` so the encoder stops too.
    // The pending entry already owns these same handles before open_bi.
    // Authoritative re-check after registration: the toggle may have been
    // revoked (and even re-enabled) while opening. A bare toggle reread is
    // not generation proof across off/on, so the sampled revocation epoch
    // decides too; either way the just-opened stream is closed and denied.
    // A cancelled pending open drops its own handles; a live writer finishes.
    if shared.mirror_epoch(&session.peer) != epoch
        || !mirror_open_allowed(shared, &session.peer)
        || owner.is_some_and(|owner| !shared.mirror_capture_current(&session.peer, owner))
    {
        stream.close();
        return Err(Error::Denied);
    }
    pending.transferred = true;
    Ok(stream)
}

/// Send priorities: sound before video, as a gap in it is the more
/// noticeable; both after the control stream (`session::CONTROL_PRIORITY`).
pub(crate) const VIDEO_PRIORITY: i32 = 1;
pub(crate) const AUDIO_PRIORITY: i32 = 2;

impl MirrorStream {
    pub(crate) fn spawn_stream(
        send: SendStream,
        recv: RecvStream,
        audio: bool,
        on_end: Option<(Arc<Shared>, i64, std::time::Instant)>,
    ) -> MirrorStream {
        Self::spawn_stream_with_lifetime(send, recv, audio, on_end, Arc::default(), Arc::default())
    }

    fn spawn_stream_with_lifetime(
        mut send: SendStream,
        mut recv: RecvStream,
        audio: bool,
        on_end: Option<(Arc<Shared>, i64, std::time::Instant)>,
        closed: Arc<AtomicBool>,
        stop: Arc<tokio::sync::Notify>,
    ) -> MirrorStream {
        // Sound and video go out as soon as they're written; sound first.
        let _ = send.set_priority(if audio { AUDIO_PRIORITY } else { VIDEO_PRIORITY });
        let (queue, mut packets) =
            mpsc::channel::<(PacketKind, u64, Vec<u8>)>(if audio { AUDIO_QUEUE } else { QUEUE });
        let done = closed.clone();
        let stopped = stop.clone();
        tokio::spawn(async move {
            let mut ended = Box::pin(async move {
                // The PC stops reading (STOP_SENDING) when it's done watching.
                let _ = recv.read_to_end(64).await;
            });
            loop {
                let next = tokio::select! {
                    biased;
                    _ = stopped.notified() => break,
                    _ = &mut ended => break,
                    next = packets.recv() => next,
                };
                let Some((kind, time, data)) = next else { break };
                if done.load(Ordering::Acquire) {
                    break;
                }
                let Ok(header) = video_packet_header(kind, time, data.len()) else { continue };
                let written = async {
                    send.write_all(&header).await?;
                    send.write_all(&data).await
                };
                let result = tokio::select! {
                    biased;
                    _ = stopped.notified() => break,
                    _ = &mut ended => break,
                    result = written => result,
                };
                if result.is_err() {
                    break;
                }
            }
            done.store(true, Ordering::Release);
            let _ = send.finish();
            if let Some((shared, row_id, started_at)) = on_end {
                shared.finish_timeline_session(row_id, started_at.elapsed().as_secs().max(1));
            }
        });
        MirrorStream { queue, closed, stop, resyncing: Mutex::new(false), audio, control_owner: None }
    }
}

/// Whether a stream's token answers the live request. Without negotiation
/// (either side predates `mirror.correlation`) everything passes, so old
/// phones and PCs keep their old behavior; when negotiated, the seen token
/// must equal a live expectation; an unstamped stream answers no request.
fn mirror_token_accepted(expected: Option<u64>, seen: Option<u64>, negotiated: bool) -> bool {
    if !negotiated {
        return true;
    }
    expected.is_some() && expected == seen
}

/// Close handles of one live outgoing mirror stream (phone side): the
/// same `closed` flag and `stop` signal the stream's sending task owns.
/// Closing finishes the QUIC stream even when the peer keeps reading;
/// the next [`MirrorStream::send`] then reports [`MirrorSend::Closed`].
/// Mic and webcam streams never register here.
#[derive(Debug, Clone)]
pub(crate) struct MirrorLiveOut {
    closed: Arc<AtomicBool>,
    stop: Arc<tokio::sync::Notify>,
    owner: Option<MirrorControlContext>,
}

/// Owns registration until the network writer takes over. Dropping a cancelled
/// future or any error path closes and removes only this pending lifetime.
struct PendingMirrorOpen<'a> {
    shared: &'a Shared,
    peer: DeviceId,
    live: MirrorLiveOut,
    transferred: bool,
}

impl<'a> PendingMirrorOpen<'a> {
    fn new(shared: &'a Shared, peer: DeviceId, owner: Option<MirrorControlContext>) -> Self {
        let live = MirrorLiveOut { closed: Arc::default(), stop: Arc::default(), owner };
        shared.track_mirror_lifetime(&peer, live.clone());
        Self { shared, peer, live, transferred: false }
    }
}

impl Drop for PendingMirrorOpen<'_> {
    fn drop(&mut self) {
        if !self.transferred {
            self.live.closed.store(true, Ordering::Release);
            self.live.stop.notify_one();
            self.shared.untrack_mirror_lifetime(&self.peer, &self.live);
        }
    }
}

impl Shared {
    /// Whether `peer` announced capability `cap`.
    /// Only explicit local owner consent is an input grant. Store errors deny.
    pub(crate) fn phone_control_granted(&self, peer: &DeviceId) -> bool {
        is_local_phone(self)
            && self.store.get_peer(peer).ok().flatten().is_some_and(|record| {
                matches!(record.info.kind, crate::DeviceKind::Desktop | crate::DeviceKind::Laptop)
            })
            && self
                .store
                .toggles(peer)
                .ok()
                .is_some_and(|toggles| toggles.get(PHONE_CONTROL_TOGGLE) == Some(&true))
    }

    pub(crate) fn phone_control_epoch_for(&self, peer: &DeviceId) -> Option<u64> {
        let caps = self.local_capabilities();
        if !caps.iter().any(|cap| cap == mirror::INPUT_REQUEST)
            || !caps.iter().any(|cap| cap == mirror::INPUT)
        {
            return None;
        }
        // Same lock/order as grant transition: never combine an old allow row
        // with the newer epoch committed by an OFF action.
        let fence = self.typing_fence();
        if !self.toggle_on(peer, TOGGLE) || !self.phone_control_granted(peer) {
            return None;
        }
        let typing = fence.current(peer);
        (!typing.stopped).then_some(typing.generation)
    }

    pub(crate) fn phone_control_caps_for(&self, peer: &DeviceId, caps: Vec<String>) -> Vec<String> {
        let mut caps: Vec<_> = caps
            .into_iter()
            .filter(|cap| cap != mirror::CONTROL_ALLOWED && cap != mirror::CONTROL_STOPPED)
            .collect();
        if caps.iter().any(|cap| cap == mirror::INPUT_REQUEST) && self.phone_control_granted(peer) {
            if self.mirror_typing_stopped(peer) {
                caps.push(mirror::CONTROL_STOPPED.into());
            } else {
                caps.push(mirror::CONTROL_ALLOWED.into());
            }
        }
        caps
    }

    pub(crate) fn mirror_control_reason(
        &self,
        peer: &DeviceId,
        mirroring: u32,
        request: u64,
    ) -> &'static str {
        if self.cancel.is_cancelled() || self.session(peer).is_none() {
            return "offline";
        }
        let Some(record) = self.store.get_peer(peer).ok().flatten() else { return "offline" };
        if !is_local_desktop(self)
            || !matches!(record.info.kind, crate::DeviceKind::Phone | crate::DeviceKind::Tablet)
            || !self.local_capabilities().iter().any(|cap| cap == mirror::INPUT_REQUEST)
            || !self.local_capabilities().iter().any(|cap| cap == mirror::VIEW)
            || !self.mirror_correlated(peer)
            || !record.caps.contains(if mirroring == mirror::SCREEN {
                mirror::CAPTURE
            } else {
                mirror::VIRTUAL_DISPLAY
            })
            || !record.caps.contains(mirror::INPUT_REQUEST)
            || (mirroring != mirror::SCREEN && !record.caps.contains(mirror::APP_WINDOWS_OWNED_V1))
        {
            return "unsupported";
        }
        if !self.toggle_on(peer, TOGGLE) {
            return "mirroringOff";
        }
        if record.caps.contains(mirror::CONTROL_STOPPED) {
            return "stopped";
        }
        if !record.caps.contains(mirror::CONTROL_ALLOWED) {
            return "ownerApproval";
        }
        if !record.caps.contains(mirror::INPUT) {
            return "inputUnavailable";
        }
        if self.session(peer).is_none_or(|session| session.mirror_input_epoch.load(Ordering::Acquire) == 0) {
            return "unsupported";
        }
        if self.mirror_outgoing.lock().unwrap_or_else(|e| e.into_inner()).get(&(*peer, mirroring))
            != Some(&request)
        {
            return "notShowing";
        }
        "available"
    }

    pub(crate) fn mirror_capture_context(
        &self,
        peer: &DeviceId,
        mirroring: u32,
        request: u64,
    ) -> Option<MirrorControlContext> {
        if !is_local_phone(self)
            || !self.toggle_on(peer, TOGGLE)
            || !self.store.is_paired(peer).ok()?
            || (mirroring != mirror::SCREEN
                && !self.local_capabilities().iter().any(|cap| cap == mirror::VIRTUAL_DISPLAY))
        {
            return None;
        }
        let session = self.session(peer)?;
        let connection = session.mirror_control_id;
        let capture = {
            let incoming = self.mirror_incoming.lock().unwrap_or_else(|e| e.into_inner());
            let accepted = incoming.get(&(*peer, mirroring))?;
            if accepted.request != request || accepted.connection != connection {
                return None;
            }
            accepted.capture
        };
        Some(MirrorControlContext {
            session: mirroring,
            capture,
            request,
            connection,
            generation: self.mirror_typing_epoch(peer),
        })
    }

    pub(crate) fn mirror_capture_current(&self, peer: &DeviceId, context: MirrorControlContext) -> bool {
        !self.cancel.is_cancelled()
            && self.mirror_capture_context(peer, context.session, context.request).is_some_and(|owner| {
                owner.connection == context.connection && owner.capture == context.capture
            })
    }

    pub(crate) fn phone_control_current(&self, peer: &DeviceId, context: MirrorControlContext) -> bool {
        if context.connection == 0
            || self.cancel.is_cancelled()
            || !self.session(peer).is_some_and(|session| session.mirror_control_id == context.connection)
            || !self.peer_supports(peer, mirror::INPUT_REQUEST)
            || !self.mirror_correlated(peer)
            || self.phone_control_epoch_for(peer) != Some(context.generation)
            || !self.mirror_typing_current(peer, context.generation)
            || (context.session != mirror::SCREEN
                && !self.local_capabilities().iter().any(|cap| cap == mirror::VIRTUAL_DISPLAY))
        {
            return false;
        }
        self.mirror_incoming
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(*peer, context.session))
            .is_some_and(|accepted| {
                accepted.request == context.request
                    && accepted.connection == context.connection
                    && accepted.capture == context.capture
                    && accepted
                        .video
                        .as_ref()
                        .and_then(Weak::upgrade)
                        .is_some_and(|closed| !closed.load(Ordering::Acquire))
            })
    }

    /// Commit grant and its invalidating epoch atomically under the existing
    /// durable fence's CAS. A grant change never clears a prior Stop.
    pub(crate) fn set_phone_control_grant(&self, peer: &DeviceId, enabled: bool) -> Result<()> {
        if self.cancel.is_cancelled()
            || !is_local_phone(self)
            || !self.store.get_peer(peer)?.is_some_and(|record| {
                matches!(record.info.kind, crate::DeviceKind::Desktop | crate::DeviceKind::Laptop)
            })
        {
            return Err(Error::Denied);
        }
        let mut fence = self.typing_fence();
        let mut changed = fence.clone();
        let previous = changed.current(peer);
        let generation = changed.advance();
        changed.peers.insert(*peer, Typing { generation, stopped: previous.stopped, owner_off: None });
        // Any serialization, transaction or CAS error retains live denial;
        // only a successful atomic commit restores the previous stopped bit.
        let mut denied = changed.clone();
        denied.peers.insert(*peer, Typing { generation, stopped: true, owner_off: None });
        *fence = denied;
        let stored = StoredTypingFence {
            version: 2,
            next: changed.next,
            floor: changed.floor,
            peers: changed.peers.iter().map(|(id, typing)| (id.to_string(), *typing)).collect(),
            node_running: Some(changed.node_running),
        };
        let value = serde_json::to_string(&stored)
            .map_err(|error| Error::Internal(format!("can't encode mirror typing fence: {error}")))?;
        // On error, the live denial above and existing durable running marker
        // force recovery instead of trusting the old allow row.
        self.store.compare_and_set_phone_control_grant(
            changed.persisted_value.as_deref(),
            &value,
            peer,
            enabled,
        )?;
        changed.persisted_value = Some(value);
        *fence = changed;
        Ok(())
    }

    pub(crate) fn peer_supports(&self, peer: &DeviceId, cap: &str) -> bool {
        self.store.get_peer(peer).ok().flatten().is_some_and(|r| r.caps.contains(cap))
    }

    /// Whether both sides negotiate mirror request correlation.
    pub(crate) fn mirror_correlated(&self, peer: &DeviceId) -> bool {
        self.local_capabilities().iter().any(|c| c == mirror::CORRELATION)
            && self.peer_supports(peer, mirror::CORRELATION)
    }

    /// Records a request token this device sent (`mirror.start`).
    pub(crate) fn note_mirror_outgoing(&self, peer: &DeviceId, session: u32, request: u64) {
        self.mirror_outgoing.lock().unwrap_or_else(|e| e.into_inner()).insert((*peer, session), request);
    }

    /// Takes the recorded outgoing token (a stop ends the request).
    pub(crate) fn take_mirror_outgoing(&self, peer: &DeviceId, session: u32) -> Option<u64> {
        self.mirror_outgoing.lock().unwrap_or_else(|e| e.into_inner()).remove(&(*peer, session))
    }

    /// An owned stream is registered before its first config. End that exact
    /// request even if config enqueueing has not yet bound its video flag.
    fn close_mirror_owned_request(
        &self,
        peer: &DeviceId,
        session: u32,
        request: u64,
        connection: u64,
        capture: u64,
    ) {
        let mut all = self.mirror_live_out.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(streams) = all.get_mut(peer) {
            for stream in streams.iter() {
                if stream.owner.is_some_and(|owner| {
                    owner.session == session
                        && owner.request == request
                        && owner.connection == connection
                        && owner.capture == capture
                }) {
                    stream.closed.store(true, Ordering::Release);
                    stream.stop.notify_one();
                }
            }
            streams.retain(|stream| !stream.closed.load(Ordering::Acquire));
        }
    }

    /// Records a request token this device accepted (`mirror.start`).
    pub(crate) fn note_mirror_incoming(
        &self,
        peer: &DeviceId,
        session: u32,
        request: u64,
        connection: u64,
    ) -> Option<u64> {
        static NEXT_CAPTURE: AtomicU64 = AtomicU64::new(1);
        let capture = if session == mirror::SCREEN {
            0
        } else {
            NEXT_CAPTURE.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1)).ok()?
        };
        let mut incoming = self.mirror_incoming.lock().unwrap_or_else(|e| e.into_inner());
        if session == mirror::SCREEN
            && incoming
                .get(&(*peer, session))
                .is_some_and(|owner| owner.request == request && owner.connection == connection)
        {
            return Some(0);
        }
        let previous = incoming.insert(
            (*peer, session),
            IncomingMirror { request, connection, capture, video: None, stop: None },
        );
        drop(incoming);
        if let Some(previous) = previous {
            self.close_mirror_owned_request(
                peer,
                session,
                previous.request,
                previous.connection,
                previous.capture,
            );
            if let Some(closed) = previous.video.as_ref().and_then(Weak::upgrade) {
                closed.store(true, Ordering::Release);
            }
            if let Some(stop) = previous.stop.as_ref().and_then(Weak::upgrade) {
                stop.notify_one();
            }
        }
        Some(capture)
    }

    pub(crate) fn incoming_mirror_request(&self, peer: &DeviceId, session: u32) -> Option<u64> {
        self.mirror_incoming
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(*peer, session))
            .map(|accepted| accepted.request)
    }

    pub(crate) fn take_mirror_incoming_request(
        &self,
        peer: &DeviceId,
        session: u32,
        request: u64,
        connection: u64,
        capture: u64,
    ) -> bool {
        let mut incoming = self.mirror_incoming.lock().unwrap_or_else(|e| e.into_inner());
        if !incoming.get(&(*peer, session)).is_some_and(|accepted| {
            accepted.request == request && accepted.connection == connection && accepted.capture == capture
        }) {
            return false;
        }
        if let Some(accepted) = incoming.remove(&(*peer, session)) {
            self.close_mirror_owned_request(
                peer,
                session,
                accepted.request,
                accepted.connection,
                accepted.capture,
            );
            if let Some(closed) = accepted.video.as_ref().and_then(Weak::upgrade) {
                closed.store(true, Ordering::Release);
            }
            if let Some(stop) = accepted.stop.as_ref().and_then(Weak::upgrade) {
                stop.notify_one();
            }
        }
        true
    }

    /// Whether a `mirror.stop` ends the recorded request: a stop tagged for
    /// a different request than the one recorded is stale (a replacement
    /// start already superseded it) and takes no action. Clears the record
    /// when the stop applies.
    pub(crate) fn mirror_stop_applies(
        &self,
        peer: &DeviceId,
        session: u32,
        request: Option<u64>,
        connection: u64,
    ) -> Option<u64> {
        let mut incoming = self.mirror_incoming.lock().unwrap_or_else(|e| e.into_inner());
        if request.is_some()
            && incoming.get(&(*peer, session)).is_some_and(|owner| owner.connection != connection)
        {
            return None;
        }
        match (request, incoming.get(&(*peer, session)).map(|accepted| accepted.request)) {
            (Some(stop), Some(current)) if stop != current => None,
            _ => {
                let accepted = incoming.remove(&(*peer, session));
                let capture = accepted.as_ref().map(|owner| owner.capture);
                if let Some(accepted) = accepted {
                    self.close_mirror_owned_request(
                        peer,
                        session,
                        accepted.request,
                        accepted.connection,
                        accepted.capture,
                    );
                    if let Some(closed) = accepted.video.as_ref().and_then(Weak::upgrade) {
                        closed.store(true, Ordering::Release);
                    }
                    if let Some(stop) = accepted.stop.as_ref().and_then(Weak::upgrade) {
                        stop.notify_one();
                    }
                }
                capture.or_else(|| (session == mirror::SCREEN).then_some(0))
            }
        }
    }

    /// Invalidate a peer's accepted requests and output handles together.
    /// Returns exact platform capture owners for a later owned teardown.
    /// Admission holds this same epoch lock, so an old start cannot insert
    /// ownership across OFF/ON or connection replacement.
    pub(crate) fn clear_mirror_peer(&self, peer: &DeviceId) -> Vec<(u32, Option<u64>, u64, u64)> {
        let mut epochs = self.mirror_revoke_epoch.lock().unwrap_or_else(|e| e.into_inner());
        let next = epochs.get(peer).copied().unwrap_or(0).saturating_add(1);
        epochs.insert(*peer, next);
        self.mirror_outgoing.lock().unwrap_or_else(|e| e.into_inner()).retain(|(p, _), _| p != peer);
        let mut incoming = self.mirror_incoming.lock().unwrap_or_else(|e| e.into_inner());
        let mut owners: Vec<_> = incoming
            .iter()
            .filter(|((p, _), _)| p == peer)
            .map(|((_, session), owner)| (*session, Some(owner.request), owner.connection, owner.capture))
            .collect();
        incoming.retain(|(p, _), _| p != peer);
        drop(incoming);
        let live =
            self.mirror_live_out.lock().unwrap_or_else(|e| e.into_inner()).remove(peer).unwrap_or_default();
        for stream in live {
            stream.closed.store(true, Ordering::Release);
            stream.stop.notify_one();
        }
        self.stop_showing_all(peer);
        let legacy = {
            let mut captures = self.mirror_legacy_apps.lock().unwrap_or_else(|e| e.into_inner());
            let legacy: Vec<_> = captures.iter().filter(|((p, _), _)| p == peer)
                .map(|((_, mirroring), connection)| (*mirroring, *connection)).collect();
            captures.retain(|(p, _), _| p != peer);
            legacy
        };
        if !legacy.is_empty() {
            let platform = self.platform.clone();
            let peer = *peer;
            self.runtime.spawn_blocking(move || {
                for (mirroring, connection) in legacy {
                    platform.mirror_app_stop_requested_legacy(&peer, mirroring, connection);
                }
            });
        }
        owners.sort_unstable();
        owners
    }

    /// The per-peer mirroring revocation epoch: bumped by every
    /// [`Self::clear_mirror_peer`] on revocation or connection replacement.
    /// A stream opened across a
    /// revocation observes the bump even if the toggle was re-enabled
    /// since, so off/on cannot escape the re-check in [`open`].
    pub(crate) fn mirror_epoch(&self, peer: &DeviceId) -> u64 {
        self.mirror_revoke_epoch.lock().unwrap_or_else(|e| e.into_inner()).get(peer).copied().unwrap_or(0)
    }

    /// Remembers a newly opened outgoing mirror stream of `peer` (screen,
    /// app window, or sound): revocation closes it even when the peer
    /// keeps reading. Prunes streams that already finished so dead handles
    /// never accumulate; per-peer only, never another peer's.
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn track_mirror_outgoing(&self, peer: &DeviceId, stream: &MirrorStream) {
        let live = MirrorLiveOut {
            closed: stream.closed.clone(),
            stop: stream.stop.clone(),
            owner: stream.control_owner.as_ref().and_then(|(_, _, _, owner)| *owner),
        };
        self.track_mirror_lifetime(peer, live);
    }

    fn track_mirror_lifetime(&self, peer: &DeviceId, live: MirrorLiveOut) {
        let mut all = self.mirror_live_out.lock().unwrap_or_else(|e| e.into_inner());
        let entry = all.entry(*peer).or_default();
        entry.retain(|s: &MirrorLiveOut| !s.closed.load(Ordering::Acquire));
        entry.push(live);
    }

    /// Remove only this pending open on cancellation/error, including when
    /// another request or connection already registered a successor.
    fn untrack_mirror_lifetime(&self, peer: &DeviceId, live: &MirrorLiveOut) {
        let mut all = self.mirror_live_out.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = all.get_mut(peer) {
            entry.retain(|s: &MirrorLiveOut| {
                !(Arc::ptr_eq(&s.closed, &live.closed) && Arc::ptr_eq(&s.stop, &live.stop))
            });
            if entry.is_empty() {
                all.remove(peer);
            }
        }
    }

    /// A new stop signal for the PC's reader of `peer`'s video or sound
    /// stream (`op`) of a mirroring.
    pub(crate) fn new_mirror_stop(
        &self,
        peer: &DeviceId,
        mirroring: u32,
        op: &'static str,
    ) -> Arc<tokio::sync::Notify> {
        let signal = Arc::new(tokio::sync::Notify::new());
        self.mirror_stops
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((*peer, mirroring, op), signal.clone());
        signal
    }

    /// Stops showing (and playing) a mirroring of `peer` here (kept until
    /// the readers see it).
    pub(crate) fn stop_showing(&self, peer: &DeviceId, mirroring: u32) {
        let mut stops = self.mirror_stops.lock().unwrap_or_else(|e| e.into_inner());
        for op in [mirror::OP_VIDEO, mirror::OP_AUDIO] {
            if let Some(signal) = stops.remove(&(*peer, mirroring, op)) {
                signal.notify_one();
            }
        }
    }

    /// Stops showing (and playing) all mirror sessions of `peer` here.
    pub(crate) fn stop_showing_all(&self, peer: &DeviceId) {
        let mut stops = self.mirror_stops.lock().unwrap_or_else(|e| e.into_inner());
        let keys: Vec<(DeviceId, u32, &'static str)> =
            stops.keys().filter(|(p, _, _)| p == peer).copied().collect();
        for key in keys {
            if let Some(signal) = stops.remove(&key) {
                signal.notify_one();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_full_queue_drops_frames_until_a_keyframe() {
        let (queue, mut packets) = mpsc::channel(2);
        let stream = MirrorStream {
            queue,
            closed: Arc::default(),
            stop: Arc::default(),
            resyncing: Mutex::new(false),
            audio: false,
            control_owner: None,
        };
        let send = |kind| tokio::task::block_in_place(|| stream.send(kind, 0, vec![1]));
        assert_eq!(send(PacketKind::Keyframe), MirrorSend::Queued);
        assert_eq!(send(PacketKind::Frame), MirrorSend::Queued);
        assert_eq!(send(PacketKind::Frame), MirrorSend::NeedKeyframe, "full");
        packets.recv().await;
        // Room again, but the decoder needs a fresh start first.
        assert_eq!(send(PacketKind::Frame), MirrorSend::NeedKeyframe);
        assert_eq!(send(PacketKind::Keyframe), MirrorSend::Queued);
        packets.recv().await;
        assert_eq!(send(PacketKind::Frame), MirrorSend::Queued);
        drop(packets);
        assert_eq!(send(PacketKind::Keyframe), MirrorSend::Closed);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sound_drops_only_what_doesnt_fit() {
        let (queue, mut packets) = mpsc::channel(1);
        let stream = MirrorStream {
            queue,
            closed: Arc::default(),
            stop: Arc::default(),
            resyncing: Mutex::new(false),
            audio: true,
            control_owner: None,
        };
        let send = |kind| tokio::task::block_in_place(|| stream.send(kind, 0, vec![1]));
        assert_eq!(send(PacketKind::Frame), MirrorSend::Queued);
        assert_eq!(send(PacketKind::Frame), MirrorSend::Dropped, "full");
        packets.recv().await;
        // No keyframe to wait for: the next one goes.
        assert_eq!(send(PacketKind::Frame), MirrorSend::Queued);
    }

    #[test]
    fn stale_tokens_are_refused_only_when_negotiated() {
        // Legacy peers (or unstamped messages) always pass.
        assert!(mirror_token_accepted(None, None, false));
        assert!(mirror_token_accepted(Some(5), None, false));
        assert!(mirror_token_accepted(None, Some(5), false));
        assert!(mirror_token_accepted(Some(4), Some(5), false));
        // Negotiated: the stream must answer the live request.
        assert!(mirror_token_accepted(Some(5), Some(5), true));
        assert!(!mirror_token_accepted(None, None, true));
        assert!(!mirror_token_accepted(Some(5), None, true));
        assert!(!mirror_token_accepted(None, Some(5), true));
        assert!(!mirror_token_accepted(Some(4), Some(5), true));
    }
}
