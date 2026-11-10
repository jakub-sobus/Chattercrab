//! Windows Graphics Capture on the encoder's Direct3D device.
//!
//! The desktop stays in GPU memory: a BGRA texture is scaled and converted to
//! NV12 by the video processor, then handed to the Media Foundation encoder as
//! a DXGI surface. No full-frame CPU copy on the way in.

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{FALSE, HWND, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::Win32::UI::WindowsAndMessaging::EnumWindows;
use windows::core::{Interface, factory};

use super::CaptureWorker;
use crate::ScreenSourceKind;

pub(super) fn try_run(mut worker: CaptureWorker) -> Result<(), Box<CaptureWorker>> {
    let Some(device) = worker.encoder.d3d_device() else {
        return Err(Box::new(worker));
    };
    let mut session = match GpuSession::open(&device, &worker) {
        Ok(session) => session,
        Err(error) => {
            tracing::warn!(%error, "gpu capture unavailable, using screenshots");
            return Err(Box::new(worker));
        }
    };
    let (captured_w, captured_h) = session.captured;
    let (encode_w, encode_h) = worker.encoded;
    if let Err(error) = session.prepare(encode_w, encode_h) {
        tracing::warn!(%error, "gpu color conversion unavailable, using screenshots");
        return Err(Box::new(worker));
    }
    let Some(nv12) = session.nv12.clone() else {
        return Err(Box::new(worker));
    };
    let period = std::time::Duration::from_secs_f64(1.0 / f64::from(worker.fps.max(1)));
    match worker.encoder.encode_dxgi(&nv12, true) {
        Ok(units) => {
            if !worker.send_units(units, period) {
                return Ok(());
            }
        }
        Err(error) => {
            tracing::warn!(%error, "gpu encode rejected the surface, using screenshots");
            worker
                .keyframe
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return Err(Box::new(worker));
        }
    }
    tracing::info!(
        captured_w,
        captured_h,
        encode_w,
        encode_h,
        "gpu screen capture"
    );
    worker.gpu_loop(session);
    Ok(())
}

impl CaptureWorker {
    fn gpu_loop(&mut self, mut session: GpuSession) {
        let period = Duration::from_secs_f64(1.0 / f64::from(self.fps.max(1)));
        let mut next = Instant::now();
        while !self.stop.load(std::sync::atomic::Ordering::Acquire) {
            session.pull_latest();
            if session.bgra.is_some() {
                if self.tx.capacity() == 0 {
                    self.note_drop();
                } else if let Err(error) = session.convert() {
                    tracing::warn!(%error, "gpu color conversion");
                } else {
                    let Some(nv12) = session.nv12.clone() else {
                        continue;
                    };
                    let force_idr = self
                        .keyframe
                        .swap(false, std::sync::atomic::Ordering::Relaxed);
                    let started = Instant::now();
                    match self.encoder.encode_dxgi(&nv12, force_idr) {
                        Ok(units) => {
                            let took = started.elapsed();
                            self.note_encode(took, period);
                            if !self.send_units(units, period) {
                                return;
                            }
                        }
                        Err(error) => tracing::warn!(%error, "gpu h264 encode"),
                    }
                }
            }
            next += period;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
        }
    }
}

struct GpuSession {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    pool: Direct3D11CaptureFramePool,
    session: windows::Graphics::Capture::GraphicsCaptureSession,
    bgra: Option<ID3D11Texture2D>,
    nv12: Option<ID3D11Texture2D>,
    captured: (u32, u32),
    processor: Option<ID3D11VideoProcessor>,
    enumerator: Option<ID3D11VideoProcessorEnumerator>,
    input_view: Option<ID3D11VideoProcessorInputView>,
    output_view: Option<ID3D11VideoProcessorOutputView>,
    video_context: Option<ID3D11VideoContext>,
    frame_index: u32,
}

impl GpuSession {
    fn open(device: &ID3D11Device, worker: &CaptureWorker) -> Result<Self, String> {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
        let item = capture_item(&worker.source)?;
        let size = item.Size().map_err(|error| error.to_string())?;
        let winrt_device = winrt_device(device)?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .map_err(|error| error.to_string())?;
        let session = pool
            .CreateCaptureSession(&item)
            .map_err(|error| error.to_string())?;
        if let Err(error) = session.SetIsBorderRequired(false) {
            tracing::debug!(%error, "capture border could not be hidden");
        }
        if let Err(error) = session.SetIsCursorCaptureEnabled(true) {
            tracing::debug!(%error, "cursor capture was left at the default");
        }
        session.StartCapture().map_err(|error| error.to_string())?;
        let context = unsafe { device.GetImmediateContext() }.map_err(|error| error.to_string())?;
        let mut gpu = Self {
            device: device.clone(),
            context,
            pool,
            session,
            bgra: None,
            nv12: None,
            captured: (size.Width.unsigned_abs(), size.Height.unsigned_abs()),
            processor: None,
            enumerator: None,
            input_view: None,
            output_view: None,
            video_context: None,
            frame_index: 0,
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while gpu.bgra.is_none() && Instant::now() < deadline {
            gpu.pull_latest();
            if gpu.bgra.is_none() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        if gpu.bgra.is_none() {
            return Err("gpu capture produced no frame".into());
        }
        Ok(gpu)
    }

    fn pull_latest(&mut self) {
        loop {
            let frame = match self.pool.TryGetNextFrame() {
                Ok(frame) => frame,
                Err(_) => break,
            };
            if let Err(error) = self.copy_frame(&frame) {
                tracing::debug!(%error, "gpu capture frame");
            }
            let _ = frame.Close();
        }
    }

    fn copy_frame(&mut self, frame: &Direct3D11CaptureFrame) -> Result<(), String> {
        let surface = frame.Surface().map_err(|error| error.to_string())?;
        let access = surface
            .cast::<windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess>()
            .map_err(|error| error.to_string())?;
        let source: ID3D11Texture2D =
            unsafe { access.GetInterface() }.map_err(|error| error.to_string())?;
        if self.bgra.is_none() {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { source.GetDesc(&mut desc) };
            self.captured = (desc.Width, desc.Height);
            self.bgra = Some(create_texture(
                &self.device,
                desc.Width,
                desc.Height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            )?);
        }
        let Some(bgra) = self.bgra.clone() else {
            return Ok(());
        };
        unsafe { self.context.CopyResource(&bgra, &source) };
        Ok(())
    }

    fn prepare(&mut self, encode_w: u32, encode_h: u32) -> Result<(), String> {
        let bgra = self.bgra.as_ref().ok_or("no capture texture")?;
        let nv12 = create_texture(
            &self.device,
            encode_w,
            encode_h,
            DXGI_FORMAT_NV12,
            D3D11_BIND_RENDER_TARGET.0 as u32,
        )?;
        self.nv12 = Some(nv12.clone());
        let video: ID3D11VideoDevice = self.device.cast().map_err(|error| error.to_string())?;
        let video_context: ID3D11VideoContext =
            self.context.cast().map_err(|error| error.to_string())?;
        let (src_w, src_h) = self.captured;
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: src_w,
            InputHeight: src_h,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: encode_w,
            OutputHeight: encode_h,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { video.CreateVideoProcessorEnumerator(&desc) }
            .map_err(|error| format!("video processor enumerator: {error}"))?;
        let processor = unsafe { video.CreateVideoProcessor(&enumerator, 0) }
            .map_err(|error| format!("video processor: {error}"))?;
        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut input_view = None;
        unsafe {
            video
                .CreateVideoProcessorInputView(
                    bgra,
                    &enumerator,
                    &input_desc,
                    Some(&mut input_view),
                )
                .map_err(|error| format!("video input view: {error}"))?;
        }
        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut output_view = None;
        unsafe {
            video
                .CreateVideoProcessorOutputView(
                    &nv12,
                    &enumerator,
                    &output_desc,
                    Some(&mut output_view),
                )
                .map_err(|error| format!("video output view: {error}"))?;
        }
        self.enumerator = Some(enumerator);
        self.processor = Some(processor);
        self.input_view = input_view;
        self.output_view = output_view;
        self.video_context = Some(video_context);
        self.convert()
    }

    fn convert(&mut self) -> Result<(), String> {
        let (Some(processor), Some(output), Some(input), Some(context)) = (
            self.processor.clone(),
            self.output_view.clone(),
            self.input_view.clone(),
            self.video_context.clone(),
        ) else {
            return Err("video processor is not ready".into());
        };
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: TRUE,
            pInputSurface: ManuallyDrop::new(Some(input)),
            ..Default::default()
        };
        let result = unsafe {
            context.VideoProcessorBlt(
                &processor,
                &output,
                self.frame_index,
                std::slice::from_ref(&stream),
            )
        };
        let _held = unsafe { ManuallyDrop::take(&mut stream.pInputSurface) };
        self.frame_index = self.frame_index.wrapping_add(1);
        result.map_err(|error| format!("video processor blit: {error}"))
    }
}

impl Drop for GpuSession {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

fn create_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    bind: u32,
) -> Result<ID3D11Texture2D, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width.max(2),
        Height: height.max(2),
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|error| format!("texture {width}x{height}: {error}"))?;
    }
    texture.ok_or_else(|| "texture was not created".to_owned())
}

fn winrt_device(device: &ID3D11Device) -> Result<IDirect3DDevice, String> {
    let dxgi: IDXGIDevice = device.cast().map_err(|error| error.to_string())?;
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
        .map_err(|error| format!("winrt d3d device: {error}"))?;
    inspectable.cast().map_err(|error| error.to_string())
}

fn capture_item(source: &crate::ScreenSource) -> Result<GraphicsCaptureItem, String> {
    let interop: IGraphicsCaptureItemInterop =
        factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(|error| format!("graphics capture factory: {error}"))?;
    match source.kind {
        ScreenSourceKind::Monitor => {
            let monitor =
                find_monitor(source.id).ok_or_else(|| "monitor handle was not found".to_owned())?;
            unsafe { interop.CreateForMonitor(monitor) }.map_err(|error| error.to_string())
        }
        ScreenSourceKind::Window => {
            let hwnd =
                find_window(source.id).ok_or_else(|| "window handle was not found".to_owned())?;
            unsafe { interop.CreateForWindow(hwnd) }.map_err(|error| error.to_string())
        }
    }
}

fn find_monitor(id: u32) -> Option<HMONITOR> {
    struct Find {
        id: u32,
        found: HMONITOR,
    }
    unsafe extern "system" fn each(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        state: LPARAM,
    ) -> windows::core::BOOL {
        let find = unsafe { &mut *(state.0 as *mut Find) };
        if monitor.0 as u32 == find.id {
            find.found = monitor;
            FALSE
        } else {
            TRUE
        }
    }
    let mut find = Find {
        id,
        found: HMONITOR(std::ptr::null_mut()),
    };
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(each),
            LPARAM(&mut find as *mut Find as isize),
        );
    }
    if find.found.is_invalid() {
        None
    } else {
        Some(find.found)
    }
}

fn find_window(id: u32) -> Option<HWND> {
    struct Find {
        id: u32,
        found: HWND,
    }
    unsafe extern "system" fn each(hwnd: HWND, state: LPARAM) -> windows::core::BOOL {
        let find = unsafe { &mut *(state.0 as *mut Find) };
        if hwnd.0 as u32 == find.id {
            find.found = hwnd;
            FALSE
        } else {
            TRUE
        }
    }
    let mut find = Find {
        id,
        found: HWND(std::ptr::null_mut()),
    };
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut find as *mut Find as isize));
    }
    if find.found.is_invalid() {
        None
    } else {
        Some(find.found)
    }
}
