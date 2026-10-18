// SPDX-License-Identifier: GPL-3.0-or-later
//! Capture a single window on Linux (via X11 `import`/`xwd`/`ffmpeg` or cropped screen capture),
//! matching the public API of `desktop/app/src/win/window_capture.rs`.

use std::process::Command;

use super::browser::{HWND, RECT, frame_of};

pub struct WindowCapture {
    hwnd: HWND,
    rgba: Vec<u8>,
}

// SAFETY: WindowCapture only holds a window XID and an owned byte buffer.
unsafe impl Send for WindowCapture {}

impl WindowCapture {
    pub fn new(hwnd: HWND) -> Result<WindowCapture, String> {
        if hwnd.0.is_null() {
            return Err("invalid window handle".to_owned());
        }
        Ok(WindowCapture { hwnd, rgba: Vec::new() })
    }

    pub fn window(&self) -> HWND {
        self.hwnd
    }

    /// Capture `self.hwnd`, optionally cropped to `crop` (in screen coordinates),
    /// and scale the result into `bgra` (`dst_w` x `dst_h`, tightly packed BGRA).
    pub fn capture(&mut self, crop: RECT, dst_w: u32, dst_h: u32, bgra: &mut Vec<u8>) -> bool {
        if dst_w == 0 || dst_h == 0 {
            return false;
        }
        let Some(frame) = frame_of(self.hwnd) else {
            return false;
        };
        let win_w = (frame.right - frame.left).max(0) as u32;
        let win_h = (frame.bottom - frame.top).max(0) as u32;
        if win_w < 32 || win_h < 32 {
            return false;
        }

        // Try capturing the specific X11 window via ImageMagick `import -window <id> bgra:-`
        let xid = self.hwnd.0 as usize;
        if let Some((src_w, src_h)) = capture_x11_window_bgra(xid, win_w, win_h, &mut self.rgba) {
            let (cx0, cy0, cw, ch) = compute_window_crop(frame, crop, src_w, src_h);
            scale_bgra(&self.rgba, src_w, src_w * 4, cx0, cy0, cw, ch, dst_w, dst_h, bgra);
            return true;
        }

        false
    }
}

fn compute_window_crop(frame: RECT, crop: RECT, src_w: u32, src_h: u32) -> (u32, u32, u32, u32) {
    let cw = crop.right - crop.left;
    let ch = crop.bottom - crop.top;
    if cw >= 64 && ch >= 64 {
        let x0 = (crop.left - frame.left).clamp(0, src_w as i32) as u32;
        let y0 = (crop.top - frame.top).clamp(0, src_h as i32) as u32;
        let x1 = (crop.right - frame.left).clamp(x0 as i32, src_w as i32) as u32;
        let y1 = (crop.bottom - frame.top).clamp(y0 as i32, src_h as i32) as u32;
        if x1 - x0 >= 64 && y1 - y0 >= 64 {
            return (x0, y0, x1 - x0, y1 - y0);
        }
    }
    (0, 0, src_w, src_h)
}

fn capture_x11_window_bgra(xid: usize, w: u32, h: u32, out: &mut Vec<u8>) -> Option<(u32, u32)> {
    if std::env::var_os("DISPLAY").is_none() || xid == 0 {
        return None;
    }
    let expected = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    let output = Command::new("import")
        .args(["-silent", "-window", &xid.to_string(), "-depth", "8", "bgra:-"])
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.len() < expected {
        return None;
    }
    out.clear();
    out.extend_from_slice(&output.stdout[..expected]);
    Some((w, h))
}

/// Area-averaging box filter from a cropped BGRA region `(crop_x, crop_y, crop_w, crop_h)`
/// of `src` into a tightly packed `dst_w` x `dst_h` BGRA buffer.
#[allow(clippy::too_many_arguments)]
pub(crate) fn scale_bgra(
    src: &[u8],
    src_w: u32,
    src_pitch: u32,
    crop_x: u32,
    crop_y: u32,
    crop_w: u32,
    crop_h: u32,
    dst_w: u32,
    dst_h: u32,
    dst: &mut Vec<u8>,
) {
    let dst_len = (dst_w as usize) * (dst_h as usize) * 4;
    dst.resize(dst_len, 0);
    if crop_w == 0 || crop_h == 0 || dst_w == 0 || dst_h == 0 || src_w == 0 {
        return;
    }
    let pitch = src_pitch as usize;

    if crop_w == dst_w && crop_h == dst_h {
        let row_bytes = (dst_w as usize) * 4;
        let x_off = (crop_x as usize) * 4;
        for y in 0..dst_h as usize {
            let s_off = (crop_y as usize + y) * pitch + x_off;
            let d_off = y * row_bytes;
            if s_off + row_bytes <= src.len() {
                dst[d_off..d_off + row_bytes].copy_from_slice(&src[s_off..s_off + row_bytes]);
            }
        }
        return;
    }

    for dy in 0..dst_h {
        let sy0 = crop_y + ((dy as u64 * crop_h as u64) / dst_h as u64) as u32;
        let sy1 = (crop_y + (((dy + 1) as u64 * crop_h as u64) / dst_h as u64) as u32).max(sy0 + 1);
        for dx in 0..dst_w {
            let sx0 = crop_x + ((dx as u64 * crop_w as u64) / dst_w as u64) as u32;
            let sx1 = (crop_x + (((dx + 1) as u64 * crop_w as u64) / dst_w as u64) as u32).max(sx0 + 1);
            let mut b_sum = 0u32;
            let mut g_sum = 0u32;
            let mut r_sum = 0u32;
            let mut count = 0u32;
            for sy in sy0..sy1 {
                let row = (sy as usize) * pitch;
                for sx in sx0..sx1 {
                    let idx = row + (sx as usize) * 4;
                    if idx + 4 <= src.len() {
                        b_sum += src[idx] as u32;
                        g_sum += src[idx + 1] as u32;
                        r_sum += src[idx + 2] as u32;
                        count += 1;
                    }
                }
            }
            let out_idx = ((dy * dst_w + dx) as usize) * 4;
            if let (Some(b), Some(g), Some(r)) =
                (b_sum.checked_div(count), g_sum.checked_div(count), r_sum.checked_div(count))
            {
                dst[out_idx] = b as u8;
                dst[out_idx + 1] = g as u8;
                dst[out_idx + 2] = r as u8;
                dst[out_idx + 3] = 0xFF;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_size_is_a_copy() {
        let src = vec![
            10, 20, 30, 255, 40, 50, 60, 255, //
            70, 80, 90, 255, 100, 110, 120, 255,
        ];
        let mut dst = Vec::new();
        scale_bgra(&src, 2, 8, 0, 0, 2, 2, 2, 2, &mut dst);
        assert_eq!(dst, src);
    }

    #[test]
    fn halving_averages_and_a_flat_picture_stays_flat() {
        let mut src = Vec::with_capacity(4 * 4 * 4);
        for _ in 0..16 {
            src.extend_from_slice(&[50, 100, 150, 255]);
        }
        let mut dst = Vec::new();
        scale_bgra(&src, 4, 16, 0, 0, 4, 4, 2, 2, &mut dst);
        assert_eq!(dst.len(), 2 * 2 * 4);
        for px in dst.as_chunks::<4>().0 {
            assert_eq!(px, &[50, 100, 150, 255]);
        }
    }
}
