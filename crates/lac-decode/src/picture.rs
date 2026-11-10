//! Decoded video pictures and their conversion to RGB.
//!
//! A [`Frame`] holds a picture as the decoder produced it: one to four
//! planes in FFmpeg's pixel format, described by a [`PixelLayout`] (a copy
//! of the format's `AVPixFmtDescriptor`). The conversion to 8-bit RGB is
//! our own code, so that this crate needs libavcodec only (no libswscale):
//! it reads each component through the layout, which covers every planar,
//! semi-planar (NV12) and packed format of 8 to 16 bits, and applies the
//! YCbCr matrix the stream names (ITU-R BT.601, BT.709, BT.2020).
//!
//! The conversion is meant for a preview: chroma is taken from the
//! nearest sample (no interpolation), and HDR transfer functions (PQ,
//! HLG) are not tone-mapped, so HDR videos look pale.

use lac_core::MediaTime;

/// Where one component (Y, Cb, Cr, alpha; or R, G, B) of a pixel is
/// stored, as in FFmpeg's `AVComponentDescriptor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Component {
    /// The plane holding the component.
    pub plane: usize,
    /// Bytes from one pixel's component to the next pixel's.
    pub step: usize,
    /// Bytes before the first pixel's component.
    pub offset: usize,
    /// Bits the value is shifted left by in its storage unit.
    pub shift: u32,
    /// Significant bits of the value.
    pub depth: u32,
}

/// The memory layout of a pixel format (FFmpeg's `AVPixFmtDescriptor`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PixelLayout {
    /// FFmpeg's name of the format, e.g. `yuv420p`.
    pub name: String,
    /// The components: Y, Cb, Cr (and alpha) for YCbCr formats, R, G, B
    /// (and alpha) for RGB formats, Y (and alpha) for gray formats.
    pub components: Vec<Component>,
    /// Chroma planes are this many times (as a power of 2) narrower...
    pub log2_chroma_w: u32,
    /// ...and lower than the picture.
    pub log2_chroma_h: u32,
    /// The components are R, G and B.
    pub rgb: bool,
    /// Values wider than 8 bits are stored big-endian.
    pub big_endian: bool,
    /// The JPEG ("full range") variant of a YCbCr format (`yuvj420p`).
    pub full_range: bool,
}

/// The YCbCr matrix of a picture (ITU-T H.273 `matrix_coefficients`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matrix {
    /// ITU-R BT.601 (SD video, JPEG).
    Bt601,
    /// ITU-R BT.709 (HD video).
    Bt709,
    /// ITU-R BT.2020 non-constant luminance (UHD and HDR video).
    Bt2020,
}

impl Matrix {
    /// The luma weights of red and blue (Kr, Kb).
    fn weights(self) -> (f32, f32) {
        match self {
            Self::Bt601 => (0.299, 0.114),
            Self::Bt709 => (0.2126, 0.0722),
            Self::Bt2020 => (0.2627, 0.0593),
        }
    }
}

/// One plane of a [`Frame`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plane {
    /// The plane's bytes, `stride` per row.
    pub data: Vec<u8>,
    /// Bytes from one row to the next.
    pub stride: usize,
    /// Bytes of picture data in a row (the rest is padding).
    pub row_bytes: usize,
    /// Rows of the plane.
    pub rows: usize,
}

/// A decoded picture in the decoder's pixel format.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    /// Presentation time (ticks of the video timescale).
    pub time: MediaTime,
    /// Width in pixels, before rotation.
    pub width: u32,
    /// Height in pixels, before rotation.
    pub height: u32,
    /// The pixel format.
    pub layout: PixelLayout,
    /// The planes, as [`PixelLayout::components`] refer to them.
    pub planes: Vec<Plane>,
    /// The YCbCr matrix (ignored for RGB and gray formats).
    pub matrix: Matrix,
    /// Y spans 0 to the maximum (instead of 16..235 at 8 bits).
    pub full_range: bool,
}

/// A picture as 8-bit RGB, rows from top to bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    /// Presentation time (ticks of the video timescale).
    pub time: MediaTime,
    /// Width in pixels, before rotation.
    pub width: u32,
    /// Height in pixels, before rotation.
    pub height: u32,
    /// `width * height` pixels of red, green and blue bytes.
    pub rgb: Vec<u8>,
}

impl Frame {
    /// The picture data without row padding, plane after plane: what
    /// `ffmpeg -f rawvideo` writes for it. Tests compare it with FFmpeg's.
    pub fn packed(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for p in &self.planes {
            for row in 0..p.rows {
                let start = row * p.stride;
                out.extend_from_slice(&p.data[start..start + p.row_bytes]);
            }
        }
        out
    }

    /// Component `c` of the pixel at (`x`, `y`) of its plane (chroma
    /// coordinates for subsampled chroma), scaled to 0.0..=1.0.
    fn component(&self, c: &Component, x: usize, y: usize) -> f32 {
        let p = &self.planes[c.plane];
        let at = y * p.stride + x * c.step + c.offset;
        let wide = c.depth + c.shift > 8;
        let raw = if wide {
            let b = [p.data[at], p.data[at + 1]];
            u32::from(if self.layout.big_endian {
                u16::from_be_bytes(b)
            } else {
                u16::from_le_bytes(b)
            })
        } else {
            u32::from(p.data[at])
        };
        let max = (1u32 << c.depth) - 1;
        ((raw >> c.shift) & max) as f32 / max as f32
    }

    /// Converts the picture to 8-bit RGB.
    pub fn to_rgb(&self) -> Picture {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut rgb = Vec::with_capacity(w * h * 3);
        let comps = &self.layout.components;
        let full = self.full_range || self.layout.full_range;
        let (kr, kb) = self.matrix.weights();
        let kg = 1.0 - kr - kb;
        for y in 0..h {
            for x in 0..w {
                let (r, g, b) = if self.layout.rgb {
                    (
                        self.component(&comps[0], x, y),
                        self.component(&comps[1], x, y),
                        self.component(&comps[2], x, y),
                    )
                } else {
                    let depth = comps[0].depth;
                    let luma = self.component(&comps[0], x, y);
                    let (cb, cr) = if comps.len() >= 3 {
                        let (cx, cy) = (
                            x >> self.layout.log2_chroma_w,
                            y >> self.layout.log2_chroma_h,
                        );
                        (
                            self.component(&comps[1], cx, cy),
                            self.component(&comps[2], cx, cy),
                        )
                    } else {
                        // Gray: no colour.
                        (mid(depth), mid(depth))
                    };
                    let (yy, pb, pr) = normalize(luma, cb, cr, depth, full);
                    let r = yy + 2.0 * (1.0 - kr) * pr;
                    let b = yy + 2.0 * (1.0 - kb) * pb;
                    let g = (yy - kr * r - kb * b) / kg;
                    (r, g, b)
                };
                rgb.extend([to_u8(r), to_u8(g), to_u8(b)]);
            }
        }
        Picture {
            time: self.time,
            width: self.width,
            height: self.height,
            rgb,
        }
    }
}

/// The scaled value (as [`Frame::component`] returns it) of the chroma
/// midpoint `2^(depth-1)`.
fn mid(depth: u32) -> f32 {
    let max = ((1u32 << depth) - 1) as f32;
    (1u32 << (depth - 1)) as f32 / max
}

/// Y', Pb and Pr (0..1, -0.5..0.5) from scaled code values: limited
/// ("video") range puts black at 16 and white at 235, chroma at 16..240,
/// scaled by `2^(depth-8)` for deeper samples (ITU-R BT.709 6.10,
/// BT.2020 Table 5).
fn normalize(y: f32, cb: f32, cr: f32, depth: u32, full: bool) -> (f32, f32, f32) {
    let max = ((1u32 << depth) - 1) as f32;
    let (y, cb, cr) = (y * max, cb * max, cr * max);
    let half = (1u32 << (depth - 1)) as f32;
    if full {
        (y / max, (cb - half) / max, (cr - half) / max)
    } else {
        let s = (1u32 << depth.saturating_sub(8)) as f32;
        (
            (y - 16.0 * s) / (219.0 * s),
            (cb - half) / (224.0 * s),
            (cr - half) / (224.0 * s),
        )
    }
}

/// 0.0..=1.0 to a byte, rounded and clamped.
fn to_u8(v: f32) -> u8 {
    (v * 255.0).round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comp(plane: usize, step: usize, offset: usize, depth: u32) -> Component {
        Component {
            plane,
            step,
            offset,
            shift: 0,
            depth,
        }
    }

    fn yuv420p() -> PixelLayout {
        PixelLayout {
            name: "yuv420p".into(),
            components: vec![comp(0, 1, 0, 8), comp(1, 1, 0, 8), comp(2, 1, 0, 8)],
            log2_chroma_w: 1,
            log2_chroma_h: 1,
            rgb: false,
            big_endian: false,
            full_range: false,
        }
    }

    fn plane(data: Vec<u8>, stride: usize, rows: usize) -> Plane {
        Plane {
            data,
            stride,
            row_bytes: stride,
            rows,
        }
    }

    /// A 2x2 yuv420p frame of one colour.
    fn solid(y: u8, cb: u8, cr: u8, matrix: Matrix, full_range: bool) -> Frame {
        Frame {
            time: 0,
            width: 2,
            height: 2,
            layout: yuv420p(),
            planes: vec![
                plane(vec![y; 4], 2, 2),
                plane(vec![cb], 1, 1),
                plane(vec![cr], 1, 1),
            ],
            matrix,
            full_range,
        }
    }

    /// Colours from 8-bit codes, which round each component: within 1.
    fn assert_near(got: &[u8], want: [u8; 3]) {
        assert!(
            got.iter().zip(want).all(|(&g, w)| g.abs_diff(w) <= 1),
            "{got:?} is not {want:?}"
        );
    }

    fn first_pixel(f: &Frame) -> [u8; 3] {
        let p = f.to_rgb();
        assert_eq!(p.rgb.len(), 12);
        [p.rgb[0], p.rgb[1], p.rgb[2]]
    }

    #[test]
    fn limited_range_black_white_and_grey() {
        assert_eq!(
            first_pixel(&solid(16, 128, 128, Matrix::Bt709, false)),
            [0, 0, 0]
        );
        assert_eq!(
            first_pixel(&solid(235, 128, 128, Matrix::Bt709, false)),
            [255, 255, 255]
        );
        assert_eq!(
            first_pixel(&solid(126, 128, 128, Matrix::Bt601, false)),
            [128, 128, 128]
        );
    }

    /// The BT.709 and BT.601 codes of pure red (ITU-R BT.709 colour bars).
    #[test]
    fn primaries_decode_to_pure_colours() {
        assert_near(
            &first_pixel(&solid(63, 102, 240, Matrix::Bt709, false)),
            [255, 0, 0],
        );
        assert_near(
            &first_pixel(&solid(81, 90, 240, Matrix::Bt601, false)),
            [255, 0, 0],
        );
        // Full range (JPEG) red, BT.601: Y 76, Cb 85, Cr 255.
        assert_near(
            &first_pixel(&solid(76, 85, 255, Matrix::Bt601, true)),
            [255, 0, 0],
        );
        // The same codes read with the wrong matrix are visibly off.
        let wrong = first_pixel(&solid(63, 102, 240, Matrix::Bt601, false));
        assert!(wrong[1] > 10 || wrong[0] < 245, "{wrong:?}");
    }

    /// 10-bit little-endian samples with the value in the high bits
    /// (`p010le`-style shift) and NV12-style interleaved chroma.
    #[test]
    fn deep_shifted_and_interleaved_samples() {
        let layout = PixelLayout {
            name: "p010le".into(),
            components: vec![
                Component {
                    plane: 0,
                    step: 2,
                    offset: 0,
                    shift: 6,
                    depth: 10,
                },
                Component {
                    plane: 1,
                    step: 4,
                    offset: 0,
                    shift: 6,
                    depth: 10,
                },
                Component {
                    plane: 1,
                    step: 4,
                    offset: 2,
                    shift: 6,
                    depth: 10,
                },
            ],
            log2_chroma_w: 1,
            log2_chroma_h: 1,
            rgb: false,
            big_endian: false,
            full_range: false,
        };
        // White: Y 940, Cb = Cr = 512 (limited range at 10 bits).
        let y = (940u16 << 6).to_le_bytes();
        let c = (512u16 << 6).to_le_bytes();
        let f = Frame {
            time: 0,
            width: 2,
            height: 2,
            layout,
            planes: vec![plane(y.repeat(4), 4, 2), plane([c, c].concat(), 4, 1)],
            matrix: Matrix::Bt2020,
            full_range: false,
        };
        assert_eq!(f.to_rgb().rgb, vec![255; 12]);
    }

    /// Chroma of a 4:2:0 picture covers 2x2 pixels; padding is skipped.
    #[test]
    fn chroma_is_subsampled_and_padding_ignored() {
        let mut f = solid(16, 128, 128, Matrix::Bt709, false);
        f.width = 4;
        f.height = 2;
        // Y rows of 4 plus 2 bytes padding; chroma: grey left, red right.
        f.planes[0] = Plane {
            data: vec![126, 126, 63, 63, 0, 0, 126, 126, 63, 63, 0, 0],
            stride: 6,
            row_bytes: 4,
            rows: 2,
        };
        f.planes[1] = plane(vec![128, 102], 2, 1);
        f.planes[2] = plane(vec![128, 240], 2, 1);
        let p = f.to_rgb();
        assert_eq!(&p.rgb[0..3], &[128, 128, 128]);
        assert_near(&p.rgb[6..9], [255, 0, 0]);
        // Second row, third pixel: the same chroma sample.
        assert_near(&p.rgb[18..21], [255, 0, 0]);
        assert_eq!(f.packed().len(), 4 * 2 + 2 + 2);
    }

    #[test]
    fn rgb_and_gray_formats() {
        let rgb24 = Frame {
            time: 0,
            width: 1,
            height: 1,
            layout: PixelLayout {
                name: "rgb24".into(),
                components: vec![comp(0, 3, 0, 8), comp(0, 3, 1, 8), comp(0, 3, 2, 8)],
                log2_chroma_w: 0,
                log2_chroma_h: 0,
                rgb: true,
                big_endian: false,
                full_range: false,
            },
            planes: vec![plane(vec![1, 2, 3], 3, 1)],
            matrix: Matrix::Bt709,
            full_range: false,
        };
        assert_eq!(rgb24.to_rgb().rgb, vec![1, 2, 3]);
        let gray = Frame {
            layout: PixelLayout {
                name: "gray".into(),
                components: vec![comp(0, 1, 0, 8)],
                rgb: false,
                ..rgb24.layout.clone()
            },
            planes: vec![plane(vec![255], 1, 1)],
            full_range: true,
            ..rgb24.clone()
        };
        assert_eq!(gray.to_rgb().rgb, vec![255, 255, 255]);
    }
}
