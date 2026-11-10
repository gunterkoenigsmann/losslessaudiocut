//! How strongly the waveform and the track list are coloured (issue #132):
//! the track tints (issues #83, #98 and #126) and the selection.
//!
//! Each colour is a hue blended into the background it is drawn on, by a
//! strength in percent that the user sets in the settings dialog (#103).
//! The selection always stays [`SELECTION_LEAD`] points stronger than the
//! track tints, so a selected part never looks like just another track.
//!
//! Colours are plain `(r, g, b)` triples here; the GUI turns them into
//! its own colour type.

use std::ops::RangeInclusive;

/// A colour as red, green and blue.
pub type Rgb = (u8, u8, u8);

/// The hues the track tints cycle through: blue, amber, green, magenta.
/// Four keep neighbours distinct while not making the waveform look busy.
const HUES: [Rgb; 4] = [
    (0x50, 0x90, 0xf0),
    (0xf0, 0xb0, 0x40),
    (0x50, 0xc8, 0x70),
    (0xd0, 0x60, 0xd0),
];

/// The hue of the selection: a light blue. At the default strength it
/// gives the selection colour the waveform had before it was
/// configurable (`#485670` on `#1e2228`).
const SELECTION_HUE: Rgb = (0x96, 0xb7, 0xf6);

/// The track list's tints relative to the waveform's: on the light grid
/// the same hue shows more, so it gets less (0.16 against 0.25 before the
/// strength was configurable).
const LIST_FACTOR: f64 = 0.64;

/// Strengths the track tints may have, in percent. 0 turns them off;
/// above half they compete with the waveform itself.
pub const TRACK_TINT_RANGE: RangeInclusive<u8> = 0..=50;
/// Strengths the selection may have, in percent: below 15 it is hard to
/// see at all, above 60 it gets as light as the waveform's blue peaks.
pub const SELECTION_RANGE: RangeInclusive<u8> = 15..=60;
/// How many percentage points the selection stays above the track tints.
pub const SELECTION_LEAD: u8 = 10;

/// The colour strengths, in percent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Appearance {
    /// How much of its hue each track's tint mixes into the background.
    pub track_tint: u8,
    /// How much of its hue the selection mixes into the background.
    pub selection: u8,
}

impl Default for Appearance {
    /// The strengths chosen in issue #126: clearly visible tints, and the
    /// selection as it was.
    fn default() -> Self {
        Appearance {
            track_tint: 25,
            selection: 35,
        }
    }
}

impl Appearance {
    /// These strengths moved into their ranges, the selection at least
    /// [`SELECTION_LEAD`] above the track tints. A value read from the
    /// config store or a slider may be anything.
    pub fn normalized(self) -> Self {
        let track_tint = self
            .track_tint
            .clamp(*TRACK_TINT_RANGE.start(), *TRACK_TINT_RANGE.end());
        let selection = self
            .selection
            .max(track_tint.saturating_add(SELECTION_LEAD))
            .clamp(*SELECTION_RANGE.start(), *SELECTION_RANGE.end());
        Appearance {
            track_tint,
            selection,
        }
    }

    /// The tint of track `index` (0-based) behind the waveform, whose
    /// background is `background`.
    pub fn track_tint(&self, index: usize, background: Rgb) -> Rgb {
        blend(
            background,
            HUES[index % HUES.len()],
            percent(self.normalized().track_tint),
        )
    }

    /// The tint of track `index` (0-based) behind its row of the track
    /// list, whose background is `background`.
    pub fn list_tint(&self, index: usize, background: Rgb) -> Rgb {
        blend(
            background,
            HUES[index % HUES.len()],
            percent(self.normalized().track_tint) * LIST_FACTOR,
        )
    }

    /// The selection's colour on `background`.
    pub fn selection(&self, background: Rgb) -> Rgb {
        blend(
            background,
            SELECTION_HUE,
            percent(self.normalized().selection),
        )
    }
}

/// `p` percent as a fraction.
fn percent(p: u8) -> f64 {
    f64::from(p) / 100.0
}

/// `background` with `strength` (0..1) of `hue` mixed in.
pub fn blend(background: Rgb, hue: Rgb, strength: f64) -> Rgb {
    let mix = |b: u8, h: u8| {
        (f64::from(b) + (f64::from(h) - f64::from(b)) * strength)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    (
        mix(background.0, hue.0),
        mix(background.1, hue.1),
        mix(background.2, hue.2),
    )
}

/// Sum of the channel differences: how far apart two colours look,
/// roughly. For keeping colours apart.
pub fn distance(a: Rgb, b: Rgb) -> u32 {
    [(a.0, b.0), (a.1, b.1), (a.2, b.2)]
        .iter()
        .map(|&(x, y)| u32::from(x.abs_diff(y)))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The waveform's background.
    const DARK: Rgb = (0x1e, 0x22, 0x28);
    /// The track list's background.
    const LIGHT: Rgb = (0xff, 0xff, 0xff);

    #[test]
    fn defaults_keep_the_colours_of_issue_126() {
        let a = Appearance::default();
        assert_eq!(a.selection(DARK), (0x48, 0x56, 0x70));
        assert_eq!(a.track_tint(0, DARK), (0x2b, 0x3e, 0x5a));
        assert_eq!(a, a.normalized());
    }

    #[test]
    fn neighbouring_tracks_get_different_tints() {
        let a = Appearance::default();
        for i in 0..8 {
            assert_ne!(a.track_tint(i, DARK), a.track_tint(i + 1, DARK), "{i}");
            assert_ne!(a.list_tint(i, LIGHT), a.list_tint(i + 1, LIGHT), "{i}");
        }
        assert_eq!(a.list_tint(0, LIGHT), a.list_tint(HUES.len(), LIGHT));
    }

    #[test]
    fn normalizing_clamps_and_keeps_the_selection_ahead() {
        let n = |track_tint, selection| {
            Appearance {
                track_tint,
                selection,
            }
            .normalized()
        };
        assert_eq!(n(200, 0), n(50, 60));
        assert_eq!(n(0, 200), n(0, 60));
        assert_eq!(n(50, 0).selection, 60);
        assert_eq!(n(0, 0).selection, 15);
        assert_eq!(n(30, 35).selection, 40, "pushed ahead of the tints");
        assert_eq!(n(30, 55).selection, 55, "far enough ahead already");
    }

    /// At every setting the selection stays distinguishable from every
    /// track's tint, and a tint other than "off" is visible.
    #[test]
    fn every_setting_keeps_selection_and_tints_apart() {
        for track_tint in 0..=u8::MAX {
            for selection in (0..=100).step_by(5) {
                let a = Appearance {
                    track_tint,
                    selection,
                }
                .normalized();
                let sel = a.selection(DARK);
                assert!(distance(sel, DARK) >= 40, "{a:?}");
                for i in 0..HUES.len() {
                    let tint = a.track_tint(i, DARK);
                    let d = distance(sel, tint);
                    assert!(d >= 35, "{a:?}, track {i}: {d}");
                    if a.track_tint >= 10 {
                        assert!(distance(tint, DARK) >= 25, "{a:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn blending_reaches_both_ends() {
        let hue = (0x10, 0x80, 0xf0);
        assert_eq!(blend(DARK, hue, 0.0), DARK);
        assert_eq!(blend(DARK, hue, 1.0), hue);
    }
}
