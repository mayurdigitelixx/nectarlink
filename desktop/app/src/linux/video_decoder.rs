// SPDX-License-Identifier: GPL-3.0-or-later
//! FFmpeg video decoding with owned NV12 output.

use std::{
    ffi::{CStr, c_char, c_void},
    io::{Error, ErrorKind, Result},
    ptr::NonNull,
};

use super::h264::{Colors, Picture};

#[repr(C)]
#[derive(Default)]
struct NativePicture {
    y: *const u8,
    uv: *const u8,
    width: i32,
    height: i32,
    stride_y: i32,
    stride_uv: i32,
    bt601: bool,
    full_range: bool,
}

impl NativePicture {
    /// The planes must remain valid for the validated dimensions and strides until this copy completes.
    unsafe fn copy_picture(&self) -> Result<Picture> {
        let width = usize::try_from(self.width).map_err(|_| Error::other("Invalid picture width"))?;
        let height = usize::try_from(self.height).map_err(|_| Error::other("Invalid picture height"))?;
        let length = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_add(pixels / 2))
            .ok_or_else(|| Error::other("Picture size overflow"))?;
        let mut nv12 = Vec::with_capacity(length);
        for (plane, stride, rows) in
            [(self.y, self.stride_y as usize, height), (self.uv, self.stride_uv as usize, height / 2)]
        {
            for row in 0..rows {
                // SAFETY: the caller guarantees readable planes with these dimensions and strides.
                let source = unsafe { std::slice::from_raw_parts(plane.add(row * stride), width) };
                nv12.extend_from_slice(source);
            }
        }
        Ok(Picture {
            width: self.width as u32,
            height: self.height as u32,
            stride: width,
            plane_rows: height,
            nv12,
            colors: Colors { bt601: self.bt601, full_range: self.full_range },
        })
    }
}

unsafe extern "C" {
    fn nectarlink_video_decoder_create(hevc: bool, error: *mut c_char, capacity: usize) -> *mut c_void;
    fn nectarlink_video_decoder_destroy(decoder: *mut c_void);
    fn nectarlink_video_decoder_next_device(
        decoder: *mut c_void,
        error: *mut c_char,
        capacity: usize,
    ) -> bool;
    fn nectarlink_video_decoder_backend(decoder: *const c_void) -> *const c_char;
    fn nectarlink_video_decoder_send(
        decoder: *mut c_void,
        bytes: *const u8,
        length: usize,
        error: *mut c_char,
        capacity: usize,
    ) -> i32;
    fn nectarlink_video_decoder_receive(
        decoder: *mut c_void,
        frame: *mut NativePicture,
        error: *mut c_char,
        capacity: usize,
    ) -> i32;
    fn nectarlink_video_decoder_flush(decoder: *mut c_void);
}

pub(crate) struct Decoder(NonNull<c_void>);

impl Decoder {
    pub(crate) fn on_gpu(&self) -> bool {
        // SAFETY: the decoder owns a NUL-terminated backend name until it is dropped.
        let backend = unsafe { CStr::from_ptr(nectarlink_video_decoder_backend(self.0.as_ptr())) };
        backend != c"software" && backend != c"unavailable"
    }

    pub(crate) fn new(codec: &str) -> Result<Self> {
        let hevc = match codec {
            "h264" => false,
            "h265" | "hevc" => true,
            _ => return Err(Error::new(ErrorKind::Unsupported, "Hardware codec is unsupported")),
        };
        let mut error = [0 as c_char; 512];
        // SAFETY: the factory writes only within the supplied error buffer.
        let pointer = unsafe { nectarlink_video_decoder_create(hevc, error.as_mut_ptr(), error.len()) };
        let pointer = NonNull::new(pointer).ok_or_else(|| native_error(&error))?;
        // SAFETY: the live decoder owns a NUL-terminated hardware device name.
        let backend = unsafe { CStr::from_ptr(nectarlink_video_decoder_backend(pointer.as_ptr())) };
        tracing::info!(codec, backend = %backend.to_string_lossy(), "Linux video decoder initialized");
        Ok(Self(pointer))
    }

    pub(crate) fn decode(&mut self, bytes: &[u8]) -> Result<Vec<Picture>> {
        let mut pictures = Vec::new();
        self.receive(&mut pictures)?;
        let mut error = [0 as c_char; 512];
        // SAFETY: the compressed data is copied by the native decoder before this call returns.
        let result = unsafe {
            nectarlink_video_decoder_send(
                self.0.as_ptr(),
                bytes.as_ptr(),
                bytes.len(),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if result < 0 {
            return Err(native_error(&error));
        }
        self.receive(&mut pictures)?;
        Ok(pictures)
    }

    pub(crate) fn next_device(&mut self) -> bool {
        let mut error = [0 as c_char; 512];
        // SAFETY: this worker owns the decoder; no native picture pointers escape receive().
        let switched =
            unsafe { nectarlink_video_decoder_next_device(self.0.as_ptr(), error.as_mut_ptr(), error.len()) };
        if switched {
            // SAFETY: a successful switch leaves a live decoder with a NUL-terminated backend name.
            let backend = unsafe { CStr::from_ptr(nectarlink_video_decoder_backend(self.0.as_ptr())) };
            tracing::info!(backend = %backend.to_string_lossy(), "Video decoder switched");
        } else {
            tracing::debug!(error = %native_error(&error), "Video decoding options exhausted");
        }
        switched
    }

    fn receive(&mut self, pictures: &mut Vec<Picture>) -> Result<()> {
        loop {
            let mut frame = NativePicture::default();
            let mut error = [0 as c_char; 512];
            // SAFETY: the output structures live through the call; native planes remain borrowed until receive is called again.
            let result = unsafe {
                nectarlink_video_decoder_receive(self.0.as_ptr(), &mut frame, error.as_mut_ptr(), error.len())
            };
            match result {
                0 => return Ok(()),
                1 => {
                    // SAFETY: a successful receive validates the layout; its planes remain live until the next call.
                    pictures.push(unsafe { frame.copy_picture() }?);
                }
                _ => return Err(native_error(&error)),
            }
        }
    }

    pub(crate) fn flush(&mut self) {
        // SAFETY: this worker owns the decoder and no borrowed planes escape receive().
        unsafe { nectarlink_video_decoder_flush(self.0.as_ptr()) };
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: this is the sole owner of the native decoder.
        unsafe { nectarlink_video_decoder_destroy(self.0.as_ptr()) };
    }
}

fn native_error(buffer: &[c_char]) -> Error {
    // SAFETY: buffers are zero-initialized and native errors are NUL-terminated.
    Error::other(unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_visible_nv12_rows_without_the_padding() {
        let y = [1, 2, 99, 99, 3, 4, 99, 99];
        let uv = [5, 6, 99, 99];
        let frame = NativePicture {
            y: y.as_ptr(),
            uv: uv.as_ptr(),
            width: 2,
            height: 2,
            stride_y: 4,
            stride_uv: 4,
            bt601: false,
            full_range: true,
        };
        // SAFETY: both arrays stay alive and provide the rows described by the frame.
        let picture = unsafe { frame.copy_picture() }.unwrap();
        assert_eq!(picture.nv12, [1, 2, 3, 4, 5, 6]);
        assert_eq!((picture.width, picture.height, picture.stride, picture.plane_rows), (2, 2, 2, 2));
        assert_eq!(picture.colors, Colors { bt601: false, full_range: true });
    }
}
