//! What the GPU renderers of the video pane ([`crate::video_gl`] and, on
//! macOS, `video_metal`) share: the textures to upload for a picture, the
//! shader's channel picks and colour matrix, and the quad that places
//! and turns the picture.
//!
//! Both shaders compute the same thing: sample up to three textures
//! (`s0`, `s1`, `s2`), take Y, Cb and Cr as `dot(pick[3 * c], s0) +
//! dot(pick[3 * c + 1], s1) + dot(pick[3 * c + 2], s2)` and turn them into
//! RGB with the rows of a 3x4 matrix. The colour arithmetic comes from
//! [`GpuLayout`]; an RGB picture (formats without a layout) is one RGBA
//! texture with an identity matrix.

use std::borrow::Cow;

use lac_app::video::{Channel, GpuLayout, GpuTexture, Image, source_point};

/// The pixel format of a texture. All are normalized: a sample reads as
/// stored value / maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TexFormat {
    /// One 8-bit channel.
    R8,
    /// Two 8-bit channels (interleaved chroma).
    Rg8,
    /// One 16-bit channel, little-endian.
    R16,
    /// Two 16-bit channels, little-endian.
    Rg16,
    /// Red, green, blue and alpha, 8 bits each.
    Rgba8,
}

impl TexFormat {
    /// Bytes per texel.
    pub fn texel_bytes(self) -> usize {
        match self {
            TexFormat::R8 => 1,
            TexFormat::Rg8 | TexFormat::R16 => 2,
            TexFormat::Rg16 | TexFormat::Rgba8 => 4,
        }
    }
}

/// One texture to upload.
#[derive(Debug, Clone, PartialEq)]
pub struct Plane<'a> {
    /// Its format.
    pub format: TexFormat,
    /// Texels per row.
    pub width: u32,
    /// Rows.
    pub height: u32,
    /// Bytes from one row's start to the next; a multiple of the texel
    /// size.
    pub bytes_per_row: usize,
    /// The rows, top first; at least `(height - 1) * bytes_per_row +
    /// width * texel size` bytes.
    pub data: Cow<'a, [u8]>,
}

/// A picture ready for a shader: its textures (bound to units 0, 1,
/// 2) and the uniforms.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared<'a> {
    /// The textures, at most three.
    pub planes: Vec<Plane<'a>>,
    /// The `pick` uniform, see [`picks`].
    pub pick: [[f32; 4]; 9],
    /// The colour matrix rows: R, G and B of (Y, Cb, Cr, 1).
    pub rows: [[f32; 4]; 3],
}

/// The identity colour matrix, for RGB textures.
pub const IDENTITY: [[f32; 4]; 3] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
];

/// The `pick` uniform for luma at `y` and chroma at `chroma` (texture,
/// channel): entry `3 * component + texture` has a 1 in the channel's
/// place. Gray pictures pick no chroma, their matrix ignores it.
pub fn picks(y: Channel, chroma: Option<(Channel, Channel)>) -> [[f32; 4]; 9] {
    let mut p = [[0.0; 4]; 9];
    let mut set = |component: usize, (texture, channel): Channel| {
        p[3 * component + texture][channel] = 1.0;
    };
    set(0, y);
    if let Some((cb, cr)) = chroma {
        set(1, cb);
        set(2, cr);
    }
    p
}

/// The quad for a picture turned by `rotation`, as a triangle strip of
/// (x, y, u, v): the corners of the viewport in normalized device
/// coordinates (y up) with the texture coordinates of the source
/// picture shown there (row 0 of a texture is the picture's top). OpenGL
/// and Metal agree on both conventions.
pub fn quad(rotation: u16) -> [f32; 16] {
    let mut q = [0.0; 16];
    // Top left, top right, bottom left, bottom right of the display.
    for (i, (u, v)) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)]
        .into_iter()
        .enumerate()
    {
        let (su, sv) = source_point(rotation, u, v);
        q[4 * i..4 * i + 4].copy_from_slice(&[2.0 * u - 1.0, 1.0 - 2.0 * v, su, sv]);
    }
    q
}

/// The texture format of a [`GpuTexture`].
fn format_of(t: &GpuTexture) -> TexFormat {
    match (t.channels, t.bytes) {
        (1, 1) => TexFormat::R8,
        (_, 1) => TexFormat::Rg8,
        (1, _) => TexFormat::R16,
        _ => TexFormat::Rg16,
    }
}

/// GPUs read 16-bit texels in the machine's byte order; our 16-bit
/// planes are little-endian, so big-endian machines upload RGB instead.
fn native_order(layout: &GpuLayout) -> bool {
    cfg!(target_endian = "little") || layout.textures.iter().all(|t| t.bytes == 1)
}

/// The textures and uniforms for `image`: its decoded planes where a GPU
/// can read them, else the picture converted to RGBA.
pub fn prepare(image: &Image) -> Prepared<'_> {
    if let Image::Planes(frame) = image
        && let Some(layout) = frame.gpu_layout().filter(native_order)
    {
        let planes = layout
            .textures
            .iter()
            .map(|t| {
                let p = &frame.planes[t.plane];
                let format = format_of(t);
                let bytes_per_row = t.row_length * format.texel_bytes();
                // `gpu_layout` checked that the plane holds this much.
                let len = (t.height as usize - 1) * bytes_per_row
                    + t.width as usize * format.texel_bytes();
                Plane {
                    format,
                    width: t.width,
                    height: t.height,
                    bytes_per_row,
                    data: Cow::Borrowed(&p.data[..len]),
                }
            })
            .collect();
        return Prepared {
            planes,
            pick: picks(layout.y, layout.chroma),
            rows: layout.matrix,
        };
    }
    let rgb = image.to_rgb();
    let rgba: Vec<u8> = rgb
        .rgb
        .as_chunks::<3>()
        .0
        .iter()
        .flat_map(|p| [p[0], p[1], p[2], 0xff])
        .collect();
    Prepared {
        planes: vec![Plane {
            format: TexFormat::Rgba8,
            width: rgb.width,
            height: rgb.height,
            bytes_per_row: rgb.width as usize * 4,
            data: Cow::Owned(rgba),
        }],
        pick: picks((0, 0), Some(((0, 1), (0, 2)))),
        rows: IDENTITY,
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use lac_app::video::{Frame, Picture};

    /// The first picture of a video fixture, as decoded.
    pub fn fixture_frame(name: &str) -> Frame {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../lac-container/tests/fixtures")
            .join(name);
        let mut source = lac_app::Source::open(&path).expect("opens");
        let mut decoder = source.video_decoder().expect("has video");
        source.video_frame(&mut decoder, 0).expect("decodes")
    }

    /// `p` shaded at the centre of every pixel of a `width` x `height`
    /// picture, as 8-bit RGB.
    pub fn shade_all(p: &Prepared, width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let u = (x as f32 + 0.5) / width as f32;
                let v = (y as f32 + 0.5) / height as f32;
                out.extend(shade(p, u, v).map(|c| (c * 255.0).round() as u8));
            }
        }
        out
    }

    /// Asserts that two RGB pictures differ by at most 1 per component.
    pub fn assert_close(got: &[u8], want: &[u8]) {
        assert_eq!(got.len(), want.len());
        for (i, (g, w)) in got.iter().zip(want).enumerate() {
            assert!(g.abs_diff(*w) <= 1, "byte {i}: {g} vs {w}");
        }
    }

    #[test]
    fn decoded_planes_shade_like_the_cpu_conversion() {
        let frame = fixture_frame("video_h264_opengop_rot90.mp4");
        let want = frame.to_rgb();
        let image = Image::Planes(frame);
        let p = prepare(&image);
        assert!(p.planes.len() > 1, "uploads planes, not RGB");
        assert_close(&shade_all(&p, want.width, want.height), &want.rgb);
    }

    /// What the shaders compute at texture coordinate (`u`, `v`) of
    /// `p`, sampling the nearest texel: R, G and B, clamped to 0..=1.
    pub fn shade(p: &Prepared, u: f32, v: f32) -> [f32; 3] {
        let sample = |plane: &Plane| -> [f32; 4] {
            let x = ((u * plane.width as f32) as usize).min(plane.width as usize - 1);
            let y = ((v * plane.height as f32) as usize).min(plane.height as usize - 1);
            let at = y * plane.bytes_per_row + x * plane.format.texel_bytes();
            let d = &plane.data;
            let b = |i: usize| f32::from(d[at + i]) / 255.0;
            let w = |i: usize| {
                f32::from(u16::from_le_bytes([d[at + 2 * i], d[at + 2 * i + 1]])) / 65_535.0
            };
            match plane.format {
                TexFormat::R8 => [b(0), 0.0, 0.0, 1.0],
                TexFormat::Rg8 => [b(0), b(1), 0.0, 1.0],
                TexFormat::R16 => [w(0), 0.0, 0.0, 1.0],
                TexFormat::Rg16 => [w(0), w(1), 0.0, 1.0],
                TexFormat::Rgba8 => [b(0), b(1), b(2), b(3)],
            }
        };
        let mut s = [[0.0; 4]; 3];
        for (i, plane) in p.planes.iter().enumerate() {
            s[i] = sample(plane);
        }
        let dot = |a: &[f32; 4], b: &[f32; 4]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        let c = |k: usize| (0..3).map(|t| dot(&p.pick[3 * k + t], &s[t])).sum::<f32>();
        let ycc = [c(0), c(1), c(2), 1.0];
        std::array::from_fn(|k| dot(&p.rows[k], &ycc).clamp(0.0, 1.0))
    }

    #[test]
    fn picks_select_planar_and_interleaved_channels() {
        let rows = IDENTITY;
        let plane = |format, data: Vec<u8>| Plane {
            format,
            width: 1,
            height: 1,
            bytes_per_row: data.len(),
            data: Cow::Owned(data),
        };
        let r8 = |v| plane(TexFormat::R8, vec![v]);
        // Planar: Y, Cb, Cr in the red channel of textures 0, 1, 2.
        let p = Prepared {
            planes: vec![r8(10), r8(20), r8(30)],
            pick: picks((0, 0), Some(((1, 0), (2, 0)))),
            rows,
        };
        assert_eq!(
            shade(&p, 0.5, 0.5).map(|c| (c * 255.0).round()),
            [10.0, 20.0, 30.0]
        );
        // NV12: chroma interleaved in texture 1.
        let p = Prepared {
            planes: vec![r8(10), plane(TexFormat::Rg8, vec![20, 30])],
            pick: picks((0, 0), Some(((1, 0), (1, 1)))),
            rows,
        };
        assert_eq!(
            shade(&p, 0.5, 0.5).map(|c| (c * 255.0).round()),
            [10.0, 20.0, 30.0]
        );
        // Gray: no chroma, and the alpha of unused textures never counts.
        let p = Prepared {
            planes: vec![r8(10)],
            pick: picks((0, 0), None),
            rows,
        };
        assert_eq!(
            shade(&p, 0.5, 0.5).map(|c| (c * 255.0).round()),
            [10.0, 0.0, 0.0]
        );
    }

    #[test]
    fn rgb_pictures_become_rgba_with_the_identity() {
        let image = Image::Rgb(Picture {
            time: 0,
            width: 2,
            height: 1,
            rgb: vec![1, 2, 3, 4, 5, 6],
        });
        let p = prepare(&image);
        assert_eq!(p.planes.len(), 1);
        assert_eq!(p.planes[0].format, TexFormat::Rgba8);
        assert_eq!(&*p.planes[0].data, &[1, 2, 3, 255, 4, 5, 6, 255]);
        let px = shade(&p, 0.75, 0.5).map(|c| (c * 255.0).round());
        assert_eq!(px, [4.0, 5.0, 6.0]);
    }

    #[test]
    fn quad_covers_the_viewport_with_the_turned_picture() {
        let q = quad(0);
        assert_eq!(
            q,
            [
                -1.0, 1.0, 0.0, 0.0, // top left
                1.0, 1.0, 1.0, 0.0, // top right
                -1.0, -1.0, 0.0, 1.0, // bottom left
                1.0, -1.0, 1.0, 1.0, // bottom right
            ]
        );
        // Turned clockwise by 90 degrees, the source's bottom left corner
        // is at the top left.
        let q = quad(90);
        assert_eq!(&q[0..4], &[-1.0, 1.0, 0.0, 1.0]);
        assert_eq!(&q[4..8], &[1.0, 1.0, 0.0, 0.0]);
        // Every rotation shows each source corner exactly once.
        for r in [0, 90, 180, 270] {
            let q = quad(r);
            let mut corners: Vec<_> = (0..4).map(|i| (q[4 * i + 2], q[4 * i + 3])).collect();
            corners.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
            assert_eq!(corners, [(0.0, 0.0), (0.0, 1.0), (1.0, 0.0), (1.0, 1.0)]);
        }
    }
}
