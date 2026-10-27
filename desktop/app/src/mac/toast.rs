// SPDX-License-Identifier: GPL-3.0-or-later
//! macOS notification banners posted by this process.
//!
//! The banner uses the host app's own name and icon. Title, subtitle, and
//! body stay the phone title, the desktop attribution, and the phone text.
//! Phone actions and supported free-form replies use native notification
//! actions. Progress supplied with a post is included as text; icon override
//! and image are not represented.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
};

#[path = "user_notifications.rs"]
mod user_notifications;

#[allow(dead_code)]
pub const AUMID: &str = "com.thtbee.nectarlink";
pub const CALL_ANSWER: &str = "answer";
pub const CALL_DECLINE: &str = "decline";
pub const CALL_END: &str = "end";
pub const LOCAL_COPY_CODE_PREFIX: &str = user_notifications::LOCAL_COPY_CODE_PREFIX;

/// Keeps an eligible notification's OTP only in process memory and returns
/// the opaque action ID bound to that observed notification revision.
pub fn register_copy_code_action(device: &str, key: &str, code: &str) -> Option<String> {
    user_notifications::register_copy_code_action(device, key, code)
}

/// Reuses only an unconsumed same-code action during silent snapshot replacement.
pub fn existing_copy_code_action(device: &str, key: &str, code: &str) -> Option<String> {
    user_notifications::existing_copy_code_action(device, key, code)
}

/// Invalidates local OTP actions as soon as a notification revision is replaced.
pub fn invalidate_copy_code_actions(device: &str, key: &str) {
    user_notifications::invalidate_copy_code_actions(device, key);
}

/// Keeps the one-shot copy action when a silent snapshot still has the same code.
/// A changed or missing code drops that action and does not register a replacement.
pub fn retain_copy_code_if_unchanged(device: &str, key: &str, code: Option<&str>) {
    user_notifications::retain_copy_code_if_unchanged(device, key, code);
}

/// Consumes the code bound to this exact native action once.
pub fn take_copy_code_action(device: &str, key: &str, action: &str) -> Option<String> {
    user_notifications::take_copy_code_action(device, key, action)
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToastEvent {
    Opened { device: String, key: String },
    Action { device: String, key: String, action: String },
    Reply { device: String, key: String, action: String, text: String },
    Dismissed { device: String, key: String },
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct Toast {
    pub device: String,
    pub key: String,
    pub title: String,
    pub body: String,
    pub attribution: String,
    pub icon: Option<PathBuf>,
    pub image: Option<PathBuf>,
    pub actions: Vec<(String, String)>,
    pub reply: Option<(String, String)>,
    pub silent: bool,
    pub progress: Option<Progress>,
    pub call: bool,
}

impl std::fmt::Debug for Toast {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Toast").field("device", &self.device).finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub status: String,
    pub value: Option<f64>,
    pub label: String,
}

/// Authorization from `UNNotificationSettings`, or [`Unknown`] until a read
/// finishes. Nothing here treats an unread status as permission to post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationAuthorization {
    Unknown = 0,
    NotDetermined = 1,
    Denied = 2,
    Provisional = 3,
    Authorized = 4,
}

impl NotificationAuthorization {
    /// Stable label for the settings row.
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::NotDetermined => "not_determined",
            Self::Denied => "denied",
            Self::Provisional => "provisional",
            Self::Authorized => "authorized",
        }
    }

    /// Maps `UNAuthorizationStatus` (`UNNotificationSettings.h`).
    pub fn from_system(status: isize) -> Self {
        match status {
            0 => Self::NotDetermined,
            1 => Self::Denied,
            2 => Self::Authorized,
            3 => Self::Provisional,
            _ => Self::Unknown,
        }
    }
}

/// What the user can do about the current authorization. A background show
/// never chooses [`Enable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionAction {
    None,
    Enable,
    OpenSettings,
}

/// Posts only after a settings read says the app is fully authorized.
/// Provisional authorization is quiet and is not enough for a banner.
pub fn admit_post(auth: NotificationAuthorization) -> bool {
    matches!(auth, NotificationAuthorization::Authorized)
}

pub fn permission_action(auth: NotificationAuthorization) -> PermissionAction {
    match auth {
        NotificationAuthorization::NotDetermined => PermissionAction::Enable,
        NotificationAuthorization::Denied | NotificationAuthorization::Provisional => {
            PermissionAction::OpenSettings
        }
        NotificationAuthorization::Authorized | NotificationAuthorization::Unknown => PermissionAction::None,
    }
}

/// System Settings URL for this Mac's notification pane.
///
/// macOS 13 and later use the Settings extension whose bundle id is
/// `com.apple.Notifications-Settings.extension`. Earlier releases use the
/// Notifications preference pane.
pub fn notification_settings_url(macos_major: Option<u32>) -> &'static str {
    match macos_major {
        Some(major) if major < 13 => "x-apple.systempreferences:com.apple.preference.notifications",
        _ => "x-apple.systempreferences:com.apple.Notifications-Settings.extension",
    }
}

/// Banner strings, the click routing pair, and the identifier `remove` uses.
/// The phone icon is not part of this request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BannerText {
    pub title: String,
    pub subtitle: String,
    pub body: String,
    pub device: String,
    pub key: String,
    pub identifier: String,
    /// Actions supplied by the phone, plus the desktop's explicit OTP copy action.
    pub actions: Vec<(String, String)>,
    /// Present only when the phone supplied a free-form RemoteInput action.
    pub reply: Option<(String, String)>,
    /// Calls keep their existing dedicated banner handling.
    pub call: bool,
    /// Goes to the list without a banner, like Windows `SetSuppressPopup`.
    pub quiet: bool,
}

pub fn notification_identifier(device: &str, key: &str) -> String {
    format!("nl:{:x}:{device}:{:x}:{key}", device.len(), key.len())
}

/// Uses the supplied status and label without introducing untranslated UI copy.
/// UserNotifications has no progress bar, so full posts include a text snapshot.
fn progress_body(body: &str, progress: Option<&Progress>) -> String {
    let Some(progress) = progress else {
        return body.to_string();
    };
    let mut line = progress.status.trim().to_string();
    let label = progress.label.trim();
    if !label.is_empty() {
        if !line.is_empty() {
            line.push_str(" — ");
        }
        line.push_str(label);
    } else if let Some(value) = progress.value.filter(|value| value.is_finite()) {
        let percent = (value.clamp(0.0, 1.0) * 100.0).round() as u32;
        if line.is_empty() {
            line = format!("{percent}%");
        } else {
            line.push_str(&format!(" ({percent}%)"));
        }
    }
    if line.is_empty() {
        body.to_string()
    } else if body.is_empty() {
        line
    } else {
        format!("{body}\n{line}")
    }
}

pub fn banner_text(toast: &Toast) -> BannerText {
    BannerText {
        title: toast.title.clone(),
        subtitle: toast.attribution.clone(),
        body: progress_body(&toast.body, toast.progress.as_ref()),
        device: toast.device.clone(),
        key: toast.key.clone(),
        identifier: notification_identifier(&toast.device, &toast.key),
        actions: toast.actions.clone(),
        reply: toast.reply.clone(),
        call: toast.call,
        quiet: toast.silent && !toast.call && !toast.actions.iter().any(|(id, _)| id == CALL_END),
    }
}

/// `UNUserNotificationCenter` raises, aborting the process, unless it runs
/// inside an app bundle. This is the executable half of that check.
pub fn app_bundle_executable(exe: &Path) -> bool {
    let Some(macos) = exe.parent() else {
        return false;
    };
    let Some(contents) = macos.parent() else {
        return false;
    };
    macos.file_name().is_some_and(|name| name == "MacOS")
        && contents.file_name().is_some_and(|name| name == "Contents")
        && contents.parent().and_then(Path::extension).is_some_and(|ext| ext == "app")
}

const MAX_PENDING: usize = 256;

/// Posts that are waiting on a settings read or an add, per identifier.
///
/// Tickets order calls for one identifier. A remove cancels only that
/// identifier, and a device remove only that device's identifiers. `admit`
/// and `added` must run on the main queue together with the native add or
/// withdraw they decide, so no add for the identifier can interleave.
pub(super) struct PendingPosts {
    slots: Vec<PendingSlot>,
    next: u64,
    sweeps: usize,
}

struct PendingSlot {
    identifier: String,
    device: String,
    latest: u64,
    issued: u64,
    cancelled: u64,
    inflight: usize,
}

impl PendingPosts {
    pub(super) const fn new() -> Self {
        Self { slots: Vec::new(), next: 0, sweeps: 0 }
    }

    /// Starts a post. `None` when the bounded list is full of other identifiers.
    pub(super) fn begin(&mut self, device: &str, identifier: &str) -> Option<u64> {
        let index = match self.position(identifier) {
            Some(index) => index,
            None if self.slots.len() >= MAX_PENDING => return None,
            None => {
                self.slots.push(PendingSlot {
                    identifier: identifier.to_string(),
                    device: device.to_string(),
                    latest: 0,
                    issued: 0,
                    cancelled: 0,
                    inflight: 0,
                });
                self.slots.len() - 1
            }
        };
        self.next += 1;
        let slot = &mut self.slots[index];
        slot.latest = self.next;
        slot.inflight += 1;
        Some(self.next)
    }

    /// True when `ticket` is still the newest post for the identifier and no
    /// remove came after it. Otherwise the post is finished here.
    pub(super) fn admit(&mut self, identifier: &str, ticket: u64) -> bool {
        let Some(index) = self.position(identifier) else {
            return false;
        };
        let slot = &mut self.slots[index];
        if slot.latest == ticket && ticket > slot.cancelled {
            slot.issued = ticket;
            return true;
        }
        self.finish(index);
        false
    }

    /// Finishes a post that settings refused before any add.
    pub(super) fn refuse(&mut self, identifier: &str) {
        if let Some(index) = self.position(identifier) {
            self.finish(index);
        }
    }

    /// Finishes an add. True when a remove came after this add and no newer
    /// add has taken the identifier, so the caller must withdraw it.
    pub(super) fn added(&mut self, identifier: &str, ticket: u64, shown: bool) -> bool {
        let Some(index) = self.position(identifier) else {
            return false;
        };
        let slot = &self.slots[index];
        let withdraw = shown && slot.issued == ticket && slot.cancelled >= ticket;
        self.finish(index);
        withdraw
    }

    pub(super) fn cancel(&mut self, identifier: &str) {
        if let Some(index) = self.position(identifier) {
            let slot = &mut self.slots[index];
            slot.cancelled = slot.latest;
        }
    }

    /// Cancels the device's posts and keeps slots until `lists` native list
    /// reads call [`Self::end_sweep`]. Returns the cut for those reads.
    pub(super) fn cancel_device(&mut self, device: &str, lists: usize) -> u64 {
        for slot in self.slots.iter_mut().filter(|slot| slot.device == device) {
            slot.cancelled = slot.latest;
        }
        self.sweeps += lists;
        self.next
    }

    /// False when an add newer than `cut` owns the identifier now.
    pub(super) fn sweep_may_withdraw(&self, identifier: &str, cut: u64) -> bool {
        self.position(identifier).is_none_or(|index| self.slots[index].issued <= cut)
    }

    pub(super) fn end_sweep(&mut self) {
        self.sweeps = self.sweeps.saturating_sub(1);
        if self.sweeps == 0 {
            self.slots.retain(|slot| slot.inflight > 0);
        }
    }

    fn finish(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        slot.inflight = slot.inflight.saturating_sub(1);
        if slot.inflight == 0 && self.sweeps == 0 {
            self.slots.swap_remove(index);
        }
    }

    fn position(&self, identifier: &str) -> Option<usize> {
        self.slots.iter().position(|slot| slot.identifier == identifier)
    }
}

static PENDING: Mutex<PendingPosts> = Mutex::new(PendingPosts::new());

pub(super) fn pending() -> std::sync::MutexGuard<'static, PendingPosts> {
    PENDING.lock().unwrap_or_else(|error| error.into_inner())
}

type OnEvent = Arc<dyn Fn(ToastEvent) + Send + Sync>;
type AuthorizationListener = Arc<dyn Fn(NotificationAuthorization) + Send + Sync>;
static ON_EVENT: OnceLock<Mutex<Option<OnEvent>>> = OnceLock::new();
static AUTHORIZATION: AtomicU8 = AtomicU8::new(0);
static READ_GENERATION: AtomicU64 = AtomicU64::new(0);
static REQUEST_OPEN: AtomicBool = AtomicBool::new(false);
static LISTENER: OnceLock<Mutex<Option<AuthorizationListener>>> = OnceLock::new();

/// Whether the shared Windows "notifications are off" banner should appear.
///
/// That banner links to Windows Settings. On macOS it does not apply, so this
/// stays true. Mac posting uses [`authorization`], which starts unknown.
pub fn enabled() -> bool {
    true
}

pub fn authorization() -> NotificationAuthorization {
    match AUTHORIZATION.load(Ordering::Acquire) {
        1 => NotificationAuthorization::NotDetermined,
        2 => NotificationAuthorization::Denied,
        3 => NotificationAuthorization::Provisional,
        4 => NotificationAuthorization::Authorized,
        _ => NotificationAuthorization::Unknown,
    }
}

/// Called when a settings read finishes. The callback runs without the
/// listener lock held.
pub fn set_authorization_listener(listener: impl Fn(NotificationAuthorization) + Send + Sync + 'static) {
    let listener: AuthorizationListener = Arc::new(listener);
    let slot = LISTENER.get_or_init(|| Mutex::new(None));
    *slot.lock().unwrap_or_else(|error| error.into_inner()) = Some(listener);
}

pub fn set_process_id() {}

pub fn unregister() {
    user_notifications::unregister();
}

pub fn start(_icon: PathBuf, on_event: impl Fn(ToastEvent) + Send + Sync + 'static) {
    let callback: OnEvent = Arc::new(on_event);
    match ON_EVENT.get() {
        Some(slot) => *slot.lock().unwrap_or_else(|error| error.into_inner()) = Some(callback),
        None => {
            let _ = ON_EVENT.set(Mutex::new(Some(callback)));
        }
    }
    // Install the notification-center delegate and read status. This does not
    // request permission.
    user_notifications::install_delegate();
    user_notifications::refresh_authorization();
}

pub fn show(toast: Toast) {
    user_notifications::post(toast);
}

pub fn update_progress(_device: &str, _key: &str, _progress: Progress) {}

pub fn remove(device: &str, key: &str) {
    user_notifications::remove(device, key);
}

pub fn remove_device(device: &str) {
    user_notifications::remove_device(device);
}

/// Reads notification settings. Does not request permission.
pub fn refresh_authorization() {
    user_notifications::refresh_authorization();
}

/// Asks for alert and sound permission. Refuses unless the last read was
/// not-determined, so startup and background shows cannot prompt.
pub fn request_authorization() {
    if permission_action(authorization()) != PermissionAction::Enable {
        return;
    }
    user_notifications::request_authorization();
}

pub fn open_notification_settings() {
    let url = notification_settings_url(macos_major());
    if let Err(error) = super::shell::open_url(url) {
        tracing::warn!(error, "can't open notification settings");
    }
}

pub(super) fn emit(event: ToastEvent) {
    let callback =
        ON_EVENT.get().and_then(|slot| slot.lock().unwrap_or_else(|error| error.into_inner()).clone());
    if let Some(callback) = callback {
        callback(event);
    }
}

pub(super) fn begin_authorization_read() -> u64 {
    READ_GENERATION.fetch_add(1, Ordering::AcqRel) + 1
}

pub(super) fn finish_authorization_read(generation: u64, auth: NotificationAuthorization) {
    if READ_GENERATION.load(Ordering::Acquire) != generation {
        return;
    }
    store_authorization(auth);
}

pub(super) fn begin_authorization_request() -> bool {
    REQUEST_OPEN.swap(true, Ordering::AcqRel)
}

pub(super) fn end_authorization_request() {
    REQUEST_OPEN.store(false, Ordering::Release);
}

fn store_authorization(auth: NotificationAuthorization) {
    AUTHORIZATION.store(auth as u8, Ordering::Release);
    let listener =
        LISTENER.get().and_then(|slot| slot.lock().unwrap_or_else(|error| error.into_inner()).clone());
    if let Some(listener) = listener {
        listener(auth);
    }
}

fn macos_major() -> Option<u32> {
    super::os_version().split('.').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_snapshot_preserves_body_and_supplied_labels() {
        let mut progress =
            Progress { status: "Загрузка".into(), value: Some(0.42), label: String::new() };
        assert_eq!(progress_body("File\nDetails", Some(&progress)), "File\nDetails\nЗагрузка (42%)");
        progress.label = "42 из 100 МБ".into();
        assert_eq!(progress_body("File", Some(&progress)), "File\nЗагрузка — 42 из 100 МБ");
        progress.status.clear();
        progress.label.clear();
        progress.value = Some(2.0);
        assert_eq!(progress_body("", Some(&progress)), "100%");
        progress.value = Some(-1.0);
        assert_eq!(progress_body("", Some(&progress)), "0%");
    }

    #[test]
    fn invalid_or_missing_progress_does_not_invent_a_percentage() {
        let mut progress = Progress { status: String::new(), value: Some(f64::NAN), label: String::new() };
        assert_eq!(progress_body("  Original\n", Some(&progress)), "  Original\n");
        progress.value = Some(f64::INFINITY);
        assert_eq!(progress_body("Original", Some(&progress)), "Original");
        progress.value = None;
        progress.status = "Working".into();
        assert_eq!(progress_body("Original", Some(&progress)), "Original\nWorking");
        assert_eq!(progress_body("  Original\n", None), "  Original\n");
    }

    #[test]
    fn authorized_banner_keeps_phone_title_attribution_and_body() {
        let app_name = "ChatGPT";
        let device_name = "Example Phone";
        let device = "device-1";
        let key = "note-1";
        let toast = Toast {
            device: device.into(),
            key: key.into(),
            title: "Thread".into(),
            body: "Called example".into(),
            attribution: format!("{app_name} · {device_name}"),
            icon: Some(PathBuf::from("phone-app.png")),
            image: None,
            actions: vec![("copy".into(), "Copy".into())],
            reply: Some(("reply".into(), "Reply".into())),
            silent: false,
            progress: None,
            call: false,
        };
        let banner = banner_text(&toast);
        assert_eq!(banner.title, "Thread");
        assert_eq!(banner.subtitle, "ChatGPT · Example Phone");
        assert_eq!(banner.body, "Called example");
        assert_eq!(banner.device, device);
        assert_eq!(banner.key, key);
        assert_ne!(banner.title, app_name);
        assert_eq!(banner.identifier, notification_identifier(device, key));
        assert_eq!(notification_identifier(device, key), notification_identifier(device, key));
        assert_ne!(notification_identifier(device, key), notification_identifier(device, "note-2"));
        assert_ne!(notification_identifier("a:b", "c"), notification_identifier("a", "b:c"));
        assert!(admit_post(NotificationAuthorization::from_system(2)));
        assert_eq!(permission_action(NotificationAuthorization::Authorized), PermissionAction::None);
        assert!(!banner.identifier.contains("osascript"));
        assert!(!banner.identifier.contains("display notification"));
        assert!(!banner.quiet);
        let refreshed = banner_text(&Toast {
            actions: vec![("latest:0".into(), "Action".into())],
            reply: Some(("latest:1".into(), "Reply".into())),
            silent: true,
            ..toast.clone()
        });
        assert!(refreshed.quiet);
        assert_eq!(refreshed.actions, vec![("latest:0".into(), "Action".into())]);
        assert_eq!(refreshed.reply, Some(("latest:1".into(), "Reply".into())));
        let silent = Toast { silent: true, actions: Vec::new(), ..toast.clone() };
        assert!(banner_text(&silent).quiet);
        assert!(!banner_text(&Toast { call: true, ..silent.clone() }).quiet);
        let ending = Toast { actions: vec![(CALL_END.into(), "End".into())], ..silent };
        assert!(!banner_text(&ending).quiet);
        assert!(app_bundle_executable(Path::new("/Applications/NectarLink.app/Contents/MacOS/nectarlink")));

        let id = banner.identifier.as_str();
        let other = notification_identifier(device, "note-2");
        let mut pending = PendingPosts::new();

        // Same-key updates coalesce to the newest, whichever settings read finishes first.
        let older = pending.begin(device, id).unwrap();
        let newer = pending.begin(device, id).unwrap();
        assert!(!pending.admit(id, older));
        assert!(pending.admit(id, newer));
        assert!(!pending.added(id, newer, true));
        assert!(pending.slots.is_empty());
        let older = pending.begin(device, id).unwrap();
        let newer = pending.begin(device, id).unwrap();
        assert!(pending.admit(id, newer));
        assert!(!pending.admit(id, older));
        assert!(!pending.added(id, newer, true));
        assert!(pending.slots.is_empty());

        // An add already issued is replaced by the newer add; its late completion withdraws nothing.
        let older = pending.begin(device, id).unwrap();
        assert!(pending.admit(id, older));
        let newer = pending.begin(device, id).unwrap();
        assert!(pending.admit(id, newer));
        assert!(!pending.added(id, older, true));
        assert!(!pending.added(id, newer, true));
        assert!(pending.slots.is_empty());

        // Removing one key leaves another key's post alone.
        let kept = pending.begin(device, &other).unwrap();
        let dropped = pending.begin(device, id).unwrap();
        pending.cancel(id);
        assert!(!pending.admit(id, dropped));
        assert!(pending.admit(&other, kept));
        assert!(!pending.added(&other, kept, true));
        assert!(pending.slots.is_empty());
    }

    #[test]
    fn unread_or_refused_authorization_does_not_admit_a_banner() {
        for auth in [
            NotificationAuthorization::Unknown,
            NotificationAuthorization::NotDetermined,
            NotificationAuthorization::Denied,
            NotificationAuthorization::Provisional,
            NotificationAuthorization::from_system(4),
            NotificationAuthorization::from_system(99),
        ] {
            assert!(!admit_post(auth), "{auth:?}");
        }
        assert_eq!(permission_action(NotificationAuthorization::NotDetermined), PermissionAction::Enable);
        assert_eq!(permission_action(NotificationAuthorization::Denied), PermissionAction::OpenSettings);
        assert_eq!(permission_action(NotificationAuthorization::Provisional), PermissionAction::OpenSettings);
        assert_eq!(permission_action(NotificationAuthorization::Unknown), PermissionAction::None);
        assert_eq!(NotificationAuthorization::from_system(0), NotificationAuthorization::NotDetermined);
        assert_eq!(NotificationAuthorization::from_system(1), NotificationAuthorization::Denied);
        assert_eq!(NotificationAuthorization::from_system(3), NotificationAuthorization::Provisional);
        assert_eq!(
            notification_settings_url(Some(12)),
            "x-apple.systempreferences:com.apple.preference.notifications"
        );
        assert_eq!(
            notification_settings_url(Some(13)),
            "x-apple.systempreferences:com.apple.Notifications-Settings.extension"
        );
        assert_eq!(
            notification_settings_url(None),
            "x-apple.systempreferences:com.apple.Notifications-Settings.extension"
        );

        for exe in [
            "/repo/target/debug/nectarlink",
            "/repo/target/debug/Contents/MacOS/nectarlink",
            "/Applications/NectarLink/Contents/MacOS/nectarlink",
            "/Applications/NectarLink.app/Contents/nectarlink",
            "nectarlink",
            "/",
        ] {
            assert!(!app_bundle_executable(Path::new(exe)), "{exe}");
        }

        let device = "device-1";
        let id = notification_identifier(device, "note-1");
        let id = id.as_str();
        let mut pending = PendingPosts::new();

        // Refused settings and a remove before the settings read both drop the post.
        pending.begin(device, id).unwrap();
        pending.refuse(id);
        assert!(pending.slots.is_empty());
        let post = pending.begin(device, id).unwrap();
        pending.cancel(id);
        assert!(!pending.admit(id, post));
        assert!(pending.slots.is_empty());

        // A remove after the add was issued makes the late completion withdraw it.
        let post = pending.begin(device, id).unwrap();
        assert!(pending.admit(id, post));
        pending.cancel(id);
        assert!(pending.added(id, post, true));
        assert!(pending.slots.is_empty());

        // A failed add after a remove has nothing to withdraw.
        let post = pending.begin(device, id).unwrap();
        assert!(pending.admit(id, post));
        pending.cancel(id);
        assert!(!pending.added(id, post, false));
        assert!(pending.slots.is_empty());

        // Remove, then a newer post: the stale add withdraws only while the newer one is unissued.
        let stale = pending.begin(device, id).unwrap();
        assert!(pending.admit(id, stale));
        pending.cancel(id);
        let fresh = pending.begin(device, id).unwrap();
        assert!(pending.added(id, stale, true));
        assert!(pending.admit(id, fresh));
        assert!(!pending.added(id, fresh, true));
        let stale = pending.begin(device, id).unwrap();
        assert!(pending.admit(id, stale));
        pending.cancel(id);
        let fresh = pending.begin(device, id).unwrap();
        assert!(pending.admit(id, fresh));
        assert!(!pending.added(id, stale, true));
        assert!(!pending.added(id, fresh, true));
        assert!(pending.slots.is_empty());

        // A device remove cancels only that device and spares adds made after its cut.
        let other = notification_identifier("device-2", "note-1");
        let old = notification_identifier(device, "note-0");
        let gone = pending.begin(device, id).unwrap();
        let kept = pending.begin("device-2", &other).unwrap();
        let cut = pending.cancel_device(device, 2);
        assert!(!pending.admit(id, gone));
        assert!(pending.admit(&other, kept));
        let fresh = pending.begin(device, id).unwrap();
        assert!(pending.admit(id, fresh));
        assert!(!pending.added(id, fresh, true));
        assert!(!pending.sweep_may_withdraw(id, cut));
        assert!(pending.sweep_may_withdraw(&old, cut));
        pending.end_sweep();
        assert!(!pending.sweep_may_withdraw(id, cut));
        pending.end_sweep();
        assert!(!pending.added(&other, kept, true));
        assert!(pending.slots.is_empty());

        // The waiting list is bounded; a full list still takes updates for a waiting key.
        let mut pending = PendingPosts::new();
        for index in 0..MAX_PENDING {
            assert!(pending.begin(device, &format!("id-{index}")).is_some());
        }
        assert!(pending.begin(device, "id-overflow").is_none());
        assert!(pending.begin(device, "id-0").is_some());
        assert_eq!(pending.slots.len(), MAX_PENDING);
    }
}
