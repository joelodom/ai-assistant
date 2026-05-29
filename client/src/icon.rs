//! Procedurally-generated application icon.
//!
//! eframe sets the macOS Dock icon at runtime from the viewport's `IconData`
//! (it calls AppKit `setApplicationIconImage:`), so we don't need an `.app`
//! bundle or an `.icns` on disk — we just hand it RGBA pixels. Generating them
//! in code keeps the repo asset-free and lets the icon track the app's palette.
//!
//! The mark: a blue→teal rounded "squircle" tile with a bold white AI sparkle
//! and a small twinkle. Rendered at 4× supersampling for smooth edges, with
//! straight (un-premultiplied) alpha so the rounded corners are transparent.

use eframe::egui::IconData;

const SIZE: usize = 256; // final icon edge (multiple of 4, as IconData wants)
const SS: usize = 4; // supersampling factor per axis
const CORNER: f32 = 56.0; // tile corner radius, in icon-space px

// Tile gradient, top-left → bottom-right, matching the in-app accents.
const C0: [f32; 3] = [0.357, 0.639, 1.000]; // #5BA3FF blue
const C1: [f32; 3] = [0.122, 0.702, 0.604]; // #1FB39A teal

/// Build the application icon.
pub fn app_icon() -> IconData {
    let big = sparkle(118.0, 114.0, 82.0, 0.17);
    let small = sparkle(184.0, 180.0, 30.0, 0.17);

    let mut rgba = Vec::with_capacity(SIZE * SIZE * 4);
    let inv = 1.0 / (SS * SS) as f32;

    for fy in 0..SIZE {
        for fx in 0..SIZE {
            let mut acc = [0.0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = fx as f32 + (sx as f32 + 0.5) / SS as f32;
                    let y = fy as f32 + (sy as f32 + 0.5) / SS as f32;
                    let p = sample(x, y, &big, &small);
                    acc[0] += p[0];
                    acc[1] += p[1];
                    acc[2] += p[2];
                    acc[3] += p[3];
                }
            }
            for c in acc {
                rgba.push((c * inv * 255.0).round().clamp(0.0, 255.0) as u8);
            }
        }
    }

    IconData {
        rgba,
        width: SIZE as u32,
        height: SIZE as u32,
    }
}

/// Color + straight alpha at one (super-sampled) point, in 0..1.
fn sample(x: f32, y: f32, big: &[(f32, f32)], small: &[(f32, f32)]) -> [f32; 4] {
    let base = gradient(x, y);
    let inside_tile = sdf_rrect(x - 128.0, y - 128.0, 128.0, 128.0, CORNER) <= 0.0;
    // rgb stays the gradient color even where transparent, so averaging across
    // the tile edge doesn't darken it (no fringe).
    let a = if inside_tile { 1.0 } else { 0.0 };
    let rgb = if inside_tile && (in_poly(x, y, big) || in_poly(x, y, small)) {
        [1.0, 1.0, 1.0]
    } else {
        base
    };
    [rgb[0], rgb[1], rgb[2], a]
}

fn gradient(x: f32, y: f32) -> [f32; 3] {
    let t = ((x + y) / (2.0 * SIZE as f32)).clamp(0.0, 1.0);
    [
        C0[0] + (C1[0] - C0[0]) * t,
        C0[1] + (C1[1] - C0[1]) * t,
        C0[2] + (C1[2] - C0[2]) * t,
    ]
}

/// Eight vertices of a 4-point sparkle (tips on the axes), wound in order.
fn sparkle(cx: f32, cy: f32, outer: f32, inner_ratio: f32) -> Vec<(f32, f32)> {
    let inner = outer * inner_ratio;
    let mut v = Vec::with_capacity(8);
    for k in 0..4 {
        let tip = k as f32 * std::f32::consts::FRAC_PI_2;
        let mid = tip + std::f32::consts::FRAC_PI_4;
        v.push((cx + outer * tip.cos(), cy + outer * tip.sin()));
        v.push((cx + inner * mid.cos(), cy + inner * mid.sin()));
    }
    v
}

/// Signed distance to a rounded box centered at origin (negative = inside).
fn sdf_rrect(px: f32, py: f32, bx: f32, by: f32, r: f32) -> f32 {
    let qx = px.abs() - bx + r;
    let qy = py.abs() - by + r;
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let inside = qx.max(qy).min(0.0);
    outside + inside - r
}

/// Even-odd ray-cast point-in-polygon test.
fn in_poly(x: f32, y: f32, v: &[(f32, f32)]) -> bool {
    let n = v.len();
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = v[i];
        let (xj, yj) = v[j];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_has_expected_shape() {
        let icon = app_icon();
        assert_eq!(icon.width, SIZE as u32);
        assert_eq!(icon.height, SIZE as u32);
        assert_eq!(icon.rgba.len(), SIZE * SIZE * 4);

        // Corner pixel is outside the rounded tile → transparent.
        let corner_a = icon.rgba[3];
        assert_eq!(corner_a, 0, "rounded corner should be transparent");

        // Center pixel is inside the tile and opaque.
        let ci = ((SIZE / 2) * SIZE + (SIZE / 2)) * 4;
        assert_eq!(icon.rgba[ci + 3], 255, "center should be opaque");
    }

    /// Write a PNG preview to /tmp when AI_ASSISTANT_WRITE_ICON is set:
    ///   AI_ASSISTANT_WRITE_ICON=1 cargo test -p client emit_preview_png
    #[test]
    fn emit_preview_png() {
        if std::env::var("AI_ASSISTANT_WRITE_ICON").is_err() {
            return;
        }
        let icon = app_icon();
        let png = encode_png(&icon.rgba, icon.width, icon.height);
        std::fs::write("/tmp/ai-assistant-icon.png", png).unwrap();
    }

    // --- Minimal, dependency-free PNG encoder (test-only) ----------------
    // 8-bit RGBA, single IDAT using zlib *stored* (uncompressed) deflate
    // blocks. Not efficient — just correct, so we can eyeball the result.

    fn encode_png(rgba: &[u8], w: u32, h: u32) -> Vec<u8> {
        let mut out = vec![137, 80, 78, 71, 13, 10, 26, 10];

        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, RGBA, no interlace
        chunk(&mut out, b"IHDR", &ihdr);

        // Filtered scanlines: each row prefixed with filter byte 0 (none).
        let mut raw = Vec::with_capacity((w * 4 + 1) as usize * h as usize);
        for y in 0..h as usize {
            raw.push(0);
            let row = &rgba[y * w as usize * 4..(y + 1) * w as usize * 4];
            raw.extend_from_slice(row);
        }
        chunk(&mut out, b"IDAT", &zlib_stored(&raw));

        chunk(&mut out, b"IEND", &[]);
        out
    }

    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc_in = Vec::with_capacity(4 + data.len());
        crc_in.extend_from_slice(kind);
        crc_in.extend_from_slice(data);
        out.extend_from_slice(&crc32(&crc_in).to_be_bytes());
    }

    fn zlib_stored(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x78, 0x01]; // zlib header (deflate, 32K window)
        let mut i = 0;
        while i < data.len() {
            let len = (data.len() - i).min(0xFFFF);
            let final_block = i + len >= data.len();
            out.push(if final_block { 1 } else { 0 }); // BFINAL, BTYPE=00
            out.extend_from_slice(&(len as u16).to_le_bytes());
            out.extend_from_slice(&(!(len as u16)).to_le_bytes());
            out.extend_from_slice(&data[i..i + len]);
            i += len;
        }
        out.extend_from_slice(&adler32(data).to_be_bytes());
        out
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        c ^ 0xFFFF_FFFF
    }

    fn adler32(data: &[u8]) -> u32 {
        let (mut a, mut b) = (1u32, 0u32);
        for &x in data {
            a = (a + x as u32) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }
}
