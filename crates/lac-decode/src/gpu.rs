//! How to show a decoded [`Frame`] with a GPU: which planes to upload as
//! textures and the affine colour conversion a shader applies to the
//! texture samples.
//!
//! The preview can convert YCbCr to RGB on the CPU ([`Frame::to_rgb`]),
//! but a GPU does it, and the scaling to the pane, for free. A
//! [`GpuLayout`] describes a frame as one texture per plane (one or two
//! channels of 8 or 16 bits: planar YUV, NV12-style interleaved chroma,
//! 10-bit `yuv420p10le` and MSB-aligned `p010le`) and a 3x4 matrix that
//! turns the three samples, as a GPU normalizes them (stored value /
//! 255 or / 65535), into R, G and B in 0.0..=1.0. It is the arithmetic of
//! [`Frame::to_rgb`] rearranged: the tests compare both pixel by pixel.
//!
//! Formats a GPU cannot read this way (RGB, packed 4:2:2 such as `yuyv422`
//! with luma and chroma in one plane, big-endian samples) have no layout;
//! a renderer then uploads [`Frame::to_rgb`]'s picture instead.

use crate::picture::{Component, Frame};

/// One texture: a plane of the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuTexture {
    /// The plane of [`Frame::planes`] it is made from.
    pub plane: usize,
    /// Channels per texel: 1 (`R`) or 2 (`RG`, interleaved chroma).
    pub channels: usize,
    /// Bytes per channel: 1 (8-bit texture) or 2 (16-bit, little-endian).
    pub bytes: usize,
    /// Texels per row.
    pub width: u32,
    /// Rows.
    pub height: u32,
    /// Texels from one row's start to the next (the plane's stride),
    /// what OpenGL calls `GL_UNPACK_ROW_LENGTH`.
    pub row_length: usize,
}

/// Where a component is: texture index and channel (0 = red, 1 = green).
pub type Channel = (usize, usize);

/// How to show a frame with textures and a colour matrix.
#[derive(Debug, Clone, PartialEq)]
pub struct GpuLayout {
    /// The textures, luma first.
    pub textures: Vec<GpuTexture>,
    /// The luma sample.
    pub y: Channel,
    /// The chroma samples; `None` for gray pictures (their matrix ignores
    /// chroma).
    pub chroma: Option<(Channel, Channel)>,
    /// Row `k` gives colour `k` (R, G, B) of the normalized texture
    /// samples as `m[k][0] * y + m[k][1] * cb + m[k][2] * cr + m[k][3]`.
    pub matrix: [[f32; 4]; 3],
}

/// Bytes a component's storage unit takes.
fn unit_bytes(c: &Component) -> usize {
    if c.depth + c.shift > 8 { 2 } else { 1 }
}

/// The factor from a texture sample (stored unit / its maximum, as a GPU
/// normalizes it) to the component's code value, which is the stored
/// unit shifted right by `shift` (low bits of MSB-aligned formats are 0).
fn code_scale(c: &Component) -> f32 {
    let unit_max = if unit_bytes(c) == 2 { 65_535.0 } else { 255.0 };
    unit_max / (1u32 << c.shift) as f32
}

impl GpuLayout {
    /// What a shader that samples the nearest texels computes for pixel
    /// (`x`, `y`) of `frame`: R, G and B, unclamped. The reference that
    /// tests compare GPU output and [`Frame::to_rgb`] with.
    ///
    /// # Panics
    /// If the pixel lies outside `frame`, or the layout is not `frame`'s.
    pub fn pixel(&self, frame: &Frame, x: usize, y: usize) -> [f32; 3] {
        let sample = |(t, ch): Channel, chroma: bool| {
            let tex = &self.textures[t];
            let (tx, ty) = if chroma {
                (
                    x >> frame.layout.log2_chroma_w,
                    y >> frame.layout.log2_chroma_h,
                )
            } else {
                (x, y)
            };
            let p = &frame.planes[tex.plane];
            let at = ty * p.stride + (tx * tex.channels + ch) * tex.bytes;
            if tex.bytes == 2 {
                f32::from(u16::from_le_bytes([p.data[at], p.data[at + 1]])) / 65_535.0
            } else {
                f32::from(p.data[at]) / 255.0
            }
        };
        let v = [
            sample(self.y, false),
            self.chroma.map_or(0.0, |(cb, _)| sample(cb, true)),
            self.chroma.map_or(0.0, |(_, cr)| sample(cr, true)),
        ];
        let m = self.matrix;
        std::array::from_fn(|k| m[k][0] * v[0] + m[k][1] * v[1] + m[k][2] * v[2] + m[k][3])
    }
}

impl Frame {
    /// The textures and colour matrix to show this frame with a GPU, or
    /// `None` if its format cannot be uploaded plane by plane (see the
    /// module documentation).
    pub fn gpu_layout(&self) -> Option<GpuLayout> {
        let l = &self.layout;
        if l.rgb || l.big_endian || l.components.is_empty() {
            return None;
        }
        let comps = &l.components;
        let color = comps.len() >= 3;
        // Luma must have a plane of its own: its texture is full size,
        // chroma textures are subsampled.
        if color && (comps[1].plane == comps[0].plane || comps[2].plane == comps[0].plane) {
            return None;
        }
        let mut textures: Vec<GpuTexture> = Vec::new();
        let mut channel_of = |c: &Component, chroma: bool| -> Option<Channel> {
            let bytes = unit_bytes(c);
            if !c.step.is_multiple_of(bytes) || !c.offset.is_multiple_of(bytes) {
                return None;
            }
            let channels = c.step / bytes;
            let channel = c.offset / bytes;
            if !(1..=2).contains(&channels) || channel >= channels {
                return None;
            }
            let p = self.planes.get(c.plane)?;
            let texel = bytes * channels;
            if !p.stride.is_multiple_of(texel) {
                return None;
            }
            let (sw, sh) = if chroma {
                (l.log2_chroma_w, l.log2_chroma_h)
            } else {
                (0, 0)
            };
            // Chroma planes round up: a 3-pixel row has 2 chroma samples.
            let width = self.width.div_ceil(1 << sw);
            let height = self.height.div_ceil(1 << sh);
            if p.rows < height as usize
                || p.data.len() < (height as usize - 1) * p.stride + width as usize * texel
            {
                return None;
            }
            let t = GpuTexture {
                plane: c.plane,
                channels,
                bytes,
                width,
                height,
                row_length: p.stride / texel,
            };
            let index = match textures.iter().position(|x| x.plane == c.plane) {
                Some(i) if textures[i] == t => i,
                // One plane read two ways: not a layout we know.
                Some(_) => return None,
                None => {
                    textures.push(t);
                    textures.len() - 1
                }
            };
            Some((index, channel))
        };
        let y = channel_of(&comps[0], false)?;
        let chroma = if color {
            Some((channel_of(&comps[1], true)?, channel_of(&comps[2], true)?))
        } else {
            None
        };
        Some(GpuLayout {
            textures,
            y,
            chroma,
            matrix: self.gpu_matrix(),
        })
    }

    /// The colour matrix of [`GpuLayout::matrix`]: [`Frame::to_rgb`]'s
    /// normalization (full or limited range, ITU-R BT.709 6.10 and
    /// BT.2020 Table 5) and YCbCr matrix, applied to texture samples.
    fn gpu_matrix(&self) -> [[f32; 4]; 3] {
        let comps = &self.layout.components;
        let full = self.full_range || self.layout.full_range;
        let (kr, kb) = self.matrix.weights();
        let kg = 1.0 - kr - kb;
        // Y' = ay * sample + by.
        let yc = &comps[0];
        let depth = yc.depth;
        let max = ((1u32 << depth) - 1) as f32;
        let s = (1u32 << depth.saturating_sub(8)) as f32;
        let (ay, by) = if full {
            (code_scale(yc) / max, 0.0)
        } else {
            (code_scale(yc) / (219.0 * s), -16.0 * s / (219.0 * s))
        };
        let Some(cc) = comps.get(1).filter(|_| comps.len() >= 3) else {
            // Gray: Y' only.
            return [[ay, 0.0, 0.0, by]; 3];
        };
        // Pb = ac * sample + bc (likewise Pr).
        let cdepth = cc.depth;
        let cmax = ((1u32 << cdepth) - 1) as f32;
        let cs = (1u32 << cdepth.saturating_sub(8)) as f32;
        let half = (1u32 << (cdepth - 1)) as f32;
        let (ac, bc) = if full {
            (code_scale(cc) / cmax, -half / cmax)
        } else {
            (code_scale(cc) / (224.0 * cs), -half / (224.0 * cs))
        };
        // R = Y' + rv * Pr, B = Y' + bu * Pb, G = Y' - gu * Pb - gv * Pr.
        let rv = 2.0 * (1.0 - kr);
        let bu = 2.0 * (1.0 - kb);
        let gu = kb * bu / kg;
        let gv = kr * rv / kg;
        [
            [ay, 0.0, rv * ac, by + rv * bc],
            [ay, -gu * ac, -gv * ac, by - (gu + gv) * bc],
            [ay, bu * ac, 0.0, by + bu * bc],
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picture::{Matrix, PixelLayout, Plane};

    fn comp(plane: usize, step: usize, offset: usize, shift: u32, depth: u32) -> Component {
        Component {
            plane,
            step,
            offset,
            shift,
            depth,
        }
    }

    fn layout(name: &str, components: Vec<Component>, sub: u32) -> PixelLayout {
        PixelLayout {
            name: name.into(),
            components,
            log2_chroma_w: sub,
            log2_chroma_h: sub,
            rgb: false,
            big_endian: false,
            full_range: false,
        }
    }

    /// A plane of `rows` rows of `row` bytes plus 3 bytes of padding,
    /// filled with a pattern.
    fn plane(row: usize, rows: usize, seed: u32) -> Plane {
        let stride = row + 3 + (4 - (row + 3) % 4) % 4;
        let data = (0..stride * rows)
            .map(|i| ((i as u32 * 37 + seed * 101) % 251) as u8)
            .collect();
        Plane {
            data,
            stride,
            row_bytes: row,
            rows,
        }
    }

    /// A 16-bit plane whose values use `depth` bits shifted by `shift`.
    fn plane16(texels: usize, rows: usize, depth: u32, shift: u32, seed: u32) -> Plane {
        let stride = (texels + 2) * 2;
        let mut data = Vec::with_capacity(stride * rows);
        for i in 0..stride / 2 * rows {
            let v = ((i as u32 * 97 + seed * 13) % (1 << depth)) as u16;
            data.extend_from_slice(&(v << shift).to_le_bytes());
        }
        Plane {
            data,
            stride,
            row_bytes: texels * 2,
            rows,
        }
    }

    /// The GPU arithmetic gives [`Frame::to_rgb`]'s colours (within the
    /// rounding of one 8-bit step) at every pixel.
    fn assert_matches_cpu(f: &Frame) {
        let g = f
            .gpu_layout()
            .unwrap_or_else(|| panic!("{}: no layout", f.layout.name));
        let cpu = f.to_rgb();
        for y in 0..f.height as usize {
            for x in 0..f.width as usize {
                let got = g.pixel(f, x, y);
                for (k, value) in got.iter().enumerate() {
                    let want = cpu.rgb[(y * f.width as usize + x) * 3 + k];
                    let have = (value * 255.0).round().clamp(0.0, 255.0) as u8;
                    assert!(
                        have.abs_diff(want) <= 1,
                        "{} {:?} {}x{} component {k}: {have} vs {want}",
                        f.layout.name,
                        f.matrix,
                        x,
                        y
                    );
                }
            }
        }
    }

    fn frame(layout: PixelLayout, planes: Vec<Plane>, w: u32, h: u32, m: Matrix) -> Frame {
        Frame {
            time: 0,
            width: w,
            height: h,
            layout,
            planes,
            matrix: m,
            full_range: false,
        }
    }

    #[test]
    fn planar_8_bit_formats_match_the_cpu_conversion() {
        // 5x3: odd sizes round the chroma planes up.
        for (sub, name) in [(1, "yuv420p"), (0, "yuv444p")] {
            let cw = 5usize.div_ceil(1 << sub);
            let ch = 3usize.div_ceil(1 << sub);
            for m in [Matrix::Bt601, Matrix::Bt709, Matrix::Bt2020] {
                let mut f = frame(
                    layout(
                        name,
                        vec![
                            comp(0, 1, 0, 0, 8),
                            comp(1, 1, 0, 0, 8),
                            comp(2, 1, 0, 0, 8),
                        ],
                        sub,
                    ),
                    vec![plane(5, 3, 1), plane(cw, ch, 2), plane(cw, ch, 3)],
                    5,
                    3,
                    m,
                );
                assert_matches_cpu(&f);
                f.full_range = true;
                assert_matches_cpu(&f);
            }
        }
    }

    #[test]
    fn nv12_reads_chroma_from_two_channels() {
        let f = frame(
            layout(
                "nv12",
                vec![
                    comp(0, 1, 0, 0, 8),
                    comp(1, 2, 0, 0, 8),
                    comp(1, 2, 1, 0, 8),
                ],
                1,
            ),
            vec![plane(4, 4, 1), plane(4, 2, 2)],
            4,
            4,
            Matrix::Bt709,
        );
        let g = f.gpu_layout().expect("layout");
        assert_eq!(g.textures.len(), 2);
        assert_eq!(g.textures[1].channels, 2);
        assert_eq!(g.chroma, Some(((1, 0), (1, 1))));
        assert_matches_cpu(&f);
    }

    #[test]
    fn deep_samples_use_16_bit_textures() {
        // yuv420p10le: 10 bits in the low bits of 16.
        let f = frame(
            layout(
                "yuv420p10le",
                vec![
                    comp(0, 2, 0, 0, 10),
                    comp(1, 2, 0, 0, 10),
                    comp(2, 2, 0, 0, 10),
                ],
                1,
            ),
            vec![
                plane16(4, 2, 10, 0, 1),
                plane16(2, 1, 10, 0, 2),
                plane16(2, 1, 10, 0, 3),
            ],
            4,
            2,
            Matrix::Bt2020,
        );
        assert_eq!(f.gpu_layout().expect("layout").textures[0].bytes, 2);
        assert_matches_cpu(&f);
        // p010le: 10 bits in the high bits, chroma interleaved.
        let f = frame(
            layout(
                "p010le",
                vec![
                    comp(0, 2, 0, 6, 10),
                    comp(1, 4, 0, 6, 10),
                    comp(1, 4, 2, 6, 10),
                ],
                1,
            ),
            vec![plane16(4, 2, 10, 6, 1), plane16(4, 1, 10, 6, 2)],
            4,
            2,
            Matrix::Bt2020,
        );
        assert_matches_cpu(&f);
    }

    #[test]
    fn gray_ignores_chroma() {
        let mut f = frame(
            layout("gray", vec![comp(0, 1, 0, 0, 8)], 0),
            vec![plane(3, 2, 1)],
            3,
            2,
            Matrix::Bt709,
        );
        assert_eq!(f.gpu_layout().expect("layout").chroma, None);
        assert_matches_cpu(&f);
        f.full_range = true;
        assert_matches_cpu(&f);
    }

    #[test]
    fn formats_a_gpu_cannot_read_by_plane_have_no_layout() {
        // RGB: the CPU picture is uploaded instead.
        let mut rgb = layout(
            "rgb24",
            vec![
                comp(0, 3, 0, 0, 8),
                comp(0, 3, 1, 0, 8),
                comp(0, 3, 2, 0, 8),
            ],
            0,
        );
        rgb.rgb = true;
        let f = frame(rgb, vec![plane(6, 2, 1)], 2, 2, Matrix::Bt709);
        assert!(f.gpu_layout().is_none());
        // yuyv422: luma and chroma share a plane.
        let yuyv = layout(
            "yuyv422",
            vec![
                comp(0, 2, 0, 0, 8),
                comp(0, 4, 1, 0, 8),
                comp(0, 4, 3, 0, 8),
            ],
            1,
        );
        let f = frame(yuyv, vec![plane(8, 2, 1)], 4, 2, Matrix::Bt709);
        assert!(f.gpu_layout().is_none());
        // Big-endian samples.
        let mut be = layout(
            "yuv420p10be",
            vec![
                comp(0, 2, 0, 0, 10),
                comp(1, 2, 0, 0, 10),
                comp(2, 2, 0, 0, 10),
            ],
            1,
        );
        be.big_endian = true;
        let f = frame(
            be,
            vec![
                plane16(4, 2, 10, 0, 1),
                plane16(2, 1, 10, 0, 2),
                plane16(2, 1, 10, 0, 3),
            ],
            4,
            2,
            Matrix::Bt709,
        );
        assert!(f.gpu_layout().is_none());
    }
}
