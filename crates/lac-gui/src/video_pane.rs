//! The video pane: the picture of a video file at the cursor, or at the
//! playhead while playing, fitted into the pane and turned as the file
//! says. The pictures come from a [`lac_app::video::VideoLoader`],
//! polled by the main window's timer.
//!
//! Two ways of drawing, chosen at run time (architecture section 13,
//! "Drawing the picture"):
//!
//! - **OpenGL** ([`crate::video_gl`]): an OpenGL canvas fills the pane,
//!   the GPU converts the decoded planes to RGB and scales them. Used
//!   when the display offers OpenGL and a 3.2 core context works.
//! - **Bitmaps**: the pane itself paints the picture converted to RGB
//!   and scaled by [`lac_app::video::fit`]. Used without OpenGL, when it
//!   fails at any point (the canvas is then hidden for good), and when
//!   the environment variable `LAC_VIDEO_RENDERER` is `software`. This
//!   is also the way on for systems that drop OpenGL (macOS has
//!   deprecated it).

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

/// Names the environment variable that forces drawing with bitmaps
/// (value `software`), for broken drivers and for comparing the two.
const RENDERER_VARIABLE: &str = "LAC_VIDEO_RENDERER";

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

/// The video pane: a panel, with an OpenGL canvas filling it when
/// OpenGL is used.
#[derive(Clone)]
pub struct VideoPane {
    /// The pane's window, what the pane manager docks.
    pub panel: Panel,
    /// The OpenGL canvas, if OpenGL is tried.
    canvas: Option<GLCanvas>,
    gl: Rc<RefCell<Gl>>,
}

/// The canvas' pixel format: true colour, double buffered.
fn attributes() -> GLAttributes {
    let mut a = GLAttributes::new();
    a.platform_defaults().rgba().double_buffer().end_list();
    a
}

impl VideoPane {
    /// Creates the (empty) pane, with an OpenGL canvas unless the
    /// display cannot do OpenGL or the user asked for bitmaps.
    pub fn create(parent: &Frame) -> VideoPane {
        let panel = Panel::builder(parent).build();
        // Painted completely by `draw`; erasing first would flicker.
        panel.set_background_style(BackgroundStyle::Paint);
        let forced = std::env::var(RENDERER_VARIABLE).is_ok_and(|v| v == "software");
        let attrs = attributes();
        let canvas = (!forced && GLCanvas::is_display_supported_attrs(&attrs)).then(|| {
            let canvas = GLCanvas::builder(&panel).build_with_attributes(&attrs);
            // Everything is drawn by the renderer.
            canvas.set_background_style(BackgroundStyle::Paint);
            let sizer = BoxSizer::builder(Orientation::Vertical).build();
            sizer.add(&canvas, 1, SizerFlag::Expand, 0);
            panel.set_sizer(sizer, true);
            canvas
        });
        if canvas.is_none() {
            crate::log_pane::append(&trf(
                "Video: drawing pictures without OpenGL ({reason})",
                &[(
                    "reason",
                    &if forced {
                        format!("{RENDERER_VARIABLE}=software")
                    } else {
                        "no OpenGL display".to_owned()
                    },
                )],
            ));
        }
        VideoPane {
            panel,
            canvas,
            gl: Rc::new(RefCell::new(Gl::Waiting)),
        }
    }

    /// What the video loader should deliver for this pane: decoded
    /// planes while OpenGL may draw them, RGB otherwise.
    pub fn output(&self) -> Output {
        if self.canvas.is_some() && !matches!(*self.gl.borrow(), Gl::Failed) {
            Output::Planes
        } else {
            Output::Rgb
        }
    }

    /// The OpenGL canvas, while OpenGL is (still) tried.
    pub fn canvas(&self) -> Option<GLCanvas> {
        self.canvas
            .filter(|_| !matches!(*self.gl.borrow(), Gl::Failed))
    }

    /// Repaints the picture.
    pub fn refresh(&self) {
        match self.canvas() {
            Some(c) => c.refresh(false, None),
            None => self.panel.refresh(false, None),
        }
    }

    /// Paints the canvas with OpenGL: `image` turned clockwise by
    /// `rotation` degrees, centred and as large as fits, black around
    /// it. Call from the canvas' paint handler, after creating its
    /// `PaintDC`.
    ///
    /// On failure the canvas is hidden for good, the pane paints with
    /// bitmaps from now on, and the reason is returned (once); the
    /// caller then switches the loader to [`Output::Rgb`].
    pub fn paint_gl(&self, image: Option<&Arc<Image>>, rotation: u16) -> Result<(), String> {
        let Some(canvas) = self.canvas() else {
            return Ok(());
        };
        let mut gl = self.gl.borrow_mut();
        let result = paint(&mut gl, &canvas, image, rotation);
        if result.is_err() {
            *gl = Gl::Failed;
            drop(gl);
            canvas.hide();
            self.panel.layout();
            self.panel.refresh(false, None);
        }
        result
    }
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
        crate::log_pane::append(&trf(
            "Video: drawing pictures with {renderer}",
            &[("renderer", &renderer.description)],
        ));
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
