//! The video pane: the picture of a video file at the cursor, or at the
//! playhead while playing, fitted into the pane and turned as the file
//! says. The pictures come from a [`lac_app::video::VideoLoader`],
//! polled by the main window's timer.
//!
//! Ways of drawing, chosen at run time in this order (architecture
//! section 13, "Drawing the picture"):
//!
//! - **Metal** (macOS only, `video_metal`): a panel filling the pane
//!   has a `CAMetalLayer`; the GPU converts the decoded planes to RGB
//!   and scales them. For when macOS drops OpenGL (issue #172).
//! - **OpenGL** ([`crate::video_gl`]): an OpenGL canvas fills the pane
//!   and does the same. Used when the display offers OpenGL and a 3.2
//!   core context works.
//! - **Bitmaps**: the pane itself paints the picture converted to RGB
//!   and scaled by [`lac_app::video::fit`]. Used without either, when
//!   one fails at any point (its window is then hidden for good), and
//!   when the environment variable `LAC_VIDEO_RENDERER` is `software`.
//!
//! `LAC_VIDEO_RENDERER=opengl` skips Metal, for comparing the two.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use lac_app::i18n::trf;
use lac_app::video::{Image, Output, fit};
use wxdragon::prelude::*;
use wxdragon::widgets::glcanvas::{GLAttributes, GLContextAttrs};

use crate::video_gl::Renderer;

/// Around and behind the picture.
const BACKGROUND: Colour = Colour::rgb(0, 0, 0);

/// Names the environment variable that chooses the way of drawing:
/// `software` (bitmaps) for broken drivers and for comparing, `opengl`
/// to skip Metal on macOS.
const RENDERER_VARIABLE: &str = "LAC_VIDEO_RENDERER";

/// The picture to show and its clockwise rotation in degrees, or `None`
/// when that cannot be asked right now (the window state is busy).
pub type Source = dyn Fn() -> Option<(Option<Arc<Image>>, u16)>;

/// The OpenGL side of the pane.
enum Gl {
    /// The canvas exists; the context is created at its first paint,
    /// when the window system has realized it (GTK needs that).
    Waiting,
    /// Drawing with OpenGL (boxed: the GL function table is large).
    Ready(GLContext, Box<Renderer>),
    /// OpenGL failed; the canvas is hidden.
    Failed,
}

/// The Metal side of the pane (macOS).
#[cfg(target_os = "macos")]
struct Metal {
    /// The panel whose view has the layer.
    view: Panel,
    layer: crate::video_metal::LayerView,
    /// The image whose textures are uploaded (by address, as in
    /// `video_gl`).
    uploaded: Option<usize>,
}

/// The video pane: a panel, with a Metal view or an OpenGL canvas
/// filling it while one of them is used.
#[derive(Clone)]
pub struct VideoPane {
    /// The pane's window, what the pane manager docks.
    pub panel: Panel,
    /// The OpenGL canvas, if OpenGL is tried.
    canvas: Option<GLCanvas>,
    gl: Rc<RefCell<Gl>>,
    /// Drawing with Metal, until that fails.
    #[cfg(target_os = "macos")]
    metal: Rc<RefCell<Option<Metal>>>,
    /// Where the picture comes from, see [`VideoPane::bind`].
    source: Rc<RefCell<Option<Box<Source>>>>,
}

/// The canvas' pixel format: true colour, double buffered.
fn attributes() -> GLAttributes {
    let mut a = GLAttributes::new();
    a.platform_defaults().rgba().double_buffer().end_list();
    a
}

/// Fills `panel` with `child`.
fn fill_with(panel: &Panel, child: &impl WxWidget) {
    let sizer = BoxSizer::builder(Orientation::Vertical).build();
    sizer.add(child, 1, SizerFlag::Expand, 0);
    panel.set_sizer(sizer, true);
}

/// Logs that the pane draws without a GPU renderer, and why.
fn log_without(what: &str, reason: &str) {
    crate::log_pane::append(&trf(
        "Video: drawing pictures without {renderer} ({reason})",
        &[("renderer", &what), ("reason", &reason)],
    ));
}

/// Logs the GPU renderer the pane draws with.
fn log_with(description: &str) {
    crate::log_pane::append(&trf(
        "Video: drawing pictures with {renderer}",
        &[("renderer", &description)],
    ));
}

impl VideoPane {
    /// Creates the (empty) pane, with Metal (macOS) or an OpenGL canvas
    /// unless neither works or the user asked for bitmaps. Call
    /// [`VideoPane::bind`] next.
    pub fn create(parent: &Frame) -> VideoPane {
        let panel = Panel::builder(parent).build();
        // Painted completely by `draw`; erasing first would flicker.
        panel.set_background_style(BackgroundStyle::Paint);
        let choice = std::env::var(RENDERER_VARIABLE).unwrap_or_default();
        let forced = choice == "software";
        #[cfg(target_os = "macos")]
        let metal = if forced || choice == "opengl" {
            None
        } else {
            create_metal(&panel)
        };
        #[cfg(target_os = "macos")]
        let with_metal = metal.is_some();
        #[cfg(not(target_os = "macos"))]
        let with_metal = false;
        let attrs = attributes();
        let canvas =
            (!forced && !with_metal && GLCanvas::is_display_supported_attrs(&attrs)).then(|| {
                let canvas = GLCanvas::builder(&panel).build_with_attributes(&attrs);
                // Everything is drawn by the renderer.
                canvas.set_background_style(BackgroundStyle::Paint);
                fill_with(&panel, &canvas);
                canvas
            });
        if canvas.is_none() && !with_metal {
            let reason = if forced {
                format!("{RENDERER_VARIABLE}=software")
            } else {
                "no OpenGL display".to_owned()
            };
            log_without("OpenGL", &reason);
        }
        VideoPane {
            panel,
            canvas,
            gl: Rc::new(RefCell::new(Gl::Waiting)),
            #[cfg(target_os = "macos")]
            metal: Rc::new(RefCell::new(metal)),
            source: Rc::new(RefCell::new(None)),
        }
    }

    /// Draws the pictures `source` gives: paints them and repaints
    /// after resizing, in whichever way the pane draws.
    pub fn bind(&self, source: impl Fn() -> Option<(Option<Arc<Image>>, u16)> + 'static) {
        *self.source.borrow_mut() = Some(Box::new(source));
        let (pane, panel) = (self.clone(), self.panel);
        self.panel.on_paint(move |_| {
            let dc = AutoBufferedPaintDC::new(&panel);
            let (image, rotation) = pane.current().unwrap_or((None, 0));
            draw(&dc, panel.get_client_size(), image.as_deref(), rotation);
        });
        self.panel.on_size(move |e| {
            panel.refresh(false, None);
            e.skip(true);
        });
        #[cfg(target_os = "macos")]
        if let Some(view) = self.metal.borrow().as_ref().map(|m| m.view) {
            let pane = self.clone();
            view.on_paint(move |_| {
                // The layer shows what Metal drew; the paint event only
                // has to be answered.
                let _dc = PaintDC::new(&view);
                pane.draw_metal();
            });
            let pane = self.clone();
            view.on_size(move |e| {
                pane.draw_metal();
                e.skip(true);
            });
        }
        if let Some(canvas) = self.canvas {
            let pane = self.clone();
            canvas.on_paint(move |_| {
                // Every paint handler needs a paint DC, even when GL draws.
                let _dc = PaintDC::new(&canvas);
                if let Some((image, rotation)) = pane.current() {
                    pane.paint_gl(image.as_ref(), rotation);
                }
            });
            canvas.on_size(move |e| {
                canvas.refresh(false, None);
                e.skip(true);
            });
        }
    }

    /// The picture and rotation from the source.
    fn current(&self) -> Option<(Option<Arc<Image>>, u16)> {
        self.source.borrow().as_ref().and_then(|s| s())
    }

    /// What the video loader should deliver for this pane: decoded
    /// planes while a GPU may draw them, RGB otherwise.
    pub fn output(&self) -> Output {
        #[cfg(target_os = "macos")]
        if self.metal.borrow().is_some() {
            return Output::Planes;
        }
        if self.canvas().is_some() {
            Output::Planes
        } else {
            Output::Rgb
        }
    }

    /// The OpenGL canvas, while OpenGL is (still) tried.
    fn canvas(&self) -> Option<GLCanvas> {
        self.canvas
            .filter(|_| !matches!(*self.gl.borrow(), Gl::Failed))
    }

    /// Shows the current picture again (after a new one arrived).
    pub fn refresh(&self) {
        #[cfg(target_os = "macos")]
        if self.metal.borrow().is_some() {
            // Metal draws right away; no paint event needed.
            self.draw_metal();
            return;
        }
        match self.canvas() {
            Some(c) => c.refresh(false, None),
            None => self.panel.refresh(false, None),
        }
    }

    /// Paints the canvas with OpenGL: `image` turned clockwise by
    /// `rotation` degrees, centred and as large as fits, black around
    /// it. Call from the canvas' paint handler, after creating its
    /// `PaintDC`. On failure the canvas is hidden for good and the pane
    /// paints with bitmaps from now on.
    fn paint_gl(&self, image: Option<&Arc<Image>>, rotation: u16) {
        let Some(canvas) = self.canvas() else {
            return;
        };
        let mut gl = self.gl.borrow_mut();
        if let Err(e) = paint(&mut gl, &canvas, image, rotation) {
            *gl = Gl::Failed;
            drop(gl);
            log_without("OpenGL", &e);
            canvas.hide();
            self.panel.layout();
            self.panel.refresh(false, None);
        }
    }

    /// Draws the current picture with Metal; on failure hides the Metal
    /// view for good and paints with bitmaps from now on.
    #[cfg(target_os = "macos")]
    fn draw_metal(&self) {
        // Nothing to draw while the state is busy; the next picture or
        // size change draws again.
        let Some((image, rotation)) = self.current() else {
            return;
        };
        let mut slot = self.metal.borrow_mut();
        let Some(m) = slot.as_mut() else {
            return;
        };
        if let Err(e) = draw_with_metal(m, image.as_ref(), rotation) {
            let view = m.view;
            *slot = None;
            drop(slot);
            log_without("Metal", &e);
            view.hide();
            self.panel.layout();
            self.panel.refresh(false, None);
        }
    }
}

/// A panel filling `panel` whose view hosts a Metal layer, or `None`
/// (logged) when Metal cannot draw there.
#[cfg(target_os = "macos")]
fn create_metal(panel: &Panel) -> Option<Metal> {
    let view = Panel::builder(panel).build();
    view.set_background_style(BackgroundStyle::Paint);
    fill_with(panel, &view);
    match crate::video_metal::LayerView::attach_to(&view) {
        Ok(layer) => {
            log_with(&layer.renderer.description);
            Some(Metal {
                view,
                layer,
                uploaded: None,
            })
        }
        Err(e) => {
            log_without("Metal", &e);
            view.destroy();
            panel.set_sizer(BoxSizer::builder(Orientation::Vertical).build(), true);
            None
        }
    }
}

/// [`VideoPane::draw_metal`] without the clean-up after a failure.
#[cfg(target_os = "macos")]
fn draw_with_metal(m: &mut Metal, image: Option<&Arc<Image>>, rotation: u16) -> Result<(), String> {
    let (w, h) = m.layer.pixel_size();
    let rect = match image {
        Some(image) => {
            let key = Arc::as_ptr(image) as usize;
            if m.uploaded != Some(key) {
                m.layer.renderer.upload(&crate::video_gpu::prepare(image))?;
                m.uploaded = Some(key);
            }
            let (iw, ih) = image.size();
            lac_app::video::fit_rect(iw, ih, rotation, w, h)
        }
        None => None,
    };
    m.layer.draw(rect, rotation)
}

/// [`VideoPane::paint_gl`] without the clean-up after a failure.
fn paint(
    gl: &mut Gl,
    canvas: &GLCanvas,
    image: Option<&Arc<Image>>,
    rotation: u16,
) -> Result<(), String> {
    if matches!(gl, Gl::Waiting) {
        // 3.2 core is the newest macOS offers, and enough for GLSL 1.50;
        // forward compatible because macOS gives core profiles only so.
        let mut attrs = GLContextAttrs::new();
        attrs
            .core_profile()
            .ogl_version(3, 2)
            .forward_compatible()
            .end_list();
        let context = GLContext::with_attrs(canvas, &attrs)
            .filter(GLContext::is_ok)
            .ok_or("no OpenGL 3.2 context")?;
        if !context.set_current(canvas) {
            return Err("cannot make the OpenGL context current".into());
        }
        let renderer = Renderer::new()?;
        log_with(&renderer.description);
        *gl = Gl::Ready(context, Box::new(renderer));
    }
    let Gl::Ready(context, renderer) = gl else {
        return Ok(());
    };
    if !context.set_current(canvas) {
        return Err("cannot make the OpenGL context current".into());
    }
    let size = canvas.pixel_size();
    let (Ok(w), Ok(h)) = (u32::try_from(size.width), u32::try_from(size.height)) else {
        return Ok(());
    };
    renderer.draw(image, rotation, w, h)?;
    canvas.swap_buffers();
    Ok(())
}

/// Paints `image`, turned clockwise by `rotation` degrees, centred and
/// as large as fits onto `dc` of `size`; black without a picture. The
/// bitmap way of drawing.
pub fn draw(dc: &impl DeviceContext, size: Size, image: Option<&Image>, rotation: u16) {
    dc.set_background(BACKGROUND);
    dc.clear();
    let (Ok(w), Ok(h)) = (u32::try_from(size.width), u32::try_from(size.height)) else {
        return;
    };
    let Some(f) = image.and_then(|i| fit(&i.to_rgb(), rotation, w, h)) else {
        return;
    };
    if let Some(bitmap) = Bitmap::from_rgba(&f.rgba, f.width, f.height) {
        let at = |v: u32| i32::try_from(v).unwrap_or(0);
        dc.draw_bitmap(&bitmap, at(f.x), at(f.y), false);
    }
}
