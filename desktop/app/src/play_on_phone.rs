// SPDX-License-Identifier: GPL-3.0-or-later
//! Play on phone, on this PC (docs/protocol/media.md §4): moves what one of
//! its players plays to a phone. A protected streaming service opens on
//! the phone in its app and the PC pauses; anything else the phone watches
//! streamed from the PC, which keeps playing it.

#![allow(unsafe_code)]

use crate::i18n::tr;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use nectarlink_core::{DeviceId, Error, FeatureState, LinkState, MediaAction, PlayOnPhone};
#[cfg(windows)]
use windows::Win32::{
    Foundation::{HWND, RECT},
    System::Com::{COINIT_MULTITHREADED, CoInitializeEx},
};

#[cfg(target_os = "linux")]
use crate::win::browser::{HWND, RECT};
use crate::{
    bridge::app::{describe, show_message},
    core_host,
    win::browser::{self, Automation, Window},
};

/// The phone to play on: the first one online that can watch a PC's
/// playback, else the first one online.
pub fn phone() -> Option<(DeviceId, String)> {
    core_host::host().hub.read(|s| {
        let online: Vec<_> =
            s.devices.iter().filter(|d| matches!(d.link, LinkState::Online { .. })).collect();
        let watches = |id: &DeviceId| {
            s.matrices.get(id).and_then(|m| m.state("media.play_on_phone")) == Some(FeatureState::Available)
        };
        online.iter().find(|d| watches(&d.id)).or_else(|| online.first()).map(|d| (d.id, d.info.name.clone()))
    })
}

/// Apps (by player ID or name) that play music, never a video worth
/// watching on a phone, unless they say otherwise.
const MUSIC_APPS: &[&str] = &[
    "spotify",
    "applemusic",
    "itunes",
    "zunemusic",
    "foobar2000",
    "musicbee",
    "aimp",
    "winamp",
    "tidal",
    "deezer",
    "amazonmusic",
    "youtube music",
    "soundcloud",
];

/// Whether a player looks like it plays a video: it says so, or it is
/// anything but a music app (browsers say "music" for every video).
pub(crate) fn plays_video(id: &str, app: &str, says_video: bool) -> bool {
    if says_video {
        return true;
    }
    let (id, app) = (id.to_ascii_lowercase(), app.to_ascii_lowercase());
    !MUSIC_APPS.iter().any(|m| id.contains(m) || app.contains(m))
}

/// The player of this PC's to offer for Play on phone: the first one playing
/// a video.
pub fn watchable() -> Option<String> {
    crate::media::local_views()
        .into_iter()
        .map(|v| v.player)
        .find(|p| p.playing && plays_video(&p.id, &p.app, crate::win::media_sessions::says_video(&p.id)))
        .map(|p| p.id)
}

/// When a phone was last asked to watch (its notification is up).
static ASKED: Mutex<Option<Instant>> = Mutex::new(None);
/// How long a phone has to accept before the toggle gives up waiting.
const ASK_FOR: Duration = Duration::from_secs(60);

/// Where Play on phone is: `"off"`, `"asking"` (the phone's notification
/// is up) or `"watching"`.
pub fn state() -> &'static str {
    if crate::pc_mirror::is_watching() {
        *ASKED.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return "watching";
    }
    match *ASKED.lock().unwrap_or_else(|e| e.into_inner()) {
        Some(at) if at.elapsed() < ASK_FOR => "asking",
        _ => "off",
    }
}

fn asked() {
    *ASKED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    core_host::host().hub.changed(crate::state::Changes::MEDIA);
    // The toggle stops waiting when the phone doesn't answer.
    core_host::spawn(async {
        tokio::time::sleep(ASK_FOR).await;
        core_host::host().hub.changed(crate::state::Changes::MEDIA);
    });
}

/// The Play on phone toggle: on plays the video playing now on the phone;
/// off stops the phone watching (or waiting to).
pub fn set_on(on: bool) {
    if on {
        match watchable() {
            Some(player) => play(player),
            None => core_host::host().hub.changed(crate::state::Changes::MEDIA),
        }
        return;
    }
    *ASKED.lock().unwrap_or_else(|e| e.into_inner()) = None;
    crate::pc_mirror::stop_watching();
    core_host::host().hub.changed(crate::state::Changes::MEDIA);
}

/// What this PC's `player` plays: its title, and where it is, in seconds.
fn now_playing(player: &str) -> (String, Option<u32>) {
    let views = crate::media::local_views();
    let Some(view) = views.into_iter().find(|v| v.player.id == player) else {
        return (String::new(), None);
    };
    let position = view.player.position.map(|ms| {
        let since =
            if view.player.playing { view.at.elapsed().map_or(0, |d| d.as_millis() as u64) } else { 0 };
        ((ms + since) / 1000) as u32
    });
    (view.player.title.unwrap_or_default(), position.filter(|&s| s > 0))
}

/// Plays what this PC's `player` plays on a phone (the Play on phone
/// button on the PC's media card).
pub fn play(player: String) {
    let Some((phone, name)) = phone() else {
        return show_message(tr!("PlayOnPhone", "Connect your phone to play on it."));
    };
    let spawned = std::thread::Builder::new().name("play-on-phone".into()).spawn(move || {
        #[cfg(windows)]
        // SAFETY: COM for this short-lived thread (UI Automation).
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let (title, position) = now_playing(&player);
        let window = browser::window_for(&player, &title);
        // A video file in a desktop player: the phone plays the file itself.
        let file = crate::media_file::file_for(&player, &title);
        let url = window
            .as_ref()
            .filter(|w| browser::is_browser(&w.program))
            .and_then(|w| Automation::new().ok()?.page_url(w));
        let plan = nectarlink_core::play_on_phone::plan(url.as_deref(), position);
        let Some(node) = core_host::node() else { return };
        core_host::spawn(async move {
            if let Some(file) = file {
                let shown = (!title.is_empty()).then_some(title);
                let at = u64::from(position.unwrap_or(0)) * 1000;
                match node.offer_watch_file(phone, player, shown, file, at).await {
                    Ok(()) => {
                        asked();
                        show_message(tr!("PlayOnPhone", "Tap the notification on %1 to watch.", name));
                    }
                    Err(e) => show_message(failed(&e, &name)),
                }
                return;
            }
            match plan {
                PlayOnPhone::Open { url, service } => match node.open_link(phone, url).await {
                    Ok(()) => {
                        crate::media::command(crate::media::LOCAL_DEVICE, player, MediaAction::Pause, None);
                        show_message(tr!(
                            "PlayOnPhone",
                            "Opening in %1 on %2. Tap the notification there.",
                            service,
                            name
                        ));
                    }
                    Err(e) => show_message(failed(&e, &name)),
                },
                PlayOnPhone::Stream { open_instead } => {
                    let shown = (!title.is_empty()).then_some(title);
                    match node.offer_watch(phone, player.clone(), shown).await {
                        Ok(()) => {
                            asked();
                            show_message(tr!("PlayOnPhone", "Tap the notification on %1 to watch.", name))
                        }
                        // A phone that can't watch yet: the page opens there instead.
                        Err(Error::Unsupported) if let Some(url) = open_instead => {
                            match node.open_link(phone, url).await {
                                Ok(()) => {
                                    crate::media::command(
                                        crate::media::LOCAL_DEVICE,
                                        player,
                                        MediaAction::Pause,
                                        None,
                                    );
                                    show_message(tr!(
                                        "PlayOnPhone",
                                        "Sent to %1. Tap the notification there.",
                                        name
                                    ));
                                }
                                Err(e) => show_message(failed(&e, &name)),
                            }
                        }
                        Err(e) => show_message(failed(&e, &name)),
                    }
                }
            }
        });
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "can't start Play on phone");
    }
}

fn failed(error: &Error, phone: &str) -> String {
    match error {
        Error::Unsupported => tr!("PlayOnPhone", "Update NectarLink on %1 to play on it.", phone),
        Error::Denied => tr!("PlayOnPhone", "Media is turned off for %1.", phone),
        other => describe(other),
    }
}

/// Where a watched player's picture is, kept up to date while a phone
/// watches it: the video moves when the page scrolls, the window moves or
/// goes full screen.
pub struct Watched {
    place: Arc<Mutex<Option<Place>>>,
    stop: Arc<AtomicBool>,
}

/// A watched player's picture: its window, and where in it the picture is.
#[derive(Debug, Clone, Copy)]
pub struct Place {
    /// The window's handle (as a number: handles go between threads).
    window: isize,
    /// The picture, in physical screen pixels.
    pub on_screen: RECT,
    /// The picture, in the window's own pixels (from its frame's top-left),
    /// as a capture of the window has it.
    pub in_window: RECT,
}

/// How often the picture's place is looked up again.
const FOLLOW_EVERY: Duration = Duration::from_millis(500);
/// How long a browser's player may take to show up in UI Automation
/// before the whole page is streamed instead.
const FIND_PLAYER: Duration = Duration::from_secs(2);

impl Place {
    /// The window the picture is in.
    pub fn window(&self) -> HWND {
        HWND(self.window as *mut core::ffi::c_void)
    }
}

impl Watched {
    /// Starts following `player`'s picture.
    pub fn follow(player: &str) -> Watched {
        let place = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (found, stopped, player) = (place.clone(), stop.clone(), player.to_owned());
        let spawned = std::thread::Builder::new().name("play-on-phone-follow".into()).spawn(move || {
            #[cfg(windows)]
            // SAFETY: COM for this thread, which ends with the watching.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            // Places in the pixels the capture uses.
            browser::use_physical_pixels();
            let automation = Automation::new().ok();
            let mut window: Option<Window> = None;
            let started = std::time::Instant::now();
            while !stopped.load(Ordering::Relaxed) {
                // The window it played in, while it's still there; else find it again.
                if window.as_ref().is_none_or(|w| browser::frame_of(w.hwnd).is_none()) {
                    let (title, _) = now_playing(&player);
                    window = browser::window_for(&player, &title);
                }
                // Minimized, it draws nothing and the phone's picture stops:
                // back behind the other windows, it plays on out of the way.
                if let Some(w) = window.as_ref().filter(|w| browser::is_minimized(w.hwnd)) {
                    tracing::info!(
                        "the watched player's window was minimized; restoring it behind the others"
                    );
                    browser::restore_behind(w.hwnd);
                    std::thread::sleep(FOLLOW_EVERY);
                    continue;
                }
                let place = window.as_ref().and_then(|w| {
                    let frame = browser::frame_of(w.hwnd)?;
                    let on_screen = match &automation {
                        // A browser builds what UI Automation sees of a page
                        // only once asked: give the player a moment to show
                        // up before streaming the whole page.
                        Some(a) if browser::is_browser(&w.program) => match a.player_rect(w) {
                            Some(player) => player,
                            None if started.elapsed() < FIND_PLAYER => return None,
                            None => a.page_rect(w).unwrap_or(frame),
                        },
                        _ => frame,
                    };
                    let in_window = RECT {
                        left: on_screen.left - frame.left,
                        top: on_screen.top - frame.top,
                        right: on_screen.right - frame.left,
                        bottom: on_screen.bottom - frame.top,
                    };
                    Some(Place { window: w.hwnd.0 as isize, on_screen, in_window })
                });
                if place.is_some() {
                    *found.lock().unwrap_or_else(|e| e.into_inner()) = place;
                }
                std::thread::sleep(FOLLOW_EVERY);
            }
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "can't follow the watched player");
        }
        Watched { place, stop }
    }

    /// Where the picture is now, once known.
    pub fn place(&self) -> Option<Place> {
        *self.place.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for Watched {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::plays_video;

    #[test]
    fn music_apps_are_not_offered_unless_they_say_video() {
        assert!(plays_video("chrome", "YouTube", false), "a browser's video says music");
        assert!(plays_video("F0DC299D809B9700", "Zen", false));
        assert!(plays_video(r"C:/Program Files/VideoLAN/VLC/vlc.exe", "vlc", false));
        assert!(!plays_video("Spotify.exe", "Spotify", false));
        assert!(!plays_video("chrome", "YouTube Music", false), "named by its site");
        assert!(!plays_video("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic", "Media Player", false));
        assert!(plays_video("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic", "Media Player", true));
    }
}
