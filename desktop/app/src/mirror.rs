// SPDX-License-Identifier: GPL-3.0-or-later
//! A phone's screen on this PC (docs/protocol/mirror.md): asks the phone,
//! decodes its video on a thread of its own and hands each picture to the
//! mirror window's `VideoView`. Only the newest picture is shown; when the
//! decoder falls behind or loses its place, the phone is asked for a
//! keyframe. The phone's sound, when it sends it, plays on the PC's
//! speakers.

use crate::i18n::tr;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
    },
    time::{Duration, Instant},
};

use nectarlink_core::{
    DeviceId, Error, MIRROR_SCREEN, MirrorAudioConfig, MirrorConfig, MirrorSink, MirrorStart, NodeEvent,
};

use crate::{
    bridge::{
        app::{describe, show_message},
        native::ffi,
    },
    core_host,
    state::Changes,
    transfers::{ACTION_OPEN, ACTION_SHOW, TOAST_GROUP},
    win::{
        audio_out::Player,
        h264::{Decoder, to_bgrx},
        image::{Bitmap, encode_png},
        mp4::{MirrorRecorder, local_timestamp_filename, merge_sps_pps},
        toast::{self, Toast},
    },
};

/// What the PC asks for: the phone scales its screen to fit.
const OPTIONS: MirrorStart = MirrorStart {
    max_size: 1920,
    fps: 60,
    bitrate: 8_000_000,
    // A Mac doesn't offer mirror.listen: its core refuses the phone's sound.
    audio: !cfg!(target_os = "macos"),
    session: MIRROR_SCREEN,
    app: None,
    stay_awake: false,
    screen_off: false,
    watch: None,
    // Placeholder: core `mirror::start` stamps the live generation.
    request: None,
};
/// Mac host capture, automatic clipboard/Flow and reverse host control stay
/// unavailable. Phone input has separate negotiated owner/request guards.
pub(crate) const VIEW_ONLY: bool = cfg!(target_os = "macos");
/// App windows require the negotiated owned-capture capability and phone grant.
pub(crate) const APP_WINDOWS: bool = true;
/// Packets waiting for the decoder, at most (about two seconds).
const BACKLOG: usize = 120;
/// Packets in a row that may fail to decode, each retried from a fresh
/// keyframe, before the mirroring ends.
const DECODE_FAILURES: u32 = 3;
/// How often decode times are logged.
const STATS_EVERY: Duration = Duration::from_secs(10);
/// How long an in-window status notice stays visible.
const NOTICE_DURATION: Duration = Duration::from_secs(3);

/// A mirroring on this PC: a phone and the session, 0 for its screen,
/// others for its apps in windows of their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Window {
    pub device: DeviceId,
    pub session: u32,
}

impl Window {
    pub fn screen(device: DeviceId) -> Window {
        Window { device, session: MIRROR_SCREEN }
    }

    /// Its name for QML and for `VideoView`'s stream: `<device>/<session>`.
    pub fn key(&self) -> String {
        format!("{}/{}", self.device, self.session)
    }

    pub fn parse(key: &str) -> Option<Window> {
        let (device, session) = key.split_once('/')?;
        Some(Window { device: device.parse().ok()?, session: session.parse().ok()? })
    }
}

/// Where a mirroring stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for the phone (its user agreeing to share the screen, or the
    /// app starting).
    Asking,
    Showing,
    /// It ended; the reason for the window, if any.
    Ended(Option<String>),
}

/// A mirroring the PC asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shown {
    pub phase: Phase,
    /// For an app window: the app's name, and its package.
    pub app: Option<String>,
    pub pkg: Option<String>,
}

/// A phone's apps that open in windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Apps {
    Loading,
    Ready(Vec<App>),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub pkg: String,
    pub label: String,
    /// The icon, saved for QML.
    pub icon: Option<PathBuf>,
}

/// Remembered window position and size on this PC for an app window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WindowGeometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct SavedWindows {
    /// Keyed by `<device>/<pkg>`.
    #[serde(default)]
    geometry: HashMap<String, WindowGeometry>,
    /// Keyed by `<device>`, most recently opened `pkg` first.
    #[serde(default)]
    recent: HashMap<String, Vec<String>>,
}

impl SavedWindows {
    fn path() -> PathBuf {
        core_host::host().data_dir.join("app-windows.json")
    }

    fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        let path = Self::path();
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
}

#[derive(Debug, Clone)]
struct RequestNode {
    node: nectarlink_core::Node,
    app_view: Option<nectarlink_core::MirrorView>,
}

impl RequestNode {
    async fn stop(self, window: Window) {
        if window.session == MIRROR_SCREEN {
            self.node.mirror_stop(window.device, window.session).await;
        } else if let Some(view) = self.app_view {
            let _ = view.stop_if_current().await;
        } else {
            self.node.mirror_stop(window.device, window.session).await;
        }
    }
}

#[derive(Debug)]
struct State {
    windows: HashMap<Window, Shown>,
    /// Most recently focused or interacted-with mirror window.
    active_window: Option<Window>,
    /// Last clipboard text synced on window focus so unchanged text isn't resent.
    last_focus_clip: Option<String>,
    /// Phones whose sound is coming in, and the sink that owns each: a
    /// stream that was replaced must not clear the flag of the one that
    /// replaced it.
    sound: HashMap<DeviceId, u64>,
    /// The next sound generation, one per sink: a later sink owns a device's
    /// sound over an earlier one.
    sound_generation: u64,
    /// Phones kept awake while mirroring (their screen or app windows).
    stay_awake: HashSet<DeviceId>,
    /// Phones whose physical screen is turned off while mirroring (their
    /// screen or app windows); cleared when the last window closes.
    screen_off: HashSet<DeviceId>,
    /// Phone currently receiving PC keyboard input without screen mirroring.
    keyboard_device: Option<DeviceId>,
    /// Short transient feedback notices per mirror window: `(text, shown_at, token)`.
    notices: HashMap<Window, (String, Instant, u64)>,
    /// The video stream each window shows, by number: a window key is
    /// reused, so a decoder still finishing an older stream mustn't end or
    /// clear the one after it. Cleared when the window closes or asks again.
    streams: HashMap<Window, u64>,
    /// Exact request owner, retained until close or the next request.
    requests: HashMap<Window, u64>,
    /// Original Node for each local request, never reacquired on input dequeue.
    request_nodes: HashMap<Window, RequestNode>,
    /// Actual core wire token echoed by the authenticated phone video stream.
    control_requests: HashMap<Window, u64>,
    /// Closed sessions whose STOP must precede any replacement START.
    pending_stops: HashMap<Window, RequestNode>,
    /// The first-frame deadline each opened screen stream is held to; a
    /// stream that opens again replaces it.
    first_frames: HashMap<Window, u64>,
    apps: HashMap<DeviceId, Apps>,
    saved: SavedWindows,
}

impl State {
    /// Whether `device` has a window open or opening (the screen or an app).
    fn mirroring(&self, device: &DeviceId) -> bool {
        self.windows
            .iter()
            .any(|(w, shown)| w.device == *device && matches!(shown.phase, Phase::Asking | Phase::Showing))
    }

    fn set_phase(&mut self, window: Window, phase: Phase) {
        let showing = matches!(phase, Phase::Showing);
        if matches!(phase, Phase::Ended(_)) {
            self.request_nodes.remove(&window);
            self.control_requests.remove(&window);
        }
        if showing && self.active_window.is_none() {
            self.active_window = Some(window);
        } else if !showing && self.active_window == Some(window) {
            self.active_window = self
                .windows
                .iter()
                .find(|(w, shown)| **w != window && matches!(shown.phase, Phase::Showing))
                .map(|(w, _)| *w);
        }
        if let Some(shown) = self.windows.get_mut(&window) {
            shown.phase = phase;
        }
    }

    fn owns_request(&self, window: &Window, request: u64) -> bool {
        self.requests.get(window) == Some(&request)
    }

    /// Ends `window` with a reason when `deadline` of `request` runs out
    /// while it shows no picture yet; nothing after a frame, a close, a newer
    /// request or a stream that opened again. Its stream is let go in the
    /// same step, so a picture still on its way can't show in the ended window.
    fn end_without_first_frame(&mut self, window: Window, request: u64, deadline: u64) -> bool {
        if self.first_frames.get(&window) != Some(&deadline) {
            return false;
        }
        self.first_frames.remove(&window);
        if !self.owns_request(&window, request)
            || !self.windows.get(&window).is_some_and(|w| w.phase == Phase::Showing)
            || latest_bitmap(&window).is_some()
        {
            return false;
        }
        self.streams.remove(&window);
        self.set_phase(
            window,
            Phase::Ended(Some(tr!(
                "Mirror",
                "No picture arrived from the phone. Keep the phone unlocked while it shares its screen, then try again."
            ))),
        );
        true
    }

    /// Whether `stream` is still the one `window` shows.
    fn shows(&self, window: &Window, stream: u64) -> bool {
        self.streams.get(window) == Some(&stream)
    }

    /// A sink takes over a device's sound (see `claim_sound_owner`).
    fn claim_sound(&mut self, device: DeviceId, generation: u64, playing: bool) {
        claim_sound_owner(&mut self.sound, device, generation, playing);
    }

    /// A sink lets go of a device's sound (see `release_sound_owner`).
    fn release_sound(&mut self, device: DeviceId, generation: u64) {
        release_sound_owner(&mut self.sound, device, generation);
    }
}

/// A sink takes over a device's sound: it owns the flag from now on, and
/// only it may clear it. A sink older than the owner changes nothing: a
/// late format on a replaced stream must not take the flag back, nor
/// clear the newer owner's flag when its own player fails.
fn claim_sound_owner(sound: &mut HashMap<DeviceId, u64>, device: DeviceId, generation: u64, playing: bool) {
    if sound.get(&device).is_some_and(|&owner| owner > generation) {
        return;
    }
    if playing {
        sound.insert(device, generation);
    } else {
        sound.remove(&device);
    }
}

/// A sink lets go of a device's sound. Only the owner may clear the flag:
/// a sink that was replaced must not hide the sound that replaced it.
fn release_sound_owner(sound: &mut HashMap<DeviceId, u64>, device: DeviceId, generation: u64) {
    if sound.get(&device) == Some(&generation) {
        sound.remove(&device);
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            windows: HashMap::new(),
            active_window: None,
            last_focus_clip: None,
            sound: HashMap::new(),
            sound_generation: 0,
            stay_awake: HashSet::new(),
            screen_off: HashSet::new(),
            keyboard_device: None,
            notices: HashMap::new(),
            streams: HashMap::new(),
            requests: HashMap::new(),
            request_nodes: HashMap::new(),
            control_requests: HashMap::new(),
            pending_stops: HashMap::new(),
            first_frames: HashMap::new(),
            apps: HashMap::new(),
            saved: SavedWindows::load(),
        }
    }
}

/// The user turned the phones' sound off on this PC (until turned on).
static MUTED: AtomicBool = AtomicBool::new(false);
/// The next app window's session.
static NEXT_SESSION: AtomicU32 = AtomicU32::new(1);
/// Monotonic token for auto-clearing window notices.
static NOTICE_SEQ: AtomicU64 = AtomicU64::new(1);
/// The next video stream's number (see `State::streams`).
static NEXT_STREAM: AtomicU64 = AtomicU64::new(1);
/// The next request owner, distinct even when a window closes and reopens.
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
/// The next first-frame deadline (see `State::first_frames`).
static NEXT_DEADLINE: AtomicU64 = AtomicU64::new(1);
// Keep stops ordered with requests reaching the phone.
static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

type FrameMap = HashMap<Window, (u32, u32, Arc<Vec<u8>>)>;

/// Latest displayed BGRX frame per window, for instant screenshots.
static FRAMES: Mutex<Option<FrameMap>> = Mutex::new(None);
/// Active MP4 recorders per window.
static RECORDERS: Mutex<Option<HashMap<Window, MirrorRecorder>>> = Mutex::new(None);
/// Fast atomic count of active recorders to skip locking RECORDERS on the 60fps packet/audio hot path.
static ACTIVE_RECORDERS: AtomicUsize = AtomicUsize::new(0);
/// Latest video stream dimensions per window.
static LAST_CONFIG: Mutex<Option<HashMap<Window, (u32, u32)>>> = Mutex::new(None);
/// Latest audio stream format `(rate, channels)` per device.
static LAST_AUDIO_CONFIG: Mutex<Option<HashMap<DeviceId, (u32, u8)>>> = Mutex::new(None);
/// Latest Annex-B SPS/PPS bytes per window so mid-stream recordings have parameter sets immediately.
static LAST_SPS_PPS: Mutex<Option<HashMap<Window, Vec<u8>>>> = Mutex::new(None);

pub fn muted() -> bool {
    MUTED.load(Ordering::Relaxed)
}

pub fn set_muted(muted: bool) {
    MUTED.store(muted, Ordering::Relaxed);
    core_host::host().hub.changed(Changes::MIRROR);
}

/// Whether `device`'s sound is coming in.
pub fn has_sound(device: &DeviceId) -> bool {
    state(|s| s.sound.contains_key(device))
}

/// Whether `device` is set to stay awake while mirroring.
pub fn stay_awake(device: &DeviceId) -> bool {
    state(|s| s.stay_awake.contains(device))
}

/// Whether `device`'s physical screen is turned off while mirroring.
pub fn screen_off(device: &DeviceId) -> bool {
    state(|s| s.screen_off.contains(device))
}

/// Toggles keeping the phone awake while mirroring (its screen or app windows).
pub fn set_stay_awake(window: Window, on: bool) {
    let screen_off = state(|s| {
        if on {
            s.stay_awake.insert(window.device);
        } else {
            s.stay_awake.remove(&window.device);
        }
        s.screen_off.contains(&window.device)
    });
    set_notice(
        window,
        if on { "Keeping phone awake while mirroring" } else { "Phone sleep timeout restored" },
    );
    core_host::host().hub.changed(Changes::MIRROR);
    let Some(node) = core_host::node() else { return };
    core_host::spawn(async move {
        if let Err(e) = node.mirror_power(window.device, on, screen_off).await {
            tracing::debug!(error = %e, "mirror.power didn't reach the phone");
        }
    });
}

/// Toggles turning the phone's physical screen off while mirroring (its
/// screen or app windows).
pub fn set_screen_off(window: Window, on: bool) {
    let stay_awake = state(|s| {
        if on {
            s.screen_off.insert(window.device);
        } else {
            s.screen_off.remove(&window.device);
        }
        s.stay_awake.contains(&window.device)
    });
    set_notice(window, if on { "Phone screen turned off" } else { "Phone screen turned back on" });
    core_host::host().hub.changed(Changes::MIRROR);
    let Some(node) = core_host::node() else { return };
    core_host::spawn(async move {
        if let Err(e) = node.mirror_power(window.device, stay_awake, on).await {
            tracing::debug!(error = %e, "mirror.power didn't reach the phone");
        }
    });
}

/// Short status banner for `window` if shown within the last 3 seconds.
pub fn last_notice(window: &Window) -> String {
    state(|s| {
        s.notices
            .get(window)
            .filter(|(_, at, _)| at.elapsed() < NOTICE_DURATION)
            .map(|(text, _, _)| text.clone())
            .unwrap_or_default()
    })
}

fn set_notice(window: Window, text: impl Into<String>) {
    let token = NOTICE_SEQ.fetch_add(1, Ordering::Relaxed);
    state(|s| {
        s.notices.insert(window, (text.into(), Instant::now(), token));
    });
    core_host::host().hub.changed(Changes::MIRROR);
    core_host::spawn(async move {
        tokio::time::sleep(NOTICE_DURATION).await;
        let cleared = state(|s| {
            if s.notices.get(&window).is_some_and(|(_, _, t)| *t == token) {
                s.notices.remove(&window);
                true
            } else {
                false
            }
        });
        if cleared {
            core_host::host().hub.changed(Changes::MIRROR);
        }
    });
}

fn save_latest_frame(window: Window, width: u32, height: u32, bgrx: Vec<u8>) {
    FRAMES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(window, (width, height, Arc::new(bgrx)));
}

fn clear_window_buffers(window: &Window) {
    if let Some(frames) = FRAMES.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        frames.remove(window);
    }
    if let Some(cfg) = LAST_CONFIG.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        cfg.remove(window);
    }
    if let Some(sps) = LAST_SPS_PPS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        sps.remove(window);
    }
}

fn latest_bitmap(window: &Window) -> Option<Bitmap> {
    let (width, height, raw) = FRAMES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(window))
        .map(|(w, h, b)| (*w, *h, b.clone()))?;
    if width == 0 || height == 0 {
        return None;
    }
    let mut bgra = (*raw).clone();
    for px in bgra.as_chunks_mut::<4>().0 {
        px[3] = 0xFF;
    }
    Some(Bitmap { width, height, bgra })
}

fn free_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = path.extension().and_then(|e| e.to_str());
    for n in 2..10_000u32 {
        let candidate = match ext {
            Some(ext) => dir.join(format!("{stem} ({n}).{ext}")),
            None => dir.join(format!("{stem} ({n})")),
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    first
}

/// Copies the current mirror frame for `window` to the PC clipboard as PNG.
pub fn screenshot_clipboard(window: Window) {
    let Some(bitmap) = latest_bitmap(&window) else {
        set_notice(window, "Wait for the screen to appear first");
        show_message(tr!("Mirror", "Wait for the screen to appear first."));
        return;
    };
    let copied = encode_png(&bitmap)
        .map_err(|e| e.to_string())
        .and_then(|png| crate::win::clipboard::write_image("image/png", &png));
    match copied {
        Ok(()) => {
            set_notice(window, "Screenshot copied to clipboard");
            show_message(tr!("Mirror", "Screenshot copied to clipboard."));
        }
        Err(e) => {
            tracing::warn!(error = %e, "can't copy mirror screenshot");
            set_notice(window, "Couldn't copy screenshot");
            show_message(tr!("Mirror", "The screenshot couldn't be copied to the clipboard."));
        }
    }
}

/// Saves the current mirror frame for `window` to `Downloads\Nectarlink` as a PNG file.
pub fn screenshot_file(window: Window) {
    let Some(bitmap) = latest_bitmap(&window) else {
        set_notice(window, "Wait for the screen to appear first");
        show_message(tr!("Mirror", "Wait for the screen to appear first."));
        return;
    };
    let png = match encode_png(&bitmap) {
        Ok(png) => png,
        Err(e) => {
            tracing::warn!(error = %e, "can't encode mirror screenshot");
            set_notice(window, "Couldn't save screenshot");
            show_message(tr!("Mirror", "The screenshot couldn't be saved."));
            return;
        }
    };
    let base_name = local_timestamp_filename("Screenshot", "png");
    let dir = core_host::downloads_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        let subdir = if cfg!(windows) { "Downloads\\Nectarlink" } else { "Downloads/Nectarlink" };
        tracing::warn!(error = %e, "can't create {subdir}");
        set_notice(window, "Couldn't save screenshot");
        show_message(tr!("Mirror", "The screenshot couldn't be saved."));
        return;
    }
    let path = free_path(&dir, &base_name);
    if let Err(e) = std::fs::write(&path, &png) {
        tracing::warn!(error = %e, "can't write mirror screenshot");
        set_notice(window, "Couldn't save screenshot");
        show_message(tr!("Mirror", "The screenshot couldn't be saved."));
        return;
    }
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or(&base_name).to_owned();
    if let Some(node) = core_host::node() {
        node.record_timeline(
            nectarlink_core::TimelineKind::Photo,
            window.device,
            true,
            file_name.clone(),
            tr!("Mirror", "Mirror screenshot"),
            path.to_string_lossy().into_owned(),
            png.len() as u64,
            0,
            None,
        );
    }
    let device_name =
        core_host::host().hub.read(|s| s.name_of(&window.device)).unwrap_or_else(|| "your phone".into());
    let toast_body = if cfg!(windows) {
        tr!("Mirror", "Screenshot from %1, saved in Downloads\\Nectarlink", device_name)
    } else {
        tr!("Mirror", "Screenshot from %1, saved in Downloads/Nectarlink", device_name)
    };
    toast::show(Toast {
        device: TOAST_GROUP.into(),
        key: path.to_string_lossy().into_owned(),
        title: file_name.clone(),
        body: toast_body,
        attribution: "NectarLink".into(),
        icon: None,
        image: Some(path),
        actions: vec![(ACTION_OPEN.into(), "Open".into()), (ACTION_SHOW.into(), "Show in folder".into())],
        reply: None,
        silent: false,
        progress: None,
        call: false,
    });
    set_notice(window, tr!("Mirror", "Saved %1", file_name));
    let saved_msg = if cfg!(windows) {
        tr!("Mirror", "Saved %1 to Downloads\\Nectarlink.", file_name)
    } else {
        tr!("Mirror", "Saved %1 to Downloads/Nectarlink.", file_name)
    };
    show_message(saved_msg);
}

/// Whether `window` is currently recording to an MP4 file.
pub fn is_recording(window: &Window) -> bool {
    ACTIVE_RECORDERS.load(Ordering::Relaxed) > 0
        && RECORDERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|m| m.contains_key(window))
}

/// Unix epoch milliseconds when the active recording for `window` started (`0` if not recording).
pub fn recording_started_ms(window: &Window) -> i64 {
    if ACTIVE_RECORDERS.load(Ordering::Relaxed) == 0 {
        return 0;
    }
    RECORDERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(window))
        .map_or(0, MirrorRecorder::started_epoch_ms)
}

/// Starts or stops MP4 recording for `window`.
pub fn toggle_recording(window: Window) {
    if cfg!(target_os = "macos") { return; }
    if is_recording(&window) {
        stop_recording(window);
        return;
    }
    if !matches!(phase(&window), Some(Phase::Showing)) {
        set_notice(window, "Wait for the screen to appear first");
        show_message(tr!("Mirror", "Wait for the screen to appear first."));
        return;
    }
    let base_name = local_timestamp_filename("Mirror", "mp4");
    let dir = core_host::downloads_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        let subdir = if cfg!(windows) { "Downloads\\Nectarlink" } else { "Downloads/Nectarlink" };
        tracing::warn!(error = %e, "can't create {subdir}");
        set_notice(window, "Couldn't start recording");
        show_message(tr!("Mirror", "Couldn't start recording."));
        return;
    }
    let path = free_path(&dir, &base_name);
    let (width, height) = LAST_CONFIG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(&window).copied())
        .or_else(|| {
            FRAMES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .and_then(|m| m.get(&window).map(|(w, h, _)| (*w, *h)))
        })
        .unwrap_or((0, 0));

    let audio_cfg = if window.session == MIRROR_SCREEN && has_sound(&window.device) {
        LAST_AUDIO_CONFIG
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|m| m.get(&window.device).copied())
            .or(Some((48_000, 2)))
    } else {
        None
    };

    let mut recorder = MirrorRecorder::new(path, width, height, OPTIONS.fps, audio_cfg);
    if let Some(sps_pps) =
        LAST_SPS_PPS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&window))
    {
        recorder.set_sps_pps(sps_pps);
    }

    {
        let mut guard = RECORDERS.lock().unwrap_or_else(|e| e.into_inner());
        let map = guard.get_or_insert_with(HashMap::new);
        map.insert(window, recorder);
        ACTIVE_RECORDERS.store(map.len(), Ordering::Release);
    }

    set_notice(window, "Recording started");
    core_host::host().hub.changed(Changes::MIRROR);
    if let Some(node) = core_host::node() {
        core_host::spawn(async move { node.mirror_keyframe(window.device, window.session).await });
    }
}

/// Stops and finalizes any active MP4 recording for `window`.
pub fn stop_recording(window: Window) {
    let recorder = {
        let mut guard = RECORDERS.lock().unwrap_or_else(|e| e.into_inner());
        let rec = guard.as_mut().and_then(|m| m.remove(&window));
        ACTIVE_RECORDERS.store(guard.as_ref().map_or(0, HashMap::len), Ordering::Release);
        rec
    };
    let Some(recorder) = recorder else { return };
    core_host::host().hub.changed(Changes::MIRROR);
    let duration_secs = recorder.started_at().elapsed().as_secs().max(1);

    core_host::spawn(async move {
        let res = tokio::task::spawn_blocking(move || recorder.finish()).await;
        match res {
            Ok(Ok(path)) => {
                let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("Mirror.mp4").to_owned();
                let size_bytes = std::fs::metadata(&path).map_or(0, |m| m.len());
                if let Some(node) = core_host::node() {
                    node.record_timeline(
                        nectarlink_core::TimelineKind::Recording,
                        window.device,
                        true,
                        file_name.clone(),
                        tr!("Mirror", "Mirror recording"),
                        path.to_string_lossy().into_owned(),
                        size_bytes,
                        duration_secs,
                        None,
                    );
                }
                let device_name = core_host::host()
                    .hub
                    .read(|s| s.name_of(&window.device))
                    .unwrap_or_else(|| "your phone".into());
                let toast_body = if cfg!(windows) {
                    tr!("Mirror", "Recording from %1, saved in Downloads\\Nectarlink", device_name)
                } else {
                    tr!("Mirror", "Recording from %1, saved in Downloads/Nectarlink", device_name)
                };
                toast::show(Toast {
                    device: TOAST_GROUP.into(),
                    key: path.to_string_lossy().into_owned(),
                    title: file_name.clone(),
                    body: toast_body,
                    attribution: "NectarLink".into(),
                    icon: None,
                    image: None,
                    actions: vec![
                        (ACTION_OPEN.into(), "Open".into()),
                        (ACTION_SHOW.into(), tr!("Mirror", "Show in folder")),
                    ],
                    reply: None,
                    silent: false,
                    progress: None,
                    call: false,
                });
                set_notice(window, tr!("Mirror", "Saved %1", file_name));
                let saved_msg = if cfg!(windows) {
                    tr!("Mirror", "Saved %1 to Downloads\\Nectarlink.", file_name)
                } else {
                    tr!("Mirror", "Saved %1 to Downloads/Nectarlink.", file_name)
                };
                show_message(saved_msg);
            }
            Ok(Err(msg)) => {
                set_notice(window, msg.clone());
                show_message(msg);
            }
            Err(e) => {
                tracing::warn!(error = %e, "mirror recording finalize panicked");
            }
        }
    });
}

fn feed_recorder(window: Window, feed: impl FnOnce(&mut MirrorRecorder)) {
    if let Some(recorder) = RECORDERS.lock().unwrap_or_else(|e| e.into_inner())
        .as_mut().and_then(|recorders| recorders.get_mut(&window))
    {
        feed(recorder);
    }
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn state<T>(f: impl FnOnce(&mut State) -> T) -> T {
    f(STATE.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_default())
}

fn set_phase(window: Window, phase: Phase) {
    let showing = matches!(phase, Phase::Showing);
    state(|s| s.set_phase(window, phase));
    if showing {
        ensure_gamepad_loop();
    }
    core_host::host().hub.changed(Changes::MIRROR);
}

/// Where a mirroring stands (`None`: not asked, or closed).
pub fn phase(window: &Window) -> Option<Phase> {
    state(|s| s.windows.get(window).map(|w| w.phase.clone()))
}

/// Every mirroring the PC asked for, oldest first.
pub fn windows() -> Vec<(Window, Shown)> {
    let mut all: Vec<_> = state(|s| s.windows.iter().map(|(w, s)| (*w, s.clone())).collect());
    all.sort_by_key(|(w, _)| (w.device.to_string(), w.session));
    all
}

/// Remembered window position and size for `(device, pkg)`.
pub fn geometry(device: &DeviceId, pkg: &str) -> Option<WindowGeometry> {
    state(|s| s.saved.geometry.get(&format!("{device}/{pkg}")).copied())
}

/// Saves an app window's position and size on this PC.
pub fn save_geometry(window: Window, x: i32, y: i32, width: u32, height: u32) {
    if window.session == MIRROR_SCREEN || width < 200 || height < 200 {
        return;
    }
    state(|s| {
        let Some(pkg) = s.windows.get(&window).and_then(|w| w.pkg.clone()) else { return };
        let key = format!("{}/{pkg}", window.device);
        let geom = WindowGeometry { x, y, width, height };
        if s.saved.geometry.insert(key, geom) != Some(geom) {
            s.saved.save();
        }
    });
}

/// Package names of recent apps on `device` (apps opened from this PC first,
/// followed by apps with active or recent notifications), most recent first.
pub fn recent_apps(device: &DeviceId) -> Vec<String> {
    let saved = state(|s| s.saved.recent.get(&device.to_string()).cloned().unwrap_or_default());
    let (notif_pkgs, hist_pkgs) = core_host::host().hub.read(|s| {
        let active: Vec<String> = s
            .notifications
            .iter()
            .filter(|n| &n.device == device && !n.notification.app.is_empty())
            .map(|n| n.notification.app.clone())
            .collect();
        let history: Vec<String> = s
            .history
            .iter()
            .filter(|h| &h.device == device && !h.notification.app.is_empty())
            .map(|h| h.notification.app.clone())
            .collect();
        (active, history)
    });
    merge_recent_apps(&saved, notif_pkgs, hist_pkgs)
}

pub(crate) fn merge_recent_apps(
    saved: &[String],
    active_notif_pkgs: impl IntoIterator<Item = String>,
    history_pkgs: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let mut out = Vec::with_capacity(12);
    for pkg in saved.iter().cloned().chain(active_notif_pkgs).chain(history_pkgs) {
        if !pkg.is_empty() && !out.contains(&pkg) {
            out.push(pkg);
            if out.len() >= 12 {
                break;
            }
        }
    }
    out
}

fn record_recent(s: &mut State, device: &DeviceId, pkg: &str) {
    let list = s.saved.recent.entry(device.to_string()).or_default();
    list.retain(|p| p != pkg);
    list.insert(0, pkg.to_owned());
    list.truncate(12);
    s.saved.save();
}

/// What `device` is set to while mirroring: `(stay_awake, screen_off)`.
fn power(s: &State, device: &DeviceId) -> (bool, bool) {
    (s.stay_awake.contains(device), s.screen_off.contains(device))
}

/// Asks the phone for its screen.
pub fn start(device: DeviceId) {
    // A showing screen stays: re-asking would orphan the live stream and
    // waste a consent round trip. Mac-only; Windows keeps re-ask behavior.
    if VIEW_ONLY && phase(&Window::screen(device)) == Some(Phase::Showing) {
        return;
    }
    if keyboard_device() == Some(device) {
        set_remote_keyboard(None);
    }
    let window = Window::screen(device);
    let (stay_awake, screen_off) = state(|s| {
        // The phone's screen starts on, unless app windows already turned it off.
        if !s.mirroring(&device) {
            s.screen_off.remove(&device);
        }
        s.windows.insert(window, Shown { phase: Phase::Asking, app: None, pkg: None });
        power(s, &device)
    });
    request(window, MirrorStart { stay_awake, screen_off, ..OPTIONS });
}

/// Phone currently receiving PC keyboard input without screen mirroring.
pub fn keyboard_device() -> Option<DeviceId> {
    state(|s| s.keyboard_device)
}

/// Starts or stops remote keyboard typing on `device` without screen mirroring.
pub fn set_remote_keyboard(device: Option<DeviceId>) {
    if let Some(device) = device
        && control_reason(Window::screen(device)) != "available"
    {
        request_control(&Window::screen(device).key());
        return;
    }
    let prev = state(|s| {
        let prev = s.keyboard_device;
        s.keyboard_device = device;
        prev
    });
    if prev == device {
        return;
    }
    if let Some(old) = prev {
        input(Window::screen(old), nectarlink_core::MirrorInput::Key { key: "keyboard_off".into() });
    }
    if let Some(new) = device {
        input(Window::screen(new), nectarlink_core::MirrorInput::Key { key: "keyboard_on".into() });
    }
    core_host::host().hub.changed(Changes::MIRROR);
}

/// Toggles remote keyboard typing on `device` without screen mirroring.
pub fn toggle_remote_keyboard(device: DeviceId) {
    if keyboard_device() == Some(device) {
        set_remote_keyboard(None);
    } else {
        set_remote_keyboard(Some(device));
    }
}

/// Sends a special key (`"enter"`, `"backspace"`, `"delete"`, `"left"`, `"right"`, `"up"`, `"down"`, `"tab"`, `"back"`, `"home"`)
/// to `device` while remote keyboard mode is active.
pub fn keyboard_press(device: DeviceId, key_name: String) {
    if keyboard_device() == Some(device) {
        input(Window::screen(device), nectarlink_core::MirrorInput::Key { key: key_name });
    }
}

/// Sends typed text to `device` while remote keyboard mode is active, splitting into protocol-sized chunks.
pub fn keyboard_text(device: DeviceId, text: String) {
    if keyboard_device() != Some(device) || text.is_empty() {
        return;
    }
    let window = Window::screen(device);
    let mut piece = String::new();
    for c in text.chars() {
        if piece.len() + c.len_utf8() > nectarlink_core::MIRROR_MAX_TEXT_BYTES {
            input(window, nectarlink_core::MirrorInput::Text { text: std::mem::take(&mut piece) });
        }
        piece.push(c);
    }
    if !piece.is_empty() {
        input(window, nectarlink_core::MirrorInput::Text { text: piece });
    }
}

/// Pastes the PC's current clipboard text onto `device` while remote keyboard mode is active.
pub fn keyboard_paste(device: DeviceId) {
    if keyboard_device() != Some(device) {
        return;
    }
    if let crate::win::clipboard::Clip::Text { text, .. } = crate::win::clipboard::read_for_paste() {
        keyboard_text(device, text);
    }
}

/// Opens one of the phone's apps in a window of its own (or does nothing
/// when it's already open; reuses an ended window for that app).
pub fn start_app(device: DeviceId, pkg: String, label: String) {
    if !APP_WINDOWS {
        return;
    }
    let window = state(|s| {
        if s.windows.iter().any(|(w, shown)| {
            w.device == device
                && shown.pkg.as_deref() == Some(pkg.as_str())
                && matches!(shown.phase, Phase::Asking | Phase::Showing)
        }) {
            return None;
        }
        record_recent(s, &device, &pkg);
        let ended = s
            .windows
            .iter()
            .find(|(w, shown)| w.device == device && shown.pkg.as_deref() == Some(pkg.as_str()))
            .map(|(w, _)| *w);
        let window =
            ended.unwrap_or_else(|| Window { device, session: NEXT_SESSION.fetch_add(1, Ordering::Relaxed) });
        if !s.mirroring(&device) {
            s.screen_off.remove(&device);
        }
        s.windows.insert(window, Shown { phase: Phase::Asking, app: Some(label), pkg: Some(pkg.clone()) });
        Some((window, power(s, &device)))
    });
    let Some((window, (stay_awake, screen_off))) = window else { return };
    request(
        window,
        MirrorStart {
            audio: false,
            session: window.session,
            app: Some(pkg),
            stay_awake,
            screen_off,
            ..OPTIONS
        },
    );
}

/// Reopens a mirroring window that ended (the screen or an app window).
pub fn reopen(window: Window) {
    if window.session == MIRROR_SCREEN {
        start(window.device);
        return;
    }
    if !APP_WINDOWS {
        return;
    }
    let pkg = state(|s| {
        if !s.mirroring(&window.device) {
            s.screen_off.remove(&window.device);
        }
        let shown = s.windows.get_mut(&window)?;
        let pkg = shown.pkg.clone()?;
        shown.phase = Phase::Asking;
        record_recent(s, &window.device, &pkg);
        Some((pkg, power(s, &window.device)))
    });
    let Some((pkg, (stay_awake, screen_off))) = pkg else { return };
    request(
        window,
        MirrorStart {
            audio: false,
            session: window.session,
            app: Some(pkg),
            stay_awake,
            screen_off,
            ..OPTIONS
        },
    );
}

/// Resizes an app window's display on the phone (`width` and `height` are the
/// window's logical dimensions on the PC).
pub fn resize(window: Window, width: u32, height: u32) {
    if window.session == MIRROR_SCREEN || width < 120 || height < 120 {
        return;
    }
    let (mut w, mut h) = (width.saturating_mul(2), height.saturating_mul(2));
    let longest = w.max(h);
    if longest > OPTIONS.max_size {
        w = (u64::from(w) * u64::from(OPTIONS.max_size) / u64::from(longest)) as u32;
        h = (u64::from(h) * u64::from(OPTIONS.max_size) / u64::from(longest)) as u32;
    }
    let w = (w & !1).clamp(240, 4096);
    let h = (h & !1).clamp(240, 4096);
    let legacy = state(|s| s.request_nodes.get(&window)
        .filter(|owner| owner.app_view.is_none()).map(|owner| owner.node.clone()));
    if !cfg!(target_os = "macos") && let Some(node) = legacy {
        core_host::spawn(async move { let _ = node.mirror_resize(window.device, window.session, w, h).await; });
        return;
    }
    let captured = state(|s| {
        if !s.windows.get(&window).is_some_and(|shown| shown.phase == Phase::Showing) {
            return None;
        }
        Some((
            s.request_nodes.get(&window)?.app_view.clone()?,
            *s.requests.get(&window)?,
            *s.streams.get(&window)?,
        ))
    });
    let Some((target, local_request, stream)) = captured else { return };
    core_host::spawn(async move {
        let result = target
            .send_if_current(w, h, move || {
                state(|s| {
                    s.owns_request(&window, local_request)
                        && s.shows(&window, stream)
                        && s.windows.get(&window).is_some_and(|shown| shown.phase == Phase::Showing)
                })
            })
            .await;
        if let Err(e) = result {
            tracing::debug!(error = %e, "resize didn't reach the phone");
        }
    });
}

fn request(window: Window, options: MirrorStart) {
    let node = core_host::node();
    let request = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
    state(|s| {
        s.requests.insert(window, request);
        s.request_nodes.remove(&window);
        s.control_requests.remove(&window);
        if let Some(node) = &node {
            s.request_nodes.insert(window, RequestNode { node: node.clone(), app_view: None });
        }
        s.streams.remove(&window);
        clear_window_buffers(&window);
        ffi::video_clear(&window.key());
    });
    core_host::host().hub.changed(Changes::MIRROR);
    let Some(node) = node else { return };
    let app = options.app.is_some();
    core_host::spawn(async move {
        let started = {
            let _turn = TURN.lock().await;
            if !state(|s| s.owns_request(&window, request)) {
                return;
            }
            if let Some(owner) = state(|s| s.pending_stops.remove(&window)) {
                owner.stop(window).await;
            }
            if !state(|s| s.owns_request(&window, request)) {
                return;
            }
            if app && (cfg!(target_os = "macos") || node.mirror_app_windows_owned(window.device)) {
                node.mirror_start_owned(window.device, options, move |view| {
                    state(|s| {
                        if !s.owns_request(&window, request) {
                            return false;
                        }
                        let Some(owner) = s.request_nodes.get_mut(&window) else { return false };
                        owner.app_view = Some(view);
                        true
                    })
                })
                .await
            } else {
                node.mirror_start(window.device, options).await
            }
        };
        if let Err(e) = started {
            let reason = match e {
                Error::Denied => tr!("Mirror", "Mirroring is turned off for this phone."),
                Error::Unsupported if app => tr!(
                    "Mirror",
                    "Set up Wireless debugging in NectarLink on the phone to open its apps here."
                ),
                Error::Unsupported => {
                    tr!(
                        "Mirror",
                        "This phone's app doesn't share its screen yet. Update NectarLink on the phone."
                    )
                }
                Error::Offline | Error::NotPaired => tr!("Mirror", "The phone isn't connected."),
                e => describe(&e),
            };
            let current = state(|s| {
                if !s.owns_request(&window, request) {
                    return false;
                }
                s.set_phase(window, Phase::Ended(Some(reason)));
                true
            });
            if current {
                core_host::host().hub.changed(Changes::MIRROR);
            }
            return;
        }
        // An app window has no prompt to wait for on the phone: its first frame comes within
        // seconds, or the phone couldn't start it (its stream then ends with no picture).
        if app {
            tokio::time::sleep(APP_START_TIMEOUT).await;
            let _turn = TURN.lock().await;
            let stuck = state(|s| {
                if !s.owns_request(&window, request)
                    || !s.windows.get(&window).is_some_and(|w| w.phase == Phase::Asking)
                {
                    return None;
                }
                let owner = s.request_nodes.get(&window)?.clone();
                s.set_phase(
                    window,
                    Phase::Ended(Some(tr!(
                        "Mirror",
                        "The app didn't open on the phone. Try again, or open NectarLink on the phone to check Wireless debugging."
                    ))),
                );
                Some(owner)
            });
            if let Some(owner) = stuck {
                tracing::warn!(session = window.session, "an app window never started on the phone");
                core_host::host().hub.changed(Changes::MIRROR);
                owner.stop(window).await;
            }
        }
    });
}

/// How long an app window may take to show its first frame.
const APP_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// How long the phone's screen may take to show its first frame once its
/// stream is open.
const SCREEN_FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(15);

/// Holds the screen's newly opened stream to a first frame: when none
/// arrives in time, the request still showing ends with a reason and its
/// stream stops, leaving Try again.
fn expect_first_frame(window: Window) {
    let deadline = NEXT_DEADLINE.fetch_add(1, Ordering::Relaxed);
    let armed = state(|s| {
        let request = *s.requests.get(&window)?;
        let node = s.request_nodes.get(&window)?.node.clone();
        s.first_frames.insert(window, deadline);
        Some((request, node))
    });
    let Some((request, node)) = armed else { return };
    core_host::spawn(async move {
        tokio::time::sleep(SCREEN_FIRST_FRAME_TIMEOUT).await;
        let _turn = TURN.lock().await;
        if !state(|s| s.end_without_first_frame(window, request, deadline)) {
            return;
        }
        tracing::warn!("the phone's screen never sent a picture");
        core_host::host().hub.changed(Changes::MIRROR);
        node.mirror_stop(window.device, window.session).await;
    });
}

/// Stops a mirroring (its window closed).
pub fn stop(window: Window) {
    stop_recording(window);
    state(|s| {
        if let Some(owner) = s.request_nodes.remove(&window) {
            s.pending_stops.insert(window, owner);
        }
        s.windows.remove(&window);
        s.notices.remove(&window);
        s.requests.remove(&window);
        s.control_requests.remove(&window);
        s.first_frames.remove(&window);
        s.streams.remove(&window);
        clear_window_buffers(&window);
        ffi::video_clear(&window.key());
        if !s.mirroring(&window.device) {
            s.screen_off.remove(&window.device);
        }
    });
    core_host::host().hub.changed(Changes::MIRROR);
    core_host::spawn(async move {
        let _turn = TURN.lock().await;
        // A replacement request may already have flushed this exact close.
        if let Some(owner) = state(|s| s.pending_stops.remove(&window)) {
            owner.stop(window).await;
        }
    });
}

pub fn on_event(event: &NodeEvent) {
    match event {
        NodeEvent::Mirroring { device, session, on: true } => {
            if *session != MIRROR_SCREEN {
                return;
            }
            let window = Window { device: *device, session: *session };
            set_phase(window, Phase::Showing);
            expect_first_frame(window);
        }
        // Closed here: nothing to say. Stopped on the phone: say so.
        NodeEvent::Mirroring { device, session, on: false } => {
            let window = Window { device: *device, session: *session };
            if *session != MIRROR_SCREEN {
                // Source decline/retirement may have no sink yet. A queued old
                // peer/session event cannot retire a replacement live target.
                let ended = state(|s| {
                    let Some(view) = s.request_nodes.get(&window).and_then(|owner| owner.app_view.as_ref())
                    else {
                        return false;
                    };
                    if view.is_current() {
                        return false;
                    }
                    stop_recording(window);
                    s.streams.remove(&window);
                    clear_window_buffers(&window);
                    ffi::video_clear(&window.key());
                    s.set_phase(window, Phase::Ended(None));
                    true
                });
                if ended {
                    core_host::host().hub.changed(Changes::MIRROR);
                }
                return;
            }
            stop_recording(window);
            clear_window_buffers(&window);
            if *session == MIRROR_SCREEN {
                let cleared_kb = state(|s| {
                    if s.keyboard_device == Some(*device) {
                        s.keyboard_device = None;
                        true
                    } else {
                        false
                    }
                });
                if cleared_kb {
                    core_host::host().hub.changed(Changes::MIRROR);
                }
            }
            // An ending this PC caused keeps its reason.
            if matches!(phase(&window), Some(Phase::Asking | Phase::Showing)) {
                set_phase(window, Phase::Ended(None));
            }
            state(|s| {
                if !s.mirroring(device) {
                    s.screen_off.remove(device);
                }
            });
        }
        // A phone that went away takes its app list with it.
        NodeEvent::LinkChanged { device, link: nectarlink_core::LinkState::Offline { .. } } => {
            let cleared_kb = state(|s| {
                s.apps.remove(device);
                s.screen_off.remove(device);
                if s.keyboard_device == Some(*device) {
                    s.keyboard_device = None;
                    true
                } else {
                    false
                }
            });
            if cleared_kb {
                core_host::host().hub.changed(Changes::MIRROR);
            }
        }
        NodeEvent::DeviceRemoved(id) => {
            let cleared_kb = state(|s| {
                s.apps.remove(id);
                s.screen_off.remove(id);
                s.requests.retain(|w, _| w.device != *id);
                s.request_nodes.retain(|w, _| w.device != *id);
                s.control_requests.retain(|w, _| w.device != *id);
                s.pending_stops.retain(|w, _| w.device != *id);
                s.streams.retain(|w, _| w.device != *id);
                if s.keyboard_device == Some(*id) {
                    s.keyboard_device = None;
                    true
                } else {
                    false
                }
            });
            if cleared_kb {
                core_host::host().hub.changed(Changes::MIRROR);
            }
        }
        _ => {}
    }
}

/// A phone's apps that open in windows, as last loaded.
pub fn apps(device: &DeviceId) -> Option<Apps> {
    state(|s| s.apps.get(device).cloned())
}

/// Loads (or reloads) a phone's apps, with their icons. A list already
/// loaded (this run, or saved by an earlier one) shows at once and stays up
/// while the phone sends a fresh one: only the first load ever waits.
pub fn load_apps(device: DeviceId) {
    let data_dir = core_host::host().data_dir.clone();
    let shown = state(|s| match s.apps.get(&device) {
        Some(Apps::Ready(_)) => true,
        _ => match saved_apps(&data_dir, &device) {
            Some(saved) => {
                s.apps.insert(device, Apps::Ready(saved));
                true
            }
            None => {
                s.apps.insert(device, Apps::Loading);
                false
            }
        },
    });
    core_host::host().hub.changed(Changes::MIRROR);
    let Some(node) = core_host::node() else { return };
    core_host::spawn(async move {
        let started = Instant::now();
        let loaded = match node.mirror_apps(device).await {
            Ok(apps) => {
                let apps = tokio::task::spawn_blocking(move || {
                    let apps: Vec<App> = apps
                        .into_iter()
                        .map(|a| App {
                            icon: a.icon.and_then(|png| crate::icons::save(&data_dir, &a.pkg, &png).ok()),
                            pkg: a.pkg,
                            label: a.label,
                        })
                        .collect();
                    save_apps(&data_dir, &device, &apps);
                    apps
                })
                .await
                .unwrap_or_default();
                tracing::info!(
                    apps = apps.len(),
                    ms = started.elapsed().as_millis() as u64,
                    "loaded a phone's apps"
                );
                Apps::Ready(apps)
            }
            // A list on screen stays: it is a moment old, not wrong.
            Err(e) if shown => {
                tracing::info!(error = %e, "can't refresh a phone's apps; keeping the last list");
                return;
            }
            Err(Error::Unsupported) => Apps::Failed(tr!(
                "Mirror",
                "Set up Wireless debugging in NectarLink on the phone to open its apps here."
            )),
            Err(e) => Apps::Failed(describe(&e)),
        };
        state(|s| s.apps.insert(device, loaded));
        core_host::host().hub.changed(Changes::MIRROR);
    });
}

fn saved_apps_path(data_dir: &Path, device: &DeviceId) -> PathBuf {
    data_dir.join("cache").join("apps").join(format!("{device}.json"))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedApp {
    pkg: String,
    label: String,
}

/// The phone's apps as an earlier run saved them, with the icons it kept.
fn saved_apps(data_dir: &Path, device: &DeviceId) -> Option<Vec<App>> {
    let bytes = std::fs::read(saved_apps_path(data_dir, device)).ok()?;
    let saved: Vec<SavedApp> = serde_json::from_slice(&bytes).ok()?;
    let apps: Vec<App> = saved
        .into_iter()
        .filter(|a| nectarlink_core::is_package_name(&a.pkg))
        .map(|a| App { icon: crate::icons::existing(data_dir, &a.pkg), pkg: a.pkg, label: a.label })
        .collect();
    (!apps.is_empty()).then_some(apps)
}

fn save_apps(data_dir: &Path, device: &DeviceId, apps: &[App]) {
    let path = saved_apps_path(data_dir, device);
    let saved: Vec<SavedApp> =
        apps.iter().map(|a| SavedApp { pkg: a.pkg.clone(), label: a.label.clone() }).collect();
    let written = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&saved).map_err(std::io::Error::other)?)?;
        std::fs::rename(&tmp, &path)
    });
    if let Err(e) = written {
        tracing::warn!(error = %e, "can't save a phone's apps");
    }
}

/// Returns cached `(pkg, label)` pairs for `device` synchronously (0 ms) for
/// the Command Palette, placing recently mirrored apps first and triggering a
/// background app list load if not yet cached.
pub fn palette_apps(device: DeviceId) -> Vec<(String, String)> {
    let recent = recent_apps(&device);
    let current = apps(&device);
    let (app_names, can_load) = core_host::host().hub.read(|s| {
        let online = s
            .devices
            .iter()
            .any(|d| d.id == device && matches!(d.link, nectarlink_core::LinkState::Online { .. }));
        let avail = s.matrices.get(&device).and_then(|m| m.state("mirroring.app_windows"))
            == Some(nectarlink_core::FeatureState::Available);
        (s.app_names.clone(), online && avail)
    });

    if current.is_none() && can_load {
        load_apps(device);
    }

    let ready_list = match current {
        Some(Apps::Ready(list)) => list,
        _ => Vec::new(),
    };

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();

    for pkg in &recent {
        let pkg_trim = pkg.trim();
        if pkg_trim.is_empty() {
            continue;
        }
        let label = ready_list
            .iter()
            .find(|a| a.pkg == pkg_trim)
            .map(|a| a.label.clone())
            .or_else(|| app_names.get(pkg_trim).cloned())
            .unwrap_or_else(|| pkg_trim.rsplit('.').next().unwrap_or(pkg_trim).to_owned());
        if !label.trim().is_empty() && seen.insert(pkg_trim.to_owned()) {
            out.push((pkg_trim.to_owned(), label));
        }
    }

    for a in ready_list {
        let pkg_trim = a.pkg.trim();
        let label_trim = a.label.trim();
        if !pkg_trim.is_empty() && !label_trim.is_empty() && seen.insert(pkg_trim.to_owned()) {
            out.push((pkg_trim.to_owned(), label_trim.to_owned()));
        }
    }

    for (pkg, label) in app_names {
        let pkg_trim = pkg.trim();
        let label_trim = label.trim();
        if !pkg_trim.is_empty() && !label_trim.is_empty() && seen.insert(pkg_trim.to_owned()) {
            out.push((pkg_trim.to_owned(), label_trim.to_owned()));
        }
    }

    out
}

/// Actual admission for the exact showing phone screen. App-window control
/// remains separately fenced; no Mac host injection/capture is enabled here.
pub fn control_reason(window: Window) -> &'static str {
    if window.session != MIRROR_SCREEN && !APP_WINDOWS {
        return "unsupported";
    }
    let target = state(|s| {
        if !s.windows.get(&window).is_some_and(|shown| shown.phase == Phase::Showing)
            || !s.streams.contains_key(&window)
            || latest_bitmap(&window).is_none()
        {
            return None;
        }
        Some((s.request_nodes.get(&window)?.node.clone(), *s.control_requests.get(&window)?))
    });
    let Some((node, wire_request)) = target else {
        return "notShowing";
    };
    node.mirror_control_reason_for_session(window.device, window.session, wire_request)
}

#[path = "mirror_scroll.rs"]
mod mirror_scroll;

/// Ordered queue reuses the existing MirrorInput handlers. Every phone event
/// captures its original Node/connection/request/epoch before entering the queue.
pub fn input(window: Window, input: nectarlink_core::MirrorInput) {
    if !cfg!(target_os = "macos") && control_reason(window) == "unsupported" {
        if let Some(node) = core_host::node() {
            core_host::spawn(async move { let _ = node.mirror_input(window.device, window.session, input).await; });
        }
        return;
    }
    if window.session != MIRROR_SCREEN && !APP_WINDOWS {
        return;
    }
    let captured = state(|s| {
        if !s.windows.get(&window).is_some_and(|shown| shown.phase == Phase::Showing)
            || latest_bitmap(&window).is_none()
        {
            return None;
        }
        Some((
            s.request_nodes.get(&window)?.node.clone(),
            *s.requests.get(&window)?,
            *s.streams.get(&window)?,
            *s.control_requests.get(&window)?,
        ))
    });
    let Some((node, local_request, stream, wire_request)) = captured else { return };
    let Ok(control) = node.mirror_control_for_session(window.device, window.session, wire_request) else {
        return;
    };
    static QUEUE: OnceLock<mirror_scroll::Inputs<nectarlink_core::MirrorControl>> = OnceLock::new();
    let mut started = false;
    let queue = QUEUE.get_or_init(|| {
        started = true;
        mirror_scroll::Inputs::new()
    });
    if started {
        core_host::spawn(async move {
            loop {
                let mirror_scroll::Queued { window, control, request, stream, input } = queue.next().await;
                if !control.is_current() {
                    continue;
                }
                let result = control
                    .send_if_current(input, move || {
                        state(|s| {
                            s.owns_request(&window, request)
                                && s.shows(&window, stream)
                                && latest_bitmap(&window).is_some()
                                && s.windows.get(&window).is_some_and(|shown| shown.phase == Phase::Showing)
                        })
                    })
                    .await;
                if let Err(error) = result {
                    tracing::debug!(%error, "input didn't reach the phone");
                }
            }
        });
    }
    let queued = mirror_scroll::Queued { window, control, request, stream, input };
    if !queue.push(queued, nectarlink_core::MirrorControl::is_current) {
        tracing::debug!("no room for this input; the phone is behind");
    }
}

/// Resolves the currently active or showing mirror window.
fn active_or_showing_window() -> Option<Window> {
    state(|s| {
        if let Some(active) = s.active_window
            && s.windows.get(&active).is_some_and(|w| matches!(w.phase, Phase::Showing))
        {
            return Some(active);
        }
        s.windows
            .iter()
            .find(|(_, shown)| matches!(shown.phase, Phase::Showing))
            .or_else(|| s.windows.iter().next())
            .map(|(w, _)| *w)
    })
}

/// Parses dropped `file://` URLs or local paths, optionally prefixed by a `<device>/<session>` window key.
pub(crate) fn parse_dropped_file_urls(raw_urls: &[String]) -> (Option<Window>, Vec<PathBuf>) {
    let mut explicit_window: Option<Window> = None;
    let mut paths: Vec<PathBuf> = Vec::new();
    for (idx, raw) in raw_urls.iter().enumerate() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if idx == 0
            && !trimmed.starts_with("file:")
            && !Path::new(trimmed).exists()
            && let Some(win) = Window::parse(trimmed)
        {
            explicit_window = Some(win);
            continue;
        }
        let local = cxx_qt_lib::QUrl::from(&cxx_qt_lib::QString::from(trimmed))
            .to_local_file()
            .map(|p| String::from(&p))
            .filter(|p| !p.is_empty())
            .or_else(|| {
                if cfg!(windows) {
                    trimmed
                        .strip_prefix("file:///")
                        .or_else(|| trimmed.strip_prefix("file://"))
                        .map(|s| s.replace('/', "\\"))
                } else {
                    trimmed.strip_prefix("file://").map(str::to_owned)
                }
            });
        let pb = PathBuf::from(local.as_deref().unwrap_or(trimmed));
        if pb.exists() {
            paths.push(pb);
        }
    }
    (explicit_window, paths)
}

/// Sends files dropped onto a mirror window to the mirrored phone and shows an
/// in-window toast notice.
pub fn drop_files(raw_urls: &[String]) {
    let (explicit_window, paths) = parse_dropped_file_urls(raw_urls);
    if paths.is_empty() {
        return;
    }
    let Some(window) = explicit_window.or_else(active_or_showing_window) else {
        return;
    };
    let notice = if paths.len() == 1 {
        let name = paths[0].file_name().and_then(|n| n.to_str()).unwrap_or("file");
        tr!("Mirror", "Sending %1…", name)
    } else {
        tr!("Mirror", "Sending %1 files…", paths.len())
    };
    set_notice(window, notice);
    crate::transfers::send(window.device, paths);
}

/// Guidance only: a desktop cannot grant phone-owner consent or rearm Stop.
/// Never synthesize a touch to ask for permission.
pub fn request_control(key: &str) {
    let Some(window) = Window::parse(key) else { return };
    let notice = match control_reason(window) {
        "available" => return,
        "ownerApproval" => {
            tr!("Mirror", "On the phone, allow this computer to control the phone in Settings.")
        }
        "stopped" => tr!(
            "Mirror",
            "Phone control was stopped on the phone. To allow this computer again, turn Screen sharing and typing off and then on for this computer in NectarLink's settings on the phone."
        ),
        "inputUnavailable" => {
            tr!("Mirror", "On the phone, enable Control from your PC or start Elevated mode.")
        }
        "unsupported" => tr!("Mirror", "Phone control is unavailable for this connection."),
        _ => tr!("Mirror", "Connect the phone and start screen mirroring to control it."),
    };
    set_notice(window, notice);
}

/// Notifies PC-to-phone cursor/keyboard Flow when the mouse enters `key`.
pub fn enter_window(key: &str) {
    if let Some(win) = Window::parse(key) {
        state(|s| {
            s.active_window = Some(win);
        });
        if !VIEW_ONLY {
            crate::input_share::enter_remote(win.device);
        }
    }
}

/// Notifies PC-to-phone cursor/keyboard Flow when the mouse leaves a mirror window.
pub fn leave_window() {
    let _ = crate::input_share::leave_remote();
}

/// Records text that arrived from a remote device so focus/hover sync never echoes it back.
#[allow(dead_code)]
pub fn note_remote_clipboard(text: &str) {
    let normalized = text.replace("\r\n", "\n");
    state(|s| {
        s.last_focus_clip = Some(normalized);
    });
}

/// Synchronizes the clipboard from PC to `device` only if the clipboard contains new text.
pub fn sync_clipboard_for_device(device: DeviceId) {
    let Some(node) = core_host::node() else { return };
    let can_send = core_host::host().hub.read(|s| {
        s.matrices.get(&device).and_then(|m| m.state("clipboard.pc_to_phone"))
            == Some(nectarlink_core::FeatureState::Available)
    });
    if !can_send {
        return;
    }

    if let crate::win::clipboard::Clip::Text { text, html, rtf } = crate::win::clipboard::read()
        && !text.is_empty()
    {
        let normalized = text.replace("\r\n", "\n");
        let changed = state(|s| {
            if s.last_focus_clip.as_deref() == Some(normalized.as_str()) {
                false
            } else {
                s.last_focus_clip = Some(normalized);
                true
            }
        });
        if changed {
            core_host::spawn(async move {
                let _ = node.send_clipboard_rich(device, text, html, rtf).await;
            });
        }
    }
}

/// Sends the PC's clipboard to the phone when `window` gains focus, if
/// "Clipboard follows control" is on.
pub fn sync_clipboard_on_focus(window: Window) {
    state(|s| {
        s.active_window = Some(window);
    });
    if !VIEW_ONLY && crate::settings::flow_follow_clipboard() {
        sync_clipboard_for_device(window.device);
    }
}

/// Standard XInput / Xbox left thumbstick deadzone.
#[allow(dead_code)]
pub const LEFT_THUMB_DEADZONE: i16 = 7849;
/// Standard XInput / Xbox right thumbstick deadzone.
#[allow(dead_code)]
pub const RIGHT_THUMB_DEADZONE: i16 = 8689;
/// Standard XInput / Xbox trigger threshold.
#[allow(dead_code)]
pub const TRIGGER_THRESHOLD: u8 = 30;

/// Normalized controller snapshot forwarded via [`nectarlink_core::MirrorInput::Gamepad`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GamepadState {
    pub buttons: u16,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
    pub lt: u8,
    pub rt: u8,
}

impl GamepadState {
    /// Converts this controller snapshot into a [`nectarlink_core::MirrorInput::Gamepad`] message.
    pub fn to_mirror_input(self) -> nectarlink_core::MirrorInput {
        nectarlink_core::MirrorInput::Gamepad {
            buttons: self.buttons,
            lx: self.lx,
            ly: self.ly,
            rx: self.rx,
            ry: self.ry,
            lt: self.lt,
            rt: self.rt,
        }
    }
}

#[allow(dead_code)]
fn deadzone_axis(val: i16, deadzone: i16) -> i16 {
    if val.saturating_abs() <= deadzone { 0 } else { val }
}

/// Applies stick deadzones and trigger thresholds so idle controller noise does
/// not generate redundant [`nectarlink_core::MirrorInput::Gamepad`] frames.
#[allow(dead_code)]
pub fn normalize_gamepad_state(raw: GamepadState) -> GamepadState {
    GamepadState {
        buttons: raw.buttons,
        lx: deadzone_axis(raw.lx, LEFT_THUMB_DEADZONE),
        ly: deadzone_axis(raw.ly, LEFT_THUMB_DEADZONE),
        rx: deadzone_axis(raw.rx, RIGHT_THUMB_DEADZONE),
        ry: deadzone_axis(raw.ry, RIGHT_THUMB_DEADZONE),
        lt: if raw.lt <= TRIGGER_THRESHOLD { 0 } else { raw.lt },
        rt: if raw.rt <= TRIGGER_THRESHOLD { 0 } else { raw.rt },
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn poll_xinput_gamepad() -> Option<GamepadState> {
    use windows::Win32::UI::Input::XboxController::{XINPUT_STATE, XInputGetState};
    for user_index in 0..4u32 {
        let mut xstate = XINPUT_STATE::default();
        // SAFETY: xstate is a valid stack-allocated XINPUT_STATE for slot 0..3.
        if unsafe { XInputGetState(user_index, &mut xstate) } == 0 {
            let gp = xstate.Gamepad;
            return Some(normalize_gamepad_state(GamepadState {
                buttons: gp.wButtons.0,
                lx: gp.sThumbLX,
                ly: gp.sThumbLY,
                rx: gp.sThumbRX,
                ry: gp.sThumbRY,
                lt: gp.bLeftTrigger,
                rt: gp.bRightTrigger,
            }));
        }
    }
    None
}

#[cfg(windows)]
fn poll_winrt_gamepad() -> Option<GamepadState> {
    use windows::Gaming::Input::{Gamepad, GamepadButtons};
    let gamepads = Gamepad::Gamepads().ok()?;
    if gamepads.Size().ok()? == 0 {
        return None;
    }
    let pad = gamepads.GetAt(0).ok()?;
    let reading = pad.GetCurrentReading().ok()?;
    let b = reading.Buttons;
    let mut buttons = 0u16;
    if (b & GamepadButtons::DPadUp) != GamepadButtons::None {
        buttons |= 0x0001;
    }
    if (b & GamepadButtons::DPadDown) != GamepadButtons::None {
        buttons |= 0x0002;
    }
    if (b & GamepadButtons::DPadLeft) != GamepadButtons::None {
        buttons |= 0x0004;
    }
    if (b & GamepadButtons::DPadRight) != GamepadButtons::None {
        buttons |= 0x0008;
    }
    if (b & GamepadButtons::Menu) != GamepadButtons::None {
        buttons |= 0x0010;
    }
    if (b & GamepadButtons::View) != GamepadButtons::None {
        buttons |= 0x0020;
    }
    if (b & GamepadButtons::LeftThumbstick) != GamepadButtons::None {
        buttons |= 0x0040;
    }
    if (b & GamepadButtons::RightThumbstick) != GamepadButtons::None {
        buttons |= 0x0080;
    }
    if (b & GamepadButtons::LeftShoulder) != GamepadButtons::None {
        buttons |= 0x0100;
    }
    if (b & GamepadButtons::RightShoulder) != GamepadButtons::None {
        buttons |= 0x0200;
    }
    if (b & GamepadButtons::A) != GamepadButtons::None {
        buttons |= 0x1000;
    }
    if (b & GamepadButtons::B) != GamepadButtons::None {
        buttons |= 0x2000;
    }
    if (b & GamepadButtons::X) != GamepadButtons::None {
        buttons |= 0x4000;
    }
    if (b & GamepadButtons::Y) != GamepadButtons::None {
        buttons |= 0x8000;
    }
    let scale_axis = |v: f64| -> i16 { (v.clamp(-1.0, 1.0) * f64::from(i16::MAX)).round() as i16 };
    let scale_trigger = |v: f64| -> u8 { (v.clamp(0.0, 1.0) * 255.0).round() as u8 };
    Some(normalize_gamepad_state(GamepadState {
        buttons,
        lx: scale_axis(reading.LeftThumbstickX),
        ly: scale_axis(reading.LeftThumbstickY),
        rx: scale_axis(reading.RightThumbstickX),
        ry: scale_axis(reading.RightThumbstickY),
        lt: scale_trigger(reading.LeftTrigger),
        rt: scale_trigger(reading.RightTrigger),
    }))
}

#[cfg(windows)]
fn poll_gamepad() -> Option<GamepadState> {
    poll_xinput_gamepad().or_else(poll_winrt_gamepad)
}

#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct JsEvent {
    time: u32,
    value: i16,
    type_: u8,
    number: u8,
}

#[cfg(target_os = "linux")]
fn apply_js_event(state: &mut GamepadState, ev: JsEvent) {
    let kind = ev.type_ & !0x80; // Mask out JS_EVENT_INIT (0x80).
    match kind {
        0x01 => {
            // JS_EVENT_BUTTON
            let mask: u16 = match ev.number {
                0 => 0x1000,  // A
                1 => 0x2000,  // B
                2 => 0x4000,  // X
                3 => 0x8000,  // Y
                4 => 0x0100,  // LB
                5 => 0x0200,  // RB
                6 => 0x0020,  // View / Back
                7 => 0x0010,  // Menu / Start
                9 => 0x0040,  // LThumb
                10 => 0x0080, // RThumb
                12 => 0x0001, // DPad Up
                13 => 0x0002, // DPad Down
                14 => 0x0004, // DPad Left
                15 => 0x0008, // DPad Right
                _ => 0,
            };
            if mask != 0 {
                if ev.value != 0 {
                    state.buttons |= mask;
                } else {
                    state.buttons &= !mask;
                }
            }
        }
        0x02 => {
            // JS_EVENT_AXIS
            match ev.number {
                0 => state.lx = ev.value,
                1 => state.ly = ev.value.saturating_neg(),
                2 => {
                    state.lt = ((i32::from(ev.value) + 32768) * 255 / 65535).clamp(0, 255) as u8;
                }
                3 => state.rx = ev.value,
                4 => state.ry = ev.value.saturating_neg(),
                5 => {
                    state.rt = ((i32::from(ev.value) + 32768) * 255 / 65535).clamp(0, 255) as u8;
                }
                6 => {
                    state.buttons &= !(0x0004 | 0x0008);
                    if ev.value < 0 {
                        state.buttons |= 0x0004; // DPad Left
                    } else if ev.value > 0 {
                        state.buttons |= 0x0008; // DPad Right
                    }
                }
                7 => {
                    state.buttons &= !(0x0001 | 0x0002);
                    if ev.value < 0 {
                        state.buttons |= 0x0001; // DPad Up
                    } else if ev.value > 0 {
                        state.buttons |= 0x0002; // DPad Down
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

#[cfg(target_os = "linux")]
struct LinuxGamepadPoller {
    fd: std::ffi::c_int,
    state: GamepadState,
}

#[cfg(target_os = "linux")]
impl Default for LinuxGamepadPoller {
    fn default() -> Self {
        Self { fd: -1, state: GamepadState::default() }
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn poll_gamepad() -> Option<GamepadState> {
    static POLLER: Mutex<LinuxGamepadPoller> = Mutex::new(LinuxGamepadPoller {
        fd: -1,
        state: GamepadState { buttons: 0, lx: 0, ly: 0, rx: 0, ry: 0, lt: 0, rt: 0 },
    });
    let mut poller = POLLER.lock().unwrap_or_else(|e| e.into_inner());
    if poller.fd < 0 {
        let paths = [c"/dev/input/js0", c"/dev/input/js1", c"/dev/input/js2", c"/dev/input/js3"];
        for path in paths {
            let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
            if fd >= 0 {
                poller.fd = fd;
                poller.state = GamepadState::default();
                break;
            }
        }
    }
    if poller.fd < 0 {
        return None;
    }

    loop {
        let mut ev = JsEvent { time: 0, value: 0, type_: 0, number: 0 };
        let n = unsafe {
            libc::read(poller.fd, (&mut ev as *mut JsEvent).cast(), std::mem::size_of::<JsEvent>())
        };
        if n == std::mem::size_of::<JsEvent>() as isize {
            apply_js_event(&mut poller.state, ev);
        } else if n < 0 {
            let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if err == libc::EAGAIN || err == libc::EWOULDBLOCK {
                break;
            }
            unsafe {
                libc::close(poller.fd);
            }
            poller.fd = -1;
            poller.state = GamepadState::default();
            return None;
        } else {
            unsafe {
                libc::close(poller.fd);
            }
            poller.fd = -1;
            poller.state = GamepadState::default();
            return None;
        }
    }

    Some(normalize_gamepad_state(poller.state))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn poll_gamepad() -> Option<GamepadState> {
    None
}

fn ensure_gamepad_loop() {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    core_host::spawn(async move {
        let mut last_state = GamepadState::default();
        loop {
            let target = active_or_showing_window().filter(|w| matches!(phase(w), Some(Phase::Showing)));
            let Some(window) = target else {
                RUNNING.store(false, Ordering::Release);
                break;
            };
            match poll_gamepad() {
                Some(current) => {
                    if current != last_state {
                        last_state = current;
                        input(window, current.to_mirror_input());
                    }
                    tokio::time::sleep(Duration::from_millis(16)).await;
                }
                None => {
                    if last_state != GamepadState::default() {
                        last_state = GamepadState::default();
                        input(window, last_state.to_mirror_input());
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }
    });
}

/// Where the core puts a phone's video (or the screen's sound, from a
/// stream of its own): only for mirrorings this PC asked for.
pub fn sink(device: &DeviceId, session: u32) -> Option<Arc<dyn MirrorSink>> {
    let window = Window { device: *device, session };
    let wanted = APP_WINDOWS || session == MIRROR_SCREEN;
    state(|s| {
        let request = *s.requests.get(&window)?;
        let live = s.windows.get(&window).is_some_and(|w| matches!(w.phase, Phase::Asking | Phase::Showing));
        if !(wanted && live) {
            return None;
        }
        // Each sink gets a generation, so a later sound stream can tell that
        // it owns a device's sound over an earlier one.
        s.sound_generation = s.sound_generation.saturating_add(1);
        Some(Arc::new(Sink::new(window, request, s.sound_generation)) as Arc<dyn MirrorSink>)
    })
}

enum Item {
    Config(MirrorConfig),
    Packet { keyframe: bool, time_us: u64, data: Vec<u8> },
    Ended,
}

/// Hands the core's packets to the decoder thread, or its sound to the
/// player; each starts with its stream's first packet (the format).
struct Sink {
    window: Window,
    request: u64,
    /// This sink's sound generation: it owns its device's sound flag while it
    /// plays, and only it may clear it.
    generation: u64,
    video: OnceLock<Video>,
    sound: Mutex<Option<Player>>,
}

/// The decoder thread's end of a sink.
struct Video {
    stream: u64,
    queue: SyncSender<Item>,
    /// Packets waiting for the decoder.
    waiting: Arc<AtomicUsize>,
    /// A packet was dropped: the frames after it are left out until a
    /// keyframe gets in, so none decodes against the missing one.
    skipping: AtomicBool,
}

impl Sink {
    fn new(window: Window, request: u64, generation: u64) -> Sink {
        Sink { window, request, generation, video: OnceLock::new(), sound: Mutex::new(None) }
    }
}

impl Video {
    fn start(window: Window, request: u64) -> Video {
        let (queue, items) = sync_channel(BACKLOG);
        let waiting = Arc::new(AtomicUsize::new(0));
        let counter = waiting.clone();
        let stream = NEXT_STREAM.fetch_add(1, Ordering::Relaxed);
        state(|s| {
            if s.owns_request(&window, request) {
                s.streams.insert(window, stream);
            }
        });
        let started = std::thread::Builder::new().name("mirror-decoder".into()).spawn(move || {
            decode(window, stream, &items, &counter);
        });
        if let Err(e) = started {
            tracing::error!(error = %e, "can't start the video decoder");
        }
        Video { stream, queue, waiting, skipping: AtomicBool::new(false) }
    }
}

impl Sink {
    fn keyframe_please(&self) {
        let window = self.window;
        if let Some(node) = core_host::node() {
            core_host::spawn(async move { node.mirror_keyframe(window.device, window.session).await });
        }
    }
}

impl MirrorSink for Sink {
    fn config(&self, config: MirrorConfig) {
        if !state(|s| {
            if !s.owns_request(&self.window, self.request) {
                return false;
            }
            if self.window.session != MIRROR_SCREEN {
                let Some(view) = s.request_nodes.get(&self.window).and_then(|owner| owner.app_view.as_ref())
                else {
                    return false;
                };
                if config.request != Some(view.request())
                    || !view.is_current()
                    || self.video.get().is_some_and(|video| !s.shows(&self.window, video.stream))
                {
                    return false;
                }
            }
            true
        }) {
            return;
        }
        let video = self.video.get_or_init(|| Video::start(self.window, self.request));
        let accepted = state(|s| {
            if !s.owns_request(&self.window, self.request) || !s.shows(&self.window, video.stream) {
                return None;
            }
            if self.window.session != MIRROR_SCREEN {
                let owner = s.request_nodes.get(&self.window)?;
                if let Some(view) = owner.app_view.as_ref() {
                    if config.request != Some(view.request()) || !view.is_current() { return None; }
                } else if cfg!(target_os = "macos") {
                    return None;
                }
                s.set_phase(self.window, Phase::Showing);
            }
            if let Some(request) = config.request {
                s.control_requests.insert(self.window, request);
            } else {
                s.control_requests.remove(&self.window);
            }
            LAST_CONFIG
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_or_insert_with(HashMap::new)
                .insert(self.window, (config.width, config.height));
            Some(feed_recorder(self.window, |rec| rec.on_config(config.width, config.height)))
        });
        let Some(()) = accepted else { return };
        // The decoder repeats its exact stream check after this queue wait.
        let _ = video.queue.send(Item::Config(config));
        core_host::host().hub.changed(Changes::MIRROR);
    }

    fn packet(&self, keyframe: bool, time_us: u64, data: Vec<u8>) {
        let Some(video) = self.video.get() else { return };
        let accepted = state(|s| {
            if !s.owns_request(&self.window, self.request) || !s.shows(&self.window, video.stream) {
                return None;
            }
            if keyframe {
                let mut sps_map = LAST_SPS_PPS.lock().unwrap_or_else(|e| e.into_inner());
                let entry = sps_map.get_or_insert_with(HashMap::new).entry(self.window).or_default();
                merge_sps_pps(entry, &data);
            }
            if ACTIVE_RECORDERS.load(Ordering::Relaxed) > 0 {
                feed_recorder(self.window, |rec| {
                    if let Err(e) = rec.on_packet(keyframe, time_us, &data) {
                        tracing::warn!(error = %e, "mirror recording packet failed");
                    }
                })
            }
            Some(())
        });
        let Some(()) = accepted else { return };
        if !keyframe && video.skipping.load(Ordering::Acquire) {
            return;
        }
        match video.queue.try_send(Item::Packet { keyframe, time_us, data }) {
            Ok(()) => {
                video.skipping.store(false, Ordering::Release);
                video.waiting.fetch_add(1, Ordering::AcqRel);
            }
            Err(TrySendError::Disconnected(_)) => {}
            // The decoder is behind; it starts over at the next keyframe.
            Err(TrySendError::Full(_)) => {
                video.skipping.store(true, Ordering::Release);
                self.keyframe_please();
            }
        }
    }

    fn ended(&self) {
        let Some(video) = self.video.get() else { return };
        let current = state(|s| {
            if !s.owns_request(&self.window, self.request) || !s.shows(&self.window, video.stream) {
                return false;
            }
            stop_recording(self.window);
            if self.window.session != MIRROR_SCREEN {
                s.streams.remove(&self.window);
                clear_window_buffers(&self.window);
                ffi::video_clear(&self.window.key());
                s.set_phase(self.window, Phase::Ended(None));
            }
            true
        });
        let _ = video.queue.send(Item::Ended);
        if current {
            core_host::host().hub.changed(Changes::MIRROR);
        }
    }

    fn audio_config(&self, config: MirrorAudioConfig) {
        LAST_AUDIO_CONFIG
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(HashMap::new)
            .insert(self.window.device, (config.rate, config.channels));
        if self.window.session == MIRROR_SCREEN
            && ACTIVE_RECORDERS.load(Ordering::Relaxed) > 0
            && let Some(rec) = RECORDERS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_mut()
                .and_then(|m| m.get_mut(&self.window))
        {
            rec.on_audio_config(config.rate, config.channels);
        }
        let (old_player, playing) = {
            let mut sound = self.sound.lock().unwrap_or_else(|e| e.into_inner());
            // A new format: a new player (the old one ends when dropped).
            let new_player = match Player::start(config.rate, u16::from(config.channels)) {
                Ok(player) => Some(player),
                Err(e) => {
                    tracing::warn!(error = %e, "can't play the phone's sound");
                    None
                }
            };
            let old_player = std::mem::replace(&mut *sound, new_player);
            let playing = sound.is_some();
            drop(sound);
            (old_player, playing)
        };
        drop(old_player);
        // This sink owns the device's sound from now on: a stream that
        // replaces it later takes the flag over.
        state(|s| s.claim_sound(self.window.device, self.generation, playing));
        core_host::host().hub.changed(Changes::MIRROR);
    }

    fn audio(&self, time_us: u64, data: Vec<u8>) {
        if self.window.session == MIRROR_SCREEN
            && ACTIVE_RECORDERS.load(Ordering::Relaxed) > 0
            && let Some(rec) = RECORDERS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_mut()
                .and_then(|m| m.get_mut(&self.window))
            && let Err(e) = rec.on_audio(time_us, &data)
        {
            tracing::debug!(error = %e, "mirror recording audio packet failed");
        }
        if muted() {
            return;
        }
        if let Some(player) = self.sound.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            player.push(data);
        }
    }

    fn audio_ended(&self) {
        let old_player = {
            let mut sound = self.sound.lock().unwrap_or_else(|e| e.into_inner());
            let old_player = sound.take();
            drop(sound);
            old_player
        };
        drop(old_player);
        // Only the sink that owns the device's sound may clear the flag: a
        // stream that was replaced must not hide the sound that replaced it.
        state(|s| s.release_sound(self.window.device, self.generation));
        core_host::host().hub.changed(Changes::MIRROR);
    }
}

/// Ends a mirroring this PC can't show, and stops the phone sending it;
/// nothing once `window` has closed or asked again (`id` is then old).
fn give_up(window: Window, id: u64, reason: String) {
    let owner = state(|s| {
        if !s.shows(&window, id) {
            return None;
        }
        let owner = s.request_nodes.get(&window)?.clone();
        s.set_phase(window, Phase::Ended(Some(reason)));
        Some(owner)
    });
    let Some(owner) = owner else { return };
    core_host::host().hub.changed(Changes::MIRROR);
    core_host::spawn(async move {
        let _turn = TURN.lock().await;
        if state(|s| s.shows(&window, id)) {
            owner.stop(window).await;
        }
    });
}


/// The decoder thread: decodes everything (later pictures depend on
/// earlier ones), shows the newest picture of each batch.
fn decode(window: Window, id: u64, items: &Receiver<Item>, waiting: &AtomicUsize) {
    let stream = window.key();
    // On the GPU when the PC has one (falling back to software on its own),
    // so big phone screens and several app windows stay smooth.
    let mut decoder = match Decoder::new_gpu("h264") {
        Ok(decoder) => decoder,
        Err(e) => {
            tracing::error!(error = %e, "no H.264 decoder");
            give_up(window, id, tr!("Mirror", "This PC can't decode the phone's video."));
            return;
        }
    };
    tracing::info!(gpu = decoder.on_gpu(), codec = decoder.codec(), "mirror decoder ready");
    let keyframe_please = || {
        if let Some(node) = core_host::node() {
            core_host::spawn(async move { node.mirror_keyframe(window.device, window.session).await });
        }
    };
    let mut size = (0, 0);
    // After a gap, pictures are garbage until the next keyframe.
    let mut need_keyframe = true;
    let mut failures = 0;
    let mut stats = Stats::new();
    while let Ok(item) = items.recv() {
        if !state(|s| s.shows(&window, id)) {
            break;
        }
        match item {
            Item::Config(config) => {
                if decoder.codec() != config.codec {
                    match Decoder::new_gpu(&config.codec) {
                        Ok(next) => {
                            decoder = next;
                            tracing::info!(
                                gpu = decoder.on_gpu(),
                                codec = decoder.codec(),
                                "mirror decoder ready"
                            );
                        }
                        Err(e) => {
                            tracing::error!(error = %e, codec = %config.codec, "no decoder for codec");
                            give_up(
                                window,
                                id,
                                format!("This PC can't decode {} video.", config.codec.to_uppercase()),
                            );
                            return;
                        }
                    }
                } else if size != (0, 0) && size != (config.width, config.height) {
                    decoder.flush();
                }
                size = (config.width, config.height);
                need_keyframe = true;
            }
            Item::Packet { keyframe, time_us, data } => {
                let behind = waiting.fetch_sub(1, Ordering::AcqRel) > 1;
                if need_keyframe && !keyframe {
                    continue;
                }
                need_keyframe = false;
                let started = Instant::now();
                let pictures = match decoder.decode(&data, time_us) {
                    Ok(pictures) => {
                        failures = 0;
                        pictures
                    }
                    Err(e) => {
                        failures += 1;
                        if failures >= DECODE_FAILURES {
                            tracing::warn!(error = %e, failures, "the phone's video keeps failing to decode");
                            give_up(window, id, tr!("Mirror", "This PC can't decode the phone's video."));
                            break;
                        }
                        tracing::debug!(error = %e, "a video packet didn't decode");
                        decoder.flush();
                        need_keyframe = true;
                        keyframe_please();
                        continue;
                    }
                };
                let decoded = started.elapsed();
                // Behind: skip showing this one, the next is already here.
                if let (Some(picture), false) = (pictures.last(), behind) {
                    let converted = Instant::now();
                    let (w, h) = if size.0 > 0 { size } else { (picture.width, picture.height) };
                    let (src_w, src_h) = (w.min(picture.width), h.min(picture.height));
                    let bgrx = to_bgrx(picture, w, h);
                    let (width, height, pixels) = if (src_w, src_h) == (w, h) || src_w == 0 || src_h == 0 {
                        (src_w, src_h, bgrx)
                    } else {
                        (w, h, scale_bgrx(&bgrx, src_w, src_h, w, h))
                    };
                    // Keep ownership through publication: close/reopen cannot
                    // clear the window and then receive this older frame.
                    let became_ready = state(|s| {
                        if s.shows(&window, id) {
                            let first = latest_bitmap(&window).is_none();
                            ffi::video_frame(&stream, width, height, &pixels);
                            save_latest_frame(window, width, height, pixels);
                            first
                        } else {
                            false
                        }
                    });
                    if became_ready {
                        core_host::host().hub.changed(Changes::MIRROR);
                    }
                    stats.shown(decoded, converted.elapsed());
                } else {
                    stats.skipped();
                }
            }
            Item::Ended => break,
        }
    }
    state(|s| {
        if s.shows(&window, id) {
            clear_window_buffers(&window);
            ffi::video_clear(&stream);
        }
    });
}

fn scale_bgrx(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let mut dst = vec![0u8; (dst_w * dst_h * 4) as usize];
    for y in 0..dst_h {
        let sy = ((u64::from(y) * u64::from(src_h)) / u64::from(dst_h)) as usize;
        let src_row = &src[sy * (src_w as usize * 4)..(sy + 1) * (src_w as usize * 4)];
        let dst_row = &mut dst[(y as usize) * (dst_w as usize * 4)..(y as usize + 1) * (dst_w as usize * 4)];
        for x in 0..dst_w as usize {
            let sx = ((x as u64 * u64::from(src_w)) / u64::from(dst_w)) as usize;
            dst_row[x * 4..x * 4 + 4].copy_from_slice(&src_row[sx * 4..sx * 4 + 4]);
        }
    }
    dst
}

/// Decode and conversion times, logged now and then.
struct Stats {
    since: Instant,
    shown: u32,
    skipped: u32,
    decode: Duration,
    convert: Duration,
}

impl Stats {
    fn new() -> Stats {
        Stats { since: Instant::now(), shown: 0, skipped: 0, decode: Duration::ZERO, convert: Duration::ZERO }
    }

    fn shown(&mut self, decode: Duration, convert: Duration) {
        self.shown += 1;
        self.decode += decode;
        self.convert += convert;
        self.maybe_log();
    }

    fn skipped(&mut self) {
        self.skipped += 1;
        self.maybe_log();
    }

    fn maybe_log(&mut self) {
        let elapsed = self.since.elapsed();
        if elapsed < STATS_EVERY || self.shown == 0 {
            return;
        }
        tracing::info!(
            fps = format!("{:.1}", f64::from(self.shown) / elapsed.as_secs_f64()),
            skipped = self.skipped,
            decode_ms = format!("{:.1}", self.decode.as_secs_f64() * 1000.0 / f64::from(self.shown)),
            convert_ms = format!("{:.1}", self.convert.as_secs_f64() * 1000.0 / f64::from(self.shown)),
            "mirroring"
        );
        *self = Stats::new();
    }
}

/// The view-only receiver in its real window, as a runnable gate for the
/// non-default `test-support` build:
/// `nectarlink-desktop --mirror-window-gate --data-dir <run>`.
///
/// The production `Mirror` bridge, `MirrorWindow.qml` and `VideoView` show a
/// second, synthetic phone node paired by QR over 127.0.0.1. That phone only
/// answers `mirror.start` and sends the committed 128x128 red keyframe, which
/// the receiver scales to each announced format; nothing is captured, and
/// there is no microphone, sound, input, app window or saved identity. The
/// last window records that keyframe at its own size into the run's
/// `received` folder, never the user's Downloads.
#[cfg(test)]
mod tests {
    use super::{
        App, GamepadState, LEFT_THUMB_DEADZONE, RIGHT_THUMB_DEADZONE, TRIGGER_THRESHOLD, merge_recent_apps,
        normalize_gamepad_state, parse_dropped_file_urls, save_apps, saved_apps, saved_apps_path,
    };

    #[test]
    fn recent_apps_combines_saved_and_notification_packages_without_duplicates() {
        let saved = vec!["com.android.settings".to_owned(), "org.telegram.messenger".to_owned()];
        let active = vec![
            "org.telegram.messenger".to_owned(),
            "com.google.android.apps.messaging".to_owned(),
            String::new(),
        ];
        let history = vec!["com.google.android.apps.messaging".to_owned(), "com.spotify.music".to_owned()];
        assert_eq!(
            merge_recent_apps(&saved, active, history),
            vec![
                "com.android.settings".to_owned(),
                "org.telegram.messenger".to_owned(),
                "com.google.android.apps.messaging".to_owned(),
                "com.spotify.music".to_owned(),
            ]
        );
    }

    #[test]
    fn gamepad_deadzone_filters_idle_noise_and_converts_to_mirror_input() {
        let idle = normalize_gamepad_state(GamepadState {
            buttons: 0,
            lx: LEFT_THUMB_DEADZONE - 10,
            ly: -LEFT_THUMB_DEADZONE,
            rx: RIGHT_THUMB_DEADZONE,
            ry: -(RIGHT_THUMB_DEADZONE - 1),
            lt: TRIGGER_THRESHOLD,
            rt: 5,
        });
        assert_eq!(idle, GamepadState::default());

        let active = normalize_gamepad_state(GamepadState {
            buttons: 0x1001, // A + DPadUp
            lx: 16_000,
            ly: -24_000,
            rx: -12_000,
            ry: 30_000,
            lt: 128,
            rt: 255,
        });
        assert_eq!(
            active.to_mirror_input(),
            nectarlink_core::MirrorInput::Gamepad {
                buttons: 0x1001,
                lx: 16_000,
                ly: -24_000,
                rx: -12_000,
                ry: 30_000,
                lt: 128,
                rt: 255,
            }
        );
    }

    #[test]
    fn parse_dropped_file_urls_extracts_existing_paths_and_optional_window_key() {
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("alpha.txt");
        std::fs::write(&file_a, b"hello").unwrap();
        let file_url = format!("file:///{}", file_a.to_string_lossy().replace('\\', "/"));
        let dev = nectarlink_core::DeviceId([0x11; 32]);
        let key = format!("{dev}/0");

        let (win, paths) = parse_dropped_file_urls(&[key, file_url]);
        assert_eq!(win.map(|w| w.device), Some(dev));
        assert_eq!(paths, vec![file_a]);
    }

    #[test]
    fn a_phones_apps_are_kept_for_the_next_run_with_their_icons() {
        let dir = tempfile::tempdir().unwrap();
        let phone = nectarlink_core::DeviceId([0x22; 32]);
        assert!(saved_apps(dir.path(), &phone).is_none(), "nothing saved yet");

        let png = b"\x89PNG\r\n\x1a\nicon";
        let icon = crate::icons::save(dir.path(), "org.example.maps", png).unwrap();
        let apps = vec![
            App { pkg: "org.example.maps".into(), label: "Maps".into(), icon: Some(icon.clone()) },
            App { pkg: "org.example.notes".into(), label: "Notes".into(), icon: None },
        ];
        save_apps(dir.path(), &phone, &apps);
        assert_eq!(saved_apps(dir.path(), &phone), Some(apps));

        // Another phone has its own list; a damaged file is no list.
        assert!(saved_apps(dir.path(), &nectarlink_core::DeviceId([0x33; 32])).is_none());
        std::fs::write(saved_apps_path(dir.path(), &phone), b"{not json").unwrap();
        assert!(saved_apps(dir.path(), &phone).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_js_event_updates_buttons_and_axes() {
        use super::{JsEvent, apply_js_event};

        let mut state = GamepadState::default();
        // Press A (0 = 0x1000) and RB (5 = 0x0200), plus init flag (0x80) on Menu (7 = 0x0010).
        apply_js_event(&mut state, JsEvent { time: 1, value: 1, type_: 0x01, number: 0 });
        apply_js_event(&mut state, JsEvent { time: 2, value: 1, type_: 0x01, number: 5 });
        apply_js_event(&mut state, JsEvent { time: 3, value: 1, type_: 0x81, number: 7 });
        assert_eq!(state.buttons, 0x1000 | 0x0200 | 0x0010);

        // Release RB.
        apply_js_event(&mut state, JsEvent { time: 4, value: 0, type_: 0x01, number: 5 });
        assert_eq!(state.buttons, 0x1000 | 0x0010);

        // Move LX (0), LY inverted (1), LT (2), RX (3), RY inverted (4), RT (5), DPad X/Y (6/7).
        apply_js_event(&mut state, JsEvent { time: 5, value: 20_000, type_: 0x02, number: 0 });
        apply_js_event(&mut state, JsEvent { time: 6, value: -18_000, type_: 0x02, number: 1 });
        apply_js_event(&mut state, JsEvent { time: 7, value: 32_767, type_: 0x02, number: 2 });
        apply_js_event(&mut state, JsEvent { time: 8, value: -15_000, type_: 0x02, number: 3 });
        apply_js_event(&mut state, JsEvent { time: 9, value: 22_000, type_: 0x02, number: 4 });
        apply_js_event(&mut state, JsEvent { time: 10, value: 0, type_: 0x02, number: 5 });
        apply_js_event(&mut state, JsEvent { time: 11, value: -32_767, type_: 0x02, number: 6 }); // DPad Left
        apply_js_event(&mut state, JsEvent { time: 12, value: -32_767, type_: 0x02, number: 7 }); // DPad Up

        let normalized = normalize_gamepad_state(state);
        assert_eq!(normalized.buttons, 0x1000 | 0x0010 | 0x0004 | 0x0001);
        assert_eq!(normalized.lx, 20_000);
        assert_eq!(normalized.ly, 18_000);
        assert_eq!(normalized.rx, -15_000);
        assert_eq!(normalized.ry, -22_000);
        assert_eq!(normalized.lt, 255);
        assert_eq!(normalized.rt, 127);
    }
}
