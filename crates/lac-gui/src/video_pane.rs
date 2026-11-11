//! The video pane: the picture of a video file at the cursor, or at the
//! playhead while playing, fitted into the pane and turned as the file
//! says ([`lac_app::video::fit`]). The pictures come from a
//! [`lac_app::video::VideoLoader`], polled by the main window's timer.

use lac_app::video::{Picture, fit};
use wxdragon::prelude::*;

/// Around and behind the picture.
const BACKGROUND: Colour = Colour::rgb(0, 0, 0);

/// Creates the (empty) video panel.
pub fn create(parent: &Frame) -> Panel {
    let panel = Panel::builder(parent).build();
    // Painted completely by `draw`; erasing first would flicker.
    panel.set_background_style(BackgroundStyle::Paint);
    panel
}

/// Paints `picture`, turned clockwise by `rotation` degrees, centred and
/// as large as fits onto `dc` of `size`; black without a picture.
pub fn draw(dc: &impl DeviceContext, size: Size, picture: Option<&Picture>, rotation: u16) {
    dc.set_background(BACKGROUND);
    dc.clear();
    let (Ok(w), Ok(h)) = (u32::try_from(size.width), u32::try_from(size.height)) else {
        return;
    };
    let Some(f) = picture.and_then(|p| fit(p, rotation, w, h)) else {
        return;
    };
    if let Some(bitmap) = Bitmap::from_rgba(&f.rgba, f.width, f.height) {
        let at = |v: u32| i32::try_from(v).unwrap_or(0);
        dc.draw_bitmap(&bitmap, at(f.x), at(f.y), false);
    }
}
