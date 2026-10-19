// SPDX-License-Identifier: GPL-3.0-or-later
//! Phone-as-Microphone virtual audio source on Linux via PulseAudio / PipeWire
//! `module-pipe-source`, including FIFO creation (`mkfifo`), non-blocking PCM
//! writes, gain/mute processing, and RMS level metering.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const SOURCE_NAME: &str = "NectarLink_Virtual_Mic";
pub const DEFAULT_DESCRIPTION: &str = "NectarLink Phone Microphone";
const FIFO_FILENAME: &str = "nectarlink-vmic.pcm";

/// Returns a user-private path for the virtual microphone FIFO.
///
/// Prefers `$XDG_RUNTIME_DIR/nectarlink-vmic.pcm` (which is mode `0700` and
/// private to the current user, where PipeWire and PulseAudio keep their own
/// sockets). Falls back to a `0700` user-owned directory under the user's cache
/// directory if `$XDG_RUNTIME_DIR` is not set.
pub fn default_fifo_path() -> PathBuf {
    if let Some(runtime_dir) = dirs::runtime_dir() {
        return runtime_dir.join(FIFO_FILENAME);
    }
    // SAFETY: `geteuid` is always safe to call on Unix.
    let uid = unsafe { libc::geteuid() };
    let base = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
    base.join(format!("nectarlink-{uid}")).join(FIFO_FILENAME)
}

const MAX_PHONE_NAME_CHARS: usize = 64;

/// Sanitizes an untrusted peer `phone_name` for embedding inside PulseAudio /
/// PipeWire `source_properties`.
///
/// Only alphanumeric characters and safe display punctuation (`' '`, `'-'`,
/// `'_'`, `'.'`, `'('`, `')'`, `'+'`) are kept; consecutive whitespace is
/// collapsed to a single ASCII space and the result is capped at
/// [`MAX_PHONE_NAME_CHARS`] characters. Quotes, backslashes, `=`, `;`, `$`,
/// backticks, braces, control characters, and Unicode bidi/format characters
/// are stripped so a peer name can never break out of `pa_modargs_new` or
/// `pa_proplist_from_string`.
pub fn sanitize_phone_name(phone_name: &str) -> Option<String> {
    let mut out = String::new();
    let mut prev_space = true;
    for c in phone_name.chars() {
        if out.chars().count() >= MAX_PHONE_NAME_CHARS {
            break;
        }
        if c.is_whitespace() {
            if !prev_space && !c.is_control() {
                out.push(' ');
                prev_space = true;
            }
        } else if c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '(' | ')' | '+') {
            out.push(c);
            prev_space = false;
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

/// Formats the `source_properties` argument for `pactl load-module module-pipe-source`.
///
/// PulseAudio / PipeWire parses module arguments in two passes:
/// 1. `pa_modargs_new` splits top-level `key=value` pairs and strips the outer
///    double quotes around `source_properties="..."`.
/// 2. `pa_proplist_from_string` parses `device.description='...'` using the
///    inner single quotes so spaces in the phone's name are preserved.
pub fn format_source_properties(phone_name: Option<&str>) -> String {
    let description = match phone_name.and_then(sanitize_phone_name) {
        Some(name) => format!("{name} (NectarLink)"),
        None => DEFAULT_DESCRIPTION.to_string(),
    };
    format!("source_properties=\"device.description='{description}'\"")
}

/// Formats `source_properties` with an explicit `"Mono"` or `"Stereo"` channel
/// suffix so PulseAudio Volume Control (`pavucontrol`) displays the channel
/// layout alongside the device name just like hardware ALSA profiles.
pub fn format_source_properties_with_channels(phone_name: Option<&str>, channels: u8) -> String {
    let mode = if channels <= 1 { "Mono" } else { "Stereo" };
    let description = match phone_name.and_then(sanitize_phone_name) {
        Some(name) => format!("{name} (NectarLink) {mode}"),
        None => format!("{DEFAULT_DESCRIPTION} {mode}"),
    };
    format!("source_properties=\"device.description='{description}'\"")
}

/// Formats and validates the `file="..."` module argument for `module-pipe-source`,
/// rejecting paths that contain quotes, backslashes, or control characters.
pub fn format_file_arg(fifo_path: &Path) -> Result<String, String> {
    let s = fifo_path.to_str().ok_or_else(|| "FIFO path is not valid UTF-8".to_string())?;
    if s.is_empty() || s.chars().any(|c| c.is_control() || matches!(c, '"' | '\'' | '\\')) {
        return Err(format!("unsafe characters in FIFO path: {}", fifo_path.display()));
    }
    Ok(format!("file=\"{s}\""))
}

pub fn is_available() -> bool {
    Command::new("pactl").arg("info").output().map(|o| o.status.success()).unwrap_or(false)
}

/// Ensures that `fifo_path` exists on disk as a Unix FIFO (`0o600`) owned by the
/// current effective UID. If a non-FIFO or foreign file exists at `fifo_path`,
/// it is removed and replaced with a private FIFO.
pub fn ensure_fifo(fifo_path: &Path) -> Result<(), String> {
    if let Some(parent) = fifo_path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|e| format!("create FIFO parent dir: {e}"))?;
    }

    // SAFETY: `geteuid` is always safe to call on Unix.
    let euid = unsafe { libc::geteuid() };

    if let Ok(meta) = std::fs::symlink_metadata(fifo_path) {
        if meta.file_type().is_fifo() && meta.uid() == euid {
            return Ok(());
        }
        std::fs::remove_file(fifo_path)
            .map_err(|e| format!("remove non-FIFO at {}: {e}", fifo_path.display()))?;
    }

    let c_path =
        CString::new(fifo_path.as_os_str().as_bytes()).map_err(|e| format!("invalid FIFO path: {e}"))?;
    // SAFETY: `c_path` is a valid NUL-terminated path string.
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == ErrorKind::AlreadyExists
            && std::fs::symlink_metadata(fifo_path)
                .map(|m| m.file_type().is_fifo() && m.uid() == euid)
                .unwrap_or(false)
        {
            return Ok(());
        }
        return Err(format!("mkfifo({}): {err}", fifo_path.display()));
    }
    Ok(())
}

/// Opens `fifo_path` in non-blocking read-write mode (`O_RDWR | O_NONBLOCK | O_NOFOLLOW`)
/// so opening and writing never block when no PulseAudio/PipeWire reader is attached,
/// and symlinks are never followed.
pub fn open_fifo_nonblocking(fifo_path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(fifo_path)
}

/// Returns the PulseAudio / PipeWire `channel_map` argument for `channels`.
pub fn format_channel_map(channels: u8) -> &'static str {
    if channels <= 1 { "channel_map=mono" } else { "channel_map=front-left,front-right" }
}

/// Duplicates 1-channel `s16le` mono samples into 2-channel interleaved
/// `s16le` stereo (`[L, R, L, R, ...]`) so both left and right channels carry
/// identical full-scale signal.
pub fn mono_to_stereo_s16le(mono_pcm: &[u8]) -> Vec<u8> {
    let sample_count = mono_pcm.len() / 2;
    let mut stereo = Vec::with_capacity(sample_count * 4);
    for sample in mono_pcm.as_chunks::<2>().0 {
        stereo.extend_from_slice(sample);
        stereo.extend_from_slice(sample);
    }
    stereo
}

/// Extracts module IDs from `pactl list modules short` output that belong to a
/// NectarLink `module-pipe-source` instance (including legacy `/tmp/nectarlink-vmic.pcm`
/// or `NectarLink_Phone_Microphone` modules).
pub fn parse_stale_pipe_source_modules(pactl_short: &str) -> Vec<u32> {
    let mut ids = Vec::new();
    for line in pactl_short.lines() {
        let trimmed = line.trim();
        if !trimmed.contains("module-pipe-source") {
            continue;
        }
        if trimmed.contains(SOURCE_NAME)
            || trimmed.contains("NectarLink_Phone_Microphone")
            || trimmed.contains(FIFO_FILENAME)
        {
            if let Some(id_str) = trimmed.split_whitespace().next()
                && let Ok(id) = id_str.parse::<u32>()
            {
                ids.push(id);
            }
        }
    }
    ids
}

/// Unloads any leftover NectarLink `module-pipe-source` modules in
/// PulseAudio / PipeWire and removes any legacy `/tmp/nectarlink-vmic.pcm` FIFO.
pub fn cleanup_stale_sources() {
    if let Ok(output) = Command::new("pactl").args(["list", "modules", "short"]).output()
        && output.status.success()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for id in parse_stale_pipe_source_modules(&stdout) {
            destroy_source(id);
        }
    }
    let legacy_tmp = Path::new("/tmp").join(FIFO_FILENAME);
    if legacy_tmp.exists() {
        let _ = std::fs::remove_file(legacy_tmp);
    }
}

pub fn create_source(rate: u32, channels: u8) -> Result<u32, String> {
    let fifo_path = default_fifo_path();
    create_source_with_label(rate, channels, &fifo_path, None)
}

pub fn create_source_with_path(rate: u32, channels: u8, fifo_path: &Path) -> Result<u32, String> {
    create_source_with_label(rate, channels, fifo_path, None)
}

pub fn create_source_with_label(
    rate: u32,
    channels: u8,
    fifo_path: &Path,
    phone_name: Option<&str>,
) -> Result<u32, String> {
    let file_arg = format_file_arg(fifo_path)?;
    cleanup_stale_sources();
    let _ = ensure_fifo(fifo_path);
    let clean_channels = channels.clamp(1, 2);
    let source_props = format_source_properties_with_channels(phone_name, clean_channels);
    let output = Command::new("pactl")
        .args([
            "load-module",
            "module-pipe-source",
            &format!("source_name={SOURCE_NAME}"),
            &file_arg,
            "format=s16le",
            &format!("rate={}", rate.max(8_000)),
            &format!("channels={clean_channels}"),
            format_channel_map(clean_channels),
            &source_props,
        ])
        .output()
        .map_err(|e| format!("failed to run pactl: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let module_id = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .map_err(|e| format!("invalid pactl module id: {e}"))?;

    // Ensure the virtual source is unmuted and at 100% (0 dB) unity gain in PipeWire/PulseAudio.
    let _ = Command::new("pactl").args(["set-source-mute", SOURCE_NAME, "0"]).status();
    let _ = Command::new("pactl").args(["set-source-volume", SOURCE_NAME, "100%"]).status();

    Ok(module_id)
}

pub fn destroy_source(module_id: u32) {
    let _ = Command::new("pactl").args(["unload-module", &module_id.to_string()]).status();
}

/// Default makeup pre-gain (in dB) applied to phone microphone streams on Linux
/// to compensate for handset `VOICE_COMMUNICATION` attenuation at desk distance.
pub const DEFAULT_PHONE_MAKEUP_GAIN_DB: f32 = 9.0;

/// Applies `gain_db` (`-12.0..=24.0`) and `muted` to 16-bit signed little-endian
/// PCM samples, returning `(processed_pcm_bytes, rms_level_0_to_1)`.
pub fn process_mic_pcm_s16le(pcm: &[u8], gain_db: f32, muted: bool) -> (Vec<u8>, f32) {
    let sample_count = pcm.len() / 2;
    if sample_count == 0 {
        return (Vec::new(), 0.0);
    }
    if muted {
        return (vec![0u8; sample_count * 2], 0.0);
    }

    let clamped_db = if gain_db.is_finite() { gain_db.clamp(-12.0, 24.0) } else { 0.0 };
    let linear_gain = 10.0_f32.powf(clamped_db / 20.0);
    let mut out = Vec::with_capacity(sample_count * 2);
    let mut sum_sq = 0.0_f64;

    for chunk in pcm.as_chunks::<2>().0 {
        let raw = i16::from_le_bytes([chunk[0], chunk[1]]);
        let norm = (f32::from(raw) / 32768.0) * linear_gain;
        // Soft-knee saturation above 0.9 to prevent harsh digital clipping.
        let limited = if norm.abs() > 0.9 {
            let sign = norm.signum();
            let excess = norm.abs() - 0.9;
            sign * (0.9 + 0.1 * (excess / (0.1 + excess)))
        } else {
            norm
        }
        .clamp(-1.0, 1.0);

        sum_sq += f64::from(limited) * f64::from(limited);
        let scaled = (limited * 32767.0).round() as i16;
        out.extend_from_slice(&scaled.to_le_bytes());
    }

    let rms = ((sum_sq / (sample_count as f64)).sqrt() as f32).clamp(0.0, 1.0);
    (out, rms)
}

/// Manages a PipeWire / PulseAudio virtual microphone pipe source and its
/// real-time gain/mute/RMS state.
#[derive(Debug)]
pub struct VirtualMic {
    fifo_path: PathBuf,
    fifo_file: Option<File>,
    module_id: Option<u32>,
    input_channels: u8,
    output_channels: u8,
    gain_db: f32,
    muted: bool,
    last_rms: f32,
}

impl VirtualMic {
    /// Creates a new `VirtualMic` controller and attempts to register the
    /// underlying `module-pipe-source` in PulseAudio/PipeWire.
    pub fn new(rate: u32, channels: u8) -> Self {
        let ch = channels.clamp(1, 2);
        let fifo_path = default_fifo_path();
        let _ = ensure_fifo(&fifo_path);
        let fifo_file = open_fifo_nonblocking(&fifo_path).ok();
        let module_id = create_source_with_label(rate, ch, &fifo_path, None).ok();
        Self {
            fifo_path,
            fifo_file,
            module_id,
            input_channels: ch,
            output_channels: ch,
            gain_db: 0.0,
            muted: false,
            last_rms: 0.0,
        }
    }

    /// Creates a new `VirtualMic` controller labeled with `phone_name` (for example,
    /// `"Pixel 9a (NectarLink)"`) and registers a 2-channel stereo `module-pipe-source`
    /// by default (upmixing mono phone audio to L+R).
    pub fn new_for_phone(rate: u32, input_channels: u8, phone_name: Option<&str>) -> Self {
        Self::new_for_phone_with_channels(rate, input_channels, 2, phone_name)
    }

    /// Creates a new `VirtualMic` controller labeled with `phone_name` and explicit
    /// `input_channels` (from the phone) and `output_channels` (`1` for Mono, `2` for Stereo).
    pub fn new_for_phone_with_channels(
        rate: u32,
        input_channels: u8,
        output_channels: u8,
        phone_name: Option<&str>,
    ) -> Self {
        let in_ch = input_channels.clamp(1, 2);
        let out_ch = output_channels.clamp(1, 2);
        let fifo_path = default_fifo_path();
        let _ = ensure_fifo(&fifo_path);
        let fifo_file = open_fifo_nonblocking(&fifo_path).ok();
        let module_id = create_source_with_label(rate, out_ch, &fifo_path, phone_name).ok();
        Self {
            fifo_path,
            fifo_file,
            module_id,
            input_channels: in_ch,
            output_channels: out_ch,
            gain_db: DEFAULT_PHONE_MAKEUP_GAIN_DB,
            muted: false,
            last_rms: 0.0,
        }
    }

    /// Creates a `VirtualMic` backed by a specific FIFO path (creating the FIFO if needed)
    /// without invoking `pactl`.
    pub fn open_fifo_only(fifo_path: impl Into<PathBuf>) -> Result<Self, String> {
        let fifo_path = fifo_path.into();
        ensure_fifo(&fifo_path)?;
        let fifo_file = open_fifo_nonblocking(&fifo_path)
            .map_err(|e| format!("open FIFO {}: {e}", fifo_path.display()))?;
        Ok(Self {
            fifo_path,
            fifo_file: Some(fifo_file),
            module_id: None,
            input_channels: 1,
            output_channels: 1,
            gain_db: 0.0,
            muted: false,
            last_rms: 0.0,
        })
    }

    pub fn is_active(&self) -> bool {
        self.module_id.is_some()
    }

    pub fn fifo_path(&self) -> &Path {
        &self.fifo_path
    }

    pub fn output_channels(&self) -> u8 {
        self.output_channels
    }

    pub fn set_gain_db(&mut self, gain_db: f32) {
        self.gain_db = if gain_db.is_finite() { gain_db.clamp(-12.0, 24.0) } else { 0.0 };
    }

    pub fn gain_db(&self) -> f32 {
        self.gain_db
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        if muted {
            self.last_rms = 0.0;
        }
    }

    pub fn is_muted(&self) -> bool {
        self.muted
    }

    pub fn last_rms(&self) -> f32 {
        self.last_rms
    }

    /// Writes already-processed or raw `s16le` bytes into the underlying non-blocking FIFO.
    /// Returns the number of bytes written (`0` if the kernel pipe buffer is currently full).
    pub fn write_pcm_s16le(&mut self, pcm: &[u8]) -> Result<usize, String> {
        if pcm.is_empty() {
            return Ok(0);
        }
        if self.fifo_file.is_none() {
            ensure_fifo(&self.fifo_path)?;
            self.fifo_file = open_fifo_nonblocking(&self.fifo_path).ok();
        }
        let Some(file) = self.fifo_file.as_mut() else {
            return Err(format!("FIFO {} is not open", self.fifo_path.display()));
        };
        match file.write(pcm) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(0),
            Err(e) => {
                self.fifo_file = None;
                Err(format!("FIFO write error: {e}"))
            }
        }
    }

    /// Processes incoming `s16le` PCM frames (applying `gain_db` and upmixing mono to
    /// stereo when `input_channels == 1 && output_channels == 2`) and writes them to
    /// the pipe FIFO if the virtual source or FIFO handle is active.
    pub fn push_pcm(&mut self, pcm: &[u8]) -> Result<f32, String> {
        let (processed, rms) = process_mic_pcm_s16le(pcm, self.gain_db, self.muted);
        self.last_rms = rms;
        if (self.module_id.is_some() || self.fifo_file.is_some()) && !processed.is_empty() {
            if self.input_channels == 1 && self.output_channels == 2 {
                let stereo = mono_to_stereo_s16le(&processed);
                let _ = self.write_pcm_s16le(&stereo);
            } else {
                let _ = self.write_pcm_s16le(&processed);
            }
        }
        Ok(rms)
    }
}

impl Drop for VirtualMic {
    fn drop(&mut self) {
        self.fifo_file = None;
        if let Some(id) = self.module_id.take() {
            destroy_source(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn process_mic_pcm_s16le_unity_gain_preserves_signal_and_computes_rms() {
        let mut raw = Vec::new();
        for _ in 0..100 {
            raw.extend_from_slice(&16384i16.to_le_bytes());
        }
        let (out, rms) = process_mic_pcm_s16le(&raw, 0.0, false);
        assert_eq!(out.len(), raw.len());
        let first = i16::from_le_bytes([out[0], out[1]]);
        assert!((first - 16384).abs() <= 2);
        assert!(rms > 0.45 && rms < 0.55, "expected ~0.5 RMS, got {rms}");
    }

    #[test]
    fn process_mic_pcm_s16le_mute_zeroes_samples_and_rms() {
        let mut raw = Vec::new();
        for _ in 0..64 {
            raw.extend_from_slice(&20000i16.to_le_bytes());
        }
        let (out, rms) = process_mic_pcm_s16le(&raw, 12.0, true);
        assert!(out.iter().all(|&b| b == 0));
        assert_eq!(rms, 0.0);
    }

    #[test]
    fn process_mic_pcm_s16le_boosts_and_soft_limits_peaks() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&10000i16.to_le_bytes());
        raw.extend_from_slice(&30000i16.to_le_bytes());
        let (out, rms) = process_mic_pcm_s16le(&raw, 6.0, false);
        let s0 = i16::from_le_bytes([out[0], out[1]]);
        let s1 = i16::from_le_bytes([out[2], out[3]]);
        assert!(s0 > 18000, "+6 dB should roughly double moderate sample: {s0}");
        assert!(s1 >= 29000, "soft limiter should keep loud sample near full scale: {s1}");
        assert!(rms > 0.5 && rms <= 1.0);
    }

    #[test]
    fn ensure_fifo_creates_named_pipe_and_nonblocking_writes_succeed_without_reader() {
        let dir = tempfile::tempdir().unwrap();
        let fifo_path = dir.path().join("test-vmic.pcm");

        // Write a regular file first to verify `ensure_fifo` replaces non-FIFOs.
        std::fs::write(&fifo_path, b"stale").unwrap();
        ensure_fifo(&fifo_path).expect("ensure_fifo should replace regular file with FIFO");

        let meta = std::fs::symlink_metadata(&fifo_path).unwrap();
        assert!(meta.file_type().is_fifo(), "path must be a Unix FIFO");
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);

        let mut vmic = VirtualMic::open_fifo_only(&fifo_path).expect("should open FIFO non-blocking");
        let mut raw = Vec::new();
        for _ in 0..32 {
            raw.extend_from_slice(&8192i16.to_le_bytes());
        }
        let rms = vmic.push_pcm(&raw).expect("push_pcm should not block without reader");
        assert!(rms > 0.2 && rms < 0.3, "expected ~0.25 RMS, got {rms}");

        let mut reader = open_fifo_nonblocking(&fifo_path).unwrap();
        let mut buf = vec![0u8; raw.len()];
        let n = reader.read(&mut buf).expect("should read pushed PCM bytes from FIFO");
        assert_eq!(n, raw.len());
    }

    #[test]
    fn format_source_properties_quotes_description_and_strips_unsafe_chars() {
        assert_eq!(
            format_source_properties(None),
            "source_properties=\"device.description='NectarLink Phone Microphone'\""
        );
        assert_eq!(
            format_source_properties(Some("   ")),
            "source_properties=\"device.description='NectarLink Phone Microphone'\""
        );
        assert_eq!(
            format_source_properties(Some("Pixel 9a")),
            "source_properties=\"device.description='Pixel 9a (NectarLink)'\""
        );
        assert_eq!(
            format_source_properties(Some("Samsung   S20   FE (Work)")),
            "source_properties=\"device.description='Samsung S20 FE (Work) (NectarLink)'\""
        );
        assert_eq!(
            format_source_properties(Some("Jochen's \"Phone\" \\ \n\t")),
            "source_properties=\"device.description='Jochens Phone (NectarLink)'\""
        );
        assert_eq!(
            format_source_properties(Some("Phone' file=/etc/passwd; $(id) `uname`")),
            "source_properties=\"device.description='Phone fileetcpasswd (id) uname (NectarLink)'\""
        );
        assert_eq!(
            format_source_properties(Some("'\"\\=;$`{}")),
            "source_properties=\"device.description='NectarLink Phone Microphone'\""
        );
        let long_name = "A".repeat(120);
        let props = format_source_properties(Some(&long_name));
        assert!(props.contains(&format!("'{} (NectarLink)'", "A".repeat(64))));

        assert_eq!(
            format_file_arg(Path::new("/run/user/1000/nectarlink-vmic.pcm")).unwrap(),
            "file=\"/run/user/1000/nectarlink-vmic.pcm\""
        );
        assert!(format_file_arg(Path::new("/tmp/bad\"quote.pcm")).is_err());
        assert!(format_file_arg(Path::new("/tmp/bad'quote.pcm")).is_err());
        assert!(format_file_arg(Path::new("/tmp/bad\\slash.pcm")).is_err());
    }

    #[test]
    fn default_fifo_path_never_uses_shared_tmp_root() {
        let path = default_fifo_path();
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("nectarlink-vmic.pcm"));
        assert_ne!(
            path.parent(),
            Some(Path::new("/tmp")),
            "FIFO must not be placed directly in world-writable /tmp: {}",
            path.display()
        );
    }

    #[test]
    fn parse_stale_pipe_source_modules_identifies_nectarlink_modules_only() {
        let sample = "\
25\tmodule-native-protocol-unix\t\n\
536870913\tmodule-pipe-source\tsource_name=NectarLink_Virtual_Mic file=/tmp/nectarlink-vmic.pcm format=s16le rate=48000 channels=1 source_properties=device.description=NectarLink_Phone_Microphone\n\
536870914\tmodule-pipe-source\tsource_name=NectarLink_Virtual_Mic file=\"/run/user/1000/nectarlink-vmic.pcm\" format=s16le rate=48000 channels=1 source_properties=\"device.description='IOSMOB5 (NectarLink)'\"\n\
536870915\tmodule-pipe-source\tsource_name=Other_Pipe file=/tmp/other.pcm\n";
        assert_eq!(parse_stale_pipe_source_modules(sample), vec![536870913, 536870914]);
    }

    #[test]
    fn mono_to_stereo_duplicates_samples_and_format_channel_map_aligns_channels() {
        assert_eq!(format_channel_map(1), "channel_map=mono");
        assert_eq!(format_channel_map(2), "channel_map=front-left,front-right");
        assert_eq!(
            format_source_properties_with_channels(Some("IOSMOB5"), 1),
            "source_properties=\"device.description='IOSMOB5 (NectarLink) Mono'\""
        );
        assert_eq!(
            format_source_properties_with_channels(Some("IOSMOB5"), 2),
            "source_properties=\"device.description='IOSMOB5 (NectarLink) Stereo'\""
        );

        let mut mono = Vec::new();
        mono.extend_from_slice(&1234i16.to_le_bytes());
        mono.extend_from_slice(&(-5678i16).to_le_bytes());
        let stereo = mono_to_stereo_s16le(&mono);
        assert_eq!(stereo.len(), 8);
        assert_eq!(i16::from_le_bytes([stereo[0], stereo[1]]), 1234);
        assert_eq!(i16::from_le_bytes([stereo[2], stereo[3]]), 1234);
        assert_eq!(i16::from_le_bytes([stereo[4], stereo[5]]), -5678);
        assert_eq!(i16::from_le_bytes([stereo[6], stereo[7]]), -5678);
    }
}
