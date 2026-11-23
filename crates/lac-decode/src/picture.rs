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
    pub(crate) fn weights(self) -> (f32, f32) {
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

    /// Converts the picture to 8-bit RGB.
    ///
    /// This runs for every picture the preview shows without a GPU (and
    /// for 4K at 30 pictures a second), so it is written for speed: each
    /// component's code values go through tables made once per picture
    /// (`Converter`), rows are read as slices, and the rows are split
    /// among the processor's cores. The arithmetic is the plain one of
    /// ITU-R BT.709 / BT.2020, see `Converter`.
    pub fn to_rgb(&self) -> Picture {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut rgb = vec![0; w * h * 3];
        if let Some(conv) = Converter::new(self) {
            let row_bytes = w * 3;
            // Threads only pay off for pictures larger than a thumbnail.
            let threads = if w * h >= PARALLEL_PIXELS {
                std::thread::available_parallelism().map_or(1, usize::from)
            } else {
                1
            };
            let rows_per_chunk = h.div_ceil(threads.max(1)).max(1);
            if threads <= 1 || row_bytes == 0 {
                conv.rows(0, &mut rgb);
            } else {
                std::thread::scope(|s| {
                    for (i, chunk) in rgb.chunks_mut(rows_per_chunk * row_bytes).enumerate() {
                        let conv = &conv;
                        s.spawn(move || conv.rows(i * rows_per_chunk, chunk));
                    }
                });
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

/// Pictures of at least this many pixels are converted on several
/// threads (a 256x256 picture takes well under a millisecond on one).
const PARALLEL_PIXELS: usize = 256 * 256;

/// How to read one component of every pixel: where it lies in its plane
/// and how its code value is stored.
struct Reader<'a> {
    plane: &'a Plane,
    /// Bytes from one pixel's component to the next pixel's.
    step: usize,
    /// Bytes before the first pixel's component.
    offset: usize,
    /// Bits the value is shifted left by in its storage unit.
    shift: u32,
    /// `2^depth - 1`: masks the value after the shift.
    mask: u32,
    /// The value is stored in two bytes.
    wide: bool,
    big_endian: bool,
}

impl<'a> Reader<'a> {
    /// A reader of `c` in `frame`; `None` if the component lies outside
    /// the planes or is deeper than 16 bits.
    fn new(frame: &'a Frame, c: &Component) -> Option<Self> {
        if c.depth == 0 || c.depth > 16 {
            return None;
        }
        Some(Reader {
            plane: frame.planes.get(c.plane)?,
            step: c.step,
            offset: c.offset,
            shift: c.shift,
            mask: (1u32 << c.depth) - 1,
            wide: c.depth + c.shift > 8,
            big_endian: frame.layout.big_endian,
        })
    }

    /// Row `y` of the plane (empty past its end).
    fn row(&self, y: usize) -> &'a [u8] {
        let start = y * self.plane.stride;
        self.plane
            .data
            .get(start..(start + self.plane.stride).min(self.plane.data.len()))
            .unwrap_or(&[])
    }

    /// The code value of the component at `x` in `row` (from
    /// [`Reader::row`]), at most [`Reader::mask`]; 0 for a pixel past
    /// the row's end.
    #[inline]
    fn at(&self, row: &[u8], x: usize) -> usize {
        let at = x * self.step + self.offset;
        let raw = if self.wide {
            match row.get(at..at + 2) {
                Some(&[a, b]) if self.big_endian => u32::from(u16::from_be_bytes([a, b])),
                Some(&[a, b]) => u32::from(u16::from_le_bytes([a, b])),
                _ => 0,
            }
        } else {
            row.get(at).map_or(0, |&v| u32::from(v))
        };
        ((raw >> self.shift) & self.mask) as usize
    }
}

/// Fractional bits of the fixed-point numbers in [`Converter`]'s tables.
const FRACTION: u32 = 16;

/// `v` (0.0..=1.0 for the visible range) in 8-bit steps as a fixed-point
/// number with [`FRACTION`] fractional bits.
fn fixed(v: f64) -> i32 {
    (v * 255.0 * f64::from(1u32 << FRACTION)).round() as i32
}

/// A table from each code value of a component (`mask + 1` entries) to
/// `value(code)` as a [`fixed`] number.
fn table(r: &Reader, value: impl Fn(f64) -> f64) -> Vec<i32> {
    (0..=r.mask).map(|v| fixed(value(f64::from(v)))).collect()
}

/// What [`Frame::to_rgb`] needs to convert rows of one picture.
///
/// R', G' and B' are linear in Y', Pb and Pr (ITU-R BT.709 3.2 and 3.3
/// solved for them):
///
/// ```text
/// R = Y + 2 (1 - Kr) Pr
/// B = Y + 2 (1 - Kb) Pb
/// G = Y - 2 Kr (1 - Kr) / Kg Pr - 2 Kb (1 - Kb) / Kg Pb
/// ```
///
/// so each term is looked up in a table per component, made once per
/// picture from the code values, and a pixel costs a few additions.
/// The tables hold fixed-point numbers ([`fixed`]); the result differs
/// from exact arithmetic by at most one 8-bit step at rounding ties.
struct Converter<'a> {
    width: usize,
    /// R, G, B (RGB formats) or Y, Cb, Cr (Cb and Cr absent for gray).
    comps: Vec<Reader<'a>>,
    rgb: bool,
    log2_chroma_w: u32,
    log2_chroma_h: u32,
    /// RGB formats: R, G and B. YCbCr: Y, and the R, G and B terms of
    /// Pb and Pr as below (empty for gray).
    first: Vec<i32>,
    /// RGB: G. YCbCr: Pr's term of R.
    second: Vec<i32>,
    /// RGB: B. YCbCr: Pr's term of G.
    third: Vec<i32>,
    /// YCbCr: Pb's term of G.
    g_cb: Vec<i32>,
    /// YCbCr: Pb's term of B.
    b_cb: Vec<i32>,
}

impl<'a> Converter<'a> {
    /// The converter for `frame`, or `None` if its layout cannot be read
    /// (then the picture stays black).
    fn new(frame: &'a Frame) -> Option<Self> {
        let l = &frame.layout;
        let comps = &l.components;
        let full = frame.full_range || l.full_range;
        let mut readers = Vec::new();
        let (first, second, third, g_cb, b_cb);
        if l.rgb {
            for c in comps.get(..3)? {
                readers.push(Reader::new(frame, c)?);
            }
            // R, G and B: code / maximum.
            let scaled = |r: &Reader| table(r, |v| v / f64::from(r.mask));
            first = scaled(&readers[0]);
            second = scaled(&readers[1]);
            third = scaled(&readers[2]);
            (g_cb, b_cb) = (Vec::new(), Vec::new());
        } else {
            // Y' 0..1 and Pb, Pr -0.5..0.5 from code values: limited
            // ("video") range puts black at 16 and white at 235, chroma at
            // 16..240, scaled by `2^(depth-8)` for deeper samples (ITU-R
            // BT.709 6.10, BT.2020 Table 5); full range spans 0..max.
            let luma = Reader::new(frame, comps.first()?)?;
            let depth = comps[0].depth;
            let max = f64::from(luma.mask);
            let half = f64::from(1u32 << (depth - 1));
            let s = f64::from(1u32 << depth.saturating_sub(8));
            let (y0, ys, cs) = if full {
                (0.0, max, max)
            } else {
                (16.0 * s, 219.0 * s, 224.0 * s)
            };
            first = table(&luma, |v| (v - y0) / ys);
            readers.push(luma);
            let (kr, kb) = frame.matrix.weights();
            let (kr, kb) = (f64::from(kr), f64::from(kb));
            let kg = 1.0 - kr - kb;
            if comps.len() >= 3 {
                readers.push(Reader::new(frame, &comps[1])?);
                readers.push(Reader::new(frame, &comps[2])?);
                let chroma = |r: &Reader, k: f64| table(r, |v| k * (v - half) / cs);
                second = chroma(&readers[2], 2.0 * (1.0 - kr));
                third = chroma(&readers[2], -2.0 * kr * (1.0 - kr) / kg);
                g_cb = chroma(&readers[1], -2.0 * kb * (1.0 - kb) / kg);
                b_cb = chroma(&readers[1], 2.0 * (1.0 - kb));
            } else {
                (second, third, g_cb, b_cb) = Default::default();
            }
        }
        Some(Converter {
            width: frame.width as usize,
            comps: readers,
            rgb: l.rgb,
            log2_chroma_w: l.log2_chroma_w,
            log2_chroma_h: l.log2_chroma_h,
            first,
            second,
            third,
            g_cb,
            b_cb,
        })
    }

    /// Converts the rows from `first` on into `out` (whole rows of RGB).
    fn rows(&self, first: usize, out: &mut [u8]) {
        let row_bytes = self.width * 3;
        if row_bytes == 0 {
            return;
        }
        let row = |c: usize, y: usize| self.comps.get(c).map_or(&[][..], |r| r.row(y));
        // Tables have an entry for every code a reader returns.
        let t = |table: &[i32], code: usize| table.get(code).copied().unwrap_or(0);
        for (i, line) in out.chunks_exact_mut(row_bytes).enumerate() {
            let y = first + i;
            let pixels = line.as_chunks_mut::<3>().0.iter_mut().enumerate();
            if self.rgb {
                let (r0, r1, r2) = (row(0, y), row(1, y), row(2, y));
                let [cr, cg, cb] = [&self.comps[0], &self.comps[1], &self.comps[2]];
                for (x, px) in pixels {
                    *px = [
                        to_u8(t(&self.first, cr.at(r0, x))),
                        to_u8(t(&self.second, cg.at(r1, x))),
                        to_u8(t(&self.third, cb.at(r2, x))),
                    ];
                }
                continue;
            }
            let luma = &self.comps[0];
            let luma_row = row(0, y);
            match (self.comps.get(1), self.comps.get(2)) {
                (Some(cb), Some(cr)) => {
                    let cy = y >> self.log2_chroma_h;
                    let (cb_row, cr_row) = (cb.row(cy), cr.row(cy));
                    for (x, px) in pixels {
                        let yy = t(&self.first, luma.at(luma_row, x));
                        let cx = x >> self.log2_chroma_w;
                        let (u, v) = (cb.at(cb_row, cx), cr.at(cr_row, cx));
                        *px = [
                            to_u8(yy + t(&self.second, v)),
                            to_u8(yy + t(&self.third, v) + t(&self.g_cb, u)),
                            to_u8(yy + t(&self.b_cb, u)),
                        ];
                    }
                }
                // Gray: no colour.
                _ => {
                    for (x, px) in pixels {
                        let v = to_u8(t(&self.first, luma.at(luma_row, x)));
                        *px = [v, v, v];
                    }
                }
            }
        }
    }
}

/// A [`fixed`] number to a byte, rounded and clamped.
#[inline]
fn to_u8(v: i32) -> u8 {
    ((v + (1 << (FRACTION - 1))) >> FRACTION).clamp(0, 255) as u8
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

    /// The textbook conversion in floating point (ITU-R BT.709 3.2, 3.3
    /// and 6.10) of 8-bit limited-range codes, as 0..=255 before
    /// rounding.
    fn reference(y: u8, cb: u8, cr: u8, matrix: Matrix) -> [f64; 3] {
        let (kr, kb) = matrix.weights();
        let (kr, kb) = (f64::from(kr), f64::from(kb));
        let yy = (f64::from(y) - 16.0) / 219.0;
        let pb = (f64::from(cb) - 128.0) / 224.0;
        let pr = (f64::from(cr) - 128.0) / 224.0;
        let r = yy + 2.0 * (1.0 - kr) * pr;
        let b = yy + 2.0 * (1.0 - kb) * pb;
        let g = (yy - kr * r - kb * b) / (1.0 - kr - kb);
        [r, g, b].map(|v| (v * 255.0).clamp(0.0, 255.0))
    }

    /// The table-driven fixed-point conversion stays within half an
    /// 8-bit step (plus a hair for the fixed point) of exact arithmetic,
    /// for every luma code and a grid of chroma codes, in every matrix.
    #[test]
    fn tables_match_exact_arithmetic() {
        let chroma: Vec<u8> = (0..=255).step_by(5).chain([128, 255]).collect();
        for matrix in [Matrix::Bt601, Matrix::Bt709, Matrix::Bt2020] {
            // One row of 256 pixels per (cb, cr) pair: luma 0..=255.
            for &cb in &chroma {
                for &cr in &chroma {
                    let f = Frame {
                        width: 256,
                        height: 1,
                        planes: vec![
                            plane((0..=255).collect(), 256, 1),
                            plane(vec![cb; 128], 128, 1),
                            plane(vec![cr; 128], 128, 1),
                        ],
                        ..solid(0, 0, 0, matrix, false)
                    };
                    let got = f.to_rgb().rgb;
                    for y in 0..=255u8 {
                        let want = reference(y, cb, cr, matrix);
                        for k in 0..3 {
                            let have = f64::from(got[usize::from(y) * 3 + k]);
                            assert!(
                                (have - want[k]).abs() <= 0.501,
                                "{matrix:?} Y {y} Cb {cb} Cr {cr}: {k} is {have}, not {}",
                                want[k]
                            );
                        }
                    }
                }
            }
        }
    }

    /// A picture large enough to be converted on several threads comes
    /// out exactly as its rows converted one by one.
    #[test]
    fn threaded_conversion_keeps_the_rows_in_place() {
        let (w, h) = (322usize, 300usize);
        let stride = w + 6;
        let luma: Vec<u8> = (0..stride * h).map(|i| (i * 7 % 251) as u8).collect();
        let cw = w.div_ceil(2);
        let cb: Vec<u8> = (0..cw * h / 2).map(|i| (i * 3 % 241) as u8).collect();
        let cr: Vec<u8> = (0..cw * h / 2).map(|i| (i * 5 % 239) as u8).collect();
        let f = Frame {
            width: w as u32,
            height: h as u32,
            planes: vec![
                Plane {
                    data: luma.clone(),
                    stride,
                    row_bytes: w,
                    rows: h,
                },
                plane(cb.clone(), cw, h / 2),
                plane(cr.clone(), cw, h / 2),
            ],
            ..solid(0, 0, 0, Matrix::Bt709, false)
        };
        assert!(w * h >= PARALLEL_PIXELS);
        let all = f.to_rgb().rgb;
        for y in 0..h {
            // Row y alone: a one-row picture with the same samples.
            let row = Frame {
                height: 1,
                planes: vec![
                    plane(luma[y * stride..y * stride + w].to_vec(), w, 1),
                    plane(cb[y / 2 * cw..(y / 2 + 1) * cw].to_vec(), cw, 1),
                    plane(cr[y / 2 * cw..(y / 2 + 1) * cw].to_vec(), cw, 1),
                ],
                ..f.clone()
            };
            assert_eq!(
                &all[y * w * 3..(y + 1) * w * 3],
                &row.to_rgb().rgb[..],
                "row {y}"
            );
        }
    }

    /// Planes shorter than the picture (a damaged decoder output) give
    /// some colour for the missing samples instead of panicking.
    #[test]
    fn short_planes_do_not_panic() {
        let mut f = solid(235, 128, 128, Matrix::Bt709, false);
        f.planes[0].data.truncate(3);
        f.planes[2].data.clear();
        let rgb = f.to_rgb().rgb;
        assert_eq!(rgb.len(), 12);
    }
}
