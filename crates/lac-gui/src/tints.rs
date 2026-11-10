//! Track tints and the selection colour (issues #83, #98, #126 and #132)
//! as wxWidgets colours. Their strengths are the user's
//! [`Appearance`]; the colours themselves are worked out in
//! `lac_app::appearance`.

use lac_app::appearance::{Appearance, Rgb};
use wxdragon::prelude::Colour;

/// Background of the track list's rows when there is only one track.
pub const LIST_PLAIN: Colour = Colour::rgb(0xff, 0xff, 0xff);

/// The tint of track `index` (0-based) behind the waveform, whose
/// background is `background`.
pub fn waveform(a: &Appearance, index: usize, background: Colour) -> Colour {
    colour(a.track_tint(index, rgb(background)))
}

/// The tint of track `index` (0-based) behind its row of the track list.
pub fn list(a: &Appearance, index: usize) -> Colour {
    colour(a.list_tint(index, rgb(LIST_PLAIN)))
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
fn colour((r, g, b): Rgb) -> Colour {
    Colour::rgb(r, g, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lac_app::appearance::distance;

    #[test]
    fn list_tints_stay_faint_on_the_light_grid() {
        // The text must stay readable at the strongest setting.
        let strongest = Appearance {
            track_tint: 50,
            selection: 60,
        };
        for i in 0..4 {
            let d = distance(rgb(list(&strongest, i)), rgb(LIST_PLAIN));
            assert!(d < 200, "list tint {i}: {d}");
            let d = distance(rgb(list(&Appearance::default(), i)), rgb(LIST_PLAIN));
            assert!((10..120).contains(&d), "default list tint {i}: {d}");
        }
    }
}
