//! Ported from bar-replays (`src/gexapi/dds.rs`), for reading BAR's pictures from
//! a local game checkout at run time; barwatch ships none of them.
//!
//! Minimal DDS decoder for BAR's `unitpics/*.dds`: the top mip level of uncompressed
//! (bit-mask) RGB/RGBA and DXT1/DXT3/DXT5 surfaces. Measured on the BAR repo
//! (2026-10): 679 uncompressed 32-bit, 23 DXT1, 1 DXT5.

use anyhow::{bail, ensure, Result};
use image::RgbaImage;

const DDPF_ALPHAPIXELS: u32 = 0x1;
const DDPF_FOURCC: u32 = 0x4;
const DDPF_RGB: u32 = 0x40;

pub fn decode(data: &[u8]) -> Result<RgbaImage> {
    ensure!(data.len() >= 128 && &data[..4] == b"DDS ", "not a DDS file");
    let u32_at = |o: usize| u32::from_le_bytes(data[o..o + 4].try_into().unwrap());
    let (height, width) = (u32_at(12), u32_at(16));
    ensure!(width > 0 && height > 0 && width <= 8192 && height <= 8192, "bad DDS size {width}x{height}");
    let (pf_flags, fourcc) = (u32_at(80), &data[84..88]);
    let body = &data[128..];
    if pf_flags & DDPF_FOURCC != 0 {
        match fourcc {
            b"DXT1" => blocks(body, width, height, 8, |b, out| color_block(&b[..8], true, out)),
            b"DXT3" => blocks(body, width, height, 16, |b, out| {
                color_block(&b[8..], false, out);
                for (i, px) in out.iter_mut().enumerate() {
                    let nibble = (b[i / 2] >> (4 * (i % 2))) & 0xf;
                    px[3] = nibble * 17;
                }
            }),
            b"DXT5" => blocks(body, width, height, 16, |b, out| {
                color_block(&b[8..], false, out);
                let alphas = alpha_palette(b[0], b[1]);
                let bits = u64::from_le_bytes([b[2], b[3], b[4], b[5], b[6], b[7], 0, 0]);
                for (i, px) in out.iter_mut().enumerate() {
                    px[3] = alphas[((bits >> (3 * i)) & 7) as usize];
                }
            }),
            other => bail!("unsupported DDS format {:?}", String::from_utf8_lossy(other)),
        }
    } else if pf_flags & DDPF_RGB != 0 {
        let bits = u32_at(88);
        let masks = [u32_at(92), u32_at(96), u32_at(100), if pf_flags & DDPF_ALPHAPIXELS != 0 { u32_at(104) } else { 0 }];
        uncompressed(body, width, height, bits, masks)
    } else {
        bail!("unsupported DDS pixel format (flags {pf_flags:#x})")
    }
}

fn uncompressed(body: &[u8], width: u32, height: u32, bits: u32, masks: [u32; 4]) -> Result<RgbaImage> {
    ensure!(matches!(bits, 16 | 24 | 32), "unsupported DDS bit count {bits}");
    let bpp = (bits / 8) as usize;
    let pitch = width as usize * bpp;
    ensure!(body.len() >= pitch * height as usize, "truncated DDS");
    let mut img = RgbaImage::new(width, height);
    for (y, row) in body.chunks_exact(pitch).take(height as usize).enumerate() {
        for (x, px) in row.chunks_exact(bpp).enumerate() {
            let mut v = [0u8; 4];
            v[..bpp].copy_from_slice(px);
            let v = u32::from_le_bytes(v);
            let mut out = [0u8, 0, 0, 255];
            for (c, &mask) in masks.iter().enumerate() {
                if mask != 0 {
                    out[c] = expand((v & mask) >> mask.trailing_zeros(), mask.count_ones());
                }
            }
            img.put_pixel(x as u32, y as u32, image::Rgba(out));
        }
    }
    Ok(img)
}

/// Scale an `n`-bit channel value to 8 bits.
fn expand(v: u32, n: u32) -> u8 {
    match n {
        0 => 0,
        8 => v as u8,
        n if n > 8 => (v >> (n - 8)) as u8,
        n => ((v * 255 + ((1 << n) - 1) / 2) / ((1 << n) - 1)) as u8,
    }
}

/// Decode 4x4 blocks of `block_size` bytes; `f` fills the block's 16 pixels (row-major).
fn blocks(body: &[u8], width: u32, height: u32, block_size: usize, f: impl Fn(&[u8], &mut [[u8; 4]; 16])) -> Result<RgbaImage> {
    let (bw, bh) = (width.div_ceil(4), height.div_ceil(4));
    ensure!(body.len() >= (bw * bh) as usize * block_size, "truncated DDS");
    let mut img = RgbaImage::new(width, height);
    let mut px = [[0u8; 4]; 16];
    for (i, b) in body.chunks_exact(block_size).take((bw * bh) as usize).enumerate() {
        f(b, &mut px);
        let (bx, by) = ((i as u32 % bw) * 4, (i as u32 / bw) * 4);
        for (j, p) in px.iter().enumerate() {
            let (x, y) = (bx + j as u32 % 4, by + j as u32 / 4);
            if x < width && y < height {
                img.put_pixel(x, y, image::Rgba(*p));
            }
        }
    }
    Ok(img)
}

fn rgb565(c: u16) -> [u16; 3] {
    let (r, g, b) = ((c >> 11) & 31, (c >> 5) & 63, c & 31);
    [(r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2)]
}

/// BC1 colour block. `dxt1` enables the 3-colour + transparent mode (c0 <= c1).
fn color_block(b: &[u8], dxt1: bool, out: &mut [[u8; 4]; 16]) {
    let (c0, c1) = (u16::from_le_bytes([b[0], b[1]]), u16::from_le_bytes([b[2], b[3]]));
    let (p0, p1) = (rgb565(c0), rgb565(c1));
    let mix = |w0: u16, w1: u16, d: u16| -> [u8; 4] {
        let ch = |i: usize| ((p0[i] * w0 + p1[i] * w1) / d) as u8;
        [ch(0), ch(1), ch(2), 255]
    };
    let palette = if c0 > c1 || !dxt1 {
        [mix(1, 0, 1), mix(0, 1, 1), mix(2, 1, 3), mix(1, 2, 3)]
    } else {
        [mix(1, 0, 1), mix(0, 1, 1), mix(1, 1, 2), [0, 0, 0, 0]]
    };
    let idx = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    for (i, px) in out.iter_mut().enumerate() {
        *px = palette[((idx >> (2 * i)) & 3) as usize];
    }
}

fn alpha_palette(a0: u8, a1: u8) -> [u8; 8] {
    let (a0w, a1w) = (a0 as u16, a1 as u16);
    let mut p = [a0, a1, 0, 0, 0, 0, 0, 0];
    if a0 > a1 {
        for i in 1..7u16 {
            p[i as usize + 1] = (((7 - i) * a0w + i * a1w) / 7) as u8;
        }
    } else {
        for i in 1..5u16 {
            p[i as usize + 1] = (((5 - i) * a0w + i * a1w) / 5) as u8;
        }
        p[6] = 0;
        p[7] = 255;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(width: u32, height: u32, pf_flags: u32, fourcc: &[u8; 4], bits: u32, masks: [u32; 4]) -> Vec<u8> {
        let mut h = vec![0u8; 128];
        h[..4].copy_from_slice(b"DDS ");
        h[4..8].copy_from_slice(&124u32.to_le_bytes());
        h[12..16].copy_from_slice(&height.to_le_bytes());
        h[16..20].copy_from_slice(&width.to_le_bytes());
        h[76..80].copy_from_slice(&32u32.to_le_bytes());
        h[80..84].copy_from_slice(&pf_flags.to_le_bytes());
        h[84..88].copy_from_slice(fourcc);
        h[88..92].copy_from_slice(&bits.to_le_bytes());
        for (i, m) in masks.iter().enumerate() {
            h[92 + 4 * i..96 + 4 * i].copy_from_slice(&m.to_le_bytes());
        }
        h
    }

    #[test]
    fn bgra32() {
        let mut d = header(2, 1, DDPF_RGB | DDPF_ALPHAPIXELS, b"\0\0\0\0", 32, [0xff0000, 0xff00, 0xff, 0xff000000]);
        d.extend_from_slice(&[1, 2, 3, 4, 10, 20, 30, 40]); // B G R A
        let img = decode(&d).unwrap();
        assert_eq!(img.get_pixel(0, 0).0, [3, 2, 1, 4]);
        assert_eq!(img.get_pixel(1, 0).0, [30, 20, 10, 40]);
    }

    #[test]
    fn dxt1_modes() {
        // 4-colour block: c0 = white > c1 = black, index 2 = 2/3 white
        let mut d = header(4, 4, DDPF_FOURCC, b"DXT1", 0, [0; 4]);
        d.extend_from_slice(&[0xff, 0xff, 0, 0, 0b1110_0100, 0, 0, 0]);
        let img = decode(&d).unwrap();
        assert_eq!(img.get_pixel(0, 0).0, [255, 255, 255, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 0, 255]);
        assert_eq!(img.get_pixel(2, 0).0, [170, 170, 170, 255]);
        assert_eq!(img.get_pixel(3, 0).0, [85, 85, 85, 255]);
        // 3-colour block: c0 <= c1, index 3 = transparent
        let mut d = header(4, 4, DDPF_FOURCC, b"DXT1", 0, [0; 4]);
        d.extend_from_slice(&[0, 0, 0xff, 0xff, 0b1100_0000, 0, 0, 0]);
        assert_eq!(decode(&d).unwrap().get_pixel(3, 0).0, [0, 0, 0, 0]);
    }

    #[test]
    fn dxt5_alpha() {
        let mut d = header(4, 4, DDPF_FOURCC, b"DXT5", 0, [0; 4]);
        // a0=255 a1=0, pixel 0 index 0, pixel 1 index 1, pixel 2 index 2 (6/7*255)
        d.extend_from_slice(&[255, 0, 0b10_001_000, 0, 0, 0, 0, 0]);
        d.extend_from_slice(&[0xff, 0xff, 0, 0, 0, 0, 0, 0]);
        let img = decode(&d).unwrap();
        assert_eq!(img.get_pixel(0, 0).0[3], 255);
        assert_eq!(img.get_pixel(1, 0).0[3], 0);
        assert_eq!(img.get_pixel(2, 0).0[3], 218);
    }
}
