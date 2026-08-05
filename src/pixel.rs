//! RFB pixel formats and conversion from the internal framebuffer format.
//!
//! Internally every pixel is a `u32` in `0x00RRGGBB`. Clients may ask for a
//! different layout through `SetPixelFormat`, so everything that touches the
//! wire goes through [`PixCodec`].

/// RFB `PIXEL_FORMAT` (16 bytes on the wire).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_colour: bool,
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

impl PixelFormat {
    /// What we advertise in `ServerInit`: 32bpp little-endian `0x00RRGGBB`,
    /// i.e. the byte order B, G, R, unused.
    pub const fn default_rgb() -> Self {
        PixelFormat {
            bits_per_pixel: 32,
            depth: 24,
            big_endian: false,
            true_colour: true,
            red_max: 255,
            green_max: 255,
            blue_max: 255,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
        }
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(self.bits_per_pixel);
        out.push(self.depth);
        out.push(self.big_endian as u8);
        out.push(self.true_colour as u8);
        out.extend_from_slice(&self.red_max.to_be_bytes());
        out.extend_from_slice(&self.green_max.to_be_bytes());
        out.extend_from_slice(&self.blue_max.to_be_bytes());
        out.push(self.red_shift);
        out.push(self.green_shift);
        out.push(self.blue_shift);
        out.extend_from_slice(&[0, 0, 0]); // padding
    }

    /// Parse the 16-byte wire form. Returns `None` for formats we cannot
    /// serve (colour maps, odd bit depths).
    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < 16 {
            return None;
        }
        let pf = PixelFormat {
            bits_per_pixel: b[0],
            depth: b[1],
            big_endian: b[2] != 0,
            true_colour: b[3] != 0,
            red_max: u16::from_be_bytes([b[4], b[5]]),
            green_max: u16::from_be_bytes([b[6], b[7]]),
            blue_max: u16::from_be_bytes([b[8], b[9]]),
            red_shift: b[10],
            green_shift: b[11],
            blue_shift: b[12],
        };
        if !pf.true_colour {
            return None;
        }
        if !matches!(pf.bits_per_pixel, 8 | 16 | 32) {
            return None;
        }
        if pf.red_max == 0 || pf.green_max == 0 || pf.blue_max == 0 {
            return None;
        }
        // Channels must fit inside the pixel.
        for (max, shift) in [
            (pf.red_max, pf.red_shift),
            (pf.green_max, pf.green_shift),
            (pf.blue_max, pf.blue_shift),
        ] {
            let hi = shift as u32 + 32 - (max as u32).leading_zeros();
            if hi > pf.bits_per_pixel as u32 {
                return None;
            }
        }
        Some(pf)
    }

    pub fn bytes_per_pixel(&self) -> usize {
        self.bits_per_pixel as usize / 8
    }
}

/// Converts internal `0x00RRGGBB` pixels into a client's pixel format.
pub struct PixCodec {
    pf: PixelFormat,
    bpp: usize,
    /// Size of a ZRLE CPIXEL (3 when the format has a spare byte, else `bpp`).
    cpixel: usize,
    /// Offset of the CPIXEL bytes inside the 4-byte wire pixel.
    cpixel_off: usize,
    /// True when the internal layout is byte-identical to the wire layout, so
    /// whole rows can be copied instead of converted pixel by pixel.
    direct: bool,
}

impl PixCodec {
    pub fn new(pf: PixelFormat) -> Self {
        let bpp = pf.bytes_per_pixel();
        let mask = (pf.red_max as u32) << pf.red_shift
            | (pf.green_max as u32) << pf.green_shift
            | (pf.blue_max as u32) << pf.blue_shift;

        // CPIXEL: 32bpp with depth <= 24 and all significant bits in three of
        // the four bytes (RFB spec, ZRLE encoding).
        let (cpixel, cpixel_off) = if pf.bits_per_pixel == 32 && pf.depth <= 24 {
            if mask & 0xff00_0000 == 0 {
                (3, if pf.big_endian { 1 } else { 0 })
            } else if mask & 0x0000_00ff == 0 {
                (3, if pf.big_endian { 0 } else { 1 })
            } else {
                (4, 0)
            }
        } else {
            (bpp, 0)
        };

        let direct = pf.bits_per_pixel == 32
            && !pf.big_endian
            && (pf.red_max, pf.green_max, pf.blue_max) == (255, 255, 255)
            && (pf.red_shift, pf.green_shift, pf.blue_shift) == (16, 8, 0);

        PixCodec { pf, bpp, cpixel, cpixel_off, direct }
    }

    pub fn format(&self) -> PixelFormat {
        self.pf
    }

    #[cfg(test)]
    pub fn bytes_per_pixel(&self) -> usize {
        self.bpp
    }

    pub fn cpixel_size(&self) -> usize {
        self.cpixel
    }

    /// The 4-byte wire representation of a pixel; only the first
    /// `bytes_per_pixel()` bytes are meaningful (for `cpixel`, see
    /// [`PixCodec::push_cpixel`]).
    #[inline]
    fn wire(&self, px: u32) -> [u8; 4] {
        let r = (px >> 16) & 0xff;
        let g = (px >> 8) & 0xff;
        let b = px & 0xff;
        let v = scale(r, self.pf.red_max) << self.pf.red_shift
            | scale(g, self.pf.green_max) << self.pf.green_shift
            | scale(b, self.pf.blue_max) << self.pf.blue_shift;
        if self.pf.big_endian {
            // Big endian: the most significant byte of the pixel goes first,
            // so shift the value up into the top of the word.
            (v << (32 - self.pf.bits_per_pixel as u32)).to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    }

    #[inline]
    pub fn push_pixel(&self, px: u32, out: &mut Vec<u8>) {
        let w = self.wire(px);
        out.extend_from_slice(&w[..self.bpp]);
    }

    #[inline]
    pub fn push_cpixel(&self, px: u32, out: &mut Vec<u8>) {
        if self.cpixel == self.bpp && self.cpixel_off == 0 {
            self.push_pixel(px, out);
            return;
        }
        let w = self.wire(px);
        out.extend_from_slice(&w[self.cpixel_off..self.cpixel_off + self.cpixel]);
    }

    /// Append `len` pixels starting at `src` in wire format.
    pub fn push_row(&self, src: &[u32], out: &mut Vec<u8>) {
        if self.direct {
            out.reserve(src.len() * 4);
            for &px in src {
                out.extend_from_slice(&px.to_le_bytes());
            }
        } else {
            out.reserve(src.len() * self.bpp);
            for &px in src {
                self.push_pixel(px, out);
            }
        }
    }
}

/// Scale an 8-bit channel to an arbitrary maximum, with rounding.
#[inline]
fn scale(v: u32, max: u16) -> u32 {
    if max == 255 {
        v
    } else {
        (v * max as u32 + 127) / 255
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_format_roundtrips() {
        let pf = PixelFormat::default_rgb();
        let mut buf = Vec::new();
        pf.write(&mut buf);
        assert_eq!(buf.len(), 16);
        assert_eq!(PixelFormat::parse(&buf), Some(pf));
    }

    #[test]
    fn bgrx_is_direct_and_three_byte_cpixel() {
        let c = PixCodec::new(PixelFormat::default_rgb());
        assert!(c.direct);
        assert_eq!(c.cpixel_size(), 3);
        let mut out = Vec::new();
        c.push_pixel(0x00_11_22_33, &mut out);
        assert_eq!(out, vec![0x33, 0x22, 0x11, 0x00]);
        out.clear();
        c.push_cpixel(0x00_11_22_33, &mut out);
        assert_eq!(out, vec![0x33, 0x22, 0x11]);
    }

    #[test]
    fn rgb565_conversion() {
        let pf = PixelFormat {
            bits_per_pixel: 16,
            depth: 16,
            big_endian: false,
            true_colour: true,
            red_max: 31,
            green_max: 63,
            blue_max: 31,
            red_shift: 11,
            green_shift: 5,
            blue_shift: 0,
        };
        let c = PixCodec::new(pf);
        assert_eq!(c.bytes_per_pixel(), 2);
        let mut out = Vec::new();
        c.push_pixel(0x00_ff_ff_ff, &mut out);
        assert_eq!(out, vec![0xff, 0xff]);
        out.clear();
        c.push_pixel(0x00_ff_00_00, &mut out);
        assert_eq!(u16::from_le_bytes([out[0], out[1]]), 31 << 11);
    }

    #[test]
    fn big_endian_pixels() {
        let pf = PixelFormat { big_endian: true, ..PixelFormat::default_rgb() };
        let c = PixCodec::new(pf);
        let mut out = Vec::new();
        c.push_pixel(0x00_11_22_33, &mut out);
        assert_eq!(out, vec![0x00, 0x11, 0x22, 0x33]);
        out.clear();
        // Significant bytes are the low three, which sit last in big endian.
        c.push_cpixel(0x00_11_22_33, &mut out);
        assert_eq!(out, vec![0x11, 0x22, 0x33]);
    }

    #[test]
    fn rejects_colour_map_and_odd_depths() {
        let mut buf = Vec::new();
        PixelFormat { true_colour: false, ..PixelFormat::default_rgb() }.write(&mut buf);
        assert!(PixelFormat::parse(&buf).is_none());
        buf.clear();
        PixelFormat { bits_per_pixel: 24, ..PixelFormat::default_rgb() }.write(&mut buf);
        assert!(PixelFormat::parse(&buf).is_none());
    }
}
