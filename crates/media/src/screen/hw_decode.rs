#![allow(unsafe_op_in_unsafe_fn)]

//! Hardware H.264 decode on Windows.
//!
//! The Media Foundation decoder runs on the GPU. Output NV12 is copied once into
//! a staging texture, cropped to the visible picture into pooled plane buffers,
//! and handed to the UI, which uploads the two planes and does the color
//! conversion in a shader. OpenH264 is used when no hardware decoder opens.

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFDXGIBuffer, IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFSample,
    IMFTransform, METransformHaveOutput, METransformNeedInput, MF_E_NO_EVENTS_AVAILABLE,
    MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_EVENT_FLAG_NO_WAIT, MF_LOW_LATENCY, MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE,
    MF_MT_SUBTYPE, MF_SA_D3D11_AWARE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
    MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFSTARTUP_FULL, MFSampleExtension_CleanPoint, MFStartup,
    MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_LOCALMFT, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_FRIENDLY_NAME_Attribute, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264,
    MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::core::Interface;

use super::receive::contains_idr;
use crate::Nv12Frame;
use crate::video_queue::PlanePool;

enum DecoderEvent {
    Quiet,
    NeedInput,
    HaveOutput,
    Other,
}

/// GPU H.264 decoder. Touched only from the screen decoder thread.
pub struct HwDecoder {
    transform: IMFTransform,
    event_gen: Option<IMFMediaEventGenerator>,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    _manager: IMFDXGIDeviceManager,
    input_id: u32,
    output_id: u32,
    need_input: u32,
    provides_samples: bool,
    staging: Option<ID3D11Texture2D>,
    /// Output frame size (`MF_MT_FRAME_SIZE`), padded to whole macroblocks.
    width: u32,
    height: u32,
    /// Picture without the macroblock padding (1080 rows of a 1088-row frame).
    visible: (u32, u32),
    pool: PlanePool,
    frames: u64,
    /// Pictures already pulled out of the decoder, oldest first.
    ready: std::collections::VecDeque<Nv12Frame>,
}

impl HwDecoder {
    /// Opens an H.264 decoder that accepts a Direct3D device.
    ///
    /// Vendor decoders are tagged hardware. The inbox H.264 decoder is not, and
    /// that is the one which drives NVDEC or Quick Sync through DXVA. Hardware
    /// entries are tried first, then every other D3D11-aware decoder.
    pub fn open() -> Result<Self, String> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
            let mut candidates = Vec::new();
            collect(
                &mut candidates,
                MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0,
                true,
            )?;
            let hardware = candidates.len();
            collect(
                &mut candidates,
                MFT_ENUM_FLAG_SYNCMFT.0
                    | MFT_ENUM_FLAG_ASYNCMFT.0
                    | MFT_ENUM_FLAG_LOCALMFT.0
                    | MFT_ENUM_FLAG_HARDWARE.0
                    | MFT_ENUM_FLAG_SORTANDFILTER.0,
                false,
            )?;
            tracing::info!(hardware, listed = candidates.len(), "h264 decoder search");
            if candidates.is_empty() {
                return Err("no h264 decoder is registered".into());
            }
            candidates.sort_by_key(|candidate| (!candidate.aware, !candidate.hardware));
            let mut errors = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for candidate in &candidates {
                if !seen.insert(candidate.name.clone()) {
                    continue;
                }
                tracing::info!(
                    decoder = %candidate.name,
                    d3d11_aware = candidate.aware,
                    hardware = candidate.hardware,
                    "h264 decoder candidate"
                );
                match bind(&candidate.activate, &candidate.name) {
                    Ok(decoder) => return Ok(decoder),
                    Err(error) => {
                        tracing::warn!(decoder = %candidate.name, %error, "h264 decoder rejected");
                        errors.push(format!("{}: {error}", candidate.name));
                    }
                }
            }
            Err(errors.join("; "))
        }
    }

    /// Decodes one Annex-B access unit. `present` copies the picture out; a skipped
    /// frame is still decoded so later pictures stay valid.
    pub fn decode(&mut self, au: &[u8], present: bool) -> Result<Option<Nv12Frame>, String> {
        let sample = input_sample(au, self.frames, contains_idr(au))?;
        self.submit(sample, present)?;
        self.frames += 1;
        self.drain(present)?;
        if !present {
            self.ready.clear();
            return Ok(None);
        }
        while self.ready.len() > 2 {
            self.ready.pop_front();
        }
        Ok(self.ready.pop_front())
    }

    fn submit(&mut self, sample: IMFSample, present: bool) -> Result<(), String> {
        let transform = self.transform.clone();
        if self.event_gen.is_none() {
            return unsafe { transform.ProcessInput(self.input_id, &sample, 0) }
                .map_err(|error| format!("decoder input: {error}"));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(30);
        loop {
            self.drain(present)?;
            if self.need_input > 0 {
                self.need_input -= 1;
                return unsafe { transform.ProcessInput(self.input_id, &sample, 0) }
                    .map_err(|error| format!("decoder input: {error}"));
            }
            if std::time::Instant::now() >= deadline {
                return Err("timed out waiting for the hardware decoder".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    fn drain(&mut self, present: bool) -> Result<(), String> {
        let transform = self.transform.clone();
        let Some(events) = self.event_gen.clone() else {
            if let Some(frame) = self.take_output(&transform, present)? {
                self.ready.push_back(frame);
            }
            return Ok(());
        };
        loop {
            match self.poll_event(&events)? {
                DecoderEvent::HaveOutput => {
                    if let Some(frame) = self.take_output(&transform, present)? {
                        self.ready.push_back(frame);
                    }
                }
                DecoderEvent::NeedInput | DecoderEvent::Other => {}
                DecoderEvent::Quiet => break,
            }
        }
        Ok(())
    }

    fn poll_event(&mut self, events: &IMFMediaEventGenerator) -> Result<DecoderEvent, String> {
        let event = unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) };
        let event = match event {
            Ok(event) => event,
            Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => {
                return Ok(DecoderEvent::Quiet);
            }
            Err(error) => return Err(format!("decoder event: {error}")),
        };
        let kind =
            unsafe { event.GetType() }.map_err(|error| format!("decoder event type: {error}"))?;
        if kind == METransformNeedInput.0 as u32 {
            self.need_input += 1;
            Ok(DecoderEvent::NeedInput)
        } else if kind == METransformHaveOutput.0 as u32 {
            Ok(DecoderEvent::HaveOutput)
        } else {
            Ok(DecoderEvent::Other)
        }
    }

    fn take_output(
        &mut self,
        transform: &IMFTransform,
        present: bool,
    ) -> Result<Option<Nv12Frame>, String> {
        unsafe {
            loop {
                let provided = if self.provides_samples {
                    None
                } else {
                    Some(output_sample(self.width, self.height)?)
                };
                let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: self.output_id,
                    pSample: std::mem::ManuallyDrop::new(provided),
                    dwStatus: 0,
                    pEvents: std::mem::ManuallyDrop::new(None),
                }];
                let mut status = 0u32;
                let result = transform.ProcessOutput(0, &mut buffers, &mut status);
                let sample = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
                let _events = std::mem::ManuallyDrop::take(&mut buffers[0].pEvents);
                match result {
                    Ok(()) => {
                        let Some(sample) = sample else {
                            return Ok(None);
                        };
                        if !present {
                            return Ok(None);
                        }
                        return copy_nv12(
                            &self.device,
                            &self.context,
                            &sample,
                            &mut self.staging,
                            (self.width, self.height),
                            self.visible,
                            &mut self.pool,
                        );
                    }
                    Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                        return Ok(None);
                    }
                    Err(error) if error.code() == MF_E_NOTACCEPTING => return Ok(None),
                    Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        let new_type = nv12_output(transform, self.output_id)
                            .ok_or_else(|| "decoder offered no output type".to_owned())?;
                        transform
                            .SetOutputType(self.output_id, &new_type, 0)
                            .map_err(|err| format!("decoder output type: {err}"))?;
                        if let Ok(info) = transform.GetOutputStreamInfo(self.output_id) {
                            self.provides_samples = info.dwFlags
                                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                                    | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                                != 0;
                        }
                        read_size(&new_type, &mut self.width, &mut self.height);
                        self.visible =
                            read_aperture(&new_type).unwrap_or((self.width, self.height));
                        tracing::debug!(
                            width = self.width,
                            height = self.height,
                            visible = ?self.visible,
                            "h264 decoder output size"
                        );
                    }
                    Err(error) => return Err(format!("decoder output: {error}")),
                }
            }
        }
    }
}

struct DecoderCandidate {
    activate: IMFActivate,
    name: String,
    aware: bool,
    hardware: bool,
}

unsafe fn collect(
    out: &mut Vec<DecoderCandidate>,
    flags: i32,
    hardware: bool,
) -> Result<(), String> {
    let found = enumerate(flags).map_err(|error| error.to_string())?;
    for (index, activate) in found.into_iter().enumerate() {
        let aware = activate.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 1;
        out.push(DecoderCandidate {
            name: friendly_name(&activate, out.len() + index),
            aware,
            hardware,
            activate,
        });
    }
    Ok(())
}

unsafe fn bind(activate: &IMFActivate, name: &str) -> Result<HwDecoder, String> {
    let (device, context, manager) = device()?;
    let transform: IMFTransform = activate
        .ActivateObject()
        .map_err(|error| format!("activating decoder: {error}"))?;
    let mut is_async = false;
    let mut d3d11_aware = false;
    if let Ok(attrs) = transform.GetAttributes() {
        is_async = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 1;
        d3d11_aware = attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 1;
        let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        if is_async {
            attrs
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .map_err(|error| format!("unlocking decoder: {error}"))?;
        }
    }
    transform
        .ProcessMessage(
            windows::Win32::Media::MediaFoundation::MFT_MESSAGE_SET_D3D_MANAGER,
            manager.as_raw() as usize,
        )
        .map_err(|error| format!("decoder d3d manager: {error}"))?;
    let input = MFCreateMediaType().map_err(|error| error.to_string())?;
    input
        .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
        .and_then(|()| input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264))
        .map_err(|error| format!("decoder input type: {error}"))?;
    transform
        .SetInputType(0, &input, 0)
        .map_err(|error| format!("decoder input type: {error}"))?;
    let _ = prefer_nv12(&transform);
    let mut provides_samples = true;
    if let Ok(info) = transform.GetOutputStreamInfo(0) {
        provides_samples = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
            != 0;
    }
    let event_gen = if is_async {
        Some(
            transform
                .cast::<IMFMediaEventGenerator>()
                .map_err(|error| error.to_string())?,
        )
    } else {
        None
    };
    let _ = transform.ProcessMessage(
        windows::Win32::Media::MediaFoundation::MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
        0,
    );
    let _ = transform.ProcessMessage(
        windows::Win32::Media::MediaFoundation::MFT_MESSAGE_NOTIFY_START_OF_STREAM,
        0,
    );
    tracing::info!(
        decoder = name,
        async_mft = is_async,
        d3d11_aware,
        provides_samples,
        "h264 decoder opened"
    );
    Ok(HwDecoder {
        transform,
        event_gen,
        device,
        context,
        _manager: manager,
        input_id: 0,
        output_id: 0,
        need_input: 0,
        provides_samples,
        staging: None,
        width: 0,
        height: 0,
        visible: (0, 0),
        pool: PlanePool::default(),
        frames: 0,
        ready: std::collections::VecDeque::new(),
    })
}

unsafe fn device() -> Result<(ID3D11Device, ID3D11DeviceContext, IMFDXGIDeviceManager), String> {
    let mut device = None;
    let mut context = None;
    D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE(std::ptr::null_mut()),
        windows::Win32::Graphics::Direct3D11::D3D11_CREATE_DEVICE_VIDEO_SUPPORT
            | windows::Win32::Graphics::Direct3D11::D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        Some(&[D3D_FEATURE_LEVEL_11_0]),
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        Some(&mut context),
    )
    .map_err(|error| format!("decoder device: {error}"))?;
    let device = device.ok_or_else(|| "decoder device was not created".to_owned())?;
    let context = context.ok_or_else(|| "decoder context was not created".to_owned())?;
    if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
        let _ = multithread.SetMultithreadProtected(true);
    }
    let dxgi: IDXGIDevice = device.cast().map_err(|error| error.to_string())?;
    let mut token = 0u32;
    let mut manager = None;
    MFCreateDXGIDeviceManager(&mut token, &mut manager).map_err(|error| error.to_string())?;
    let manager = manager.ok_or_else(|| "decoder device manager was not created".to_owned())?;
    manager
        .ResetDevice(&dxgi, token)
        .map_err(|error| format!("decoder device manager: {error}"))?;
    Ok((device, context, manager))
}

unsafe fn enumerate(flags: i32) -> windows::core::Result<Vec<IMFActivate>> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut activates = std::ptr::null_mut();
    let mut count = 0u32;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_DECODER,
        MFT_ENUM_FLAG(flags),
        Some(&input),
        None,
        &mut activates,
        &mut count,
    )?;
    let mut out = Vec::with_capacity(count as usize);
    if !activates.is_null() {
        for index in 0..count as usize {
            if let Some(activate) = (*activates.add(index)).take() {
                out.push(activate);
            }
        }
        CoTaskMemFree(Some(activates.cast()));
    }
    Ok(out)
}

unsafe fn friendly_name(activate: &IMFActivate, index: usize) -> String {
    let mut value = windows::core::PWSTR::null();
    let mut len = 0u32;
    if activate
        .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut value, &mut len)
        .is_err()
        || value.is_null()
    {
        return format!("h264 decoder {index}");
    }
    let name = value.to_string().unwrap_or_default();
    CoTaskMemFree(Some(value.as_ptr().cast()));
    if name.is_empty() {
        format!("h264 decoder {index}")
    } else {
        name
    }
}

unsafe fn prefer_nv12(transform: &IMFTransform) -> Result<(), String> {
    let Some(kind) = nv12_output(transform, 0) else {
        return Ok(());
    };
    transform
        .SetOutputType(0, &kind, 0)
        .map_err(|error| format!("decoder output type: {error}"))
}

unsafe fn nv12_output(
    transform: &IMFTransform,
    stream: u32,
) -> Option<windows::Win32::Media::MediaFoundation::IMFMediaType> {
    let mut fallback = None;
    for index in 0..32 {
        let Ok(kind) = transform.GetOutputAvailableType(stream, index) else {
            break;
        };
        if fallback.is_none() {
            fallback = Some(kind.clone());
        }
        if transform_subtype(&kind).as_ref() == Some(&MFVideoFormat_NV12) {
            return Some(kind);
        }
    }
    fallback
}

fn transform_subtype(
    kind: &windows::Win32::Media::MediaFoundation::IMFMediaType,
) -> Option<windows::core::GUID> {
    unsafe { kind.GetGUID(&MF_MT_SUBTYPE).ok() }
}

fn input_sample(au: &[u8], index: u64, keyframe: bool) -> Result<IMFSample, String> {
    unsafe {
        let buffer = MFCreateMemoryBuffer(au.len() as u32).map_err(|error| error.to_string())?;
        let mut data = std::ptr::null_mut();
        buffer
            .Lock(&mut data, None, None)
            .map_err(|error| error.to_string())?;
        std::ptr::copy_nonoverlapping(au.as_ptr(), data, au.len());
        buffer.Unlock().map_err(|error| error.to_string())?;
        buffer
            .SetCurrentLength(au.len() as u32)
            .map_err(|error| error.to_string())?;
        let sample = MFCreateSample().map_err(|error| error.to_string())?;
        sample
            .AddBuffer(&buffer)
            .map_err(|error| error.to_string())?;
        let _ = sample.SetSampleTime((index as i64) * 10_000_000 / 60);
        let _ = sample.SetSampleDuration(10_000_000 / 60);
        if keyframe {
            let _ = sample.SetUINT32(&MFSampleExtension_CleanPoint, 1);
        }
        Ok(sample)
    }
}

fn output_sample(width: u32, height: u32) -> Result<IMFSample, String> {
    let size = (width.max(2) * height.max(2) * 3 / 2).max(64);
    unsafe {
        let buffer = MFCreateMemoryBuffer(size).map_err(|error| error.to_string())?;
        let sample = MFCreateSample().map_err(|error| error.to_string())?;
        sample
            .AddBuffer(&buffer)
            .map_err(|error| error.to_string())?;
        Ok(sample)
    }
}

fn read_size(
    media_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
    width: &mut u32,
    height: &mut u32,
) {
    let Ok(packed) = (unsafe {
        media_type.GetUINT64(&windows::Win32::Media::MediaFoundation::MF_MT_FRAME_SIZE)
    }) else {
        return;
    };
    *width = (packed >> 32) as u32;
    *height = packed as u32;
}

/// The visible picture inside an output type: `MF_MT_MINIMUM_DISPLAY_APERTURE`
/// when it starts at the top-left corner.
fn read_aperture(
    media_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
) -> Option<(u32, u32)> {
    // MFVideoArea: two MFOffset { fract: u16, value: i16 }, then SIZE { cx, cy }.
    let mut area = [0u8; 16];
    let mut len = 0u32;
    unsafe { media_type.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, &mut area, Some(&mut len)) }
        .ok()?;
    let int = |at: usize| i32::from_le_bytes([area[at], area[at + 1], area[at + 2], area[at + 3]]);
    let width = u32::try_from(int(8)).ok()?;
    let height = u32::try_from(int(12)).ok()?;
    let origin = area[2..4] == [0, 0] && area[6..8] == [0, 0];
    (len == 16 && origin && width > 0 && height > 0).then_some((width, height))
}

/// `visible` clamped to a `width` x `height` surface; the whole surface when unknown.
fn crop_to(visible: (u32, u32), width: u32, height: u32) -> (u32, u32) {
    match visible {
        (0, _) | (_, 0) => (width, height),
        (w, h) => (w.min(width), h.min(height)),
    }
}

fn copy_nv12(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    sample: &IMFSample,
    staging: &mut Option<ID3D11Texture2D>,
    frame_size: (u32, u32),
    visible: (u32, u32),
    pool: &mut PlanePool,
) -> Result<Option<Nv12Frame>, String> {
    let buffer = unsafe { sample.GetBufferByIndex(0) }.map_err(|error| error.to_string())?;
    if let Ok(dxgi) = buffer.cast::<IMFDXGIBuffer>() {
        log_surface(true);
        return copy_dxgi(device, context, &dxgi, staging, visible, pool);
    }
    log_surface(false);
    copy_memory(&buffer, frame_size, visible, pool)
}

fn log_surface(dxgi: bool) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| tracing::info!(dxgi, "h264 decoder output"));
}

fn copy_dxgi(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    dxgi: &IMFDXGIBuffer,
    staging: &mut Option<ID3D11Texture2D>,
    visible: (u32, u32),
    pool: &mut PlanePool,
) -> Result<Option<Nv12Frame>, String> {
    let texture = unsafe {
        let mut raw = std::ptr::null_mut();
        dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)
            .map_err(|error| format!("decoder texture: {error}"))?;
        if raw.is_null() {
            return Err("decoder texture was empty".into());
        }
        ID3D11Texture2D::from_raw(raw)
    };
    let subresource = unsafe { dxgi.GetSubresourceIndex() }.unwrap_or(0);
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    // The publisher may resize its window; the staging copy must match the surface.
    let fits = staging.as_ref().is_some_and(|stage| {
        let mut current = D3D11_TEXTURE2D_DESC::default();
        unsafe { stage.GetDesc(&mut current) };
        current.Width == desc.Width && current.Height == desc.Height
    });
    if !fits {
        *staging = Some(staging_texture(device, desc.Width, desc.Height)?);
    }
    let Some(stage) = staging.as_ref() else {
        return Ok(None);
    };
    let (width, height) = crop_to(visible, desc.Width, desc.Height);
    unsafe {
        context.CopySubresourceRegion(stage, 0, 0, 0, 0, &texture, subresource, None);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(stage, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|error| format!("mapping decoder output: {error}"))?;
        let pitch = mapped.RowPitch as usize;
        let rows = desc.Height as usize;
        let src = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * (rows + rows / 2));
        let frame = nv12_from_rows(src, pitch, rows, width, height, pool);
        context.Unmap(stage, 0);
        Ok(frame)
    }
}

fn staging_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|error| format!("decoder staging texture: {error}"))?;
    }
    texture.ok_or_else(|| "decoder staging texture was not created".to_owned())
}

fn copy_memory(
    buffer: &windows::Win32::Media::MediaFoundation::IMFMediaBuffer,
    (width, height): (u32, u32),
    visible: (u32, u32),
    pool: &mut PlanePool,
) -> Result<Option<Nv12Frame>, String> {
    if width < 2 || height < 2 {
        return Ok(None);
    }
    let (visible_w, visible_h) = crop_to(visible, width, height);
    unsafe {
        let mut data = std::ptr::null_mut();
        let mut len = 0u32;
        buffer
            .Lock(&mut data, None, Some(&mut len))
            .map_err(|error| error.to_string())?;
        let bytes = std::slice::from_raw_parts(data, len as usize);
        let frame = nv12_from_rows(
            bytes,
            width as usize,
            height as usize,
            visible_w,
            visible_h,
            pool,
        );
        buffer.Unlock().map_err(|error| error.to_string())?;
        Ok(frame)
    }
}

/// Copies the top-left `width` x `height` picture out of NV12 rows `pitch` bytes
/// apart, whose chroma plane starts after `plane_rows` luma rows, into tight
/// pooled planes. `None` when `src` is too short for that.
fn nv12_from_rows(
    src: &[u8],
    pitch: usize,
    plane_rows: usize,
    width: u32,
    height: u32,
    pool: &mut PlanePool,
) -> Option<Nv12Frame> {
    let (w, h) = (width as usize, height as usize);
    let uv_row = w.div_ceil(2) * 2;
    let uv_rows = h.div_ceil(2);
    let uv_base = pitch * plane_rows;
    if w == 0 || h == 0 || uv_row > pitch || h > plane_rows {
        return None;
    }
    if src.len() < uv_base + pitch * (uv_rows - 1) + uv_row {
        return None;
    }
    let y = pool.fill(w * h, |out| {
        for (row, dst) in out.chunks_exact_mut(w).enumerate() {
            dst.copy_from_slice(&src[row * pitch..][..w]);
        }
    });
    let uv = pool.fill(uv_row * uv_rows, |out| {
        for (row, dst) in out.chunks_exact_mut(uv_row).enumerate() {
            dst.copy_from_slice(&src[uv_base + row * pitch..][..uv_row]);
        }
    });
    Some(Nv12Frame {
        width,
        height,
        y_stride: width,
        uv_stride: uv_row as u32,
        y,
        uv,
        seq: Nv12Frame::next_seq(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_cropped_to_the_visible_picture() {
        // 4x4 picture in a 6-byte pitch, 6-row surface (2 padding rows), chroma after.
        let (pitch, rows) = (6usize, 6usize);
        let mut src = vec![0u8; pitch * (rows + rows / 2)];
        for row in 0..rows {
            for col in 0..pitch {
                src[row * pitch + col] = (row * 10 + col) as u8;
            }
        }
        for row in 0..rows / 2 {
            for col in 0..pitch {
                src[(rows + row) * pitch + col] = 100 + (row * 10 + col) as u8;
            }
        }
        let mut pool = PlanePool::default();
        let frame = nv12_from_rows(&src, pitch, rows, 4, 4, &mut pool).expect("frame");
        assert_eq!(
            (frame.width, frame.height, frame.y_stride, frame.uv_stride),
            (4, 4, 4, 4)
        );
        assert_eq!(
            &frame.y[..],
            &[0, 1, 2, 3, 10, 11, 12, 13, 20, 21, 22, 23, 30, 31, 32, 33]
        );
        assert_eq!(&frame.uv[..], &[100, 101, 102, 103, 110, 111, 112, 113]);
        // The chroma rows of the visible picture end before the surface does.
        assert!(nv12_from_rows(&src[..pitch * 8], pitch, rows, 4, 4, &mut pool).is_some());
        assert!(nv12_from_rows(&src[..pitch * 7 + 3], pitch, rows, 4, 4, &mut pool).is_none());
        assert!(nv12_from_rows(&src, pitch, rows, 8, 4, &mut pool).is_none());
    }

    #[test]
    fn crop_falls_back_to_the_surface() {
        assert_eq!(crop_to((1920, 1080), 1920, 1088), (1920, 1080));
        assert_eq!(crop_to((0, 0), 1920, 1088), (1920, 1088));
        assert_eq!(crop_to((4000, 1080), 1920, 1088), (1920, 1080));
    }

    /// Needs a hardware decoder; returns early without one.
    #[test]
    fn hardware_decoder_crops_1080p_and_follows_a_resize() {
        let mut decoder = match HwDecoder::open() {
            Ok(decoder) => decoder,
            Err(error) => {
                eprintln!("hardware decoder unavailable: {error}");
                return;
            }
        };
        for (w, h, chroma) in [(1920usize, 1080usize, 160u8), (1280, 720, 100)] {
            let mut encoder = openh264::encoder::Encoder::new().expect("encoder");
            // Smooth luma ramp, flat chroma.
            let mut planes = vec![chroma; w * h * 3 / 2];
            for (i, px) in planes[..w * h].iter_mut().enumerate() {
                *px = (16 + (i % w) * 200 / w) as u8;
            }
            let yuv = openh264::formats::YUVBuffer::from_vec(planes.clone(), w, h);
            let mut shown = None;
            for _ in 0..8 {
                let au = encoder.encode(&yuv).expect("encode").to_vec();
                if let Some(frame) = decoder.decode(&au, true).expect("decode") {
                    shown = Some(frame);
                    break;
                }
            }
            let frame = shown.expect("no picture from the hardware decoder");
            assert_eq!((frame.width as usize, frame.height as usize), (w, h));
            assert_eq!(frame.y.len(), w * h);
            assert_eq!(frame.uv.len(), w * h / 2);
            let error: u64 = frame
                .y
                .iter()
                .zip(&planes[..w * h])
                .map(|(&got, &want)| u64::from(got.abs_diff(want)))
                .sum();
            assert!(error / (w * h) as u64 <= 3, "luma differs by {error}");
            let error: u64 = frame
                .uv
                .iter()
                .map(|&got| u64::from(got.abs_diff(chroma)))
                .sum();
            assert!(
                error / frame.uv.len() as u64 <= 3,
                "chroma differs by {error}"
            );
        }
    }
}
