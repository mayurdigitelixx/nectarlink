// SPDX-License-Identifier: GPL-3.0-or-later
//! H.264 decoding with Windows' own decoder (Media Foundation's H.264
//! MFT, licensed with Windows), in low-latency mode: each access unit in,
//! its picture out right away, as NV12.

use std::{mem::ManuallyDrop, sync::Mutex, time::Duration};

use windows::{
    Win32::{
        Foundation::E_FAIL,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAP_READ,
                D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
                D3D11CreateDevice, ID3D11Device, ID3D11Multithread, ID3D11Texture2D,
            },
            Dxgi::Common::DXGI_SAMPLE_DESC,
        },
        Media::MediaFoundation::{
            CLSID_MSH264DecoderMFT, CODECAPI_AVDecNumWorkerThreads, IMF2DBuffer, IMFActivate, IMFDXGIBuffer,
            IMFDXGIDeviceManager, IMFMediaType, IMFSample, IMFTransform, MF_E_NOTACCEPTING,
            MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY, MF_MT_FRAME_SIZE,
            MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX, MF_SA_D3D11_AWARE,
            MF_VERSION, MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
            MFMediaType_Video, MFNominalRange_0_255, MFSTARTUP_NOSOCKET, MFStartup,
            MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_LOCALMFT, MFT_ENUM_FLAG_SORTANDFILTER,
            MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
            MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
            MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
            MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_AV1, MFVideoFormat_H264, MFVideoFormat_HEVC,
            MFVideoFormat_NV12, MFVideoTransferMatrix_BT601,
        },
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        },
    },
    core::Interface,
};

/// A decoded picture: NV12, `stride` bytes per row, the chroma plane after
/// `plane_rows` rows of luma (the decoder pads the height).
#[derive(Debug)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub plane_rows: usize,
    pub nv12: Vec<u8>,
    /// How its colors turn into RGB, as the stream says.
    pub colors: Colors,
}

/// The YUV-to-RGB formula a stream uses (its VUI, as the decoder reports
/// it). Phones are asked for BT.709 limited range, but not all comply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Colors {
    pub bt601: bool,
    pub full_range: bool,
}

impl Colors {
    /// Fixed-point (×256) coefficients: luma scale, then V→R, U→G, V→G, U→B.
    fn coefficients(self) -> (i32, i32, i32, i32, i32) {
        let luma = if self.full_range { 256 } else { 298 };
        let chroma = match (self.bt601, self.full_range) {
            (false, false) => (459, 55, 136, 541),
            (false, true) => (403, 48, 120, 475),
            (true, false) => (409, 100, 208, 516),
            (true, true) => (359, 88, 183, 454),
        };
        (luma, chroma.0, chroma.1, chroma.2, chroma.3)
    }
}

/// Initializes COM on the calling thread and starts Media Foundation once for
/// the process so concurrent decoders and transcoders never tear down Media
/// Foundation's platform work queues under each other.
pub(crate) fn ensure_mf_started() -> windows::core::Result<()> {
    static MF_STARTED: Mutex<bool> = Mutex::new(false);
    // SAFETY: plain COM and Media Foundation startup calls.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let mut started = MF_STARTED.lock().unwrap_or_else(|e| e.into_inner());
        if !*started {
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)?;
            *started = true;
        }
        Ok(())
    }
}

/// Enumerates and activates a synchronous local Media Foundation transform for
/// `category` matching `input_subtype` -> `output_subtype`.
pub(crate) fn create_mft(
    category: windows::core::GUID,
    input_subtype: windows::core::GUID,
    output_subtype: windows::core::GUID,
) -> windows::core::Result<IMFTransform> {
    let input_info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: input_subtype };
    let output_info =
        MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: output_subtype };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnumEx allocates the `activates` array with CoTaskMemAlloc; we take ownership and free it.
    unsafe {
        MFTEnumEx(
            category,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input_info),
            Some(&output_info),
            &mut activates,
            &mut count,
        )?;
        let mut found: Option<IMFTransform> = None;
        if !activates.is_null() {
            let slice = std::slice::from_raw_parts_mut(activates, count as usize);
            for slot in slice.iter_mut() {
                if let Some(act) = slot.take()
                    && found.is_none()
                    && let Ok(t) = act.ActivateObject::<IMFTransform>()
                {
                    found = Some(t);
                }
            }
            CoTaskMemFree(Some(activates.cast()));
        }
        found.ok_or_else(|| windows::core::Error::from(E_FAIL))
    }
}

/// A Direct3D 11 device on the default GPU, shared with Media Foundation.
fn gpu_device_manager() -> windows::core::Result<IMFDXGIDeviceManager> {
    // SAFETY: plain Direct3D and Media Foundation calls with owned values.
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
        let device = device.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        // The decoder uses the device from its own threads.
        if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
            let _ = multithread.SetMultithreadProtected(true);
        }
        let mut token = 0u32;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager = manager.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        manager.ResetDevice(&device, token)?;
        Ok(manager)
    }
}

/// Whether an access unit buffer contains a coded slice/frame for `codec`.
fn has_slice(codec: &str, data: &[u8]) -> bool {
    match codec {
        "h265" => data.windows(4).any(|w| w[..3] == [0, 0, 1] && ((w[3] >> 1) & 0x3f) <= 21),
        "av1" => !data.is_empty(),
        _ => data.windows(4).any(|w| w[..3] == [0, 0, 1] && matches!(w[3] & 0x1f, 1 | 5)),
    }
}

pub struct Decoder {
    codec: String,
    transform: IMFTransform,
    /// The GPU the decoder uses, kept alive with it; `None` when it decodes in software.
    gpu: Option<IMFDXGIDeviceManager>,
    /// Where GPU pictures are copied to be read, reused while the size stays.
    staging: Option<(ID3D11Texture2D, D3D11_TEXTURE2D_DESC)>,
    /// The decoded size, as the output type says (padded).
    coded: (u32, u32),
    provides_samples: bool,
    output_size: u32,
    colors: Colors,
}

impl std::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decoder")
            .field("codec", &self.codec)
            .field("coded", &self.coded)
            .finish_non_exhaustive()
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: tells the transform streaming is over before it is released.
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

impl Decoder {
    /// A low-latency software decoder for `"h264"`, `"h265"`, or `"av1"`, for
    /// the calling thread (which it stays on).
    pub fn new_for_codec(codec: &str) -> windows::core::Result<Decoder> {
        Self::create(codec, false)
    }

    /// As [`Decoder::new_for_codec`], decoding on the GPU when the PC can
    /// (4K H.264 takes about 60 ms a frame in software, far over a 30 fps
    /// frame's 33 ms). Falls back to software when it can't, here or later
    /// in the stream (see [`Decoder::decode`]).
    pub fn new_gpu(codec: &str) -> windows::core::Result<Decoder> {
        Self::create(codec, true).or_else(|e| {
            tracing::info!(error = %e, "no GPU video decoding; decoding in software");
            Self::new_for_codec(codec)
        })
    }

    /// Whether this decoder runs on the GPU.
    pub fn on_gpu(&self) -> bool {
        self.gpu.is_some()
    }

    fn create(codec: &str, gpu: bool) -> windows::core::Result<Decoder> {
        // SAFETY: plain COM and Media Foundation calls with owned values.
        unsafe {
            ensure_mf_started()?;
            let (subtype, transform) = match codec {
                "h265" => (
                    MFVideoFormat_HEVC,
                    create_mft(MFT_CATEGORY_VIDEO_DECODER, MFVideoFormat_HEVC, MFVideoFormat_NV12)?,
                ),
                "av1" => (
                    MFVideoFormat_AV1,
                    create_mft(MFT_CATEGORY_VIDEO_DECODER, MFVideoFormat_AV1, MFVideoFormat_NV12)?,
                ),
                _ => (
                    MFVideoFormat_H264,
                    CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)?,
                ),
            };
            // One picture in, one out: no reordering delay, single worker thread.
            let attributes = transform.GetAttributes().ok();
            if let Some(attributes) = &attributes {
                let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
                let _ = attributes.SetUINT32(&CODECAPI_AVDecNumWorkerThreads, 1);
            }
            // The GPU, before any media type is set.
            let gpu = if gpu {
                let aware = attributes
                    .as_ref()
                    .is_some_and(|a| a.GetUINT32(&MF_SA_D3D11_AWARE).is_ok_and(|v| v != 0));
                if !aware {
                    return Err(windows::core::Error::from(E_FAIL));
                }
                let manager = gpu_device_manager()?;
                transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
                Some(manager)
            } else {
                None
            };
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
            transform.SetInputType(0, &input, 0)?;
            let mut decoder = Decoder {
                codec: codec.to_owned(),
                transform,
                gpu,
                staging: None,
                coded: (0, 0),
                provides_samples: false,
                output_size: 0,
                colors: Colors::default(),
            };
            decoder.choose_output()?;
            decoder.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            decoder.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(decoder)
        }
    }

    pub fn codec(&self) -> &str {
        &self.codec
    }

    /// Picks NV12 output (again after the stream's size changed).
    fn choose_output(&mut self) -> windows::core::Result<()> {
        // SAFETY: as above.
        unsafe {
            let mut index = 0;
            let chosen: IMFMediaType = loop {
                let candidate = self.transform.GetOutputAvailableType(0, index)?;
                if candidate.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    break candidate;
                }
                index += 1;
            };
            self.transform.SetOutputType(0, &chosen, 0)?;
            let size = chosen.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
            self.coded = ((size >> 32) as u32, size as u32);
            // Unknown until the stream says (then the type changes again).
            self.colors = Colors {
                bt601: chosen
                    .GetUINT32(&MF_MT_YUV_MATRIX)
                    .is_ok_and(|m| m == MFVideoTransferMatrix_BT601.0 as u32),
                full_range: chosen
                    .GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE)
                    .is_ok_and(|r| r == MFNominalRange_0_255.0 as u32),
            };
            let info = self.transform.GetOutputStreamInfo(0)?;
            self.provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            self.output_size = info.cbSize;
            Ok(())
        }
    }

    /// Decodes one access unit (Annex B); returns the pictures it completed.
    /// If decoding on the GPU fails, the decoder switches to software for the
    /// rest of the stream and returns the error: the caller starts over from
    /// the next keyframe, as after any decoding error.
    pub fn decode(&mut self, data: &[u8], time_us: u64) -> windows::core::Result<Vec<Picture>> {
        let result = self.decode_on(data, time_us);
        if result.is_err()
            && self.gpu.is_some()
            && let Ok(software) = Self::create(&self.codec, false)
        {
            tracing::info!("GPU video decoding failed; decoding in software");
            *self = software;
        }
        result
    }

    fn decode_on(&mut self, data: &[u8], time_us: u64) -> windows::core::Result<Vec<Picture>> {
        let expects_picture = has_slice(&self.codec, data);
        let sample = sample_of(data, time_us)?;
        let mut pictures = Vec::new();
        // SAFETY: as above.
        loop {
            match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                Ok(()) => break,
                // Full: take what's ready first, then try again.
                Err(e) if e.code() == MF_E_NOTACCEPTING => pictures.extend(self.drain(false)?),
                Err(e) => return Err(e),
            }
        }
        pictures.extend(self.drain(expects_picture)?);
        Ok(pictures)
    }

    /// Starts over (after lost packets): the next keyframe decodes cleanly.
    pub fn flush(&mut self) {
        // SAFETY: as above.
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }

    fn drain(&mut self, expects_picture: bool) -> windows::core::Result<Vec<Picture>> {
        let mut pictures = Vec::new();
        let mut waits = 0u32;
        loop {
            let provided = if self.provides_samples {
                None
            } else {
                // SAFETY: as above.
                let buffer = unsafe { MFCreateMemoryBuffer(self.output_size.max(1))? };
                let sample = unsafe { MFCreateSample()? };
                unsafe { sample.AddBuffer(&buffer)? };
                Some(sample)
            };
            let mut output = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(provided),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0;
            // SAFETY: `output` holds a sample of the size the decoder asked
            // for (or none when it provides its own); both fields are taken
            // back below so they're released.
            let result = unsafe { self.transform.ProcessOutput(0, &mut output, &mut status) };
            let sample = unsafe { ManuallyDrop::take(&mut output[0].pSample) };
            drop(unsafe { ManuallyDrop::take(&mut output[0].pEvents) });
            match result {
                Ok(()) => {
                    if let Some(sample) = sample {
                        pictures.push(self.picture(&sample)?);
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    // When an access unit includes both an SPS that triggers
                    // `MF_E_TRANSFORM_STREAM_CHANGE` and a coded slice, the MFT's
                    // worker thread signals the stream change as soon as it parses
                    // the SPS and continues decoding the slice in parallel. Under
                    // heavy CPU load, the next `ProcessOutput` after `SetOutputType`
                    // can run before the worker thread finishes pushing the picture.
                    if expects_picture && pictures.is_empty() && waits < 500 {
                        waits += 1;
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    return Ok(pictures);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => self.choose_output()?,
                Err(e) => return Err(e),
            }
        }
    }

    fn picture(&mut self, sample: &IMFSample) -> windows::core::Result<Picture> {
        let (width, height) = self.coded;
        // SAFETY: the buffer is locked while it's read and unlocked after.
        unsafe {
            let buffer = sample.ConvertToContiguousBuffer()?;
            if self.gpu.is_some()
                && let Ok(dxgi) = buffer.cast::<IMFDXGIBuffer>()
            {
                let mut texture: Option<ID3D11Texture2D> = None;
                dxgi.GetResource(&ID3D11Texture2D::IID, std::ptr::from_mut(&mut texture).cast())?;
                let texture = texture.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
                let index = dxgi.GetSubresourceIndex()?;
                return self.read_texture(&texture, index, width, height);
            }
            if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
                let mut scan0 = std::ptr::null_mut();
                let mut pitch = 0;
                two_d.Lock2D(&mut scan0, &mut pitch)?;
                let stride = pitch.unsigned_abs() as usize;
                let rows = height as usize;
                let len = stride * rows * 3 / 2;
                let nv12 = std::slice::from_raw_parts(scan0, len).to_vec();
                two_d.Unlock2D()?;
                return Ok(Picture { width, height, stride, plane_rows: rows, nv12, colors: self.colors });
            }
            let mut data = std::ptr::null_mut();
            let mut length = 0;
            buffer.Lock(&mut data, None, Some(&mut length))?;
            let nv12 = std::slice::from_raw_parts(data, length as usize).to_vec();
            buffer.Unlock()?;
            let stride = width as usize;
            let plane_rows = (nv12.len() * 2 / 3) / stride.max(1);
            Ok(Picture { width, height, stride, plane_rows, nv12, colors: self.colors })
        }
    }
}

impl Decoder {
    /// Copies a decoded picture out of GPU memory. Windows' own path makes a
    /// new staging texture and waits on it for every picture (about 14 ms at
    /// 4K); one kept for the stream takes a few.
    fn read_texture(
        &mut self,
        texture: &ID3D11Texture2D,
        index: u32,
        width: u32,
        height: u32,
    ) -> windows::core::Result<Picture> {
        // SAFETY: Direct3D calls on the decoder's own device; the staging
        // texture is mapped while it's read and unmapped after.
        unsafe {
            let device = texture.GetDevice()?;
            let context = device.GetImmediateContext()?;
            let mut source = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut source);
            let reuse = self.staging.as_ref().is_some_and(|(_, d)| {
                (d.Width, d.Height, d.Format) == (source.Width, source.Height, source.Format)
            });
            if !reuse {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: source.Width,
                    Height: source.Height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: source.Format,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };
                let mut staging: Option<ID3D11Texture2D> = None;
                device.CreateTexture2D(&desc, None, Some(&mut staging))?;
                let staging = staging.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
                self.staging = Some((staging, desc));
            }
            let (staging, _) = self.staging.as_ref().ok_or_else(|| windows::core::Error::from(E_FAIL))?;
            context.CopySubresourceRegion(staging, 0, 0, 0, 0, texture, index, None);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            // NV12: the chroma plane follows the texture's full height of luma.
            let stride = mapped.RowPitch as usize;
            let plane_rows = source.Height as usize;
            let len = stride * plane_rows * 3 / 2;
            let nv12 = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), len).to_vec();
            context.Unmap(staging, 0);
            Ok(Picture { width, height, stride, plane_rows, nv12, colors: self.colors })
        }
    }
}

fn sample_of(data: &[u8], time_us: u64) -> windows::core::Result<IMFSample> {
    // SAFETY: the buffer is at least `data.len()` long and locked while
    // it's written.
    unsafe {
        let buffer = MFCreateMemoryBuffer(data.len() as u32)?;
        let mut target = std::ptr::null_mut();
        buffer.Lock(&mut target, None, None)?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), target, data.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(data.len() as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        // In 100-nanosecond units.
        sample.SetSampleTime(time_us as i64 * 10)?;
        Ok(sample)
    }
}

/// Pixels from which a frame is converted on several cores: 4K takes about
/// 24 ms on one core, most of a 30 fps frame's 33 ms, while 720p is quick
/// enough on the calling thread.
const PARALLEL_FROM_PIXELS: usize = 1920 * 1080;

/// NV12 (in the picture's own colors) to 32-bit BGRX, the visible
/// `width` x `height`. Large frames are split into bands of rows, one per
/// core (scoped threads cost microseconds, against milliseconds of work).
pub fn to_bgrx(picture: &Picture, width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width.min(picture.width) as usize, height.min(picture.height) as usize);
    let mut out = vec![0u8; w * h * 4];
    if w == 0 || h == 0 {
        return out;
    }
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8);
    if w * h < PARALLEL_FROM_PIXELS || cores < 2 {
        convert_rows(picture, w, 0, &mut out);
        return out;
    }
    // Bands of an even number of rows, so each starts on a chroma row.
    let band = h.div_ceil(cores).next_multiple_of(2);
    std::thread::scope(|scope| {
        for (i, rows) in out.chunks_mut(band * w * 4).enumerate() {
            scope.spawn(move || convert_rows(picture, w, i * band, rows));
        }
    });
    out
}

fn convert_rows(p: &Picture, w: usize, first: usize, out: &mut [u8]) {
    let uv_plane = p.stride * p.plane_rows;
    let (luma, vr, ug, vg, ub) = p.colors.coefficients();
    let black = if p.colors.full_range { 0 } else { 16 };
    for (i, row) in out.chunks_exact_mut(w * 4).enumerate() {
        let y_row = first + i;
        let y_line = &p.nv12[y_row * p.stride..][..w];
        let uv_line = &p.nv12[uv_plane + (y_row / 2) * p.stride..][..w.next_multiple_of(2).min(p.stride)];
        for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let c = (i32::from(y_line[x]) - black) * luma;
            let d = i32::from(uv_line[x & !1]) - 128;
            let e = i32::from(uv_line[(x & !1) + 1]) - 128;
            px[0] = clamp((c + ub * d + 128) >> 8);
            px[1] = clamp((c - ug * d - vg * e + 128) >> 8);
            px[2] = clamp((c + vr * e + 128) >> 8);
            px[3] = 255;
        }
    }
}

fn clamp(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn bench_to_bgrx_4k() {
        let (w, h) = (3840u32, 2160u32);
        let stride = w as usize;
        let picture = Picture {
            width: w,
            height: h,
            stride,
            plane_rows: h as usize,
            nv12: (0..stride * h as usize * 3 / 2).map(|i| (i % 251) as u8).collect(),
            colors: Colors::default(),
        };
        let start = std::time::Instant::now();
        let n = 30;
        for _ in 0..n {
            std::hint::black_box(to_bgrx(&picture, w, h));
        }
        eprintln!("BENCH to_bgrx 4K: {:.1} ms/frame", start.elapsed().as_secs_f64() * 1000.0 / n as f64);
    }

    #[test]
    #[ignore]
    fn bench_decode_4k() {
        let path = std::env::var("BENCH_4K_H264").expect("BENCH_4K_H264");
        let stream = std::fs::read(path).unwrap();
        let units = access_units(&stream);
        for gpu in [false, true] {
            let mut decoder =
                if gpu { Decoder::new_gpu("h264").unwrap() } else { Decoder::new_for_codec("h264").unwrap() };
            let on_gpu = decoder.on_gpu();
            let start = std::time::Instant::now();
            let mut pictures = 0;
            let mut first_out = None;
            for (i, u) in units.iter().enumerate() {
                let got = decoder.decode(u, i as u64 * 33_333).unwrap().len();
                if got > 0 && first_out.is_none() {
                    first_out = Some(i);
                }
                pictures += got;
            }
            eprintln!(
                "BENCH decode 4K gpu={on_gpu}: {} units, {} pictures, first picture after unit {:?}, {:.1} ms/unit",
                units.len(),
                pictures,
                first_out,
                start.elapsed().as_secs_f64() * 1000.0 / units.len() as f64
            );
        }
    }

    #[test]
    fn the_stream_says_which_colors() {
        // 3 frames tagged BT.601, full range (as some phones encode).
        let units = access_units(include_bytes!("testdata/bt601-full.h264"));
        let mut decoder = Decoder::new_for_codec("h264").unwrap();
        let pictures: Vec<Picture> =
            units.iter().enumerate().flat_map(|(i, u)| decoder.decode(u, i as u64).unwrap()).collect();
        assert_eq!(pictures.last().unwrap().colors, Colors { bt601: true, full_range: true });
    }

    #[test]
    fn colors_convert() {
        // 4x2: black, white, and BT.709 red and blue.
        let (w, h) = (4usize, 2usize);
        let mut nv12 = vec![0u8; w * h * 3 / 2];
        nv12[..8].copy_from_slice(&[16, 235, 63, 32, 16, 235, 63, 32]);
        // Chroma for pixel pairs (0,1) and (2,3): neutral; then red's... the
        // second pair holds red's chroma (u=102, v=240).
        nv12[8..12].copy_from_slice(&[128, 128, 102, 240]);
        let picture =
            Picture { width: 4, height: 2, stride: 4, plane_rows: 2, nv12, colors: Colors::default() };
        let out = to_bgrx(&picture, 4, 2);
        assert_eq!(&out[..4], &[0, 0, 0, 255], "black");
        assert_eq!(&out[4..8], &[255, 255, 255, 255], "white");
        let red = &out[8..12];
        assert!(red[2] > 240 && red[1] < 20 && red[0] < 20, "red: {red:?}");
    }

    #[test]
    fn odd_sizes_are_cropped_safely() {
        let picture = Picture {
            width: 6,
            height: 4,
            stride: 8,
            plane_rows: 4,
            nv12: vec![128; 8 * 6],
            colors: Colors::default(),
        };
        assert_eq!(to_bgrx(&picture, 5, 3).len(), 5 * 3 * 4);
        assert_eq!(to_bgrx(&picture, 10, 10).len(), 6 * 4 * 4, "never larger than the picture");
    }

    /// Splits an Annex B stream into access units at its access unit
    /// delimiters (a 4-byte start code, then NAL type 9).
    fn access_units(stream: &[u8]) -> Vec<&[u8]> {
        let starts: Vec<usize> =
            (0..stream.len()).filter(|&i| stream[i..].starts_with(&[0, 0, 0, 1, 9])).collect();
        starts
            .iter()
            .enumerate()
            .map(|(n, &s)| &stream[s..*starts.get(n + 1).unwrap_or(&stream.len())])
            .collect()
    }

    /// Decodes `units` on the GPU and in software and checks every picture
    /// matches (decoding is exact). Skips when this PC has no GPU decoding
    /// (CI runners) or no decoder for the codec (HEVC is an optional Windows
    /// extension).
    fn gpu_matches_software(codec: &str, units: &[&[u8]]) {
        let Ok(mut software) = Decoder::new_for_codec(codec) else { return };
        let Ok(mut gpu) = Decoder::new_gpu(codec) else { return };
        if !gpu.on_gpu() {
            return;
        }
        let mut compared = 0;
        for (i, unit) in units.iter().enumerate() {
            let a = software.decode(unit, i as u64 * 33_333).unwrap();
            let b = gpu.decode(unit, i as u64 * 33_333).unwrap();
            assert!(gpu.on_gpu(), "{codec}: fell back to software at unit {i}");
            assert_eq!(a.len(), b.len(), "{codec} unit {i}");
            for (a, b) in a.iter().zip(&b) {
                assert_eq!((a.width, a.height), (b.width, b.height));
                let (w, h) = (a.width as usize, a.height as usize);
                for row in 0..h {
                    assert_eq!(
                        a.nv12[row * a.stride..][..w],
                        b.nv12[row * b.stride..][..w],
                        "{codec} unit {i} luma row {row}"
                    );
                }
                let (ca, cb) = (a.stride * a.plane_rows, b.stride * b.plane_rows);
                for row in 0..h / 2 {
                    assert_eq!(
                        a.nv12[ca + row * a.stride..][..w],
                        b.nv12[cb + row * b.stride..][..w],
                        "{codec} unit {i} chroma row {row}"
                    );
                }
                compared += 1;
            }
        }
        assert!(compared > 0, "{codec}: no pictures to compare");
    }

    #[test]
    fn the_gpu_decodes_the_same_h264_pictures_as_software() {
        gpu_matches_software("h264", &access_units(include_bytes!("testdata/testsrc.h264")));
    }

    #[test]
    fn the_gpu_decodes_the_same_h265_pictures_as_software() {
        // 10 frames of FFmpeg's test pattern, 320x240, HEVC, access unit delimiters.
        let stream = include_bytes!("testdata/testsrc.h265");
        let starts: Vec<usize> =
            (0..stream.len()).filter(|&i| stream[i..].starts_with(&[0, 0, 0, 1, 0x46])).collect();
        let units: Vec<&[u8]> = starts
            .iter()
            .enumerate()
            .map(|(k, &at)| &stream[at..starts.get(k + 1).copied().unwrap_or(stream.len())])
            .collect();
        assert_eq!(units.len(), 10);
        gpu_matches_software("h265", &units);
    }

    #[test]
    fn a_stream_decodes_picture_by_picture() {
        // 10 frames of FFmpeg's test pattern, 320x240, a keyframe every 5,
        // no B-frames, as a phone's encoder sends them.
        let stream = include_bytes!("testdata/testsrc.h264");
        let units = access_units(stream);
        assert_eq!(units.len(), 10);
        let mut decoder = Decoder::new_for_codec("h264").unwrap();
        let mut pictures = Vec::new();
        for (i, unit) in units.iter().enumerate() {
            let got = decoder.decode(unit, i as u64 * 33_333).unwrap();
            // Low-latency mode: each picture comes out with its own unit.
            assert_eq!(got.len(), 1, "unit {i}");
            pictures.extend(got);
        }
        let first = &pictures[0];
        assert_eq!((first.width, first.height), (320, 240));
        assert_eq!(first.colors, Colors { bt601: false, full_range: false }, "tagged BT.709, limited");
        let bgrx = to_bgrx(first, 320, 240);
        // A colorful test pattern, not a blank picture.
        let distinct: std::collections::HashSet<&[u8; 4]> = bgrx.as_chunks::<4>().0.iter().collect();
        assert!(distinct.len() > 50, "{} colors", distinct.len());
    }
}
