//! The video pane's OpenGL renderer: uploads the decoded planes of a
//! picture as textures and lets a shader convert YCbCr to RGB, scale and
//! turn the picture (architecture section 13, "Drawing the picture").
//!
//! The textures and the colour arithmetic come from
//! [`crate::video_gpu::prepare`]; this module only moves them to the GPU.
//! One shader serves every format.
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
use lac_app::video::{Image, fit_rect};

use crate::video_gpu::{IDENTITY, Plane, TexFormat, prepare, quad};

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
            let p = prepare(image);
            for (unit, plane) in p.planes.iter().enumerate() {
                self.texture(unit, plane);
            }
            self.colour = (p.pick, p.rows);
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

    /// Fills texture `unit` with `plane`.
    fn texture(&self, unit: usize, plane: &Plane) {
        let (internal, format, ty) = match plane.format {
            TexFormat::R8 => (glow::R8, glow::RED, glow::UNSIGNED_BYTE),
            TexFormat::Rg8 => (glow::RG8, glow::RG, glow::UNSIGNED_BYTE),
            TexFormat::R16 => (glow::R16, glow::RED, glow::UNSIGNED_SHORT),
            TexFormat::Rg16 => (glow::RG16, glow::RG, glow::UNSIGNED_SHORT),
            TexFormat::Rgba8 => (glow::RGBA8, glow::RGBA, glow::UNSIGNED_BYTE),
        };
        let gl = &self.gl;
        let int = |v: usize| i32::try_from(v).unwrap_or(i32::MAX);
        // SAFETY: see the module documentation; `plane.data` holds
        // `height` rows of `bytes_per_row` bytes (the last one only
        // `width` texels), which is all GL reads with these unpack
        // settings.
        unsafe {
            gl.active_texture(glow::TEXTURE0 + unit as u32);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.textures[unit]));
            // Row length counts texels, not bytes.
            let row_length = plane.bytes_per_row / plane.format.texel_bytes();
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, int(row_length));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                internal as i32,
                int(plane.width as usize),
                int(plane.height as usize),
                0,
                format,
                ty,
                PixelUnpackData::Slice(Some(&plane.data)),
            );
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
        }
    }
}
