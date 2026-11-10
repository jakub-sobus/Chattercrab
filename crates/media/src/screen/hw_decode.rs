#![allow(unsafe_op_in_unsafe_fn)]

//! Hardware H.264 decode on Windows.
//!
//! The Media Foundation decoder runs on the GPU. Output NV12 is copied once into
//! a staging texture and handed to the UI, which uploads the two planes and does
//! the color conversion in a shader. OpenH264 is used when no hardware decoder opens.

use std::sync::Arc;

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
    MF_EVENT_FLAG_NO_WAIT, MF_LOW_LATENCY, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SA_D3D11_AWARE,
    MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFCreateDXGIDeviceManager,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSTARTUP_FULL,
    MFSampleExtension_CleanPoint, MFStartup, MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG,
    MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_LOCALMFT,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_FRIENDLY_NAME_Attribute,
    MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264,
    MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::core::Interface;

use super::receive::contains_idr;
use crate::Nv12Frame;

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
    width: u32,
    height: u32,
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
            std::thread::sleep(std::time::Duration::from_micros(200));
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
                            &mut self.width,
                            &mut self.height,
                            true,
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

fn copy_nv12(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    sample: &IMFSample,
    staging: &mut Option<ID3D11Texture2D>,
    width: &mut u32,
    height: &mut u32,
    log: bool,
) -> Result<Option<Nv12Frame>, String> {
    let buffer = unsafe { sample.GetBufferByIndex(0) }.map_err(|error| error.to_string())?;
    if let Ok(dxgi) = buffer.cast::<IMFDXGIBuffer>() {
        if log {
            log_surface(true);
        }
        return copy_dxgi(device, context, &dxgi, staging, width, height);
    }
    if log {
        log_surface(false);
    }
    copy_memory(&buffer, width, height)
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
    width: &mut u32,
    height: &mut u32,
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
    *width = desc.Width;
    *height = desc.Height;
    if staging.is_none() {
        *staging = Some(staging_texture(device, desc.Width, desc.Height)?);
    }
    let Some(stage) = staging.as_ref() else {
        return Ok(None);
    };
    unsafe {
        context.CopySubresourceRegion(stage, 0, 0, 0, 0, &texture, subresource, None);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(stage, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|error| format!("mapping decoder output: {error}"))?;
        let frame = nv12_from_mapped(&mapped, desc.Width, desc.Height);
        context.Unmap(stage, 0);
        Ok(Some(frame))
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
    width: &mut u32,
    height: &mut u32,
) -> Result<Option<Nv12Frame>, String> {
    if *width < 2 || *height < 2 {
        return Ok(None);
    }
    unsafe {
        let mut data = std::ptr::null_mut();
        let mut len = 0u32;
        buffer
            .Lock(&mut data, None, Some(&mut len))
            .map_err(|error| error.to_string())?;
        let bytes = std::slice::from_raw_parts(data, len as usize);
        let frame = nv12_from_tight(bytes, *width, *height);
        buffer.Unlock().map_err(|error| error.to_string())?;
        Ok(frame)
    }
}

fn nv12_from_mapped(mapped: &D3D11_MAPPED_SUBRESOURCE, width: u32, height: u32) -> Nv12Frame {
    let stride = mapped.RowPitch as usize;
    let src = unsafe {
        std::slice::from_raw_parts(mapped.pData as *const u8, stride * height as usize * 3 / 2)
    };
    let y_rows = height as usize;
    let uv_rows = height as usize / 2;
    let mut y = Vec::with_capacity(width as usize * y_rows);
    for row in 0..y_rows {
        let start = row * stride;
        y.extend_from_slice(&src[start..start + width as usize]);
    }
    let uv_base = stride * y_rows;
    let mut uv = Vec::with_capacity(width as usize * uv_rows);
    for row in 0..uv_rows {
        let start = uv_base + row * stride;
        uv.extend_from_slice(&src[start..start + width as usize]);
    }
    Nv12Frame {
        width,
        height,
        y_stride: width,
        uv_stride: width,
        y: Arc::from(y),
        uv: Arc::from(uv),
    }
}

fn nv12_from_tight(bytes: &[u8], width: u32, height: u32) -> Option<Nv12Frame> {
    let y_len = width as usize * height as usize;
    let uv_len = y_len / 2;
    if bytes.len() < y_len + uv_len {
        return None;
    }
    Some(Nv12Frame {
        width,
        height,
        y_stride: width,
        uv_stride: width,
        y: Arc::from(bytes[..y_len].to_vec()),
        uv: Arc::from(bytes[y_len..y_len + uv_len].to_vec()),
    })
}
