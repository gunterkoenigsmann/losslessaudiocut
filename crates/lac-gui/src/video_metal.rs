//! The video pane's Metal renderer (macOS): the same drawing as
//! [`crate::video_gl`], for when macOS no longer offers OpenGL (Apple
//! deprecated it in macOS 10.14). Issue #172.
//!
//! A `CAMetalLayer` is a sublayer of the native view (`NSView`) of a
//! panel filling the pane. The textures and uniforms come from
//! [`crate::video_gpu::prepare`] and the placement from
//! [`crate::video_gpu::quad`], so both GPU renderers compute the same
//! picture; the Metal Shading Language shader below is the GLSL one
//! translated.
//!
//! The Metal calls are Objective-C FFI, so this module may use `unsafe`
//! (AGENTS.md). Everything runs on the GUI thread, which AppKit requires
//! for views and layers.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::NSView;
use objc2_core_foundation::CGSize;
use objc2_foundation::NSString;
use objc2_metal::{
    MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLLibrary, MTLLoadAction, MTLPixelFormat,
    MTLPrimitiveType, MTLRegion, MTLRenderCommandEncoder, MTLRenderPassDescriptor,
    MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLSamplerAddressMode,
    MTLSamplerDescriptor, MTLSamplerMinMagFilter, MTLSamplerState, MTLStoreAction, MTLTexture,
    MTLTextureDescriptor, MTLTextureUsage, MTLViewport,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer, CATransaction};

use crate::video_gpu::{Plane, Prepared, TexFormat, quad};

/// The shaders: a quad from `quad` (x, y, u, v per corner, in buffer 0)
/// and the colour conversion of `video_gpu` (uniforms in buffer 0 of the
/// fragment stage).
const SHADERS: &str = r"
#include <metal_stdlib>
using namespace metal;

struct Corner {
    float4 position [[position]];
    float2 uv;
};

vertex Corner corners(uint id [[vertex_id]], constant float4 *quad [[buffer(0)]]) {
    Corner c;
    c.position = float4(quad[id].xy, 0.0, 1.0);
    c.uv = quad[id].zw;
    return c;
}

struct Colour {
    float4 pick[9];
    float4 rows[3];
};

fragment float4 colour(Corner in [[stage_in]],
                       constant Colour &c [[buffer(0)]],
                       texture2d<float> plane0 [[texture(0)]],
                       texture2d<float> plane1 [[texture(1)]],
                       texture2d<float> plane2 [[texture(2)]],
                       sampler s [[sampler(0)]]) {
    float4 s0 = plane0.sample(s, in.uv);
    float4 s1 = plane1.sample(s, in.uv);
    float4 s2 = plane2.sample(s, in.uv);
    float4 ycc = float4(
        dot(c.pick[0], s0) + dot(c.pick[1], s1) + dot(c.pick[2], s2),
        dot(c.pick[3], s0) + dot(c.pick[4], s1) + dot(c.pick[5], s2),
        dot(c.pick[6], s0) + dot(c.pick[7], s1) + dot(c.pick[8], s2),
        1.0);
    float3 rgb = float3(dot(c.rows[0], ycc), dot(c.rows[1], ycc), dot(c.rows[2], ycc));
    return float4(saturate(rgb), 1.0);
}
";

/// What the layer and offscreen targets store: blue, green, red, alpha,
/// 8 bits each, without sRGB conversion (like OpenGL's default
/// framebuffer, so both show the same values).
const TARGET_FORMAT: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm;

type Device = ProtocolObject<dyn MTLDevice>;
type Texture = ProtocolObject<dyn MTLTexture>;

/// The uniforms as the shader's `Colour` struct lays them out: 12
/// `float4`s.
#[repr(C)]
#[derive(Clone, Copy)]
struct Colour {
    pick: [[f32; 4]; 9],
    rows: [[f32; 4]; 3],
}

/// The device, pipeline and textures, independent of where they draw.
pub struct Renderer {
    device: Retained<Device>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
    /// The picture's textures; unused units hold a 1x1 texture (Metal
    /// must not sample an unbound slot).
    textures: [Retained<Texture>; 3],
    colour: Colour,
    /// "Metal (Apple M1)", for the log.
    pub description: String,
}

impl Renderer {
    /// The system's default Metal device with the shaders compiled;
    /// `smooth` chooses linear filtering (else nearest texels, which the
    /// tests compare exactly).
    pub fn new(smooth: bool) -> Result<Renderer, String> {
        let device = MTLCreateSystemDefaultDevice().ok_or("no Metal device")?;
        let description = format!("Metal ({})", device.name());
        let queue = device.newCommandQueue().ok_or("no Metal command queue")?;
        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(SHADERS), None)
            .map_err(|e| format!("Metal shader: {}", e.localizedDescription()))?;
        let function = |name: &str| {
            library
                .newFunctionWithName(&NSString::from_str(name))
                .ok_or(format!("Metal shader: no function {name}"))
        };
        let descriptor = MTLRenderPipelineDescriptor::new();
        let (vertex, fragment) = (function("corners")?, function("colour")?);
        descriptor.setVertexFunction(Some(&vertex));
        descriptor.setFragmentFunction(Some(&fragment));
        // SAFETY: index 0 exists, a pipeline has up to 8 colour
        // attachments.
        unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) }
            .setPixelFormat(TARGET_FORMAT);
        let pipeline = device
            .newRenderPipelineStateWithDescriptor_error(&descriptor)
            .map_err(|e| format!("Metal pipeline: {}", e.localizedDescription()))?;
        let s = MTLSamplerDescriptor::new();
        let filter = if smooth {
            MTLSamplerMinMagFilter::Linear
        } else {
            MTLSamplerMinMagFilter::Nearest
        };
        s.setMinFilter(filter);
        s.setMagFilter(filter);
        s.setSAddressMode(MTLSamplerAddressMode::ClampToEdge);
        s.setTAddressMode(MTLSamplerAddressMode::ClampToEdge);
        let sampler = device
            .newSamplerStateWithDescriptor(&s)
            .ok_or("no Metal sampler")?;
        let blank = || texture(&device, TexFormat::R8, 1, 1).ok_or("no Metal texture");
        let textures = [blank()?, blank()?, blank()?];
        Ok(Renderer {
            device,
            queue,
            pipeline,
            sampler,
            textures,
            colour: Colour {
                pick: [[0.0; 4]; 9],
                rows: [[0.0; 4]; 3],
            },
            description,
        })
    }

    /// Uploads a picture's textures and uniforms for the next draws.
    pub fn upload(&mut self, p: &Prepared) -> Result<(), String> {
        for (unit, plane) in p.planes.iter().enumerate().take(3) {
            let t = &self.textures[unit];
            let fits = t.pixelFormat() == pixel_format(plane.format)
                && t.width() == plane.width as usize
                && t.height() == plane.height as usize;
            if !fits {
                self.textures[unit] =
                    texture(&self.device, plane.format, plane.width, plane.height)
                        .ok_or("no Metal texture")?;
            }
            fill(&self.textures[unit], plane);
        }
        self.colour = Colour {
            pick: p.pick,
            rows: p.rows,
        };
        Ok(())
    }

    /// Encodes drawing the uploaded picture into `target`: cleared to
    /// black, the picture turned by `rotation` into `rect` (x, y, width,
    /// height in pixels from the top left), if any.
    fn encode(
        &self,
        buffer: &ProtocolObject<dyn MTLCommandBuffer>,
        target: &Texture,
        rect: Option<(u32, u32, u32, u32)>,
        rotation: u16,
    ) -> Result<(), String> {
        let pass = MTLRenderPassDescriptor::new();
        // SAFETY: index 0 exists, a pass has up to 8 colour attachments.
        let attachment = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        attachment.setTexture(Some(target));
        attachment.setLoadAction(MTLLoadAction::Clear);
        attachment.setClearColor(MTLClearColor {
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        });
        attachment.setStoreAction(MTLStoreAction::Store);
        let encoder = buffer
            .renderCommandEncoderWithDescriptor(&pass)
            .ok_or("no Metal render encoder")?;
        if let Some((x, y, w, h)) = rect {
            encoder.setRenderPipelineState(&self.pipeline);
            // Metal's viewport origin is the top left, like `rect`'s.
            encoder.setViewport(MTLViewport {
                originX: f64::from(x),
                originY: f64::from(y),
                width: f64::from(w),
                height: f64::from(h),
                znear: 0.0,
                zfar: 1.0,
            });
            let q = quad(rotation);
            // SAFETY: the pointers and lengths describe live values;
            // Metal copies them before the calls return. Slots and units
            // are those the shaders declare.
            unsafe {
                encoder.setVertexBytes_length_atIndex(
                    NonNull::from(&q).cast::<c_void>(),
                    size_of_val(&q),
                    0,
                );
                encoder.setFragmentBytes_length_atIndex(
                    NonNull::from(&self.colour).cast::<c_void>(),
                    size_of::<Colour>(),
                    0,
                );
                for (unit, t) in self.textures.iter().enumerate() {
                    encoder.setFragmentTexture_atIndex(Some(t), unit);
                }
                encoder.setFragmentSamplerState_atIndex(Some(&self.sampler), 0);
                encoder.drawPrimitives_vertexStart_vertexCount(
                    MTLPrimitiveType::TriangleStrip,
                    0,
                    4,
                );
            }
        }
        encoder.endEncoding();
        Ok(())
    }

    /// Draws the uploaded picture into a new `width` x `height` image as
    /// the pane would, and returns its pixels as RGB rows. For tests.
    #[cfg(test)]
    pub fn render_offscreen(
        &self,
        width: u32,
        height: u32,
        rect: Option<(u32, u32, u32, u32)>,
        rotation: u16,
    ) -> Result<Vec<u8>, String> {
        use objc2_metal::{MTLBlitCommandEncoder, MTLStorageMode};
        let d = target_descriptor(width, height);
        // Managed: the CPU can read it back after a synchronize, on
        // Intel and Apple GPUs alike.
        d.setStorageMode(MTLStorageMode::Managed);
        d.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
        let target = self
            .device
            .newTextureWithDescriptor(&d)
            .ok_or("no Metal target")?;
        let buffer = self
            .queue
            .commandBuffer()
            .ok_or("no Metal command buffer")?;
        self.encode(&buffer, &target, rect, rotation)?;
        let blit = buffer.blitCommandEncoder().ok_or("no Metal blit encoder")?;
        blit.synchronizeResource(ProtocolObject::from_ref(&*target));
        blit.endEncoding();
        buffer.commit();
        buffer.waitUntilCompleted();
        let row = width as usize * 4;
        let mut bgra = vec![0u8; row * height as usize];
        // SAFETY: `bgra` holds `height` rows of `row` bytes, the whole
        // region read.
        unsafe {
            target.getBytes_bytesPerRow_fromRegion_mipmapLevel(
                NonNull::new(bgra.as_mut_ptr().cast()).ok_or("null buffer")?,
                row,
                region(width, height),
                0,
            );
        }
        Ok(bgra
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[2], p[1], p[0]])
            .collect())
    }
}

/// A texture descriptor for a `width` x `height` render target.
fn target_descriptor(width: u32, height: u32) -> Retained<MTLTextureDescriptor> {
    // SAFETY: plain values; sizes of 0 are clamped by the caller.
    unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            TARGET_FORMAT,
            width.max(1) as usize,
            height.max(1) as usize,
            false,
        )
    }
}

/// The Metal format of a texture format.
fn pixel_format(f: TexFormat) -> MTLPixelFormat {
    match f {
        TexFormat::R8 => MTLPixelFormat::R8Unorm,
        TexFormat::Rg8 => MTLPixelFormat::RG8Unorm,
        TexFormat::R16 => MTLPixelFormat::R16Unorm,
        TexFormat::Rg16 => MTLPixelFormat::RG16Unorm,
        TexFormat::Rgba8 => MTLPixelFormat::RGBA8Unorm,
    }
}

/// A new sampled texture, or `None` if the device refuses.
fn texture(
    device: &Device,
    format: TexFormat,
    width: u32,
    height: u32,
) -> Option<Retained<Texture>> {
    // SAFETY: plain values.
    let d = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            pixel_format(format),
            width.max(1) as usize,
            height.max(1) as usize,
            false,
        )
    };
    d.setUsage(MTLTextureUsage::ShaderRead);
    device.newTextureWithDescriptor(&d)
}

/// The region of a whole `width` x `height` texture.
fn region(width: u32, height: u32) -> MTLRegion {
    MTLRegion {
        origin: objc2_metal::MTLOrigin { x: 0, y: 0, z: 0 },
        size: objc2_metal::MTLSize {
            width: width as usize,
            height: height as usize,
            depth: 1,
        },
    }
}

/// Copies `plane` into `t`, which has its format and size.
fn fill(t: &Texture, plane: &Plane) {
    let Some(data) = NonNull::new(plane.data.as_ptr().cast_mut().cast::<c_void>()) else {
        return;
    };
    // SAFETY: `plane.data` holds `height` rows of `bytes_per_row` bytes
    // (the last one at least `width` texels), all that Metal reads for
    // the region; it only reads through the pointer.
    unsafe {
        t.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
            region(plane.width, plane.height),
            0,
            data,
            plane.bytes_per_row,
        );
    }
}

/// A [`Renderer`] drawing into a `CAMetalLayer` on a view.
pub struct LayerView {
    /// The view hosting the layer.
    view: Retained<NSView>,
    layer: Retained<CAMetalLayer>,
    /// Draws.
    pub renderer: Renderer,
}

impl LayerView {
    /// Gives the native view `ns_view` a Metal sublayer drawn by a new
    /// [`Renderer`].
    ///
    /// # Safety
    /// `ns_view` must be an `NSView` (what wxWidgets' `GetHandle` returns
    /// on macOS) that outlives the result, and this must run on the main
    /// thread.
    pub unsafe fn attach(ns_view: *mut c_void) -> Result<LayerView, String> {
        let renderer = Renderer::new(true)?;
        // SAFETY: the caller guarantees an NSView; retaining keeps it
        // alive as long as we use it.
        let view = unsafe { Retained::retain(ns_view.cast::<NSView>()) }.ok_or("no NSView")?;
        let layer = CAMetalLayer::new();
        layer.setDevice(Some(&renderer.device));
        layer.setPixelFormat(TARGET_FORMAT);
        // We draw whole frames and read nothing back.
        layer.setFramebufferOnly(true);
        // A sublayer of the view's own layer: works whether wxWidgets
        // made the view layer-backed already or not, and AppKit's drawing
        // into the view's layer stays underneath. `draw` keeps its frame
        // on the view's bounds.
        view.setWantsLayer(true);
        view.layer()
            .ok_or("the view has no layer")?
            .addSublayer(&layer);
        Ok(LayerView {
            view,
            layer,
            renderer,
        })
    }

    /// Gives `panel`'s native view a Metal sublayer drawn by a new
    /// [`Renderer`]. Must run on the GUI thread.
    pub fn attach_to(panel: &wxdragon::prelude::Panel) -> Result<LayerView, String> {
        use wxdragon::prelude::WxWidget;
        // SAFETY: on macOS wxWidgets' `GetHandle` gives the panel's
        // NSView, which the result keeps alive (retained); the caller is
        // on the GUI thread.
        unsafe { LayerView::attach(panel.get_handle()) }
    }

    /// The view's size in physical pixels.
    pub fn pixel_size(&self) -> (u32, u32) {
        let scale = self.view.window().map_or(1.0, |w| w.backingScaleFactor());
        let b = self.view.bounds();
        let px = |v: f64| (v * scale).round().max(0.0) as u32;
        (px(b.size.width), px(b.size.height))
    }

    /// Draws the uploaded picture turned by `rotation` into `rect` of
    /// the view (pixels from the top left, see `pixel_size`), black
    /// around it; all black for `None`.
    pub fn draw(&self, rect: Option<(u32, u32, u32, u32)>, rotation: u16) -> Result<(), String> {
        let (w, h) = self.pixel_size();
        if w == 0 || h == 0 {
            return Ok(());
        }
        let scale = self.view.window().map_or(1.0, |w| w.backingScaleFactor());
        // Without animating the change, which a layer would by default.
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        self.layer.setFrame(self.view.bounds());
        CATransaction::commit();
        self.layer.setContentsScale(scale);
        self.layer.setDrawableSize(CGSize {
            width: f64::from(w),
            height: f64::from(h),
        });
        let drawable = self.layer.nextDrawable().ok_or("no Metal drawable")?;
        let r = &self.renderer;
        let buffer = r.queue.commandBuffer().ok_or("no Metal command buffer")?;
        r.encode(&buffer, &drawable.texture(), rect, rotation)?;
        buffer.presentDrawable(ProtocolObject::from_ref(&*drawable));
        buffer.commit();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video_gpu::prepare;
    use crate::video_gpu::tests::{assert_close, fixture_frame};
    use lac_app::video::{Image, fit};

    /// A renderer with nearest-texel sampling, or `None` (with a note in
    /// the test output) on a Mac without a Metal device, such as some
    /// virtual machines.
    fn renderer() -> Option<Renderer> {
        match Renderer::new(false) {
            Ok(r) => {
                println!("testing with {}", r.description);
                Some(r)
            }
            Err(e) if e == "no Metal device" => {
                println!("no Metal device here; nothing to compare");
                None
            }
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn metal_draws_like_the_cpu_conversion() {
        let Some(mut r) = renderer() else {
            return;
        };
        let frame = fixture_frame("video_h264_opengop_rot90.mp4");
        let cpu = frame.to_rgb();
        let (w, h) = (cpu.width, cpu.height);
        let image = Image::Planes(frame);
        r.upload(&prepare(&image)).expect("uploads");
        // Unscaled and unturned: every pixel as the CPU converts it.
        let got = r
            .render_offscreen(w, h, Some((0, 0, w, h)), 0)
            .expect("renders");
        assert_close(&got, &cpu.rgb);
        // Turned: as `fit` turns the CPU's picture.
        for rotation in [90, 180, 270] {
            let (dw, dh) = if rotation == 180 { (w, h) } else { (h, w) };
            let want = fit(&cpu, rotation, dw, dh).expect("fits");
            let want: Vec<u8> = want
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[1], p[2]])
                .collect();
            let got = r
                .render_offscreen(dw, dh, Some((0, 0, dw, dh)), rotation)
                .expect("renders");
            assert_close(&got, &want);
        }
    }

    #[test]
    fn metal_draws_rgb_pictures_and_black_around_them() {
        let Some(mut r) = renderer() else {
            return;
        };
        let image = Image::Rgb(lac_app::video::Picture {
            time: 0,
            width: 2,
            height: 1,
            rgb: vec![255, 0, 0, 0, 0, 255],
        });
        r.upload(&prepare(&image)).expect("uploads");
        // A 4 x 3 target with the picture in the middle row.
        let got = r
            .render_offscreen(4, 3, Some((1, 1, 2, 1)), 0)
            .expect("renders");
        let black = [0u8; 12];
        assert_eq!(&got[..12], &black, "top row");
        assert_eq!(&got[12..24], &[0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 0]);
        assert_eq!(&got[24..], &black, "bottom row");
        // Without a picture: all black.
        let got = r.render_offscreen(4, 3, None, 0).expect("renders");
        assert!(got.iter().all(|&b| b == 0));
    }
}
