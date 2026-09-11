//! An off-screen pixel buffer in the framebuffer's native format that
//! implements the embedded-graphics `DrawTarget`, plus a scaling adapter for
//! big text. Software only, no allocation while drawing.

use core::convert::Infallible;

use embedded_graphics::pixelcolor::{Rgb888, RgbColor};
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::Rectangle;

/// How one colour channel is packed into a pixel word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Channel {
    pub offset: u32,
    pub length: u32,
}

impl Channel {
    #[inline]
    fn pack(&self, v: u8) -> u32 {
        ((v as u32) >> (8 - self.length.min(8))) << self.offset
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelFormat {
    pub bytes_per_pixel: usize,
    pub red: Channel,
    pub green: Channel,
    pub blue: Channel,
}

impl PixelFormat {
    pub const XRGB8888: PixelFormat = PixelFormat {
        bytes_per_pixel: 4,
        red: Channel { offset: 16, length: 8 },
        green: Channel { offset: 8, length: 8 },
        blue: Channel { offset: 0, length: 8 },
    };
    pub const RGB565: PixelFormat = PixelFormat {
        bytes_per_pixel: 2,
        red: Channel { offset: 11, length: 5 },
        green: Channel { offset: 5, length: 6 },
        blue: Channel { offset: 0, length: 5 },
    };

    #[inline]
    pub fn encode(&self, c: Rgb888) -> u32 {
        self.red.pack(c.r()) | self.green.pack(c.g()) | self.blue.pack(c.b())
    }
}

/// Rotation of the drawing relative to the physical framebuffer, clockwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rotation {
    R0,
    R90,
    R180,
    R270,
}

impl Rotation {
    pub fn from_degrees(d: u32) -> Option<Self> {
        match d % 360 {
            0 => Some(Rotation::R0),
            90 => Some(Rotation::R90),
            180 => Some(Rotation::R180),
            270 => Some(Rotation::R270),
            _ => None,
        }
    }
}

pub struct Canvas {
    /// Physical framebuffer geometry.
    phys_w: usize,
    phys_h: usize,
    line_length: usize,
    format: PixelFormat,
    rotation: Rotation,
    /// Logical (drawing) size.
    width: usize,
    height: usize,
    buf: Vec<u8>,
}

impl Canvas {
    pub fn new(phys_w: usize, phys_h: usize, line_length: usize, format: PixelFormat, rotation: Rotation) -> Self {
        let (width, height) = match rotation {
            Rotation::R0 | Rotation::R180 => (phys_w, phys_h),
            Rotation::R90 | Rotation::R270 => (phys_h, phys_w),
        };
        Self { phys_w, phys_h, line_length, format, rotation, width, height, buf: vec![0; line_length * phys_h] }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn rotation(&self) -> Rotation {
        self.rotation
    }

    /// Raw bytes in framebuffer layout (line_length * physical height).
    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Map a physical touch coordinate to the logical drawing space.
    pub fn touch_to_logical(&self, px: i32, py: i32) -> (i32, i32) {
        let (w, h) = (self.phys_w as i32, self.phys_h as i32);
        match self.rotation {
            Rotation::R0 => (px, py),
            Rotation::R90 => (py, w - 1 - px),
            Rotation::R180 => (w - 1 - px, h - 1 - py),
            Rotation::R270 => (h - 1 - py, px),
        }
    }

    #[inline]
    fn phys(&self, x: usize, y: usize) -> (usize, usize) {
        match self.rotation {
            Rotation::R0 => (x, y),
            Rotation::R90 => (self.phys_w - 1 - y, x),
            Rotation::R180 => (self.phys_w - 1 - x, self.phys_h - 1 - y),
            Rotation::R270 => (y, self.phys_h - 1 - x),
        }
    }

    #[inline]
    fn put_raw(&mut self, px: usize, py: usize, v: u32) {
        let bpp = self.format.bytes_per_pixel;
        let i = py * self.line_length + px * bpp;
        let b = v.to_le_bytes();
        self.buf[i..i + bpp].copy_from_slice(&b[..bpp]);
    }

    #[inline]
    fn put(&mut self, x: usize, y: usize, v: u32) {
        let (px, py) = self.phys(x, y);
        self.put_raw(px, py, v);
    }

    /// Export as binary PPM (for snapshots and tests).
    pub fn to_ppm(&self) -> Vec<u8> {
        let mut out = format!("P6\n{} {}\n255\n", self.width, self.height).into_bytes();
        let f = self.format;
        for y in 0..self.height {
            for x in 0..self.width {
                let (px, py) = self.phys(x, y);
                let i = py * self.line_length + px * f.bytes_per_pixel;
                let mut w = [0u8; 4];
                w[..f.bytes_per_pixel].copy_from_slice(&self.buf[i..i + f.bytes_per_pixel]);
                let v = u32::from_le_bytes(w);
                let unpack = |c: Channel| -> u8 {
                    let m = (1u32 << c.length) - 1;
                    let raw = (v >> c.offset) & m;
                    ((raw * 255) / m.max(1)) as u8
                };
                out.extend_from_slice(&[unpack(f.red), unpack(f.green), unpack(f.blue)]);
            }
        }
        out
    }
}

impl OriginDimensions for Canvas {
    fn size(&self) -> Size {
        Size::new(self.width as u32, self.height as u32)
    }
}

impl DrawTarget for Canvas {
    type Color = Rgb888;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        let (w, h) = (self.width as i32, self.height as i32);
        for Pixel(p, c) in pixels {
            if p.x >= 0 && p.y >= 0 && p.x < w && p.y < h {
                let v = self.format.encode(c);
                self.put(p.x as usize, p.y as usize, v);
            }
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        let area = area.intersection(&self.bounding_box());
        let Some(br) = area.bottom_right() else { return Ok(()) };
        let v = self.format.encode(color);
        let (x0, y0) = (area.top_left.x as usize, area.top_left.y as usize);
        let (x1, y1) = (br.x as usize, br.y as usize);
        if self.rotation == Rotation::R0 {
            let bpp = self.format.bytes_per_pixel;
            let b = v.to_le_bytes();
            for y in y0..=y1 {
                let start = y * self.line_length + x0 * bpp;
                let row = &mut self.buf[start..start + (x1 - x0 + 1) * bpp];
                for px in row.chunks_exact_mut(bpp) {
                    px.copy_from_slice(&b[..bpp]);
                }
            }
        } else {
            for y in y0..=y1 {
                for x in x0..=x1 {
                    self.put(x, y, v);
                }
            }
        }
        Ok(())
    }

    fn clear(&mut self, color: Self::Color) -> Result<(), Self::Error> {
        self.fill_solid(&self.bounding_box(), color)
    }
}

/// Draws into `target` with every pixel enlarged to a `k`×`k` block. Used to
/// render the bitmap fonts at display size.
pub struct Scaled<'a, T> {
    pub target: &'a mut T,
    pub k: u32,
    pub origin: Point,
}

impl<'a, T: DrawTarget<Color = Rgb888>> OriginDimensions for Scaled<'a, T> {
    fn size(&self) -> Size {
        let s = self.target.bounding_box().size;
        Size::new(s.width / self.k.max(1), s.height / self.k.max(1))
    }
}

impl<'a, T: DrawTarget<Color = Rgb888>> DrawTarget for Scaled<'a, T> {
    type Color = Rgb888;
    type Error = T::Error;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        let k = self.k as i32;
        for Pixel(p, c) in pixels {
            let r = Rectangle::new(self.origin + Point::new(p.x * k, p.y * k), Size::new(self.k, self.k));
            self.target.fill_solid(&r, c)?;
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        let k = self.k as i32;
        let r = Rectangle::new(self.origin + Point::new(area.top_left.x * k, area.top_left.y * k), area.size * self.k);
        self.target.fill_solid(&r, color)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_graphics::primitives::PrimitiveStyle;

    #[test]
    fn encodes_rgb565_and_xrgb() {
        let c = Rgb888::new(255, 128, 8);
        assert_eq!(PixelFormat::XRGB8888.encode(c), 0x00FF_8008);
        assert_eq!(PixelFormat::RGB565.encode(c), (31 << 11) | (32 << 5) | 1);
    }

    #[test]
    fn rotation_maps_touch_and_pixels_consistently() {
        for rot in [Rotation::R0, Rotation::R90, Rotation::R180, Rotation::R270] {
            let mut c = Canvas::new(8, 4, 8 * 4, PixelFormat::XRGB8888, rot);
            // Paint logical pixel (1, 2) and find it in the physical buffer.
            Rectangle::new(Point::new(1, 2), Size::new(1, 1))
                .into_styled(PrimitiveStyle::with_fill(Rgb888::WHITE))
                .draw(&mut c)
                .unwrap();
            let mut found = None;
            for py in 0..4 {
                for px in 0..8 {
                    let i = py * 32 + px * 4;
                    if c.bytes()[i] != 0 {
                        found = Some((px as i32, py as i32));
                    }
                }
            }
            let (px, py) = found.expect("pixel painted");
            assert_eq!(c.touch_to_logical(px, py), (1, 2), "rotation {rot:?}");
        }
    }

    #[test]
    fn fill_is_clipped() {
        let mut c = Canvas::new(4, 4, 16, PixelFormat::XRGB8888, Rotation::R0);
        c.fill_solid(&Rectangle::new(Point::new(-2, -2), Size::new(100, 100)), Rgb888::RED).unwrap();
        assert!(c.bytes().chunks(4).all(|p| p == [0, 0, 255, 0]));
    }
}
