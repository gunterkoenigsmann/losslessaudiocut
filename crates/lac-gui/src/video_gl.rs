//! The video pane's OpenGL renderer: uploads the decoded planes of a
//! picture as textures and lets a shader convert YCbCr to RGB, scale and
//! turn the picture (architecture section 13, "Drawing the picture").
//!
//! The colour arithmetic comes from [`GpuLayout`]; this
//! module only moves it to the GPU. One shader serves every format: it
//! samples up to three textures, picks the luma and chroma channels with
//! the dot products in [`picks`] and applies the layout's 3x4 matrix. An
//! RGB picture (formats without a GPU layout) is one RGB texture with an
//! identity matrix.
//!
//! Needs OpenGL 3.2 core (GLSL 1.50), which macOS gives as its newest
//! version; [`crate::video_pane`] falls back to drawing bitmaps when it
//! is missing or anything here fails.
//!
//! The GL calls are FFI and so `unsafe`; this module is the one place in
//! `lac-gui` allowed to use it (AGENTS.md). Every call is made on the GUI
//! thread with the renderer's context current, which is what the
//! `// SAFETY:` comments rely on.

#![allow(unsafe_code)]

use glow::{HasContext, PixelUnpackData};
use lac_app::video::{Channel, GpuLayout, GpuTexture, Image, fit_rect, source_point};

/// Passes the corner's position and texture coordinate on.
const VERTEX_SHADER: &str = r"#version 150
in vec2 pos;
in vec2 tex;
out vec2 uv;
void main() {
    uv = tex;
    gl_Position = vec4(pos, 0.0, 1.0);
}
";

/// Picks Y, Cb and Cr from the three textures (`pick[3 * component +
/// texture]` is dotted with that texture's sample) and turns them into
/// RGB with the matrix rows. Sampler arrays cannot be indexed by a
/// uniform in GLSL 1.50, hence the dot products.
const FRAGMENT_SHADER: &str = r"#version 150
uniform sampler2D plane0;
uniform sampler2D plane1;
uniform sampler2D plane2;
uniform vec4 pick[9];
uniform vec4 rows[3];
in vec2 uv;
out vec4 colour;
void main() {
    vec4 s0 = texture(plane0, uv);
    vec4 s1 = texture(plane1, uv);
    vec4 s2 = texture(plane2, uv);
    vec4 ycc = vec4(
        dot(pick[0], s0) + dot(pick[1], s1) + dot(pick[2], s2),
        dot(pick[3], s0) + dot(pick[4], s1) + dot(pick[5], s2),
        dot(pick[6], s0) + dot(pick[7], s1) + dot(pick[8], s2),
        1.0);
    vec3 rgb = vec3(dot(rows[0], ycc), dot(rows[1], ycc), dot(rows[2], ycc));
    colour = vec4(clamp(rgb, 0.0, 1.0), 1.0);
}
";

/// Attribute locations, bound before linking.
const POS: u32 = 0;
/// See [`POS`].
const TEX: u32 = 1;

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
/// picture shown there (row 0 of a texture is the picture's top).
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

/// The identity colour matrix, for RGB textures.
const IDENTITY: [[f32; 4]; 3] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
];

/// The GL objects drawing the picture.
pub struct Renderer {
    gl: glow::Context,
    program: glow::Program,
    vertices: glow::Buffer,
    textures: [glow::Texture; 3],
    pick: Option<glow::UniformLocation>,
    rows: Option<glow::UniformLocation>,
    /// The image whose textures are uploaded (compared by address, the
    /// loader hands out each picture once in an `Arc`).
    uploaded: Option<usize>,
    /// What the shader needs for the uploaded image: picks and matrix.
    colour: ([[f32; 4]; 9], [[f32; 4]; 3]),
    /// "OpenGL 4.6 (Mesa …)", for the log.
    pub description: String,
}

/// Error text for a GL object that could not be created.
fn fail(what: &str, e: String) -> String {
    format!("{what}: {e}")
}

impl Renderer {
    /// Loads the GL functions and builds the shader, vertex buffer and
    /// textures. The caller must have made a context current, and keep
    /// it current whenever it calls [`Renderer::draw`]. Fails with the
    /// reason if the context is older than 3.2 or a shader does not
    /// compile.
    pub fn new() -> Result<Renderer, String> {
        // SAFETY: a context is current (documented requirement). The loader
        // returns null for missing functions, which glow turns into
        // panicking stubs we never call without checking the version.
        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                wxdragon::prelude::GLContext::proc_address(name).cast_const()
            })
        };
        let v = gl.version();
        let description = format!("OpenGL {}.{} {}", v.major, v.minor, v.vendor_info);
        if v.is_embedded || (v.major, v.minor) < (3, 2) {
            return Err(format!("{description}: version 3.2 or newer needed"));
        }
        // SAFETY: see the module documentation.
        unsafe {
            let program = gl.create_program().map_err(|e| fail("program", e))?;
            let mut shaders = Vec::new();
            for (kind, source) in [
                (glow::VERTEX_SHADER, VERTEX_SHADER),
                (glow::FRAGMENT_SHADER, FRAGMENT_SHADER),
            ] {
                let s = gl.create_shader(kind).map_err(|e| fail("shader", e))?;
                gl.shader_source(s, source);
                gl.compile_shader(s);
                if !gl.get_shader_compile_status(s) {
                    return Err(fail("shader", gl.get_shader_info_log(s)));
                }
                gl.attach_shader(program, s);
                shaders.push(s);
            }
            gl.bind_attrib_location(program, POS, "pos");
            gl.bind_attrib_location(program, TEX, "tex");
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                return Err(fail("program", gl.get_program_info_log(program)));
            }
            for s in shaders {
                gl.detach_shader(program, s);
                gl.delete_shader(s);
            }
            gl.use_program(Some(program));
            for (unit, name) in ["plane0", "plane1", "plane2"].into_iter().enumerate() {
                let at = gl.get_uniform_location(program, name);
                gl.uniform_1_i32(at.as_ref(), unit as i32);
            }
            let pick = gl.get_uniform_location(program, "pick");
            let rows = gl.get_uniform_location(program, "rows");

            // Core profiles draw nothing without a vertex array object.
            let vao = gl
                .create_vertex_array()
                .map_err(|e| fail("vertex array", e))?;
            gl.bind_vertex_array(Some(vao));
            let vertices = gl.create_buffer().map_err(|e| fail("buffer", e))?;
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vertices));
            let stride = 4 * std::mem::size_of::<f32>() as i32;
            gl.enable_vertex_attrib_array(POS);
            gl.vertex_attrib_pointer_f32(POS, 2, glow::FLOAT, false, stride, 0);
            gl.enable_vertex_attrib_array(TEX);
            gl.vertex_attrib_pointer_f32(TEX, 2, glow::FLOAT, false, stride, 8);

            let mut textures = Vec::new();
            for unit in 0..3 {
                let t = gl.create_texture().map_err(|e| fail("texture", e))?;
                gl.active_texture(glow::TEXTURE0 + unit);
                gl.bind_texture(glow::TEXTURE_2D, Some(t));
                for (p, v) in [
                    (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                    (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                    (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                    (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
                ] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, p, v as i32);
                }
                // Something to sample before the first picture.
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::R8 as i32,
                    1,
                    1,
                    0,
                    glow::RED,
                    glow::UNSIGNED_BYTE,
                    PixelUnpackData::Slice(Some(&[0])),
                );
                textures.push(t);
            }
            // Rows of our planes start anywhere, not at 4-byte boundaries.
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            let textures = [textures[0], textures[1], textures[2]];
            let error = gl.get_error();
            if error != glow::NO_ERROR {
                return Err(format!("{description}: error {error:#x} while setting up"));
            }
            Ok(Renderer {
                gl,
                program,
                vertices,
                textures,
                pick,
                rows,
                uploaded: None,
                colour: ([[0.0; 4]; 9], IDENTITY),
                description,
            })
        }
    }

    /// Draws `image` turned by `rotation` and fitted into a canvas of
    /// `width` x `height` physical pixels, black around it (all black
    /// without a picture). The caller swaps the buffers.
    pub fn draw(
        &mut self,
        image: Option<&std::sync::Arc<Image>>,
        rotation: u16,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let gl = &self.gl;
        let int = |v: u32| i32::try_from(v).unwrap_or(i32::MAX);
        // SAFETY: see the module documentation.
        unsafe {
            gl.viewport(0, 0, int(width), int(height));
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }
        let Some(image) = image else {
            return Ok(());
        };
        let (w, h) = image.size();
        let Some((x, y, fw, fh)) = fit_rect(w, h, rotation, width, height) else {
            return Ok(());
        };
        let key = std::sync::Arc::as_ptr(image) as usize;
        if self.uploaded != Some(key) {
            self.colour = self.upload(image);
            self.uploaded = Some(key);
        }
        let (pick, rows) = &self.colour;
        let gl = &self.gl;
        // SAFETY: see the module documentation.
        unsafe {
            gl.use_program(Some(self.program));
            gl.uniform_4_f32_slice(self.pick.as_ref(), pick.as_flattened());
            gl.uniform_4_f32_slice(self.rows.as_ref(), rows.as_flattened());
            // GL counts rows from the bottom.
            gl.viewport(int(x), int(height - y - fh), int(fw), int(fh));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vertices));
            let q = quad(rotation);
            let bytes: Vec<u8> = q.iter().flat_map(|f| f.to_ne_bytes()).collect();
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &bytes, glow::STREAM_DRAW);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            match gl.get_error() {
                glow::NO_ERROR => Ok(()),
                e => Err(format!("error {e:#x} while drawing")),
            }
        }
    }

    /// Uploads `image`'s textures; returns the picks and matrix to draw
    /// them with.
    fn upload(&self, image: &Image) -> ([[f32; 4]; 9], [[f32; 4]; 3]) {
        if let Image::Planes(frame) = image
            && let Some(layout) = frame.gpu_layout().filter(native_order)
        {
            for (unit, t) in layout.textures.iter().enumerate() {
                let p = &frame.planes[t.plane];
                // `gpu_layout` checked that the plane holds this much.
                let len = (t.height as usize - 1) * p.stride + t.width as usize * texel(t);
                self.texture(unit, t, &p.data[..len]);
            }
            return (picks(layout.y, layout.chroma), layout.matrix);
        }
        let rgb = image.to_rgb();
        let t = GpuTexture {
            plane: 0,
            channels: 3,
            bytes: 1,
            width: rgb.width,
            height: rgb.height,
            row_length: rgb.width as usize,
        };
        self.texture(0, &t, &rgb.rgb);
        (picks((0, 0), Some(((0, 1), (0, 2)))), IDENTITY)
    }

    /// Fills texture `unit` with `data` as `t` describes it.
    fn texture(&self, unit: usize, t: &GpuTexture, data: &[u8]) {
        let (internal, format) = match (t.channels, t.bytes) {
            (1, 1) => (glow::R8, glow::RED),
            (2, 1) => (glow::RG8, glow::RG),
            (1, _) => (glow::R16, glow::RED),
            (2, _) => (glow::RG16, glow::RG),
            _ => (glow::RGB8, glow::RGB),
        };
        let ty = if t.bytes == 2 {
            glow::UNSIGNED_SHORT
        } else {
            glow::UNSIGNED_BYTE
        };
        let gl = &self.gl;
        let int = |v: usize| i32::try_from(v).unwrap_or(i32::MAX);
        // SAFETY: see the module documentation; `data` holds `height`
        // rows of `row_length` texels (the last one only `width`), which
        // is all GL reads with these unpack settings.
        unsafe {
            gl.active_texture(glow::TEXTURE0 + unit as u32);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.textures[unit]));
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, int(t.row_length));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                internal as i32,
                int(t.width as usize),
                int(t.height as usize),
                0,
                format,
                ty,
                PixelUnpackData::Slice(Some(data)),
            );
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
        }
    }
}

/// Bytes per texel.
fn texel(t: &GpuTexture) -> usize {
    t.channels * t.bytes
}

/// GL reads 16-bit texels in the machine's byte order; our 16-bit planes
/// are little-endian, so big-endian machines upload RGB instead.
fn native_order(layout: &GpuLayout) -> bool {
    cfg!(target_endian = "little") || layout.textures.iter().all(|t| t.bytes == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the shader computes from `samples` (one RGBA sample per
    /// texture) with `pick` and `rows`.
    fn shade(pick: &[[f32; 4]; 9], rows: &[[f32; 4]; 3], samples: [[f32; 4]; 3]) -> [f32; 3] {
        let dot = |a: &[f32; 4], b: &[f32; 4]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        let c = |k: usize| {
            (0..3)
                .map(|t| dot(&pick[3 * k + t], &samples[t]))
                .sum::<f32>()
        };
        let ycc = [c(0), c(1), c(2), 1.0];
        std::array::from_fn(|k| dot(&rows[k], &ycc))
    }

    #[test]
    fn picks_select_planar_and_interleaved_channels() {
        let rows = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
        ];
        let s = [
            [0.1, 0.2, 0.0, 1.0],
            [0.3, 0.4, 0.0, 1.0],
            [0.5, 0.6, 0.0, 1.0],
        ];
        // Planar: Y, Cb, Cr in the red channel of textures 0, 1, 2.
        let p = picks((0, 0), Some(((1, 0), (2, 0))));
        assert_eq!(shade(&p, &rows, s), [0.1, 0.3, 0.5]);
        // NV12: chroma interleaved in texture 1.
        let p = picks((0, 0), Some(((1, 0), (1, 1))));
        assert_eq!(shade(&p, &rows, s), [0.1, 0.3, 0.4]);
        // Gray: no chroma, and the alpha of unused textures never counts.
        let p = picks((0, 0), None);
        assert_eq!(shade(&p, &rows, s), [0.1, 0.0, 0.0]);
        // RGB in one texture.
        let p = picks((0, 0), Some(((0, 1), (0, 2))));
        assert_eq!(shade(&p, &IDENTITY, s), [0.1, 0.2, 0.0]);
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
