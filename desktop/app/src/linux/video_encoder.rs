// SPDX-License-Identifier: GPL-3.0-or-later
//! Hardware video encoding through the optional FFmpeg runtime libraries.

use std::{
    ffi::{CStr, c_char, c_void},
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

unsafe extern "C" {
    fn nectarlink_video_encoder_create(
        hevc: bool,
        width: i32,
        height: i32,
        fps: i32,
        bitrate: i32,
        error: *mut c_char,
        capacity: usize,
    ) -> *mut c_void;
    fn nectarlink_video_encoder_destroy(encoder: *mut c_void);
    fn nectarlink_video_encoder_next_device(
        encoder: *mut c_void,
        error: *mut c_char,
        capacity: usize,
    ) -> bool;
    fn nectarlink_video_encoder_backend(encoder: *const c_void) -> *const c_char;
    fn nectarlink_video_encoder_send(
        encoder: *mut c_void,
        bytes: *const u8,
        length: usize,
        time_us: i64,
        error: *mut c_char,
        capacity: usize,
    ) -> i32;
    fn nectarlink_video_encoder_receive(
        encoder: *mut c_void,
        bytes: *mut *const u8,
        length: *mut usize,
        keyframe: *mut bool,
        error: *mut c_char,
        capacity: usize,
    ) -> i32;
    fn nectarlink_video_encoder_request_keyframe(encoder: *mut c_void);
    fn nectarlink_video_encoder_set_bitrate(encoder: *mut c_void, bitrate: i32) -> bool;
}

pub(crate) struct Encoder {
    native: NonNull<c_void>,
    last_output: Instant,
}

impl Encoder {
    pub(crate) fn new(codec: &str, width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self, String> {
        let hevc = match codec {
            "h264" => false,
            "h265" => true,
            _ => return Err(format!("Unsupported hardware codec: {codec}")),
        };
        let number = |value: u32| i32::try_from(value).map_err(|_| "Encoder setting is too large".to_owned());
        let mut error = [0 as c_char; 512];
        // SAFETY: the factory copies these settings and writes only within the error buffer.
        let native = unsafe {
            nectarlink_video_encoder_create(
                hevc,
                number(width)?,
                number(height)?,
                number(fps)?,
                number(bitrate)?,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        let native = NonNull::new(native).ok_or_else(|| error_message(&error))?;
        // SAFETY: the live encoder owns a NUL-terminated backend name.
        let backend = unsafe { CStr::from_ptr(nectarlink_video_encoder_backend(native.as_ptr())) };
        tracing::info!(codec, backend = %backend.to_string_lossy(), "Linux hardware encoder initialized");
        Ok(Self { native, last_output: Instant::now() })
    }

    pub(crate) fn request_keyframe(&self) -> bool {
        // SAFETY: encoder controls and frame submission run on the capture thread.
        unsafe { nectarlink_video_encoder_request_keyframe(self.native.as_ptr()) };
        true
    }

    pub(crate) fn next_device(&mut self) -> bool {
        let mut error = [0 as c_char; 512];
        // SAFETY: this capture thread owns the encoder and its packet storage.
        let switched = unsafe {
            nectarlink_video_encoder_next_device(self.native.as_ptr(), error.as_mut_ptr(), error.len())
        };
        if switched {
            self.last_output = Instant::now();
        } else {
            tracing::debug!(error = %error_message(&error), "Hardware encoding devices exhausted");
        }
        switched
    }

    pub(crate) fn set_bitrate(&self, bitrate: u32) -> bool {
        let Ok(bitrate) = i32::try_from(bitrate) else { return false };
        // SAFETY: the pointer stays valid until this encoder is dropped.
        unsafe { nectarlink_video_encoder_set_bitrate(self.native.as_ptr(), bitrate) }
    }

    pub(crate) fn encode(
        &mut self,
        nv12: &[u8],
        hns: i64,
        stop: &AtomicBool,
    ) -> Result<Vec<(bool, Vec<u8>)>, String> {
        if stop.load(Ordering::Relaxed) {
            return Ok(Vec::new());
        }
        let mut packets = Vec::new();
        self.receive(&mut packets)?;
        let mut error = [0 as c_char; 512];
        // SAFETY: input is borrowed only for this call; the native encoder copies it into an owned frame.
        let result = unsafe {
            nectarlink_video_encoder_send(
                self.native.as_ptr(),
                nv12.as_ptr(),
                nv12.len(),
                hns / 10,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if result < 0 {
            return Err(error_message(&error));
        }
        self.receive(&mut packets)?;
        if self.last_output.elapsed() >= Duration::from_secs(2) {
            return Err("Hardware video encoder stopped producing output".into());
        }
        Ok(packets)
    }

    fn receive(&mut self, packets: &mut Vec<(bool, Vec<u8>)>) -> Result<(), String> {
        loop {
            let mut bytes = std::ptr::null();
            let mut length = 0;
            let mut keyframe = false;
            let mut error = [0 as c_char; 512];
            // SAFETY: output pointers refer to locals and the native encoder remains live.
            let result = unsafe {
                nectarlink_video_encoder_receive(
                    self.native.as_ptr(),
                    &mut bytes,
                    &mut length,
                    &mut keyframe,
                    error.as_mut_ptr(),
                    error.len(),
                )
            };
            match result {
                0 => return Ok(()),
                1 if !bytes.is_null() && length > 0 => {
                    // SAFETY: packet storage remains valid until the next receive call; copy it now.
                    let owned = unsafe { std::slice::from_raw_parts(bytes, length) }.to_vec();
                    packets.push((keyframe, owned));
                    self.last_output = Instant::now();
                }
                _ => return Err(error_message(&error)),
            }
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: this is the sole owner; destruction releases codec, frames, packets, and device.
        unsafe { nectarlink_video_encoder_destroy(self.native.as_ptr()) };
    }
}

fn error_message(buffer: &[c_char]) -> String {
    // SAFETY: buffers are zero-initialized and the native API always NUL-terminates errors.
    unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy().into_owned()
}
