//! ZRLE encoder (RFB encoding 16).
//!
//! The rectangle is split into 64x64 tiles; each tile is emitted as a solid
//! colour, a packed palette, a palette RLE or raw pixels, whichever is
//! smallest, and the whole stream goes through one zlib stream that lives as
//! long as the client session.

use flate2::{Compress, Compression, FlushCompress};

use crate::pixel::PixCodec;
use crate::screen::{Frame, Rect, TILE};

/// Largest palette we will build before giving up and emitting raw pixels.
const MAX_PALETTE: usize = 127;

pub struct ZrleEncoder {
    comp: Compress,
    raw: Vec<u8>,
    zbuf: Vec<u8>,
    tile: Vec<u32>,
    palette: Vec<u32>,
    indices: Vec<u8>,
}

impl ZrleEncoder {
    pub fn new() -> Self {
        ZrleEncoder {
            comp: Compress::new(Compression::new(1), true),
            raw: Vec::new(),
            zbuf: Vec::new(),
            tile: Vec::with_capacity((TILE * TILE) as usize),
            palette: Vec::with_capacity(MAX_PALETTE + 1),
            indices: Vec::with_capacity((TILE * TILE) as usize),
        }
    }

    /// Append the ZRLE body (4-byte length + zlib data) for `r` to `out`.
    pub fn encode(&mut self, frame: &Frame, codec: &PixCodec, r: Rect, out: &mut Vec<u8>) {
        self.raw.clear();
        for ty in (r.y..r.y + r.h).step_by(TILE as usize) {
            let th = TILE.min(r.y + r.h - ty);
            for tx in (r.x..r.x + r.w).step_by(TILE as usize) {
                let tw = TILE.min(r.x + r.w - tx);
                self.tile.clear();
                for row in 0..th {
                    self.tile.extend_from_slice(frame.row(ty + row, tx, tw));
                }
                encode_tile(
                    &self.tile,
                    tw as usize,
                    th as usize,
                    codec,
                    &mut self.palette,
                    &mut self.indices,
                    &mut self.raw,
                );
            }
        }

        // One zlib stream per session, flushed after every rectangle.
        self.zbuf.clear();
        let mut consumed = 0usize;
        loop {
            if self.zbuf.capacity() - self.zbuf.len() < 1024 {
                self.zbuf.reserve(self.raw.len().max(4096));
            }
            let space = self.zbuf.capacity() - self.zbuf.len();
            let in0 = self.comp.total_in();
            let out0 = self.comp.total_out();
            let res = self
                .comp
                .compress_vec(&self.raw[consumed..], &mut self.zbuf, FlushCompress::Sync);
            if res.is_err() {
                break;
            }
            consumed += (self.comp.total_in() - in0) as usize;
            let produced = (self.comp.total_out() - out0) as usize;
            if consumed >= self.raw.len() && produced < space {
                break;
            }
        }

        out.extend_from_slice(&(self.zbuf.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.zbuf);
    }
}

fn encode_tile(
    tile: &[u32],
    w: usize,
    h: usize,
    codec: &PixCodec,
    palette: &mut Vec<u32>,
    indices: &mut Vec<u8>,
    out: &mut Vec<u8>,
) {
    palette.clear();
    indices.clear();

    // Build the palette with a one-entry cache; screen content is extremely
    // local, so this is usually a single comparison per pixel.
    let mut last = 0usize;
    let mut overflow = false;
    for &px in tile {
        if !palette.is_empty() && palette[last] == px {
            indices.push(last as u8);
            continue;
        }
        match palette.iter().position(|&p| p == px) {
            Some(i) => {
                last = i;
                indices.push(i as u8);
            }
            None => {
                if palette.len() >= MAX_PALETTE {
                    overflow = true;
                    break;
                }
                palette.push(px);
                last = palette.len() - 1;
                indices.push(last as u8);
            }
        }
    }

    if overflow {
        out.push(0); // raw
        for &px in tile {
            codec.push_cpixel(px, out);
        }
        return;
    }

    match palette.len() {
        1 => {
            out.push(1); // solid
            codec.push_cpixel(palette[0], out);
        }
        2..=16 => {
            out.push(palette.len() as u8);
            for &px in palette.iter() {
                codec.push_cpixel(px, out);
            }
            let bits = match palette.len() {
                2 => 1,
                3..=4 => 2,
                _ => 4,
            };
            pack_indices(indices, w, h, bits, out);
        }
        _ => {
            let rle_len = rle_size(indices);
            let packed_len = palette.len() * codec.cpixel_size() + w.div_ceil(2) * h;
            if rle_len < packed_len {
                out.push(128 + palette.len() as u8);
                for &px in palette.iter() {
                    codec.push_cpixel(px, out);
                }
                write_palette_rle(indices, out);
            } else {
                out.push(0);
                for &px in tile {
                    codec.push_cpixel(px, out);
                }
            }
        }
    }
}

/// Pack palette indices, `bits` per pixel, MSB first, each row padded to a
/// whole byte.
fn pack_indices(indices: &[u8], w: usize, h: usize, bits: u32, out: &mut Vec<u8>) {
    for row in 0..h {
        let mut acc: u32 = 0;
        let mut filled = 0u32;
        for col in 0..w {
            acc = acc << bits | indices[row * w + col] as u32;
            filled += bits;
            if filled == 8 {
                out.push(acc as u8);
                acc = 0;
                filled = 0;
            }
        }
        if filled > 0 {
            out.push((acc << (8 - filled)) as u8);
        }
    }
}

/// Byte count a palette RLE body would take, used to pick the cheaper coding.
fn rle_size(indices: &[u8]) -> usize {
    let mut size = 0;
    let mut i = 0;
    while i < indices.len() {
        let v = indices[i];
        let mut run = 1;
        while i + run < indices.len() && indices[i + run] == v {
            run += 1;
        }
        size += 1 + if run > 1 { 1 + (run - 1) / 255 } else { 0 };
        i += run;
    }
    size
}

fn write_palette_rle(indices: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < indices.len() {
        let v = indices[i];
        let mut run = 1;
        while i + run < indices.len() && indices[i + run] == v {
            run += 1;
        }
        i += run;
        if run == 1 {
            out.push(v);
        } else {
            out.push(v | 0x80);
            let mut len = run - 1;
            while len >= 255 {
                out.push(255);
                len -= 255;
            }
            out.push(len as u8);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pixel::PixelFormat;

    fn codec() -> PixCodec {
        PixCodec::new(PixelFormat::default_rgb())
    }

    #[test]
    fn solid_tile_is_one_subencoding_plus_pixel() {
        let tile = vec![0x00_11_22_33u32; 64 * 64];
        let mut out = Vec::new();
        encode_tile(&tile, 64, 64, &codec(), &mut Vec::new(), &mut Vec::new(), &mut out);
        assert_eq!(out, vec![1, 0x33, 0x22, 0x11]);
    }

    #[test]
    fn two_colour_tile_packs_one_bit_per_pixel() {
        let mut tile = vec![0u32; 8 * 2];
        for i in 0..8 {
            tile[i] = if i % 2 == 0 { 0 } else { 0xffffff };
        }
        let mut out = Vec::new();
        encode_tile(&tile, 8, 2, &codec(), &mut Vec::new(), &mut Vec::new(), &mut out);
        // subencoding 2, two 3-byte palette entries, then one byte per row.
        assert_eq!(out[0], 2);
        assert_eq!(out.len(), 1 + 6 + 2);
        assert_eq!(out[7], 0b0101_0101);
        assert_eq!(out[8], 0b0000_0000);
    }

    #[test]
    fn noise_tile_falls_back_to_raw() {
        let tile: Vec<u32> = (0..64 * 64).map(|i| i as u32 * 7919 & 0xffffff).collect();
        let mut out = Vec::new();
        encode_tile(&tile, 64, 64, &codec(), &mut Vec::new(), &mut Vec::new(), &mut out);
        assert_eq!(out[0], 0);
        assert_eq!(out.len(), 1 + 64 * 64 * 3);
    }

    #[test]
    fn rle_run_lengths_use_255_chunks() {
        let mut out = Vec::new();
        write_palette_rle(&vec![3u8; 300], &mut out);
        // index|0x80, then 299 encoded as 255 + 44
        assert_eq!(out, vec![0x83, 255, 44]);
    }

    #[test]
    fn encoded_rect_is_a_valid_zlib_stream() {
        let mut frame = Frame::new(128, 128);
        for (i, px) in frame.px.iter_mut().enumerate() {
            *px = (i as u32 % 3) * 0x404040;
        }
        let mut enc = ZrleEncoder::new();
        let mut out = Vec::new();
        enc.encode(&frame, &codec(), Rect { x: 0, y: 0, w: 128, h: 128 }, &mut out);
        let len = u32::from_be_bytes([out[0], out[1], out[2], out[3]]) as usize;
        assert_eq!(len + 4, out.len());
        let mut d = flate2::Decompress::new(true);
        let mut plain = Vec::with_capacity(1 << 20);
        d.decompress_vec(&out[4..], &mut plain, flate2::FlushDecompress::Sync).unwrap();
        // 4 tiles, each with at least a subencoding byte.
        assert!(plain.len() > 4);
    }
}
