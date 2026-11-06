#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

//! Annex-B sequence, picture, and slice headers for Constrained Baseline.
//!
//! VA-API drivers that advertise packed-header support (Intel iHD among them)
//! reject a picture unless the application supplies these NALs. The bytes are
//! the same operating point the Windows encoder advertises: profile 66,
//! constraint flags `0xE0`, CAVLC, no B-frames, and level 5.1 or 5.2.

use super::h264_level_idc;

/// One packed header. `bit_length` counts valid bits; a slice header may end
/// mid-byte, and the unused bits in the last byte are zero.
pub(super) struct PackedNal {
    pub bytes: Vec<u8>,
    pub bit_length: u32,
    pub has_emulation_bytes: bool,
}

/// Macroblock-aligned coded size and the H.264 crop offsets that bring it
/// back to the visible picture. Crop units are two luma samples for 4:2:0.
pub(super) struct FrameGrid {
    pub coded_w: u32,
    pub coded_h: u32,
    pub crop_right: u32,
    pub crop_bottom: u32,
}

impl FrameGrid {
    pub(super) fn cropping(&self) -> bool {
        self.crop_right != 0 || self.crop_bottom != 0
    }
}

pub(super) fn frame_grid(width: u32, height: u32) -> FrameGrid {
    let coded_w = width.div_ceil(16) * 16;
    let coded_h = height.div_ceil(16) * 16;
    FrameGrid {
        coded_w,
        coded_h,
        crop_right: (coded_w - width) / 2,
        crop_bottom: (coded_h - height) / 2,
    }
}

/// SPS NAL, including the Annex-B start code. Level follows the visible size.
pub(super) fn sequence_header(width: u32, height: u32, fps: u32) -> PackedNal {
    let grid = frame_grid(width, height);
    let fps = fps.max(1);
    let mut bits = BitBuf::default();
    bits.u(8, 66);
    bits.u(1, 1);
    bits.u(1, 1);
    bits.u(1, 1);
    bits.u(1, 0);
    bits.u(1, 0);
    bits.u(1, 0);
    bits.u(2, 0);
    bits.u(8, h264_level_idc(width, height, fps));
    bits.ue(0);
    bits.ue(0);
    bits.ue(0);
    bits.ue(0);
    bits.ue(1);
    bits.u(1, 0);
    bits.ue(grid.coded_w / 16 - 1);
    bits.ue(grid.coded_h / 16 - 1);
    bits.u(1, 1);
    bits.u(1, 1);
    bits.u(1, u32::from(grid.cropping()));
    if grid.cropping() {
        bits.ue(0);
        bits.ue(grid.crop_right);
        bits.ue(0);
        bits.ue(grid.crop_bottom);
    }
    bits.u(1, 1);
    bits.u(1, 1);
    bits.u(8, 1);
    bits.u(1, 0);
    bits.u(1, 0);
    bits.u(1, 0);
    bits.u(1, 1);
    bits.u(32, 1);
    bits.u(32, fps * 2);
    bits.u(1, 1);
    bits.u(1, 0);
    bits.u(1, 0);
    bits.u(1, 0);
    bits.u(1, 1);
    bits.u(1, 1);
    bits.ue(0);
    bits.ue(0);
    bits.ue(16);
    bits.ue(16);
    bits.ue(0);
    bits.ue(1);
    bits.rbsp_trailing();
    nal(0x67, &bits.finish(), true)
}

/// PPS NAL. CAVLC, one reference, deblocking control present, initial QP 26.
pub(super) fn picture_header() -> PackedNal {
    let mut bits = BitBuf::default();
    bits.ue(0);
    bits.ue(0);
    bits.u(1, 0);
    bits.u(1, 0);
    bits.ue(0);
    bits.ue(0);
    bits.ue(0);
    bits.u(1, 0);
    bits.u(2, 0);
    bits.se(0);
    bits.se(0);
    bits.se(0);
    bits.u(1, 1);
    bits.u(1, 0);
    bits.u(1, 0);
    bits.rbsp_trailing();
    nal(0x68, &bits.finish(), true)
}

/// Slice header only: start code, NAL byte, and `slice_header()`, without
/// slice data or RBSP trailing bits. The driver appends the coded slice.
pub(super) fn slice_header(idr: bool, frame_num: u32, poc: u32, idr_pic_id: u32) -> PackedNal {
    let mut bits = BitBuf::default();
    for byte in [0, 0, 0, 1] {
        bits.u(8, byte);
    }
    bits.u(8, if idr { 0x65 } else { 0x61 });
    bits.ue(0);
    bits.ue(if idr { 2 } else { 0 });
    bits.ue(0);
    bits.u(4, frame_num & 15);
    if idr {
        bits.ue(idr_pic_id);
    }
    bits.u(4, poc & 15);
    if idr {
        bits.u(1, 0);
        bits.u(1, 0);
    } else {
        bits.u(1, 0);
        bits.u(1, 0);
        bits.u(1, 0);
    }
    bits.se(0);
    bits.ue(0);
    bits.se(0);
    bits.se(0);
    PackedNal {
        bit_length: bits.len,
        bytes: bits.bytes,
        has_emulation_bytes: false,
    }
}

fn nal(header: u8, rbsp: &[u8], emulation: bool) -> PackedNal {
    let body = if emulation {
        prevent_emulation(rbsp)
    } else {
        rbsp.to_vec()
    };
    let mut bytes = Vec::with_capacity(5 + body.len());
    bytes.extend_from_slice(&[0, 0, 0, 1, header]);
    bytes.extend_from_slice(&body);
    let bit_length = (bytes.len() * 8) as u32;
    PackedNal {
        bytes,
        bit_length,
        has_emulation_bytes: emulation,
    }
}

fn prevent_emulation(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 8);
    let mut zeros = 0u8;
    for &byte in rbsp {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    out
}

#[derive(Default)]
struct BitBuf {
    bytes: Vec<u8>,
    len: u32,
}

impl BitBuf {
    fn u(&mut self, n: u32, value: u32) {
        if n == 0 {
            return;
        }
        debug_assert!(n <= 32);
        let value = if n == 32 {
            value
        } else {
            value & ((1u32 << n) - 1)
        };
        for bit in (0..n).rev() {
            let index = (self.len / 8) as usize;
            if self.bytes.len() == index {
                self.bytes.push(0);
            }
            if (value >> bit) & 1 == 1 {
                let shift = 7 - (self.len % 8);
                self.bytes[index] |= 1 << shift;
            }
            self.len += 1;
        }
    }

    fn ue(&mut self, value: u32) {
        let code = u64::from(value) + 1;
        let bits = 64 - code.leading_zeros();
        debug_assert!(bits <= 32);
        self.u(bits - 1, 0);
        self.u(bits, code as u32);
    }

    fn se(&mut self, value: i32) {
        // 0, 1, -1, 2, -2 map to exp-golomb codes 0, 1, 2, 3, 4.
        let code = if value > 0 {
            (u32::try_from(value).unwrap_or(u32::MAX) << 1).saturating_sub(1)
        } else {
            value.unsigned_abs() << 1
        };
        self.ue(code);
    }

    fn rbsp_trailing(&mut self) {
        self.u(1, 1);
        let remainder = self.len % 8;
        if remainder != 0 {
            self.u(8 - remainder, 0);
        }
    }

    fn finish(self) -> Vec<u8> {
        debug_assert!(self.len.is_multiple_of(8));
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_crops_only_the_macroblock_padding() {
        let exact = frame_grid(16, 16);
        assert_eq!((exact.coded_w, exact.coded_h), (16, 16));
        assert!(!exact.cropping());
        let padded = frame_grid(800, 600);
        assert_eq!((padded.coded_w, padded.coded_h), (800, 608));
        assert_eq!((padded.crop_right, padded.crop_bottom), (0, 4));
    }

    #[test]
    fn sequence_and_picture_headers_match_constrained_baseline() {
        let sps_16 = sequence_header(16, 16, 30);
        assert_eq!(
            sps_16.bytes,
            [
                0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xe0, 0x33, 0xf4, 0xf6, 0x02, 0x20, 0x00, 0x00,
                0x03, 0x00, 0x20, 0x00, 0x00, 0x07, 0x91, 0xe1, 0x10, 0x8d, 0x40,
            ]
        );
        assert!(sps_16.has_emulation_bytes);
        assert_eq!(sps_16.bit_length, sps_16.bytes.len() as u32 * 8);

        let sps_crop = sequence_header(800, 600, 60);
        assert_eq!(
            sps_crop.bytes,
            [
                0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xe0, 0x33, 0xf4, 0x06, 0x40, 0x9b, 0xf2, 0xe0,
                0x22, 0x00, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x00, 0xf1, 0x1e, 0x11, 0x08,
                0xd4,
            ]
        );

        let sps_4k = sequence_header(3840, 2160, 60);
        assert_eq!(sps_4k.bytes[5..8], [0x42, 0xe0, 0x34]);
        assert_eq!(
            sps_4k.bytes,
            [
                0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xe0, 0x34, 0xf4, 0x01, 0xe0, 0x02, 0x1f, 0x60,
                0x22, 0x00, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x00, 0xf1, 0x1e, 0x11, 0x08,
                0xd4,
            ]
        );

        let pps = picture_header();
        assert_eq!(pps.bytes, [0x00, 0x00, 0x00, 0x01, 0x68, 0xce, 0x3c, 0x80]);
    }

    #[test]
    fn slice_headers_stop_on_the_last_header_bit() {
        let idr = slice_header(true, 0, 0, 0);
        assert_eq!(idr.bytes, [0x00, 0x00, 0x00, 0x01, 0x65, 0xb8, 0x40, 0xf0]);
        assert_eq!(idr.bit_length, 60);
        assert!(!idr.has_emulation_bytes);

        let p = slice_header(false, 1, 2, 0);
        assert_eq!(p.bytes, [0x00, 0x00, 0x00, 0x01, 0x61, 0xe2, 0x43, 0xc0]);
        assert_eq!(p.bit_length, 58);
    }
}
