//! Track tints and the selection colour (issues #83, #98, #126 and #132)
//! as wxWidgets colours, and the conversions between the two. Their strengths are the user's
//! [`Appearance`]; the colours themselves are worked out in
//! `lac_app::appearance`.

use lac_app::appearance::{Appearance, Rgb};
use wxdragon::prelude::Colour;

/// The tint of track `index` (0-based) behind the waveform, whose
/// background is `background`.
pub fn waveform(a: &Appearance, index: usize, background: Colour) -> Colour {
    colour(a.track_tint(index, rgb(background)))
}

/// The selection's colour on `background`.
pub fn selection(a: &Appearance, background: Colour) -> Colour {
    colour(a.selection(rgb(background)))
}

/// `c` as an `(r, g, b)` triple.
pub fn rgb(c: Colour) -> Rgb {
    (c.r, c.g, c.b)
}

/// The colour of an `(r, g, b)` triple.
pub fn colour((r, g, b): Rgb) -> Colour {
    Colour::rgb(r, g, b)
}
