//! Track tints (issues #83 and #98): each track gets a faint colour of its
//! own, behind its part of the waveform and behind its row of the track
//! list, so which part belongs to which row is visible at a glance.
//!
//! The tints cycle through a few hues that differ from each other and
//! from the waveform's own marks. Each is blended into the background it
//! is drawn on, strongly enough to be seen in the gaps of a dense
//! waveform, but faintly enough that the waveform, the selection and the
//! text stay readable on the dark waveform and on the light grid alike.

use wxdragon::prelude::Colour;

/// The hues the tints cycle through: blue, amber, green, magenta. Four
/// keep neighbours distinct while not making the waveform look busy.
const HUES: [(u8, u8, u8); 4] = [
    (0x50, 0x90, 0xf0),
    (0xf0, 0xb0, 0x40),
    (0x50, 0xc8, 0x70),
    (0xd0, 0x60, 0xd0),
];

/// How much of the hue goes into the dark waveform background. 0.12 was
/// too faint to see on many screens, above all when the peaks fill most
/// of the lane and the tint shows only in the gaps; a quarter stays well
/// below the waveform's own marks.
const WAVEFORM_STRENGTH: f64 = 0.25;
/// How much of the hue goes into the light track list background.
const LIST_STRENGTH: f64 = 0.16;

/// Background of the track list's rows when there is only one track.
pub const LIST_PLAIN: Colour = Colour::rgb(0xff, 0xff, 0xff);

/// The tint of track `index` (0-based) behind the waveform, whose
/// background is `background`.
pub fn waveform(index: usize, background: Colour) -> Colour {
    blend(background, HUES[index % HUES.len()], WAVEFORM_STRENGTH)
}

/// The tint of track `index` (0-based) behind its row of the track list.
pub fn list(index: usize) -> Colour {
    blend(LIST_PLAIN, HUES[index % HUES.len()], LIST_STRENGTH)
}

/// `background` with `strength` (0..1) of `hue` mixed in.
fn blend(background: Colour, hue: (u8, u8, u8), strength: f64) -> Colour {
    let mix = |b: u8, h: u8| {
        (f64::from(b) + (f64::from(h) - f64::from(b)) * strength)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Colour::rgb(
        mix(background.r, hue.0),
        mix(background.g, hue.1),
        mix(background.b, hue.2),
    )
}

/// Sum of the channel differences: how far apart two colours look,
/// roughly. For tests that keep colours apart.
#[cfg(test)]
pub fn distance(a: Colour, b: Colour) -> u32 {
    [(a.r, b.r), (a.g, b.g), (a.b, b.b)]
        .iter()
        .map(|&(x, y)| u32::from(x.abs_diff(y)))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DARK: Colour = Colour::rgb(0x1e, 0x22, 0x28);

    #[test]
    fn neighbouring_tracks_get_different_tints() {
        for i in 0..8 {
            assert_ne!(waveform(i, DARK), waveform(i + 1, DARK), "track {i}");
            assert_ne!(list(i), list(i + 1), "track {i}");
        }
        assert_eq!(list(0), list(HUES.len()), "the hues cycle");
    }

    #[test]
    fn tints_are_visible_but_faint() {
        for i in 0..HUES.len() {
            // Clearly visible (the first, 12 % tints were not, issue
            // #126), but far closer to the background than to the hue, so
            // the waveform and the text keep their contrast.
            let w = distance(waveform(i, DARK), DARK);
            assert!((60..120).contains(&w), "waveform tint {i}: {w}");
            let l = distance(list(i), LIST_PLAIN);
            assert!((10..80).contains(&l), "list tint {i}: {l}");
        }
    }

    #[test]
    fn blending_reaches_both_ends() {
        let hue = (0x10, 0x80, 0xf0);
        assert_eq!(blend(DARK, hue, 0.0), DARK);
        assert_eq!(blend(DARK, hue, 1.0), Colour::rgb(0x10, 0x80, 0xf0));
    }
}
