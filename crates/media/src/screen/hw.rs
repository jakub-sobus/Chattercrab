//! Hardware H.264 encode.
//!
//! Windows uses the GPU's Media Foundation encoder (NVIDIA NVENC, Intel Quick
//! Sync, or AMD AMF, whichever the driver registered). Linux uses VA-API on a
//! DRM render node, which covers Intel and AMD directly and NVIDIA through
//! `nvidia-vaapi-driver`. libva is loaded at runtime, so the build does not
//! link it. Both paths emit Annex-B H.264 with no B-frames, which the OpenH264
//! decoder can read. The standard operating point is Constrained Baseline,
//! 1440p at 60 fps and 12 Mbps. High quality requests H.264 High at a higher
//! fixed bitrate, and 4K60 or 1440p120 uses level 5.2. Linux VA-API uses that
//! same profile, GOP, and two-frame rate-control window, with two reference
//! frames for desktop and one for game. The user picks that mode. Resolution
//! and frame rate do not.
//!
//! The Windows encoder is handed a Direct3D 11 device on the adapter that
//! registered it. Without that device, `SetOutputType` fails with
//! `MF_E_UNSUPPORTED_D3D_TYPE` and the capture thread falls back to OpenH264.
//!
//! If no hardware encoder can be opened, the capture thread uses OpenH264.

/// One encoded picture, Annex-B.
pub struct AccessUnit {
    /// NAL units of one picture, including SPS and PPS on a keyframe.
    pub data: Vec<u8>,
    /// Whether this picture is an IDR.
    pub keyframe: bool,
}

/// Rate control for one encoder open.
///
/// Standard shares keep the existing bitrate and a two-second GOP. High quality
/// follows the low-latency operating point: CBR at the spec bitrate, a GOP long
/// enough that IDRs happen only on demand, no B-frames, and a one-to-two-frame
/// rate-control window.
#[derive(Debug, Clone, Copy)]
pub struct EncodeParams {
    /// CBR target, bits per second.
    pub bitrate: u32,
    /// Frames between periodic IDRs. High quality uses a very large value.
    pub gop_frames: u32,
    /// H.264 High profile, higher bitrate, IDR only on demand.
    pub high_quality: bool,
    /// Game content: one reference and a faster preset, so moving images stay
    /// less blurry. Desktop content keeps two references and a slower preset
    /// for crisp text.
    pub game: bool,
    /// Rate-control window. 100 ms in high quality, one second otherwise.
    pub rc_window_ms: u32,
    /// 0 is the slowest preset, 100 is the fastest. Media Foundation's
    /// quality-versus-speed knob. VA-API maps it onto the driver's range.
    pub speed: u32,
}

/// Length-prefixed NALs (AVCC) become Annex-B start codes. Annex-B input is copied.
pub fn to_annex_b(src: &[u8]) -> Vec<u8> {
    if src.is_empty() || has_start_code(src) {
        return src.to_vec();
    }
    let mut out = Vec::with_capacity(src.len() + 64);
    let mut i = 0;
    while i + 4 <= src.len() {
        let len = u32::from_be_bytes([src[i], src[i + 1], src[i + 2], src[i + 3]]) as usize;
        i += 4;
        if len == 0 || i + len > src.len() {
            return src.to_vec();
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&src[i..i + len]);
        i += len;
    }
    if i != src.len() || out.is_empty() {
        src.to_vec()
    } else {
        out
    }
}

fn has_start_code(src: &[u8]) -> bool {
    src.starts_with(&[0, 0, 0, 1]) || src.starts_with(&[0, 0, 1])
}

/// Bitrate for the hardware encoder. 12 Mbps at 2560×1440 and 60 fps, scaled
/// by pixel rate. Hardware H.264 holds screen text at this rate; a higher
/// target would only spend bandwidth.
pub fn hardware_bitrate(width: u32, height: u32, fps: u32) -> u32 {
    const REF_BPS: u64 = 12_000_000;
    const REF_PIXELS: u64 = 2560 * 1440 * 60;
    let pixels = u64::from(width) * u64::from(height) * u64::from(fps.max(1));
    let bps = REF_BPS.saturating_mul(pixels) / REF_PIXELS;
    bps.clamp(4_000_000, 20_000_000) as u32
}

/// H.264 `level_idc` for this picture. 51 when it fits in level 5.1, otherwise 52.
/// Level 5.2 is what 3840×2160 at 60 fps requires; 1440p60 still fits in 5.1.
pub fn h264_level_idc(width: u32, height: u32, fps: u32) -> u32 {
    let mbs = u64::from(width.div_ceil(16)) * u64::from(height.div_ceil(16));
    let mbps = mbs.saturating_mul(u64::from(fps.max(1)));
    if mbs <= 36_864 && mbps <= 983_040 {
        51
    } else {
        52
    }
}

#[path = "h264_headers.rs"]
mod h264_headers;
#[path = "va_abi.rs"]
mod va_abi;

/// Pictures at least this large are converted on several threads.
const THREADED_PIXELS: usize = 1920 * 1080;
/// Most threads one conversion uses.
const MAX_CONVERT_THREADS: usize = 4;

/// BT.709 limited-range RGBA to NV12. `y_stride` and `uv_stride` are the bytes
/// between row starts, at least `width`.
pub(super) fn rgba_to_nv12(
    src: &[u8],
    width: usize,
    height: usize,
    y_plane: &mut [u8],
    y_stride: usize,
    uv_plane: &mut [u8],
    uv_stride: usize,
) {
    let planes = Planes {
        y: y_plane,
        y_stride,
        chroma: Chroma::Nv12(uv_plane),
        chroma_stride: uv_stride,
    };
    rgba_to_yuv420(src, width, height, planes, convert_threads(width, height));
}

/// BT.709 limited-range RGBA to tightly packed I420 (separate U and V planes).
pub(super) fn rgba_to_i420(
    src: &[u8],
    width: usize,
    height: usize,
    y_plane: &mut [u8],
    u_plane: &mut [u8],
    v_plane: &mut [u8],
) {
    let planes = Planes {
        y: y_plane,
        y_stride: width,
        chroma: Chroma::I420(u_plane, v_plane),
        chroma_stride: width / 2,
    };
    rgba_to_yuv420(src, width, height, planes, convert_threads(width, height));
}

/// Fills the coded area right of and below a `picture` inside `coded` with
/// black (Y 16, UV 128). The planes are NV12 with the given row strides.
#[cfg(any(target_os = "linux", test))]
fn fill_nv12_padding(
    picture: (usize, usize),
    coded: (usize, usize),
    y_plane: &mut [u8],
    y_stride: usize,
    uv_plane: &mut [u8],
    uv_stride: usize,
) {
    let ((w, h), (cw, ch)) = (picture, coded);
    for row in 0..ch {
        let from = if row < h { w } else { 0 };
        y_plane[row * y_stride + from..row * y_stride + cw].fill(16);
    }
    for row in 0..ch / 2 {
        let from = if row < h / 2 { w } else { 0 };
        uv_plane[row * uv_stride + from..row * uv_stride + cw].fill(128);
    }
}

/// Threads for one conversion: up to four for 1080p and larger, otherwise one.
fn convert_threads(width: usize, height: usize) -> usize {
    static CORES: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    if width * height < THREADED_PIXELS {
        return 1;
    }
    let cores = *CORES.get_or_init(|| std::thread::available_parallelism().map_or(1, usize::from));
    cores.min(MAX_CONVERT_THREADS)
}

/// Destination planes of one 4:2:0 picture. Strides are bytes between row starts.
struct Planes<'a> {
    y: &'a mut [u8],
    y_stride: usize,
    chroma: Chroma<'a>,
    chroma_stride: usize,
}

/// Chroma layout of a 4:2:0 picture.
enum Chroma<'a> {
    /// Interleaved Cb and Cr (NV12).
    Nv12(&'a mut [u8]),
    /// Separate Cb and Cr planes (I420).
    I420(&'a mut [u8], &'a mut [u8]),
}

impl<'a> Planes<'a> {
    /// Splits off the planes of the first `pairs` luma row pairs.
    fn split_rows(&mut self, pairs: usize) -> Planes<'a> {
        let stride = self.chroma_stride;
        let chroma = match &mut self.chroma {
            Chroma::Nv12(uv) => Chroma::Nv12(take_rows(uv, pairs, stride)),
            Chroma::I420(u, v) => {
                Chroma::I420(take_rows(u, pairs, stride), take_rows(v, pairs, stride))
            }
        };
        Planes {
            y: take_rows(&mut self.y, 2 * pairs, self.y_stride),
            y_stride: self.y_stride,
            chroma,
            chroma_stride: stride,
        }
    }
}

/// Removes the first `rows` rows from `plane` and returns them. The last row
/// of a plane may be shorter than `stride`.
fn take_rows<'a>(plane: &mut &'a mut [u8], rows: usize, stride: usize) -> &'a mut [u8] {
    let len = (rows * stride).min(plane.len());
    let (head, tail) = std::mem::take(plane).split_at_mut(len);
    *plane = tail;
    head
}

/// Even-sized RGBA to BT.709 limited-range 4:2:0. Each 2×2 block's chroma is
/// the average of its four pixels. Row pairs are split across `threads`.
fn rgba_to_yuv420(src: &[u8], width: usize, height: usize, mut planes: Planes<'_>, threads: usize) {
    let pairs = height / 2;
    let pair_bytes = 2 * width * 4;
    let src = &src[..pairs * pair_bytes];
    let threads = threads.clamp(1, pairs.max(1));
    if threads == 1 {
        convert_rows(src, width, planes);
        return;
    }
    let per_band = pairs.div_ceil(threads);
    std::thread::scope(|scope| {
        let mut rest = src;
        let mut left = pairs;
        while left > 0 {
            let n = per_band.min(left);
            left -= n;
            let (band, tail) = rest.split_at(n * pair_bytes);
            rest = tail;
            let out = planes.split_rows(n);
            if left == 0 {
                convert_rows(band, width, out);
            } else {
                scope.spawn(move || convert_rows(band, width, out));
            }
        }
    });
}

/// Converts the row pairs in `src` into `planes`, whose first row is the
/// first row of `src`.
fn convert_rows(src: &[u8], width: usize, mut planes: Planes<'_>) {
    let row = width * 4;
    for (k, pair) in src.chunks_exact(2 * row).enumerate() {
        let (s0, s1) = pair.split_at(row);
        let (y0, y1) = planes.y[2 * k * planes.y_stride..].split_at_mut(planes.y_stride);
        let (y0, y1) = (&mut y0[..width], &mut y1[..width]);
        let c = k * planes.chroma_stride;
        match &mut planes.chroma {
            Chroma::Nv12(uv) => row_pair_nv12(s0, s1, y0, y1, &mut uv[c..c + width]),
            Chroma::I420(u, v) => row_pair_i420(
                s0,
                s1,
                y0,
                y1,
                &mut u[c..c + width / 2],
                &mut v[c..c + width / 2],
            ),
        }
    }
}

fn row_pair_nv12(s0: &[u8], s1: &[u8], y0: &mut [u8], y1: &mut [u8], uv: &mut [u8]) {
    let src = s0.as_chunks::<8>().0.iter().zip(s1.as_chunks::<8>().0);
    let luma = y0
        .as_chunks_mut::<2>()
        .0
        .iter_mut()
        .zip(y1.as_chunks_mut::<2>().0);
    for (((a, b), (ya, yb)), c) in src.zip(luma).zip(uv.as_chunks_mut::<2>().0) {
        *c = block(a, b, ya, yb);
    }
}

fn row_pair_i420(s0: &[u8], s1: &[u8], y0: &mut [u8], y1: &mut [u8], u: &mut [u8], v: &mut [u8]) {
    let src = s0.as_chunks::<8>().0.iter().zip(s1.as_chunks::<8>().0);
    let luma = y0
        .as_chunks_mut::<2>()
        .0
        .iter_mut()
        .zip(y1.as_chunks_mut::<2>().0);
    for (((a, b), (ya, yb)), (cu, cv)) in src.zip(luma).zip(u.iter_mut().zip(v)) {
        [*cu, *cv] = block(a, b, ya, yb);
    }
}

/// One 2×2 block: two RGBA pixels from each of two rows. Writes the four luma
/// samples and returns `[Cb, Cr]` of the block average.
#[inline(always)]
fn block(a: &[u8; 8], b: &[u8; 8], ya: &mut [u8; 2], yb: &mut [u8; 2]) -> [u8; 2] {
    *ya = [luma(a[0], a[1], a[2]), luma(a[4], a[5], a[6])];
    *yb = [luma(b[0], b[1], b[2]), luma(b[4], b[5], b[6])];
    let avg = |i: usize| {
        let sum = u16::from(a[i]) + u16::from(a[i + 4]) + u16::from(b[i]) + u16::from(b[i + 4]);
        (sum >> 2) as i16
    };
    let (r, g, b) = (avg(0), avg(1), avg(2));
    [
        chroma(-26, -86, 112, r, g, b),
        chroma(112, -102, -10, r, g, b),
    ]
}

/// BT.709 luma in 16..=235: `219/255 * (0.2126 R + 0.7152 G + 0.0722 B)`, ×256.
/// The weighted sum is at most 56 228, so 16-bit lanes are enough.
#[inline(always)]
fn luma(r: u8, g: u8, b: u8) -> u8 {
    let y = (47 * u16::from(r) + 157 * u16::from(g) + 16 * u16::from(b) + 128) >> 8;
    (y + 16) as u8
}

/// BT.709 chroma in 16..=240, ×256. Each coefficient row sums to zero so grey
/// stays exactly at 128. The weighted sum stays within ±28 688, inside `i16`.
#[inline(always)]
fn chroma(cr: i16, cg: i16, cb: i16, r: i16, g: i16, b: i16) -> u8 {
    let c = (cr * r + cg * g + cb * b + 128) >> 8;
    (c + 128) as u8
}

#[cfg(windows)]
#[allow(unsafe_op_in_unsafe_fn)]
mod platform {
    use std::mem::ManuallyDrop;
    use std::ptr;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{HMODULE, VARIANT_TRUE};
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
        D3D11CreateDevice, ID3D11Device, ID3D11Multithread, ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIAdapter1,
        IDXGIDevice, IDXGIFactory1,
    };
    use windows::Win32::Media::MediaFoundation::{
        CODECAPI_AVEncCommonBufferSize, CODECAPI_AVEncCommonLowLatency,
        CODECAPI_AVEncCommonMaxBitRate, CODECAPI_AVEncCommonMeanBitRate,
        CODECAPI_AVEncCommonQualityVsSpeed, CODECAPI_AVEncCommonRateControlMode,
        CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
        CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVEncVideoMaxNumRefFrame,
        CODECAPI_AVLowLatencyMode, CODECAPI_AVScenarioInfo, ICodecAPI, IMFActivate,
        IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaEventGenerator, IMFSample, IMFTransform,
        MEError, METransformHaveOutput, METransformNeedInput, MF_E_NO_EVENTS_AVAILABLE,
        MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT,
        MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
        MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_LEVEL, MF_MT_MPEG2_PROFILE,
        MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION,
        MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_TRANSFORM_ASYNC,
        MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFCreateDXGIDeviceManager,
        MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
        MFMediaType_Video, MFNominalRange_16_235, MFSTARTUP_FULL, MFSampleExtension_CleanPoint,
        MFStartup, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_ADAPTER_LUID, MFT_ENUM_FLAG,
        MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_FRIENDLY_NAME_Attribute,
        MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
        MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
        MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
        MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
        MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
        MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709,
        MFVideoTransferMatrix_BT709, eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Base,
        eAVEncH264VProfile_ConstrainedBase, eAVEncH264VProfile_High,
        eAVScenarioInfo_DisplayRemoting, eAVScenarioInfo_LiveStreaming,
    };
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
    use windows::Win32::System::Variant::{VARIANT, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4};
    use windows::core::{IUnknown, Interface};

    use super::{AccessUnit, EncodeParams, to_annex_b};
    use crate::screen::receive::contains_idr;

    /// How long to wait for the encoder to accept a frame.
    const NEED_INPUT_TIMEOUT: Duration = Duration::from_millis(500);
    /// CPU input buffers kept for reuse. More than one, because an async encoder
    /// can still hold the previous picture when the next one arrives.
    const INPUT_RING: usize = 3;

    /// One reusable NV12 input picture.
    struct InputSlot {
        sample: IMFSample,
        buffer: IMFMediaBuffer,
    }

    impl InputSlot {
        unsafe fn new(len: u32) -> Result<Self, String> {
            let buffer = MFCreateMemoryBuffer(len).map_err(|e| format!("input buffer: {e}"))?;
            let sample = MFCreateSample().map_err(|e| format!("input sample: {e}"))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|e| format!("attaching input buffer: {e}"))?;
            Ok(Self { sample, buffer })
        }

        /// The encoder has released it: only this slot holds the sample, and
        /// only this slot and the sample hold the buffer.
        unsafe fn is_free(&self) -> bool {
            ref_count(&self.sample) == 1 && ref_count(&self.buffer) == 2
        }
    }

    /// Current COM reference count of `object`.
    unsafe fn ref_count<T: Interface>(object: &T) -> u32 {
        let raw = object.as_raw();
        let Some(unknown) = IUnknown::from_raw_borrowed(&raw) else {
            return u32::MAX;
        };
        (unknown.vtable().AddRef)(raw);
        (unknown.vtable().Release)(raw)
    }

    /// GPU H.264 encoder. Touched only from the capture thread that created it.
    pub struct HwEncoder {
        width: u32,
        height: u32,
        fps: u32,
        transform: IMFTransform,
        // Held so the MFT's DXGI device outlives clearing the manager in Drop.
        device: ID3D11Device,
        device_manager: IMFDXGIDeviceManager,
        event_gen: Option<IMFMediaEventGenerator>,
        codec_api: Option<ICodecAPI>,
        output_provides_samples: bool,
        output_buffer_size: u32,
        input_stream_id: u32,
        output_stream_id: u32,
        need_input_credits: u32,
        frames_submitted: u64,
        /// CPU input pictures, reused once the encoder releases them.
        inputs: Vec<InputSlot>,
    }

    impl HwEncoder {
        /// Opens a Media Foundation encoder for even `width` x `height` frames at `fps`.
        pub fn open(
            width: u32,
            height: u32,
            fps: u32,
            params: EncodeParams,
        ) -> Result<Self, String> {
            if width < 2 || height < 2 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
                return Err(format!(
                    "hardware encoder needs even dimensions, got {width}x{height}"
                ));
            }
            let fps = fps.max(1);
            ensure_mf()?;
            unsafe { Self::start(width, height, fps, params) }
        }

        /// The Direct3D device the encoder was opened on.
        pub fn device(&self) -> ID3D11Device {
            self.device.clone()
        }

        /// Encodes one tightly packed RGBA frame. `force_idr` starts a new GOP.
        pub fn encode(&mut self, rgba: &[u8], force_idr: bool) -> Result<Vec<AccessUnit>, String> {
            let expected = (self.width as usize) * (self.height as usize) * 4;
            if rgba.len() != expected {
                return Err(format!(
                    "frame is {} bytes, expected {expected}",
                    rgba.len()
                ));
            }
            let sample = unsafe { self.input_sample(rgba)? };
            self.submit(sample, force_idr)
        }

        /// Encodes an NV12 texture created on [`Self::device`].
        pub fn encode_dxgi(
            &mut self,
            texture: &ID3D11Texture2D,
            force_idr: bool,
        ) -> Result<Vec<AccessUnit>, String> {
            let sample = unsafe { self.surface_sample(texture)? };
            self.submit(sample, force_idr)
        }

        /// Pictures an async encoder finished since the last call, collected
        /// without new input. A synchronous encoder never has any.
        pub fn drain(&mut self) -> Result<Vec<AccessUnit>, String> {
            let mut units = Vec::new();
            if let Some(events) = self.event_gen.clone() {
                let transform = self.transform.clone();
                while self.poll(&events, &transform, &mut units)? {}
            }
            Ok(units)
        }

        fn submit(
            &mut self,
            sample: IMFSample,
            force_idr: bool,
        ) -> Result<Vec<AccessUnit>, String> {
            if let Some(api) = &self.codec_api {
                // Some drivers leave this set, so every later picture becomes an IDR.
                let force = u32::from(force_idr || self.frames_submitted == 0);
                set_codec(api, &CODECAPI_AVEncVideoForceKeyFrame, variant_u32(force));
            }
            let transform = self.transform.clone();
            let mut units = Vec::new();
            if let Some(events) = self.event_gen.clone() {
                self.encode_async(&transform, &events, sample, &mut units)?;
            } else {
                self.encode_sync(&transform, sample, &mut units)?;
            }
            self.frames_submitted += 1;
            Ok(units)
        }

        unsafe fn start(
            width: u32,
            height: u32,
            fps: u32,
            params: EncodeParams,
        ) -> Result<Self, String> {
            let activates =
                enumerate().map_err(|e| format!("enumerating hardware encoders: {e}"))?;
            if activates.is_empty() {
                return Err("no hardware h264 encoder".into());
            }
            let mut errors = Vec::new();
            for activate in &activates {
                let name = friendly_name(activate).unwrap_or_else(|| "hardware h264".to_owned());
                match open_activated(activate, &name, width, height, fps, params) {
                    Ok(encoder) => return Ok(encoder),
                    Err(error) => {
                        tracing::debug!(encoder = %name, %error, "skipping hardware encoder");
                        errors.push(format!("{name}: {error}"));
                    }
                }
            }
            Err(errors.join("; "))
        }

        unsafe fn input_sample(&mut self, rgba: &[u8]) -> Result<IMFSample, String> {
            let w = self.width as usize;
            let h = self.height as usize;
            let nv12_len = w * h * 3 / 2;
            // A fresh multi-megabyte buffer per frame costs page faults. Reuse
            // one the encoder has let go of; past the ring, allocate as before.
            let (sample, buffer) = match self.inputs.iter().find(|slot| slot.is_free()) {
                Some(slot) => (slot.sample.clone(), slot.buffer.clone()),
                None => {
                    let slot = InputSlot::new(nv12_len as u32)?;
                    let pair = (slot.sample.clone(), slot.buffer.clone());
                    if self.inputs.len() < INPUT_RING {
                        self.inputs.push(slot);
                    }
                    pair
                }
            };
            let mut data_ptr: *mut u8 = ptr::null_mut();
            buffer
                .Lock(&mut data_ptr, None, None)
                .map_err(|e| format!("locking input buffer: {e}"))?;
            {
                let dst = std::slice::from_raw_parts_mut(data_ptr, nv12_len);
                let (y_plane, uv_plane) = dst.split_at_mut(w * h);
                super::rgba_to_nv12(rgba, w, h, y_plane, w, uv_plane, w);
            }
            buffer
                .Unlock()
                .map_err(|e| format!("unlocking input buffer: {e}"))?;
            buffer
                .SetCurrentLength(nv12_len as u32)
                .map_err(|e| format!("input length: {e}"))?;
            let time = (self.frames_submitted as i64) * 10_000_000 / i64::from(self.fps);
            let duration = 10_000_000 / i64::from(self.fps);
            sample
                .SetSampleTime(time)
                .map_err(|e| format!("sample time: {e}"))?;
            sample
                .SetSampleDuration(duration)
                .map_err(|e| format!("sample duration: {e}"))?;
            Ok(sample)
        }

        unsafe fn surface_sample(&self, texture: &ID3D11Texture2D) -> Result<IMFSample, String> {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)
                .map_err(|e| format!("dxgi surface buffer: {e}"))?;
            let sample = MFCreateSample().map_err(|e| format!("input sample: {e}"))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|e| format!("attaching input buffer: {e}"))?;
            let time = (self.frames_submitted as i64) * 10_000_000 / i64::from(self.fps);
            let duration = 10_000_000 / i64::from(self.fps);
            sample
                .SetSampleTime(time)
                .map_err(|e| format!("sample time: {e}"))?;
            sample
                .SetSampleDuration(duration)
                .map_err(|e| format!("sample duration: {e}"))?;
            Ok(sample)
        }

        fn encode_async(
            &mut self,
            transform: &IMFTransform,
            events: &IMFMediaEventGenerator,
            sample: IMFSample,
            units: &mut Vec<AccessUnit>,
        ) -> Result<(), String> {
            let deadline = Instant::now() + NEED_INPUT_TIMEOUT;
            loop {
                if self.need_input_credits > 0 {
                    self.need_input_credits -= 1;
                    unsafe {
                        transform
                            .ProcessInput(self.input_stream_id, &sample, 0)
                            .map_err(|e| format!("ProcessInput: {e}"))?;
                    }
                    break;
                }
                if !self.poll(events, transform, units)? && Instant::now() >= deadline {
                    return Err("timed out waiting for the hardware encoder".into());
                }
                if self.need_input_credits == 0 {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            // Drain output that is already queued. A pipelined encoder publishes
            // this picture on a later call; sleeping for it costs the frame budget.
            while self.poll(events, transform, units)? {}
            Ok(())
        }

        fn encode_sync(
            &mut self,
            transform: &IMFTransform,
            sample: IMFSample,
            units: &mut Vec<AccessUnit>,
        ) -> Result<(), String> {
            unsafe {
                transform
                    .ProcessInput(self.input_stream_id, &sample, 0)
                    .map_err(|e| format!("ProcessInput: {e}"))?;
            }
            while self.pull_output(transform, units)? {}
            Ok(())
        }

        /// Handles one queued event. Returns false when the queue is empty.
        fn poll(
            &mut self,
            events: &IMFMediaEventGenerator,
            transform: &IMFTransform,
            units: &mut Vec<AccessUnit>,
        ) -> Result<bool, String> {
            let event = unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) };
            let event = match event {
                Ok(event) => event,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(false),
                Err(e) => return Err(format!("encoder event: {e}")),
            };
            let kind =
                unsafe { event.GetType() }.map_err(|e| format!("encoder event type: {e}"))?;
            if kind == METransformNeedInput.0 as u32 {
                self.need_input_credits += 1;
            } else if kind == METransformHaveOutput.0 as u32 {
                self.pull_output(transform, units)?;
            } else if kind == MEError.0 as u32 {
                return Err("hardware encoder reported an error".into());
            }
            Ok(true)
        }

        /// Pulls one output picture. Returns false when the encoder wants more input.
        fn pull_output(
            &mut self,
            transform: &IMFTransform,
            units: &mut Vec<AccessUnit>,
        ) -> Result<bool, String> {
            unsafe {
                loop {
                    let provided = if self.output_provides_samples {
                        None
                    } else {
                        Some(self.output_sample()?)
                    };
                    let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                        dwStreamID: self.output_stream_id,
                        pSample: ManuallyDrop::new(provided),
                        dwStatus: 0,
                        pEvents: ManuallyDrop::new(None),
                    }];
                    let mut status = 0u32;
                    let result = transform.ProcessOutput(0, &mut buffers, &mut status);
                    let sample = ManuallyDrop::take(&mut buffers[0].pSample);
                    let _events = ManuallyDrop::take(&mut buffers[0].pEvents);
                    match result {
                        Ok(()) => {
                            if let Some(sample) = sample
                                && let Some(unit) = read_sample(&sample)?
                            {
                                units.push(unit);
                            }
                            return Ok(true);
                        }
                        Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                        Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                            let new_type = transform
                                .GetOutputAvailableType(self.output_stream_id, 0)
                                .map_err(|err| format!("output type after stream change: {err}"))?;
                            transform
                                .SetOutputType(self.output_stream_id, &new_type, 0)
                                .map_err(|err| format!("resetting output type: {err}"))?;
                            if let Ok(info) = transform.GetOutputStreamInfo(self.output_stream_id) {
                                self.output_provides_samples = info.dwFlags
                                    & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                                        | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                                    != 0;
                                self.output_buffer_size = info.cbSize;
                            }
                        }
                        Err(e) => return Err(format!("ProcessOutput: {e}")),
                    }
                }
            }
        }

        unsafe fn output_sample(&self) -> Result<IMFSample, String> {
            let size = if self.output_buffer_size > 0 {
                self.output_buffer_size
            } else {
                self.width * self.height * 3 / 2
            };
            let buffer = MFCreateMemoryBuffer(size).map_err(|e| format!("output buffer: {e}"))?;
            let sample = MFCreateSample().map_err(|e| format!("output sample: {e}"))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|e| format!("attaching output buffer: {e}"))?;
            Ok(sample)
        }
    }

    impl Drop for HwEncoder {
        fn drop(&mut self) {
            unsafe {
                let _ = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
                let _ = self.transform.ProcessMessage(
                    MFT_MESSAGE_NOTIFY_END_OF_STREAM,
                    self.input_stream_id as usize,
                );
                let _ = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
                let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                // The MFT drops its manager reference above. These stay alive until then.
                let _ = (self.device.as_raw(), self.device_manager.as_raw());
            }
        }
    }

    /// Direct3D device on the GPU that owns one hardware encoder.
    struct Gpu {
        device: ID3D11Device,
        manager: IMFDXGIDeviceManager,
        adapter: String,
    }

    unsafe fn open_activated(
        activate: &IMFActivate,
        name: &str,
        width: u32,
        height: u32,
        fps: u32,
        params: EncodeParams,
    ) -> Result<HwEncoder, String> {
        let gpu = open_gpu(encoder_luid(activate))?;
        let transform: IMFTransform = activate
            .ActivateObject()
            .map_err(|e| format!("activating {name}: {e}"))?;
        let opened = bind(transform.clone(), gpu, name, width, height, fps, params);
        if opened.is_err() {
            let _ = activate.ShutdownObject();
        }
        opened
    }

    unsafe fn bind(
        transform: IMFTransform,
        gpu: Gpu,
        name: &str,
        width: u32,
        height: u32,
        fps: u32,
        params: EncodeParams,
    ) -> Result<HwEncoder, String> {
        let mut is_async = false;
        if let Ok(attrs) = transform.GetAttributes() {
            is_async = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 1;
            if is_async {
                attrs
                    .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                    .map_err(|e| format!("unlocking {name}: {e}"))?;
            }
        }

        // Without a device on this adapter, SetOutputType returns
        // MF_E_UNSUPPORTED_D3D_TYPE (0xC00D6D76).
        transform
            .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, gpu.manager.as_raw() as usize)
            .map_err(|e| format!("D3D manager on {name}: {e}"))?;

        let mut input_ids = [0u32; 1];
        let mut output_ids = [0u32; 1];
        let (input_stream_id, output_stream_id) =
            match transform.GetStreamIDs(&mut input_ids, &mut output_ids) {
                Ok(()) => (input_ids[0], output_ids[0]),
                Err(_) => (0, 0),
            };

        let bitrate = params.bitrate;
        let codec_api: Option<ICodecAPI> = transform.cast().ok();

        // Set the media types first. Media Foundation drops codec properties
        // that were written before SetOutputType.
        set_output_type(
            &transform,
            output_stream_id,
            width,
            height,
            fps,
            bitrate,
            params.high_quality,
        )?;
        set_input_type(&transform, input_stream_id, width, height, fps)?;
        if let Some(api) = &codec_api {
            configure_codec(api, fps, bitrate, params);
        }

        let info = transform
            .GetOutputStreamInfo(output_stream_id)
            .map_err(|e| format!("output stream info: {e}"))?;
        let provides = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
            != 0;
        let event_gen = if is_async {
            Some(
                transform
                    .cast::<IMFMediaEventGenerator>()
                    .map_err(|e| format!("{name} has no event generator: {e}"))?,
            )
        } else {
            None
        };
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .and_then(|()| transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0))
            .map_err(|e| format!("starting {name}: {e}"))?;

        tracing::info!(
            encoder = %name,
            adapter = %gpu.adapter,
            width,
            height,
            fps,
            bitrate,
            high_quality = params.high_quality,
            game = params.game,
            async_mft = is_async,
            "hardware h264 encoder"
        );

        Ok(HwEncoder {
            width,
            height,
            fps,
            transform,
            device: gpu.device,
            device_manager: gpu.manager,
            event_gen,
            codec_api,
            output_provides_samples: provides,
            output_buffer_size: info.cbSize,
            input_stream_id,
            output_stream_id,
            need_input_credits: 0,
            frames_submitted: 0,
            inputs: Vec::new(),
        })
    }

    unsafe fn encoder_luid(activate: &IMFActivate) -> Option<u64> {
        activate.GetUINT64(&MFT_ENUM_ADAPTER_LUID).ok()
    }

    unsafe fn open_gpu(luid: Option<u64>) -> Result<Gpu, String> {
        let adapter = find_adapter(luid)?;
        let desc = adapter
            .GetDesc1()
            .map_err(|e| format!("adapter desc: {e}"))?;
        let adapter_name = adapter_name(&desc);
        let device = create_device(&adapter, &adapter_name)?;
        if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
            let _ = multithread.SetMultithreadProtected(true);
        }
        let dxgi = device
            .cast::<IDXGIDevice>()
            .map_err(|e| format!("IDXGIDevice for {adapter_name}: {e}"))?;
        let mut token = 0u32;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)
            .map_err(|e| format!("DXGI device manager: {e}"))?;
        let manager = manager.ok_or_else(|| "DXGI device manager was not created".to_owned())?;
        manager
            .ResetDevice(&dxgi, token)
            .map_err(|e| format!("binding {adapter_name} to the encoder: {e}"))?;
        Ok(Gpu {
            device,
            manager,
            adapter: adapter_name,
        })
    }

    unsafe fn find_adapter(luid: Option<u64>) -> Result<IDXGIAdapter1, String> {
        let factory: IDXGIFactory1 =
            CreateDXGIFactory1().map_err(|e| format!("DXGI factory: {e}"))?;
        let mut best: Option<(IDXGIAdapter1, usize)> = None;
        let mut index = 0u32;
        loop {
            let adapter = match factory.EnumAdapters1(index) {
                Ok(adapter) => adapter,
                Err(_) => break,
            };
            index += 1;
            let Ok(desc) = adapter.GetDesc1() else {
                continue;
            };
            let packed = u64::from(desc.AdapterLuid.LowPart)
                | (u64::from(desc.AdapterLuid.HighPart as u32) << 32);
            if let Some(want) = luid {
                if packed == want {
                    return Ok(adapter);
                }
                continue;
            }
            if desc.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
                continue;
            }
            let memory = desc.DedicatedVideoMemory;
            if best.as_ref().is_none_or(|(_, best_mem)| memory > *best_mem) {
                best = Some((adapter, memory));
            }
        }
        match (luid, best) {
            (_, Some((adapter, _))) => Ok(adapter),
            (Some(_), None) => Err("encoder GPU is not in the DXGI adapter list".into()),
            (None, None) => Err("no hardware GPU adapter".into()),
        }
    }

    unsafe fn create_device(adapter: &IDXGIAdapter1, name: &str) -> Result<ID3D11Device, String> {
        let levels = [
            D3D_FEATURE_LEVEL_11_1,
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
        ];
        let flags = [
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        ];
        let mut last = String::from("D3D11CreateDevice was not called");
        for flag in flags {
            let mut device = None;
            match D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE(ptr::null_mut()),
                flag,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            ) {
                Ok(()) => {
                    if let Some(device) = device {
                        return Ok(device);
                    }
                    last = format!("D3D11CreateDevice returned no device for {name}");
                }
                Err(error) => last = format!("D3D11 device on {name}: {error}"),
            }
        }
        Err(last)
    }

    fn adapter_name(desc: &DXGI_ADAPTER_DESC1) -> String {
        let end = desc
            .Description
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(desc.Description.len());
        String::from_utf16_lossy(&desc.Description[..end])
    }

    fn ensure_mf() -> Result<(), String> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
        STARTED
            .get_or_init(|| unsafe {
                MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| format!("MFStartup: {e}"))
            })
            .clone()
    }

    unsafe fn enumerate() -> windows::core::Result<Vec<IMFActivate>> {
        let output_type = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let mut activates: *mut Option<IMFActivate> = ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0),
            None,
            Some(&output_type),
            &mut activates,
            &mut count,
        )?;
        let mut out = Vec::with_capacity(count as usize);
        if !activates.is_null() {
            for i in 0..count as usize {
                if let Some(activate) = (*activates.add(i)).take() {
                    out.push(activate);
                }
            }
            CoTaskMemFree(Some(activates.cast()));
        }
        Ok(out)
    }

    unsafe fn friendly_name(activate: &IMFActivate) -> Option<String> {
        let mut value = windows::core::PWSTR::null();
        let mut len = 0u32;
        activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut value, &mut len)
            .ok()?;
        if value.is_null() {
            return None;
        }
        let name = value.to_string().ok();
        CoTaskMemFree(Some(value.as_ptr().cast()));
        name
    }

    unsafe fn set_output_type(
        transform: &IMFTransform,
        stream: u32,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        high_quality: bool,
    ) -> Result<(), String> {
        // High profile is the universal fallback in the streaming spec: every
        // viewer decodes H.264, and High is the best of those. Drivers that
        // reject it fall through to Constrained Baseline, which OpenH264 still reads.
        let profiles: &[u32] = if high_quality {
            &[
                eAVEncH264VProfile_High.0 as u32,
                eAVEncH264VProfile_ConstrainedBase.0 as u32,
                eAVEncH264VProfile_Base.0 as u32,
            ]
        } else {
            &[
                eAVEncH264VProfile_ConstrainedBase.0 as u32,
                eAVEncH264VProfile_Base.0 as u32,
            ]
        };
        let mut last = String::from("no output type");
        for &profile in profiles {
            // BT.709 colour first so the encoder writes it into the VUI. A
            // driver that rejects the colour attributes gets the plain type.
            for bt709 in [true, false] {
                let output = match h264_output_type(width, height, fps, bitrate, profile, bt709) {
                    Ok(output) => output,
                    Err(e) => {
                        last = e;
                        continue;
                    }
                };
                match transform.SetOutputType(stream, &output, 0) {
                    Ok(()) => return Ok(()),
                    Err(e) => last = e.to_string(),
                }
            }
        }
        Err(format!("setting h264 output type: {last}"))
    }

    unsafe fn h264_output_type(
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        profile: u32,
        bt709: bool,
    ) -> Result<windows::Win32::Media::MediaFoundation::IMFMediaType, String> {
        let output = MFCreateMediaType().map_err(|e| format!("output media type: {e}"))?;
        output
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .and_then(|()| output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264))
            .and_then(|()| output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate))
            .and_then(|()| output.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height)))
            .and_then(|()| output.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)))
            .and_then(|()| output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)))
            .and_then(|()| {
                output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            })
            .and_then(|()| output.SetUINT32(&MF_MT_MPEG2_PROFILE, profile))
            .and_then(|()| {
                output.SetUINT32(
                    &MF_MT_MPEG2_LEVEL,
                    super::h264_level_idc(width, height, fps),
                )
            })
            .and_then(|()| if bt709 { set_bt709(&output) } else { Ok(()) })
            .map_err(|e| format!("building h264 output type: {e}"))?;
        Ok(output)
    }

    unsafe fn set_input_type(
        transform: &IMFTransform,
        stream: u32,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<(), String> {
        let mut last = String::from("no input type");
        // Tagged BT.709 limited range first, then the plain type for drivers
        // that reject the colour attributes.
        for bt709 in [true, false] {
            let input = MFCreateMediaType().map_err(|e| format!("input media type: {e}"))?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .and_then(|()| input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12))
                .and_then(|()| input.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height)))
                .and_then(|()| input.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)))
                .and_then(|()| {
                    input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                })
                .and_then(|()| input.SetUINT32(&MF_MT_DEFAULT_STRIDE, width))
                .and_then(|()| if bt709 { set_bt709(&input) } else { Ok(()) })
                .map_err(|e| format!("building nv12 input type: {e}"))?;
            match transform.SetInputType(stream, &input, 0) {
                Ok(()) => return Ok(()),
                Err(e) => last = e.to_string(),
            }
        }
        Err(format!("setting nv12 input type: {last}"))
    }

    /// BT.709 primaries, transfer, and matrix at limited (16-235) range.
    unsafe fn set_bt709(
        media_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
    ) -> windows::core::Result<()> {
        media_type
            .SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)
            .and_then(|()| {
                media_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)
            })
            .and_then(|()| {
                media_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)
            })
            .and_then(|()| {
                media_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)
            })
    }

    unsafe fn read_sample(sample: &IMFSample) -> Result<Option<AccessUnit>, String> {
        let buffer = sample
            .ConvertToContiguousBuffer()
            .map_err(|e| format!("output buffer: {e}"))?;
        let mut data_ptr: *mut u8 = ptr::null_mut();
        let mut current_len = 0u32;
        buffer
            .Lock(&mut data_ptr, None, Some(&mut current_len))
            .map_err(|e| format!("locking output: {e}"))?;
        let bytes = if data_ptr.is_null() || current_len == 0 {
            &[][..]
        } else {
            std::slice::from_raw_parts(data_ptr, current_len as usize)
        };
        let data = to_annex_b(bytes);
        let clean = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1;
        buffer
            .Unlock()
            .map_err(|e| format!("unlocking output: {e}"))?;
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(AccessUnit {
            keyframe: clean || contains_idr(&data),
            data,
        }))
    }

    fn pack(high: u32, low: u32) -> u64 {
        (u64::from(high) << 32) | u64::from(low)
    }

    fn variant_u32(value: u32) -> VARIANT {
        let mut var = VARIANT::default();
        var.Anonymous.Anonymous = ManuallyDrop::new(VARIANT_0_0 {
            vt: VT_UI4,
            wReserved1: 0,
            wReserved2: 0,
            wReserved3: 0,
            Anonymous: VARIANT_0_0_0 { ulVal: value },
        });
        var
    }

    fn variant_bool(value: bool) -> VARIANT {
        let mut var = VARIANT::default();
        var.Anonymous.Anonymous = ManuallyDrop::new(VARIANT_0_0 {
            vt: VT_BOOL,
            wReserved1: 0,
            wReserved2: 0,
            wReserved3: 0,
            Anonymous: VARIANT_0_0_0 {
                boolVal: if value {
                    VARIANT_TRUE
                } else {
                    windows::Win32::Foundation::VARIANT_FALSE
                },
            },
        });
        var
    }

    /// Rate control, content mode, and the high-quality VBV. Called after both
    /// media types are set, which is when the encoder keeps the values.
    fn configure_codec(api: &ICodecAPI, _fps: u32, bitrate: u32, params: EncodeParams) {
        set_codec(
            api,
            &CODECAPI_AVEncCommonRateControlMode,
            variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
        );
        set_codec(api, &CODECAPI_AVEncCommonMeanBitRate, variant_u32(bitrate));
        set_codec(api, &CODECAPI_AVLowLatencyMode, variant_bool(true));
        set_codec(api, &CODECAPI_AVEncMPVDefaultBPictureCount, variant_u32(0));
        set_codec(
            api,
            &CODECAPI_AVEncMPVGOPSize,
            variant_u32(params.gop_frames.max(1)),
        );
        // Display remoting keeps text sharp. Live streaming spends bits on motion.
        let scenario = if params.game {
            eAVScenarioInfo_LiveStreaming
        } else {
            eAVScenarioInfo_DisplayRemoting
        };
        set_codec(
            api,
            &CODECAPI_AVScenarioInfo,
            variant_u32(scenario.0 as u32),
        );
        if params.high_quality {
            // VBV holds the rate-control window. The property is in bits.
            let buffer_bits =
                u64::from(bitrate).saturating_mul(u64::from(params.rc_window_ms.max(1))) / 1_000;
            let buffer_bits = u32::try_from(buffer_bits).unwrap_or(u32::MAX).max(1);
            set_codec(api, &CODECAPI_AVEncCommonMaxBitRate, variant_u32(bitrate));
            set_codec(
                api,
                &CODECAPI_AVEncCommonBufferSize,
                variant_u32(buffer_bits),
            );
            // One reference for game motion, two for desktop text.
            set_codec(
                api,
                &CODECAPI_AVEncVideoMaxNumRefFrame,
                variant_u32(if params.game { 1 } else { 2 }),
            );
            set_codec(api, &CODECAPI_AVEncCommonLowLatency, variant_bool(true));
            // 0 is slowest, 100 is fastest. Game mode stays near the fast end so
            // 1440p60 fits in a frame; desktop mode is a step slower for text.
            set_codec(
                api,
                &CODECAPI_AVEncCommonQualityVsSpeed,
                variant_u32(params.speed),
            );
        }
    }

    fn set_codec(api: &ICodecAPI, guid: &windows::core::GUID, value: VARIANT) {
        unsafe {
            let _ = api.SetValue(guid, &value);
        }
    }
}

#[cfg(target_os = "linux")]
#[path = "hw_linux.rs"]
mod platform;

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use super::AccessUnit;

    /// Stub. Hardware encode is Media Foundation on Windows and VA-API on Linux.
    pub struct HwEncoder;

    impl HwEncoder {
        /// Hardware encode is not available on this platform.
        pub fn open(
            _width: u32,
            _height: u32,
            _fps: u32,
            _params: super::EncodeParams,
        ) -> Result<Self, String> {
            Err("no hardware h264 encoder".into())
        }

        /// Hardware encode is not available on this platform.
        pub fn encode(
            &mut self,
            _rgba: &[u8],
            _force_idr: bool,
        ) -> Result<Vec<AccessUnit>, String> {
            Err("no hardware h264 encoder".into())
        }
    }
}

pub use platform::HwEncoder;

#[cfg(test)]
mod tests {
    use super::*;

    /// BT.709 limited range from the defining equations, rounded.
    fn bt709_reference(r: u8, g: u8, b: u8) -> [u8; 3] {
        let (r, g, b) = (f64::from(r), f64::from(g), f64::from(b));
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let luma = 16.0 + 219.0 * y / 255.0;
        let cb = 128.0 + 224.0 * (b - y) / (1.8556 * 255.0);
        let cr = 128.0 + 224.0 * (r - y) / (1.5748 * 255.0);
        [luma.round() as u8, cb.round() as u8, cr.round() as u8]
    }

    fn convert_solid(rgb: [u8; 3]) -> ([u8; 3], [u8; 3]) {
        let rgba: Vec<u8> = (0..4).flat_map(|_| [rgb[0], rgb[1], rgb[2], 255]).collect();
        let mut y = [0u8; 4];
        let mut uv = [0u8; 2];
        rgba_to_nv12(&rgba, 2, 2, &mut y, 2, &mut uv, 2);
        assert!(y.iter().all(|&v| v == y[0]));
        let (mut y2, mut u, mut v) = ([0u8; 4], [0u8; 1], [0u8; 1]);
        rgba_to_i420(&rgba, 2, 2, &mut y2, &mut u, &mut v);
        assert_eq!(y, y2);
        ([y[0], uv[0], uv[1]], [y2[0], u[0], v[0]])
    }

    #[test]
    fn rgba_converts_with_bt709_limited_range() {
        assert_eq!(convert_solid([255, 255, 255]).0, [235, 128, 128]);
        assert_eq!(convert_solid([0, 0, 0]).0, [16, 128, 128]);
        assert_eq!(convert_solid([128, 128, 128]).0, [126, 128, 128]);
        for rgb in [
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [255, 255, 255],
            [0, 0, 0],
        ] {
            let (nv12, i420) = convert_solid(rgb);
            assert_eq!(nv12, i420);
            let reference = bt709_reference(rgb[0], rgb[1], rgb[2]);
            for (got, want) in nv12.iter().zip(reference) {
                assert!(
                    got.abs_diff(want) <= 1,
                    "{rgb:?}: got {nv12:?}, reference {reference:?}"
                );
            }
        }
        // Pure blue and red reach the top of the chroma range, not past it.
        assert_eq!(convert_solid([0, 0, 255]).0[1], 240);
        assert_eq!(convert_solid([255, 0, 0]).0[2], 240);
    }

    /// The scalar conversion the optimized one must match exactly: luma per
    /// pixel, chroma from the truncated 2×2 average, all in `i32`.
    fn reference_nv12(src: &[u8], w: usize, h: usize) -> (Vec<u8>, Vec<u8>) {
        let luma = |p: &[u8; 4]| {
            let y =
                (47 * i32::from(p[0]) + 157 * i32::from(p[1]) + 16 * i32::from(p[2]) + 128) >> 8;
            (y + 16).clamp(0, 255) as u8
        };
        let chroma = |c: [i32; 3], rgb: [i32; 3]| {
            let v = (c[0] * rgb[0] + c[1] * rgb[1] + c[2] * rgb[2] + 128) >> 8;
            (v + 128).clamp(0, 255) as u8
        };
        let y: Vec<u8> = src.as_chunks::<4>().0.iter().map(luma).collect();
        let mut uv = Vec::with_capacity(w * h / 2);
        for row in (0..h).step_by(2) {
            for col in (0..w).step_by(2) {
                let mut sum = [0i32; 3];
                for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                    let i = ((row + dy) * w + col + dx) * 4;
                    for (s, &v) in sum.iter_mut().zip(&src[i..i + 3]) {
                        *s += i32::from(v);
                    }
                }
                let rgb = sum.map(|v| v / 4);
                uv.push(chroma([-26, -86, 112], rgb));
                uv.push(chroma([112, -102, -10], rgb));
            }
        }
        (y, uv)
    }

    fn test_picture(w: usize, h: usize) -> Vec<u8> {
        (0..w * h * 4)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect()
    }

    #[test]
    fn threaded_conversion_matches_the_scalar_reference() {
        let (w, h) = (642usize, 362usize);
        let rgba = test_picture(w, h);
        let (want_y, want_uv) = reference_nv12(&rgba, w, h);
        for threads in [1, 3, 4] {
            let mut y = vec![0u8; w * h];
            let mut uv = vec![0u8; w * h / 2];
            let planes = Planes {
                y: &mut y,
                y_stride: w,
                chroma: Chroma::Nv12(&mut uv),
                chroma_stride: w,
            };
            rgba_to_yuv420(&rgba, w, h, planes, threads);
            assert!(y == want_y, "nv12 luma differs with {threads} threads");
            assert!(uv == want_uv, "nv12 chroma differs with {threads} threads");

            let (mut u, mut v) = (vec![0u8; w * h / 4], vec![0u8; w * h / 4]);
            let planes = Planes {
                y: &mut y,
                y_stride: w,
                chroma: Chroma::I420(&mut u, &mut v),
                chroma_stride: w / 2,
            };
            rgba_to_yuv420(&rgba, w, h, planes, threads);
            assert!(y == want_y, "i420 luma differs with {threads} threads");
            let interleaved: Vec<u8> = u.iter().zip(&v).flat_map(|(&cb, &cr)| [cb, cr]).collect();
            assert!(
                interleaved == want_uv,
                "i420 chroma differs with {threads} threads"
            );
        }
    }

    #[test]
    fn strided_nv12_fills_the_coded_padding() {
        let (w, h, cw, ch) = (6usize, 4usize, 16usize, 16usize);
        let (y_stride, uv_stride) = (20usize, 24usize);
        let rgba = test_picture(w, h);
        let (want_y, want_uv) = reference_nv12(&rgba, w, h);
        // The last row of each plane ends at the coded width, like a mapped VA image.
        let mut y = vec![0xAAu8; (ch - 1) * y_stride + cw];
        let mut uv = vec![0xAAu8; (ch / 2 - 1) * uv_stride + cw];
        rgba_to_nv12(&rgba, w, h, &mut y, y_stride, &mut uv, uv_stride);
        fill_nv12_padding((w, h), (cw, ch), &mut y, y_stride, &mut uv, uv_stride);
        for row in 0..ch {
            let line = &y[row * y_stride..row * y_stride + cw];
            if row < h {
                assert_eq!(&line[..w], &want_y[row * w..(row + 1) * w]);
                assert!(line[w..].iter().all(|&v| v == 16), "y row {row}");
            } else {
                assert!(line.iter().all(|&v| v == 16), "y row {row}");
            }
            if row < h - 1 {
                assert_eq!(y[row * y_stride + cw], 0xAA, "y stride gap was written");
            }
        }
        for row in 0..ch / 2 {
            let line = &uv[row * uv_stride..row * uv_stride + cw];
            if row < h / 2 {
                assert_eq!(&line[..w], &want_uv[row * w..(row + 1) * w]);
                assert!(line[w..].iter().all(|&v| v == 128), "uv row {row}");
            } else {
                assert!(line.iter().all(|&v| v == 128), "uv row {row}");
            }
        }
    }

    /// `cargo test -p chattercrab-media rgba_conversion_speed -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark"]
    fn rgba_conversion_speed() {
        let (w, h) = (2560usize, 1440usize);
        let rgba = test_picture(w, h);
        let mut y = vec![0u8; w * h];
        let mut uv = vec![0u8; w * h / 2];
        let runs = 20;
        for threads in [1, convert_threads(w, h)] {
            let started = std::time::Instant::now();
            for _ in 0..runs {
                let planes = Planes {
                    y: &mut y,
                    y_stride: w,
                    chroma: Chroma::Nv12(&mut uv),
                    chroma_stride: w,
                };
                rgba_to_yuv420(&rgba, w, h, planes, threads);
            }
            let ms = started.elapsed().as_secs_f64() * 1000.0 / f64::from(runs);
            println!("rgba to nv12 2560x1440, {threads} thread(s): {ms:.2} ms/frame");
        }
    }

    #[test]
    fn annex_b_is_unchanged_and_avcc_gains_start_codes() {
        let annex = [0u8, 0, 0, 1, 0x65, 0x88];
        assert_eq!(to_annex_b(&annex), annex);
        let avcc = [0u8, 0, 0, 2, 0x65, 0x88];
        assert_eq!(to_annex_b(&avcc), vec![0, 0, 0, 1, 0x65, 0x88]);
    }

    #[test]
    fn bitrate_is_12_mbps_at_1440p60_and_floored() {
        assert_eq!(hardware_bitrate(2560, 1440, 60), 12_000_000);
        assert_eq!(hardware_bitrate(1920, 1080, 30), 4_000_000);
        assert_eq!(hardware_bitrate(3840, 2160, 60), 20_000_000);
    }

    #[test]
    fn level_stays_5_1_until_4k60() {
        assert_eq!(h264_level_idc(2560, 1440, 60), 51);
        assert_eq!(h264_level_idc(1920, 1080, 60), 51);
        assert_eq!(h264_level_idc(3840, 2160, 30), 51);
        assert_eq!(h264_level_idc(3840, 2160, 60), 52);
        assert_eq!(h264_level_idc(2560, 1440, 120), 52);
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn hardware_encoder_roundtrips_through_openh264_when_present() {
        let mut encoder = match HwEncoder::open(
            320,
            240,
            30,
            EncodeParams {
                bitrate: hardware_bitrate(320, 240, 30),
                gop_frames: 60,
                high_quality: false,
                game: false,
                rc_window_ms: 1_000,
                speed: 100,
            },
        ) {
            Ok(encoder) => encoder,
            Err(error) => {
                eprintln!("hardware encoder unavailable: {error}");
                return;
            }
        };
        let mut decoder = crate::screen::receive::new_decoder().expect("decoder");
        let mut rgba = vec![0u8; 320 * 240 * 4];
        for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            px[0] = (i % 255) as u8;
            px[1] = 40;
            px[2] = 180;
            px[3] = 255;
        }
        let mut decoded = false;
        for n in 0..8 {
            rgba[0] = n as u8;
            let units = encoder.encode(&rgba, n == 0).expect("encode");
            for unit in units {
                assert!(
                    unit.data.starts_with(&[0, 0, 0, 1]) || unit.data.starts_with(&[0, 0, 1]),
                    "hardware encoder did not emit annex-b"
                );
                if let Some(frame) =
                    crate::screen::receive::decode_frame(&mut decoder, &unit.data).expect("decode")
                {
                    assert_eq!((frame.width, frame.height), (320, 240));
                    decoded = true;
                }
            }
            if decoded {
                return;
            }
        }
        panic!("hardware encoder output did not decode");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn high_quality_hardware_encoder_uses_high_profile_when_present() {
        let params = crate::screen::encode_params(true, 320, 240, 60, true, false);
        let mut encoder = match HwEncoder::open(320, 240, 60, params) {
            Ok(encoder) => encoder,
            Err(error) => {
                eprintln!("hardware encoder unavailable: {error}");
                return;
            }
        };
        let mut rgba = vec![0u8; 320 * 240 * 4];
        for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            px[0] = (i % 255) as u8;
            px[1] = 16;
            px[2] = 200;
            px[3] = 255;
        }
        let expect_high = encoder.high_profile_selected();
        let units = encoder.encode(&rgba, true).expect("encode");
        let mut saw_idr = false;
        let mut decoded = false;
        let mut decoder = crate::screen::receive::new_decoder().expect("decoder");
        for unit in units {
            if unit.keyframe {
                let profile = sps_profile_idc(&unit.data);
                if expect_high {
                    assert_eq!(
                        profile,
                        Some(100),
                        "high quality VA-API output should be H.264 High"
                    );
                } else {
                    assert!(
                        profile == Some(66) || profile == Some(100),
                        "unexpected fallback profile {profile:?}"
                    );
                }
                saw_idr = true;
            }
            if let Some(frame) =
                crate::screen::receive::decode_frame(&mut decoder, &unit.data).expect("decode")
            {
                assert_eq!((frame.width, frame.height), (320, 240));
                decoded = true;
            }
        }
        assert!(saw_idr, "high quality encode did not emit an IDR");
        assert!(decoded, "high quality hardware output did not decode");
    }

    #[cfg(target_os = "linux")]
    fn sps_profile_idc(au: &[u8]) -> Option<u8> {
        let mut i = 0;
        while i + 4 < au.len() {
            let (header, next) = if au[i..].starts_with(&[0, 0, 0, 1]) {
                (i + 4, i + 5)
            } else if au[i..].starts_with(&[0, 0, 1]) {
                (i + 3, i + 4)
            } else {
                i += 1;
                continue;
            };
            if au[header] & 0x1f == 7 {
                return au.get(next).copied();
            }
            i = next;
        }
        None
    }
}
