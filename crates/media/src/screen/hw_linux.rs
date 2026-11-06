//! VA-API H.264 encoder. libva and libva-drm are loaded at runtime, so a
//! machine without a GPU encoder (or without the libraries) falls back to
//! OpenH264. A render node needs `libva2`, `libva-drm2`, and a driver:
//! `intel-media-va-driver`, `mesa-va-drivers`, or `nvidia-vaapi-driver`.

use std::ffi::{CStr, c_void};
use std::fs::{File, OpenOptions};
use std::mem::size_of;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::ptr;

use libloading::Library;

use super::h264_headers::{self, PackedNal};
use super::va_abi::{
    ATTRIB_NOT_SUPPORTED, ATTRIB_PACKED_HEADERS, ATTRIB_RATE_CONTROL, ATTRIB_RT_FORMAT, BUF_CODED,
    BUF_MISC, BUF_PACKED_DATA, BUF_PACKED_HEADER, BUF_PICTURE, BUF_SEQUENCE, BUF_SLICE,
    ENTRY_ENC_SLICE, ENTRY_ENC_SLICE_LP, FOURCC_NV12, INVALID_ID, MISC_FRAME_RATE,
    MISC_RATE_CONTROL, PACKED_PICTURE, PACKED_SEQUENCE, PACKED_SLICE, PACKED_TYPE_PICTURE,
    PACKED_TYPE_SEQUENCE, PACKED_TYPE_SLICE, PICTURE_INVALID, PICTURE_SHORT_TERM, PROFILE_BASELINE,
    PROFILE_CONSTRAINED_BASELINE, PROGRESSIVE, RC_CBR, RC_VBR, RT_YUV420, STATUS_BAD_BITSTREAM,
    STATUS_FRAME_SIZE_OVERFLOW, VaCodedBufferSegment, VaConfigAttrib, VaEncMiscParameterFrameRate,
    VaEncMiscParameterRateControl, VaEncPackedHeaderParameterBuffer,
    VaEncPictureParameterBufferH264, VaEncSequenceParameterBufferH264,
    VaEncSliceParameterBufferH264, VaImage, VaImageFormat, VaPictureH264, picture_fields,
    rate_control_flags, sequence_fields, vui_fields,
};
use super::{AccessUnit, EncodeParams, h264_level_idc};

const _: () = assert!(
    size_of::<usize>() == 8,
    "VA-API layouts below are for 64-bit"
);

/// GPU H.264 encoder. The DRM fd stays open for the lifetime of the VA display.
pub struct HwEncoder {
    va: Va,
    /// Kept open because `vaGetDisplayDRM` borrows this descriptor.
    file: File,
    dpy: *mut c_void,
    context: u32,
    config: u32,
    /// Source surface, then two reconstruction surfaces.
    surfaces: [u32; 3],
    coded_buf: u32,
    image: VaImage,
    width: u32,
    height: u32,
    coded_w: u32,
    coded_h: u32,
    fps: u32,
    bitrate: u32,
    gop: u32,
    rc_window_ms: u32,
    packed: u32,
    vendor: String,
    device: String,
    profile_name: &'static str,
    rate_control: &'static str,
    frames: u64,
    frame_num: u32,
    poc: u32,
    idr_pic_id: u32,
    reference: Option<Reference>,
}

struct Reference {
    surface: u32,
    frame_num: u32,
    poc: u32,
}

impl HwEncoder {
    /// Opens a VA-API encoder for even `width` x `height` frames at `fps`.
    pub fn open(width: u32, height: u32, fps: u32, params: EncodeParams) -> Result<Self, String> {
        if width < 2 || height < 2 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(format!(
                "hardware encoder needs even dimensions, got {width}x{height}"
            ));
        }
        let mut encoder = open_device(width, height, fps, params.bitrate)?;
        encoder.gop = params.gop_frames.max(1);
        encoder.rc_window_ms = params.rc_window_ms.max(1);
        encoder.bitrate = params.bitrate;
        let blank = vec![0u8; (width as usize) * (height as usize) * 4];
        encoder
            .encode(&blank, true)
            .map_err(|error| format!("VA-API test frame failed: {error}"))?;
        encoder.reference = None;
        encoder.frames = 0;
        encoder.frame_num = 0;
        encoder.poc = 0;
        tracing::info!(
            encoder = %encoder.vendor,
            device = %encoder.device,
            profile = encoder.profile_name,
            rate_control = encoder.rate_control,
            packed_headers = encoder.packed,
            width,
            height,
            fps,
            bitrate = encoder.bitrate,
            "hardware h264 encoder"
        );
        Ok(encoder)
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
        let idr = force_idr
            || self.reference.is_none()
            || self.frames.is_multiple_of(u64::from(self.gop));
        if idr {
            self.frame_num = 0;
            self.poc = 0;
        }
        let frame_num = self.frame_num;
        let poc = self.poc;
        self.upload(rgba)?;
        let data = match self.submit(idr, frame_num, poc) {
            Ok(data) => data,
            Err(error) => {
                self.reference = None;
                return Err(error);
            }
        };
        let types = nal_types(&data);
        let keyframe = types.contains(&5);
        if idr && !keyframe {
            self.reference = None;
            return Err("hardware encoder did not emit an IDR".into());
        }
        if !keyframe && !types.contains(&1) {
            self.reference = None;
            return Err("hardware encoder did not emit a slice".into());
        }
        self.reference = Some(Reference {
            surface: self.recon_surface(),
            frame_num,
            poc,
        });
        self.frame_num = (self.frame_num + 1) & 15;
        self.poc = (self.poc + 2) & 15;
        self.frames += 1;
        if idr {
            self.idr_pic_id = self.idr_pic_id.wrapping_add(1) & 0xffff;
        }
        Ok(vec![AccessUnit { data, keyframe }])
    }

    fn recon_surface(&self) -> u32 {
        self.surfaces[1 + (self.frames % 2) as usize]
    }

    fn upload(&self, rgba: &[u8]) -> Result<(), String> {
        let nv12 = nv12_frame(rgba, self.width, self.height, self.coded_w, self.coded_h);
        let mut mapped = ptr::null_mut();
        self.va.check("mapping the NV12 image", unsafe {
            (self.va.map_buffer)(self.dpy, self.image.buf, &mut mapped)
        })?;
        let copied = copy_nv12(
            &self.image,
            mapped.cast(),
            &nv12,
            self.coded_w as usize,
            self.coded_h as usize,
        );
        let _ = unsafe { (self.va.unmap_buffer)(self.dpy, self.image.buf) };
        copied?;
        self.va.check("vaPutImage", unsafe {
            (self.va.put_image)(
                self.dpy,
                self.surfaces[0],
                self.image.image_id,
                0,
                0,
                self.coded_w,
                self.coded_h,
                0,
                0,
                self.coded_w,
                self.coded_h,
            )
        })
    }

    fn submit(&self, idr: bool, frame_num: u32, poc: u32) -> Result<Vec<u8>, String> {
        let mut ids = Vec::new();
        let result = self.render(&mut ids, idr, frame_num, poc);
        for id in ids {
            let _ = unsafe { (self.va.destroy_buffer)(self.dpy, id) };
        }
        result
    }

    fn render(
        &self,
        ids: &mut Vec<u32>,
        idr: bool,
        frame_num: u32,
        poc: u32,
    ) -> Result<Vec<u8>, String> {
        self.push_misc(ids)?;
        self.push_sequence(ids)?;
        self.push_picture(ids, idr, frame_num, poc)?;
        self.push_slice(ids, idr, frame_num, poc)?;
        self.va.check("vaBeginPicture", unsafe {
            (self.va.begin_picture)(self.dpy, self.context, self.surfaces[0])
        })?;
        let rendered = unsafe {
            (self.va.render_picture)(
                self.dpy,
                self.context,
                ids.as_mut_ptr(),
                i32::try_from(ids.len()).unwrap_or(i32::MAX),
            )
        };
        if let Err(error) = self.va.check("vaRenderPicture", rendered) {
            let _ = unsafe { (self.va.end_picture)(self.dpy, self.context) };
            return Err(error);
        }
        self.va.check("vaEndPicture", unsafe {
            (self.va.end_picture)(self.dpy, self.context)
        })?;
        self.va.check("vaSyncSurface", unsafe {
            (self.va.sync_surface)(self.dpy, self.surfaces[0])
        })?;
        Ok(normalize(self.read_coded()?))
    }

    fn push_misc(&self, ids: &mut Vec<u32>) -> Result<(), String> {
        let rate = VaEncMiscParameterRateControl {
            bits_per_second: self.bitrate,
            target_percentage: 100,
            window_size: self.rc_window_ms,
            initial_qp: 26,
            rc_flags: rate_control_flags(self.frames == 0),
            ..VaEncMiscParameterRateControl::default()
        };
        let fps = self.fps.clamp(1, u32::from(u16::MAX));
        let frame_rate = VaEncMiscParameterFrameRate {
            framerate: fps,
            ..VaEncMiscParameterFrameRate::default()
        };
        ids.push(self.buffer(BUF_MISC, &misc_bytes(MISC_RATE_CONTROL, &rate))?);
        ids.push(self.buffer(BUF_MISC, &misc_bytes(MISC_FRAME_RATE, &frame_rate))?);
        Ok(())
    }

    fn push_sequence(&self, ids: &mut Vec<u32>) -> Result<(), String> {
        let grid = h264_headers::frame_grid(self.width, self.height);
        let fps = self.fps.max(1);
        let mut sequence = zeroed::<VaEncSequenceParameterBufferH264>();
        sequence.level_idc =
            u8::try_from(h264_level_idc(self.width, self.height, fps)).unwrap_or(52);
        sequence.intra_period = self.gop;
        sequence.intra_idr_period = self.gop;
        sequence.ip_period = 1;
        sequence.bits_per_second = self.bitrate;
        sequence.max_num_ref_frames = 1;
        sequence.picture_width_in_mbs = (self.coded_w / 16) as u16;
        sequence.picture_height_in_mbs = (self.coded_h / 16) as u16;
        sequence.seq_fields = sequence_fields();
        sequence.frame_cropping_flag = u8::from(grid.cropping());
        sequence.frame_crop_right_offset = grid.crop_right;
        sequence.frame_crop_bottom_offset = grid.crop_bottom;
        sequence.vui_parameters_present_flag = 1;
        sequence.vui_fields = vui_fields();
        sequence.aspect_ratio_idc = 1;
        sequence.num_units_in_tick = 1;
        sequence.time_scale = fps.saturating_mul(2);
        ids.push(self.buffer(BUF_SEQUENCE, pod_bytes(&sequence))?);
        if self.packed & PACKED_SEQUENCE != 0 {
            self.push_packed(
                ids,
                PACKED_TYPE_SEQUENCE,
                &h264_headers::sequence_header(self.width, self.height, fps),
            )?;
        }
        Ok(())
    }

    fn push_picture(
        &self,
        ids: &mut Vec<u32>,
        idr: bool,
        frame_num: u32,
        poc: u32,
    ) -> Result<(), String> {
        let mut reference_frames = [invalid_picture(); 16];
        if !idr && let Some(reference) = &self.reference {
            reference_frames[0] = stored_picture(reference);
        }
        let picture = VaEncPictureParameterBufferH264 {
            curr_pic: VaPictureH264 {
                picture_id: self.recon_surface(),
                frame_idx: frame_num,
                flags: PICTURE_SHORT_TERM,
                top_field_order_cnt: poc as i32,
                bottom_field_order_cnt: poc as i32,
                ..VaPictureH264::default()
            },
            reference_frames,
            coded_buf: self.coded_buf,
            frame_num: frame_num as u16,
            pic_init_qp: 26,
            pic_fields: picture_fields(idr),
            ..VaEncPictureParameterBufferH264::default()
        };
        ids.push(self.buffer(BUF_PICTURE, pod_bytes(&picture))?);
        if self.packed & PACKED_PICTURE != 0 {
            self.push_packed(ids, PACKED_TYPE_PICTURE, &h264_headers::picture_header())?;
        }
        Ok(())
    }

    fn push_slice(
        &self,
        ids: &mut Vec<u32>,
        idr: bool,
        frame_num: u32,
        poc: u32,
    ) -> Result<(), String> {
        let mut list0 = [invalid_picture(); 32];
        let list1 = [invalid_picture(); 32];
        if !idr && let Some(reference) = &self.reference {
            list0[0] = stored_picture(reference);
        }
        let macroblocks = (self.coded_w / 16) * (self.coded_h / 16);
        let slice = VaEncSliceParameterBufferH264 {
            num_macroblocks: macroblocks,
            macroblock_info: INVALID_ID,
            slice_type: if idr { 2 } else { 0 },
            idr_pic_id: self.idr_pic_id as u16,
            pic_order_cnt_lsb: poc as u16,
            ref_pic_list0: list0,
            ref_pic_list1: list1,
            ..VaEncSliceParameterBufferH264::default()
        };
        ids.push(self.buffer(BUF_SLICE, pod_bytes(&slice))?);
        if self.packed & PACKED_SLICE != 0 {
            self.push_packed(
                ids,
                PACKED_TYPE_SLICE,
                &h264_headers::slice_header(idr, frame_num, poc, self.idr_pic_id),
            )?;
        }
        Ok(())
    }

    fn push_packed(&self, ids: &mut Vec<u32>, kind: u32, packed: &PackedNal) -> Result<(), String> {
        let param = VaEncPackedHeaderParameterBuffer {
            type_: kind,
            bit_length: packed.bit_length,
            has_emulation_bytes: u8::from(packed.has_emulation_bytes),
            ..VaEncPackedHeaderParameterBuffer::default()
        };
        ids.push(self.buffer(BUF_PACKED_HEADER, pod_bytes(&param))?);
        ids.push(self.buffer(BUF_PACKED_DATA, &packed.bytes)?);
        Ok(())
    }

    fn buffer(&self, kind: i32, bytes: &[u8]) -> Result<u32, String> {
        let mut id = 0u32;
        let size = u32::try_from(bytes.len()).map_err(|_| "VA buffer is too large".to_owned())?;
        self.va.check("vaCreateBuffer", unsafe {
            (self.va.create_buffer)(
                self.dpy,
                self.context,
                kind,
                size,
                1,
                bytes.as_ptr().cast_mut().cast(),
                &mut id,
            )
        })?;
        Ok(id)
    }

    fn read_coded(&self) -> Result<Vec<u8>, String> {
        let mut mapped = ptr::null_mut();
        self.va.check("mapping the coded buffer", unsafe {
            (self.va.map_buffer)(self.dpy, self.coded_buf, &mut mapped)
        })?;
        let collected = collect_segments(mapped);
        let _ = unsafe { (self.va.unmap_buffer)(self.dpy, self.coded_buf) };
        collected
    }
}

impl Drop for HwEncoder {
    fn drop(&mut self) {
        let _ = self.file.as_raw_fd();
        unsafe {
            let _ = (self.va.destroy_context)(self.dpy, self.context);
            let _ = (self.va.destroy_image)(self.dpy, self.image.image_id);
            let _ = (self.va.destroy_buffer)(self.dpy, self.coded_buf);
            let mut surfaces = self.surfaces;
            let _ = (self.va.destroy_surfaces)(self.dpy, surfaces.as_mut_ptr(), 3);
            let _ = (self.va.destroy_config)(self.dpy, self.config);
            let _ = (self.va.terminate)(self.dpy);
        }
    }
}

struct Built {
    file: File,
    session: Session,
}

struct Session {
    dpy: *mut c_void,
    context: u32,
    config: u32,
    surfaces: [u32; 3],
    coded_buf: u32,
    image: VaImage,
    width: u32,
    height: u32,
    coded_w: u32,
    coded_h: u32,
    fps: u32,
    bitrate: u32,
    packed: u32,
    vendor: String,
    device: String,
    profile_name: &'static str,
    rate_control: &'static str,
}

fn open_device(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<HwEncoder, String> {
    let va = Va::load()?;
    let nodes = render_nodes()?;
    let mut errors = Vec::new();
    for node in nodes {
        match try_node(&va, &node, width, height, fps, bitrate) {
            Ok(built) => return Ok(built.encoder(va)),
            Err(error) => {
                tracing::debug!(device = %node.display(), %error, "skipping vaapi device");
                errors.push(format!("{}: {error}", node.display()));
            }
        }
    }
    Err(errors.join("; "))
}

impl Built {
    fn encoder(self, va: Va) -> HwEncoder {
        let session = self.session;
        HwEncoder {
            va,
            file: self.file,
            dpy: session.dpy,
            context: session.context,
            config: session.config,
            surfaces: session.surfaces,
            coded_buf: session.coded_buf,
            image: session.image,
            width: session.width,
            height: session.height,
            coded_w: session.coded_w,
            coded_h: session.coded_h,
            fps: session.fps,
            bitrate: session.bitrate,
            gop: session.fps.saturating_mul(2).max(1),
            rc_window_ms: 1_000,
            packed: session.packed,
            vendor: session.vendor,
            device: session.device,
            profile_name: session.profile_name,
            rate_control: session.rate_control,
            frames: 0,
            frame_num: 0,
            poc: 0,
            idr_pic_id: 0,
            reference: None,
        }
    }
}

fn try_node(
    va: &Va,
    path: &Path,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
) -> Result<Built, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| format!("opening {}: {error}", path.display()))?;
    let dpy = unsafe { (va.get_display_drm)(file.as_raw_fd()) };
    if dpy.is_null() {
        return Err("vaGetDisplayDRM failed".into());
    }
    let mut major = 0i32;
    let mut minor = 0i32;
    va.check("vaInitialize", unsafe {
        (va.initialize)(dpy, &mut major, &mut minor)
    })?;
    let mut cleanup = Cleanup {
        va,
        dpy,
        active: true,
    };
    let session = setup(va, dpy, path, width, height, fps, bitrate, major, minor)?;
    cleanup.active = false;
    drop(cleanup);
    Ok(Built { file, session })
}

struct Cleanup<'a> {
    va: &'a Va,
    dpy: *mut c_void,
    active: bool,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        if self.active {
            unsafe {
                let _ = (self.va.terminate)(self.dpy);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn setup(
    va: &Va,
    dpy: *mut c_void,
    path: &Path,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    major: i32,
    minor: i32,
) -> Result<Session, String> {
    let vendor = vendor_string(va, dpy);
    tracing::debug!(%vendor, major, minor, device = %path.display(), "vaapi display");
    let (profile, entry) = find_entrypoint(va, dpy)?;
    let (rate_bits, mut packed) = encoder_attributes(va, dpy, profile, entry)?;
    let (rate_control, rate_flag) = choose_rate_control(rate_bits)?;
    let config = match create_config(va, dpy, profile, entry, rate_flag, packed) {
        Ok(config) => config,
        Err(error) if packed != 0 => {
            tracing::debug!(%error, "retrying VA-API config without packed headers");
            packed = 0;
            create_config(va, dpy, profile, entry, rate_flag, 0)?
        }
        Err(error) => return Err(error),
    };
    let grid = h264_headers::frame_grid(width, height);
    let mut surfaces = [0u32; 3];
    va.check("vaCreateSurfaces", unsafe {
        (va.create_surfaces)(
            dpy,
            RT_YUV420,
            grid.coded_w,
            grid.coded_h,
            surfaces.as_mut_ptr(),
            3,
            ptr::null_mut(),
            0,
        )
    })?;
    let mut context = 0u32;
    va.check("vaCreateContext", unsafe {
        (va.create_context)(
            dpy,
            config,
            grid.coded_w as i32,
            grid.coded_h as i32,
            PROGRESSIVE,
            surfaces.as_mut_ptr(),
            3,
            &mut context,
        )
    })?;
    let coded_bytes = (grid.coded_w as usize)
        .saturating_mul(grid.coded_h as usize)
        .max(1 << 20);
    let coded_bytes = u32::try_from(coded_bytes).unwrap_or(u32::MAX);
    let mut coded_buf = 0u32;
    va.check("coded buffer", unsafe {
        (va.create_buffer)(
            dpy,
            context,
            BUF_CODED,
            coded_bytes,
            1,
            ptr::null_mut(),
            &mut coded_buf,
        )
    })?;
    let image = nv12_image(va, dpy, grid.coded_w, grid.coded_h)?;
    Ok(Session {
        dpy,
        context,
        config,
        surfaces,
        coded_buf,
        image,
        width,
        height,
        coded_w: grid.coded_w,
        coded_h: grid.coded_h,
        fps,
        bitrate,
        packed,
        vendor,
        device: path.display().to_string(),
        profile_name: profile_name(profile),
        rate_control,
    })
}

fn vendor_string(va: &Va, dpy: *mut c_void) -> String {
    unsafe {
        let text = (va.vendor)(dpy);
        if text.is_null() {
            "VA-API".to_owned()
        } else {
            CStr::from_ptr(text).to_string_lossy().into_owned()
        }
    }
}

fn find_entrypoint(va: &Va, dpy: *mut c_void) -> Result<(i32, i32), String> {
    let profiles = [PROFILE_CONSTRAINED_BASELINE, PROFILE_BASELINE];
    let entries = [ENTRY_ENC_SLICE, ENTRY_ENC_SLICE_LP];
    for profile in profiles {
        let supported = entrypoints(va, dpy, profile);
        for entry in entries {
            if supported.contains(&entry) {
                return Ok((profile, entry));
            }
        }
    }
    Err("no H.264 encode entrypoint".into())
}

fn entrypoints(va: &Va, dpy: *mut c_void, profile: i32) -> Vec<i32> {
    let max = unsafe { (va.max_entrypoints)(dpy) };
    if max <= 0 {
        return Vec::new();
    }
    let mut list = vec![0i32; max as usize];
    let mut count = max;
    let status = unsafe { (va.query_entrypoints)(dpy, profile, list.as_mut_ptr(), &mut count) };
    if status != 0 || count <= 0 {
        return Vec::new();
    }
    list.truncate(usize::try_from(count.min(max)).unwrap_or(0));
    list
}

fn encoder_attributes(
    va: &Va,
    dpy: *mut c_void,
    profile: i32,
    entry: i32,
) -> Result<(u32, u32), String> {
    let mut attribs = [
        VaConfigAttrib {
            type_: ATTRIB_RATE_CONTROL,
            value: 0,
        },
        VaConfigAttrib {
            type_: ATTRIB_PACKED_HEADERS,
            value: 0,
        },
    ];
    va.check("vaGetConfigAttributes", unsafe {
        (va.get_config_attributes)(dpy, profile, entry, attribs.as_mut_ptr(), 2)
    })?;
    if attribs[0].value & ATTRIB_NOT_SUPPORTED != 0 {
        return Err("VA-API encoder has no rate control".into());
    }
    let packed = if attribs[1].value & ATTRIB_NOT_SUPPORTED != 0 {
        0
    } else {
        attribs[1].value & (PACKED_SEQUENCE | PACKED_PICTURE | PACKED_SLICE)
    };
    Ok((attribs[0].value, packed))
}

fn choose_rate_control(bits: u32) -> Result<(&'static str, u32), String> {
    if bits & RC_CBR != 0 {
        Ok(("cbr", RC_CBR))
    } else if bits & RC_VBR != 0 {
        Ok(("vbr", RC_VBR))
    } else {
        Err("no CBR or VBR rate control".into())
    }
}

fn create_config(
    va: &Va,
    dpy: *mut c_void,
    profile: i32,
    entry: i32,
    rate: u32,
    packed: u32,
) -> Result<u32, String> {
    let mut attribs = vec![
        VaConfigAttrib {
            type_: ATTRIB_RT_FORMAT,
            value: RT_YUV420,
        },
        VaConfigAttrib {
            type_: ATTRIB_RATE_CONTROL,
            value: rate,
        },
    ];
    if packed != 0 {
        attribs.push(VaConfigAttrib {
            type_: ATTRIB_PACKED_HEADERS,
            value: packed,
        });
    }
    let mut id = 0u32;
    let count = i32::try_from(attribs.len()).unwrap_or(i32::MAX);
    va.check("vaCreateConfig", unsafe {
        (va.create_config)(dpy, profile, entry, attribs.as_mut_ptr(), count, &mut id)
    })?;
    Ok(id)
}

fn nv12_image(va: &Va, dpy: *mut c_void, coded_w: u32, coded_h: u32) -> Result<VaImage, String> {
    let max = unsafe { (va.max_image_formats)(dpy) };
    if max <= 0 {
        return Err("VA-API reported no image formats".into());
    }
    let mut formats = vec![VaImageFormat::default(); max as usize];
    let mut count = max;
    va.check("vaQueryImageFormats", unsafe {
        (va.query_image_formats)(dpy, formats.as_mut_ptr(), &mut count)
    })?;
    if count <= 0 || count > max {
        return Err("VA-API image format query returned a bad count".into());
    }
    let mut format = formats[..count as usize]
        .iter()
        .find(|format| format.fourcc == FOURCC_NV12)
        .copied()
        .ok_or_else(|| "VA-API driver has no NV12 image format".to_owned())?;
    let mut image = VaImage::default();
    va.check("vaCreateImage", unsafe {
        (va.create_image)(dpy, &mut format, coded_w as i32, coded_h as i32, &mut image)
    })?;
    if u32::from(image.width) < coded_w || u32::from(image.height) < coded_h {
        return Err("VA-API image is smaller than the coded frame".into());
    }
    if image.pitches[0] < coded_w
        || image.pitches[1] < coded_w
        || image.offsets[1] <= image.offsets[0]
    {
        return Err("VA-API NV12 image layout is unusable".into());
    }
    let rows = u64::from(coded_h / 2).saturating_sub(1);
    let last =
        u64::from(image.offsets[1]) + rows * u64::from(image.pitches[1]) + u64::from(coded_w);
    if last > u64::from(image.data_size) {
        return Err("VA-API NV12 image is smaller than the frame".into());
    }
    Ok(image)
}

fn profile_name(profile: i32) -> &'static str {
    if profile == PROFILE_CONSTRAINED_BASELINE {
        "constrained baseline"
    } else {
        "baseline"
    }
}

fn render_nodes() -> Result<Vec<PathBuf>, String> {
    let dir =
        std::fs::read_dir("/dev/dri").map_err(|error| format!("no DRM render node: {error}"))?;
    let mut nodes = Vec::new();
    for entry in dir.flatten() {
        if entry.file_name().to_string_lossy().starts_with("renderD") {
            nodes.push(entry.path());
        }
    }
    nodes.sort();
    if nodes.is_empty() {
        return Err("no DRM render node in /dev/dri".into());
    }
    Ok(nodes)
}

fn invalid_picture() -> VaPictureH264 {
    VaPictureH264 {
        picture_id: INVALID_ID,
        flags: PICTURE_INVALID,
        ..VaPictureH264::default()
    }
}

fn stored_picture(reference: &Reference) -> VaPictureH264 {
    VaPictureH264 {
        picture_id: reference.surface,
        frame_idx: reference.frame_num,
        flags: PICTURE_SHORT_TERM,
        top_field_order_cnt: reference.poc as i32,
        bottom_field_order_cnt: reference.poc as i32,
        ..VaPictureH264::default()
    }
}

fn nv12_frame(rgba: &[u8], width: u32, height: u32, coded_w: u32, coded_h: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let cw = coded_w as usize;
    let ch = coded_h as usize;
    let mut tight = vec![0u8; w * h * 3 / 2];
    let (y_plane, uv_plane) = tight.split_at_mut(w * h);
    super::rgba_to_nv12(rgba, w, h, y_plane, uv_plane);
    let mut out = vec![16u8; cw * ch + cw * ch / 2];
    out[cw * ch..].fill(128);
    for row in 0..h {
        out[row * cw..row * cw + w].copy_from_slice(&y_plane[row * w..(row + 1) * w]);
    }
    let uv_base = cw * ch;
    for row in 0..h / 2 {
        let dst = uv_base + row * cw;
        out[dst..dst + w].copy_from_slice(&uv_plane[row * w..(row + 1) * w]);
    }
    out
}

fn copy_nv12(
    image: &VaImage,
    base: *mut u8,
    nv12: &[u8],
    coded_w: usize,
    coded_h: usize,
) -> Result<(), String> {
    let y_bytes = coded_w * coded_h;
    if nv12.len() < y_bytes + y_bytes / 2 {
        return Err("NV12 frame is short".into());
    }
    let data_size = image.data_size as usize;
    unsafe {
        for row in 0..coded_h {
            let dst_off = image.offsets[0] as usize + row * image.pitches[0] as usize;
            if dst_off + coded_w > data_size {
                return Err("NV12 image is smaller than the frame".into());
            }
            ptr::copy_nonoverlapping(nv12[row * coded_w..].as_ptr(), base.add(dst_off), coded_w);
        }
        let uv = &nv12[y_bytes..];
        for row in 0..coded_h / 2 {
            let dst_off = image.offsets[1] as usize + row * image.pitches[1] as usize;
            if dst_off + coded_w > data_size {
                return Err("NV12 image is smaller than the frame".into());
            }
            ptr::copy_nonoverlapping(uv[row * coded_w..].as_ptr(), base.add(dst_off), coded_w);
        }
    }
    Ok(())
}

fn normalize(data: Vec<u8>) -> Vec<u8> {
    let data = if super::has_start_code(&data) {
        data
    } else {
        super::to_annex_b(&data)
    };
    if data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1]) || data.is_empty() {
        data
    } else {
        let mut out = Vec::with_capacity(data.len() + 4);
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data);
        out
    }
}

fn nal_types(data: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i < data.len() {
        if data[i..].starts_with(&[0, 0, 0, 1]) && data.len() > i + 4 {
            types.push(data[i + 4] & 0x1f);
            i += 5;
        } else if data[i..].starts_with(&[0, 0, 1]) && data.len() > i + 3 {
            types.push(data[i + 3] & 0x1f);
            i += 4;
        } else {
            i += 1;
        }
    }
    types
}

fn collect_segments(mut ptr: *mut c_void) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for _ in 0..64 {
        if ptr.is_null() {
            return Ok(out);
        }
        let segment = unsafe { ptr::read_unaligned(ptr.cast::<VaCodedBufferSegment>()) };
        if segment.status & (STATUS_BAD_BITSTREAM | STATUS_FRAME_SIZE_OVERFLOW) != 0 {
            return Err(format!("coded buffer status {:#x}", segment.status));
        }
        if segment.size > 0 && !segment.buf.is_null() {
            let skip = (segment.bit_offset / 8) as usize;
            let size = segment.size as usize;
            if segment.bit_offset % 8 != 0 || skip > size {
                return Err("coded buffer is not byte aligned".into());
            }
            let bytes = unsafe { std::slice::from_raw_parts(segment.buf.cast::<u8>(), size) };
            out.extend_from_slice(&bytes[skip..]);
        }
        ptr = segment.next;
    }
    Err("coded buffer segment list is cyclic".into())
}

fn misc_bytes<T>(kind: i32, payload: &T) -> Vec<u8> {
    let mut bytes = vec![0u8; 4 + size_of::<T>()];
    bytes[..4].copy_from_slice(&kind.to_ne_bytes());
    bytes[4..].copy_from_slice(pod_bytes(payload));
    bytes
}

/// The sequence parameter is too large for `Default` (its reference-offset
/// table is 256 entries). Every field is an integer, so the all-zero value is valid.
fn zeroed<T>() -> T {
    unsafe { std::mem::zeroed() }
}

fn pod_bytes<T>(value: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

struct Va {
    _libs: [Library; 2],
    get_display_drm: unsafe extern "C" fn(i32) -> *mut c_void,
    initialize: unsafe extern "C" fn(*mut c_void, *mut i32, *mut i32) -> i32,
    terminate: unsafe extern "C" fn(*mut c_void) -> i32,
    vendor: unsafe extern "C" fn(*mut c_void) -> *const i8,
    error_str: unsafe extern "C" fn(i32) -> *const i8,
    max_entrypoints: unsafe extern "C" fn(*mut c_void) -> i32,
    query_entrypoints: unsafe extern "C" fn(*mut c_void, i32, *mut i32, *mut i32) -> i32,
    get_config_attributes:
        unsafe extern "C" fn(*mut c_void, i32, i32, *mut VaConfigAttrib, i32) -> i32,
    create_config:
        unsafe extern "C" fn(*mut c_void, i32, i32, *mut VaConfigAttrib, i32, *mut u32) -> i32,
    destroy_config: unsafe extern "C" fn(*mut c_void, u32) -> i32,
    create_surfaces:
        unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut u32, u32, *mut c_void, u32) -> i32,
    destroy_surfaces: unsafe extern "C" fn(*mut c_void, *mut u32, i32) -> i32,
    create_context:
        unsafe extern "C" fn(*mut c_void, u32, i32, i32, i32, *mut u32, i32, *mut u32) -> i32,
    destroy_context: unsafe extern "C" fn(*mut c_void, u32) -> i32,
    create_buffer:
        unsafe extern "C" fn(*mut c_void, u32, i32, u32, u32, *mut c_void, *mut u32) -> i32,
    destroy_buffer: unsafe extern "C" fn(*mut c_void, u32) -> i32,
    map_buffer: unsafe extern "C" fn(*mut c_void, u32, *mut *mut c_void) -> i32,
    unmap_buffer: unsafe extern "C" fn(*mut c_void, u32) -> i32,
    begin_picture: unsafe extern "C" fn(*mut c_void, u32, u32) -> i32,
    render_picture: unsafe extern "C" fn(*mut c_void, u32, *mut u32, i32) -> i32,
    end_picture: unsafe extern "C" fn(*mut c_void, u32) -> i32,
    sync_surface: unsafe extern "C" fn(*mut c_void, u32) -> i32,
    max_image_formats: unsafe extern "C" fn(*mut c_void) -> i32,
    query_image_formats: unsafe extern "C" fn(*mut c_void, *mut VaImageFormat, *mut i32) -> i32,
    create_image:
        unsafe extern "C" fn(*mut c_void, *mut VaImageFormat, i32, i32, *mut VaImage) -> i32,
    destroy_image: unsafe extern "C" fn(*mut c_void, u32) -> i32,
    put_image:
        unsafe extern "C" fn(*mut c_void, u32, u32, i32, i32, u32, u32, i32, i32, u32, u32) -> i32,
}

impl Va {
    fn load() -> Result<Self, String> {
        let lib = load_library(&["libva.so.2", "libva.so"])
            .map_err(|error| format!("libva is not installed ({error})"))?;
        let drm = load_library(&["libva-drm.so.2", "libva-drm.so"])
            .map_err(|error| format!("libva-drm is not installed ({error})"))?;
        let get_display_drm = symbol(&drm, "vaGetDisplayDRM")?;
        let initialize = symbol(&lib, "vaInitialize")?;
        let terminate = symbol(&lib, "vaTerminate")?;
        let vendor = symbol(&lib, "vaQueryVendorString")?;
        let error_str = symbol(&lib, "vaErrorStr")?;
        let max_entrypoints = symbol(&lib, "vaMaxNumEntrypoints")?;
        let query_entrypoints = symbol(&lib, "vaQueryConfigEntrypoints")?;
        let get_config_attributes = symbol(&lib, "vaGetConfigAttributes")?;
        let create_config = symbol(&lib, "vaCreateConfig")?;
        let destroy_config = symbol(&lib, "vaDestroyConfig")?;
        let create_surfaces = symbol(&lib, "vaCreateSurfaces")?;
        let destroy_surfaces = symbol(&lib, "vaDestroySurfaces")?;
        let create_context = symbol(&lib, "vaCreateContext")?;
        let destroy_context = symbol(&lib, "vaDestroyContext")?;
        let create_buffer = symbol(&lib, "vaCreateBuffer")?;
        let destroy_buffer = symbol(&lib, "vaDestroyBuffer")?;
        let map_buffer = symbol(&lib, "vaMapBuffer")?;
        let unmap_buffer = symbol(&lib, "vaUnmapBuffer")?;
        let begin_picture = symbol(&lib, "vaBeginPicture")?;
        let render_picture = symbol(&lib, "vaRenderPicture")?;
        let end_picture = symbol(&lib, "vaEndPicture")?;
        let sync_surface = symbol(&lib, "vaSyncSurface")?;
        let max_image_formats = symbol(&lib, "vaMaxNumImageFormats")?;
        let query_image_formats = symbol(&lib, "vaQueryImageFormats")?;
        let create_image = symbol(&lib, "vaCreateImage")?;
        let destroy_image = symbol(&lib, "vaDestroyImage")?;
        let put_image = symbol(&lib, "vaPutImage")?;
        Ok(Self {
            _libs: [lib, drm],
            get_display_drm,
            initialize,
            terminate,
            vendor,
            error_str,
            max_entrypoints,
            query_entrypoints,
            get_config_attributes,
            create_config,
            destroy_config,
            create_surfaces,
            destroy_surfaces,
            create_context,
            destroy_context,
            create_buffer,
            destroy_buffer,
            map_buffer,
            unmap_buffer,
            begin_picture,
            render_picture,
            end_picture,
            sync_surface,
            max_image_formats,
            query_image_formats,
            create_image,
            destroy_image,
            put_image,
        })
    }

    fn check(&self, what: &str, status: i32) -> Result<(), String> {
        if status == 0 {
            Ok(())
        } else {
            Err(format!("{what}: {}", self.error_text(status)))
        }
    }

    fn error_text(&self, status: i32) -> String {
        unsafe {
            let text = (self.error_str)(status);
            if text.is_null() {
                format!("VAStatus {status}")
            } else {
                CStr::from_ptr(text).to_string_lossy().into_owned()
            }
        }
    }
}

fn load_library(names: &[&str]) -> Result<Library, String> {
    let mut errors = Vec::new();
    for name in names {
        match unsafe { Library::new(name) } {
            Ok(library) => return Ok(library),
            Err(error) => errors.push(format!("{name}: {error}")),
        }
    }
    Err(errors.join("; "))
}

fn symbol<T: Copy>(library: &Library, name: &str) -> Result<T, String> {
    let mut raw = Vec::with_capacity(name.len() + 1);
    raw.extend_from_slice(name.as_bytes());
    raw.push(0);
    unsafe {
        library
            .get(&raw)
            .map(|symbol| *symbol)
            .map_err(|error| format!("libva is missing {name}: {error}"))
    }
}
