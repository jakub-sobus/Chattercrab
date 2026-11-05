//! Programmatically generated application icon: a simple, original crab silhouette.
//!
//! The same generator feeds the runtime window icon and, through `build.rs`, the
//! `.ico` resource embedded into the Windows executable. No image assets are shipped
//! or downloaded.

/// Silhouette color (RGB).
const CRAB_RGB: [u8; 3] = [0xD0, 0x4A, 0x2A];

/// Samples per pixel along each axis, for anti-aliasing.
const SUPERSAMPLE: u32 = 4;

/// Distance from point `p` to the segment `a`-`b`.
fn segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (px, py) = (p.0 - a.0, p.1 - a.1);
    let (bx, by) = (b.0 - a.0, b.1 - a.1);
    let len2 = bx * bx + by * by;
    let t = if len2 > 0.0 {
        ((px * bx + py * by) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (dx, dy) = (px - bx * t, py - by * t);
    (dx * dx + dy * dy).sqrt()
}

fn in_circle(p: (f32, f32), c: (f32, f32), r: f32) -> bool {
    let (dx, dy) = (p.0 - c.0, p.1 - c.1);
    dx * dx + dy * dy <= r * r
}

fn in_ellipse(p: (f32, f32), c: (f32, f32), rx: f32, ry: f32) -> bool {
    let (dx, dy) = ((p.0 - c.0) / rx, (p.1 - c.1) / ry);
    dx * dx + dy * dy <= 1.0
}

/// Whether the normalized point (x right, y down, both in 0..1) is inside the crab.
fn inside(x: f32, y: f32) -> bool {
    // The shape is symmetric: evaluate the left half only.
    let p = (if x > 0.5 { 1.0 - x } else { x }, y);

    // Body.
    if in_ellipse(p, (0.5, 0.60), 0.27, 0.17) {
        return true;
    }
    // Eye stalks and eyes.
    if segment_distance(p, (0.44, 0.47), (0.42, 0.33)) < 0.022 || in_circle(p, (0.42, 0.31), 0.04) {
        return true;
    }
    // Arm from the body to the claw.
    if segment_distance(p, (0.30, 0.52), (0.20, 0.36)) < 0.035 {
        return true;
    }
    // Claw: a disc with a wedge-shaped notch cut out of its top (the open pincer).
    let claw = (0.17, 0.25);
    if in_circle(p, claw, 0.12) {
        let notch = in_circle(p, (0.20, 0.12), 0.07) && p.0 > 0.13;
        if !notch {
            return true;
        }
    }
    // Three walking legs, each with a knee.
    for i in 0..3 {
        let fi = i as f32;
        let hip = (0.27, 0.60 + fi * 0.055);
        let knee = (0.12 - fi * 0.01, 0.58 + fi * 0.09);
        let foot = (0.06, 0.72 + fi * 0.09);
        if segment_distance(p, hip, knee) < 0.024 || segment_distance(p, knee, foot) < 0.02 {
            return true;
        }
    }
    false
}

/// Renders the crab as straight (non-premultiplied) RGBA8, `size * size * 4` bytes.
pub fn crab_rgba(size: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    let samples = SUPERSAMPLE * SUPERSAMPLE;
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0u32;
            for sy in 0..SUPERSAMPLE {
                for sx in 0..SUPERSAMPLE {
                    let x = (px as f32 + (sx as f32 + 0.5) / SUPERSAMPLE as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SUPERSAMPLE as f32) / size as f32;
                    if inside(x, y) {
                        hits += 1;
                    }
                }
            }
            let alpha = (hits * 255 / samples) as u8;
            out.extend_from_slice(&[CRAB_RGB[0], CRAB_RGB[1], CRAB_RGB[2], alpha]);
        }
    }
    out
}

/// Encodes the crab at the given sizes (each 1..=256) as a Windows `.ico` file
/// using 32-bit BMP entries.
#[cfg_attr(not(test), allow(dead_code))] // used by build.rs
pub fn crab_ico(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<(u32, Vec<u8>)> = sizes
        .iter()
        .map(|&s| (s, bmp_entry(s, &crab_rgba(s))))
        .collect();
    let mut out = Vec::new();
    // ICONDIR: reserved, type (1 = icon), image count.
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, data) in &images {
        // A dimension byte of 0 means 256.
        let dim = if *size >= 256 { 0u8 } else { *size as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in &images {
        out.extend_from_slice(data);
    }
    out
}

/// One ICO image entry: BITMAPINFOHEADER, bottom-up BGRA pixels, AND mask.
#[cfg_attr(not(test), allow(dead_code))] // used by build.rs
fn bmp_entry(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mask_row = size.div_ceil(32) * 4;
    let pixel_bytes = size * size * 4;
    let mut out = Vec::with_capacity((40 + pixel_bytes + mask_row * size) as usize);
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(size as i32).to_le_bytes());
    // Height covers the XOR bitmap plus the AND mask.
    out.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&pixel_bytes.to_le_bytes());
    out.extend_from_slice(&[0u8; 16]); // resolution and palette fields
    for row in (0..size).rev() {
        for col in 0..size {
            let i = ((row * size + col) * 4) as usize;
            out.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    // Transparency comes from the alpha channel; the AND mask stays clear.
    out.resize(out.len() + (mask_row * size) as usize, 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_has_expected_size_and_coverage() {
        let px = crab_rgba(64);
        assert_eq!(px.len(), 64 * 64 * 4);
        let opaque = px.chunks(4).filter(|p| p[3] == 255).count();
        let clear = px.chunks(4).filter(|p| p[3] == 0).count();
        // A silhouette: a solid core and a transparent surrounding.
        assert!(opaque > 64 * 64 / 8, "too little ink: {opaque}");
        assert!(clear > 64 * 64 / 3, "too little background: {clear}");
    }

    #[test]
    fn silhouette_is_symmetric() {
        let size = 48;
        let px = crab_rgba(size);
        for y in 0..size {
            for x in 0..size / 2 {
                let a = px[((y * size + x) * 4 + 3) as usize];
                let b = px[((y * size + (size - 1 - x)) * 4 + 3) as usize];
                assert!(a.abs_diff(b) <= 1, "asymmetry at ({x},{y})");
            }
        }
    }

    #[test]
    fn ico_layout() {
        let ico = crab_ico(&[16, 256]);
        assert_eq!(&ico[0..6], &[0, 0, 1, 0, 2, 0]);
        // First entry: 16x16, 32 bpp, data right after the directory.
        assert_eq!(ico[6], 16);
        assert_eq!(u16::from_le_bytes([ico[12], ico[13]]), 32);
        let size0 = u32::from_le_bytes([ico[14], ico[15], ico[16], ico[17]]);
        let off0 = u32::from_le_bytes([ico[18], ico[19], ico[20], ico[21]]);
        assert_eq!(off0, 6 + 16 * 2);
        assert_eq!(size0, 40 + 16 * 16 * 4 + 4 * 16);
        // Second entry: 256 encoded as 0.
        assert_eq!(ico[22], 0);
        let size1 = u32::from_le_bytes([ico[30], ico[31], ico[32], ico[33]]);
        assert_eq!(ico.len() as u32, off0 + size0 + size1);
    }
}
