//! Light or dark windows (issue #104).
//!
//! By default the program follows the system's theme; the user can force
//! light or dark in the settings dialog. wxWidgets switches its own
//! controls; what the program colours itself needs a palette for each:
//! the track list's cells ([`ListPalette`]). The waveform is dark in both.

use crate::appearance::{Appearance, Rgb, distance};
use crate::i18n::n_;

/// The theme the user chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    /// Like the system (the default).
    #[default]
    System,
    /// Always light.
    Light,
    /// Always dark.
    Dark,
}

impl Theme {
    /// Every choice, in the order of the settings dialog.
    pub const ALL: [Theme; 3] = [Theme::System, Theme::Light, Theme::Dark];

    /// The name in the config store.
    pub fn key(self) -> &'static str {
        match self {
            Theme::System => "system",
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }

    /// The theme stored as `key`; anything unknown means the system's.
    pub fn from_key(key: &str) -> Theme {
        Theme::ALL
            .into_iter()
            .find(|t| t.key() == key)
            .unwrap_or_default()
    }

    /// The name in the settings dialog, in English; [`crate::i18n::tr`]
    /// translates it.
    pub fn label(self) -> &'static str {
        match self {
            Theme::System => n_("Like the system"),
            Theme::Light => n_("Light"),
            Theme::Dark => n_("Dark"),
        }
    }

    /// Whether the windows are dark, given whether the system's are.
    pub fn is_dark(self, system_dark: bool) -> bool {
        match self {
            Theme::System => system_dark,
            Theme::Light => false,
            Theme::Dark => true,
        }
    }
}

/// The colours of the track list's cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListPalette {
    /// Background of a row (before the track's tint).
    pub background: Rgb,
    /// Text of a cell.
    pub text: Rgb,
    /// Text of a cell the user changed.
    pub edited: Rgb,
    /// Text of a track that is deleted entirely (it gets no file).
    pub deleted: Rgb,
}

impl Default for ListPalette {
    fn default() -> Self {
        ListPalette::LIGHT
    }
}

impl ListPalette {
    /// On light windows: the colours the list had before #104.
    pub const LIGHT: ListPalette = ListPalette {
        background: (0xff, 0xff, 0xff),
        text: (0x10, 0x10, 0x10),
        edited: (0x10, 0x50, 0xb0),
        deleted: (0x90, 0x90, 0x90),
    };

    /// On dark windows: a dark grey like GTK's and Windows' dark themes,
    /// light text, a light blue for edits.
    pub const DARK: ListPalette = ListPalette {
        background: (0x2b, 0x2d, 0x31),
        text: (0xe8, 0xe8, 0xe8),
        edited: (0x80, 0xb8, 0xff),
        deleted: (0x80, 0x80, 0x80),
    };

    /// The palette for dark (`true`) or light windows; light by default.
    pub fn new(dark: bool) -> ListPalette {
        if dark {
            ListPalette::DARK
        } else {
            ListPalette::LIGHT
        }
    }

    /// The background of row `index` of `rows`: the track's tint (the hue
    /// behind its part of the waveform, issue #98), plain with one track,
    /// which the waveform does not tint either.
    pub fn row(&self, appearance: &Appearance, index: usize, rows: usize) -> Rgb {
        if rows > 1 {
            appearance.list_tint(index, self.background)
        } else {
            self.background
        }
    }
}

/// Whether text in `text` is readable on `background` (by the rough
/// channel distance of [`distance`]).
pub fn readable(text: Rgb, background: Rgb) -> bool {
    distance(text, background) >= 250
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_keys_round_trip() {
        for t in Theme::ALL {
            assert_eq!(Theme::from_key(t.key()), t);
        }
        assert_eq!(Theme::from_key(""), Theme::System);
        assert_eq!(Theme::from_key("purple"), Theme::System);
    }

    #[test]
    fn forced_themes_ignore_the_system() {
        assert!(Theme::System.is_dark(true));
        assert!(!Theme::System.is_dark(false));
        assert!(Theme::Dark.is_dark(false));
        assert!(!Theme::Light.is_dark(true));
    }

    /// At every tint strength each kind of text stays readable on every
    /// track's row, and deleted tracks look different from the others.
    #[test]
    fn list_text_stays_readable_on_every_tint() {
        for p in [ListPalette::LIGHT, ListPalette::DARK] {
            for track_tint in 0..=50 {
                let a = Appearance {
                    track_tint,
                    selection: 60,
                }
                .normalized();
                for i in 0..4 {
                    let bg = p.row(&a, i, 4);
                    assert!(readable(p.text, bg), "{p:?} {a:?} {i}");
                    assert!(readable(p.edited, bg), "{p:?} {a:?} {i}: edited");
                    assert!(distance(p.deleted, bg) >= 100, "{p:?} {a:?} {i}");
                }
            }
            assert!(distance(p.deleted, p.text) >= 150, "{p:?}");
            assert!(distance(p.edited, p.text) >= 150, "{p:?}");
            assert_eq!(p.row(&Appearance::default(), 0, 1), p.background);
        }
    }
}
