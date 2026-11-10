//! Options remembered between runs, kept in wxWidgets' per-user config
//! store (the registry on Windows, a file in the home or config directory
//! elsewhere).
//!
//! On/off choices from the menus live here, and what the settings dialog
//! (issue #103, `settings.rs`) sets. Which panes
//! are shown, and where, is the window layout's business (see
//! `panes.rs`); everything else about a session (the open file, the edits) is
//! deliberately not remembered.

use lac_app::appearance::Appearance;
use lac_app::detector_settings::DetectorSettings;
use lac_app::theme::Theme;
use wxdragon::config::{Config, ConfigStyle};

/// Key of [`Options::keep_pitch`].
const KEEP_PITCH: &str = "/Playback/KeepPitch";
/// Key of [`Options::scrub_while_selecting`].
const SCRUB_WHILE_SELECTING: &str = "/Playback/ScrubWhileSelecting";
/// Key of [`Options::ruler`].
const RULER: &str = "/View/Ruler";
/// Key of [`Options::chained_links`].
const CHAINED_LINKS: &str = "/Tracks/ChainedLinks";
/// Key of [`Appearance::track_tint`].
const TRACK_TINT: &str = "/View/TrackTint";
/// Key of [`Appearance::selection`].
const SELECTION: &str = "/View/Selection";
/// Key of [`DetectorSettings::min_pause_ms`].
const MIN_PAUSE: &str = "/Detection/MinPauseMs";
/// Key of [`DetectorSettings::track_gap_ms`].
const TRACK_GAP: &str = "/Detection/TrackGapMs";
/// Key of [`DetectorSettings::min_track_secs`].
const MIN_TRACK: &str = "/Detection/MinTrackSecs";
/// Key of [`DetectorSettings::timbre`].
const TIMBRE: &str = "/Detection/Timbre";
/// Key of [`DetectorSettings::key`].
const KEY: &str = "/Detection/Key";
/// Key of [`Options::language`]; empty for the system's language.
const LANGUAGE: &str = "/Interface/Language";
/// Key of [`Options::theme`] ([`Theme::key`]).
const THEME: &str = "/Interface/Theme";

/// The remembered options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Play > Keep pitch: other speeds keep the pitch.
    pub keep_pitch: bool,
    /// Play > Scrub while selecting: dragging a selection plays the edge
    /// being dragged.
    pub scrub_while_selecting: bool,
    /// View > Time ruler: the time scale under the waveform.
    pub ruler: bool,
    /// Settings > Detection > Suggest chained streams as tracks: the links
    /// of a chained Ogg file are offered as track starts. Off for recorded
    /// web radio, whose stream may change its title every few seconds.
    pub chained_links: bool,
    /// Settings > Waveform: how strongly tracks and the selection are
    /// coloured (issue #132).
    pub appearance: Appearance,
    /// Settings > Detection: what counts as a pause and a track start
    /// (issue #92).
    pub detector: DetectorSettings,
    /// Settings > Language: a code of `lac_app::i18n::LANGUAGES`, `None`
    /// for the system's language. Takes effect at the next start.
    pub language: Option<&'static str>,
    /// Settings > Interface: light or dark windows, or like the system
    /// (issue #104). Takes effect at the next start.
    pub theme: Theme,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            keep_pitch: true,
            scrub_while_selecting: true,
            ruler: true,
            chained_links: true,
            appearance: Appearance::default(),
            detector: DetectorSettings::default(),
            language: None,
            theme: Theme::System,
        }
    }
}

impl Options {
    /// The stored options; defaults for whatever is not stored.
    pub fn load() -> Self {
        let c = config();
        let d = Options::default();
        Options {
            keep_pitch: c.read_bool(KEEP_PITCH, d.keep_pitch),
            scrub_while_selecting: c.read_bool(SCRUB_WHILE_SELECTING, d.scrub_while_selecting),
            ruler: c.read_bool(RULER, d.ruler),
            chained_links: c.read_bool(CHAINED_LINKS, d.chained_links),
            appearance: Appearance {
                track_tint: percent(c.read_long(TRACK_TINT, d.appearance.track_tint.into())),
                selection: percent(c.read_long(SELECTION, d.appearance.selection.into())),
            }
            .normalized(),
            detector: DetectorSettings {
                min_pause_ms: count(c.read_long(MIN_PAUSE, d.detector.min_pause_ms.into())),
                track_gap_ms: count(c.read_long(TRACK_GAP, d.detector.track_gap_ms.into())),
                min_track_secs: count(c.read_long(MIN_TRACK, d.detector.min_track_secs.into())),
                timbre: c.read_bool(TIMBRE, d.detector.timbre),
                key: c.read_bool(KEY, d.detector.key),
            }
            .normalized(),
            language: lac_app::i18n::language_code(&c.read_string(LANGUAGE, "")),
            theme: Theme::from_key(&c.read_string(THEME, d.theme.key())),
        }
    }

    /// Stores the options. Failing to store them only means they are not
    /// remembered, which is not worth bothering the user with.
    pub fn save(&self) {
        let c = config();
        c.write_bool(KEEP_PITCH, self.keep_pitch);
        c.write_bool(SCRUB_WHILE_SELECTING, self.scrub_while_selecting);
        c.write_bool(RULER, self.ruler);
        c.write_bool(CHAINED_LINKS, self.chained_links);
        c.write_long(TRACK_TINT, self.appearance.track_tint.into());
        c.write_long(SELECTION, self.appearance.selection.into());
        c.write_long(MIN_PAUSE, self.detector.min_pause_ms.into());
        c.write_long(TRACK_GAP, self.detector.track_gap_ms.into());
        c.write_long(MIN_TRACK, self.detector.min_track_secs.into());
        c.write_bool(TIMBRE, self.detector.timbre);
        c.write_bool(KEY, self.detector.key);
        c.write_string(LANGUAGE, self.language.unwrap_or(""));
        c.write_string(THEME, self.theme.key());
        c.flush(false);
    }
}

/// A stored percentage; anything out of range (a hand-edited store) is
/// pulled into range, [`Appearance::normalized`] does the rest.
fn percent(stored: i64) -> u8 {
    u8::try_from(stored.clamp(0, 100)).unwrap_or(0)
}

/// A stored count (milliseconds, seconds); a negative or huge value from
/// a hand-edited store is pulled into `u32`, [`DetectorSettings::normalized`]
/// does the rest.
fn count(stored: i64) -> u32 {
    u32::try_from(stored.clamp(0, i64::from(u32::MAX))).unwrap_or(0)
}

/// The application's config store.
pub(crate) fn config() -> Config {
    Config::new(
        "losslessaudiocut",
        None,
        None,
        None,
        ConfigStyle::USE_LOCAL_FILE,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_options_are_on_by_default() {
        // Decided with the user: hearing the dragged edge is what the tool
        // is for, and speech should stay intelligible at other speeds.
        assert_eq!(
            Options::default(),
            Options {
                keep_pitch: true,
                scrub_while_selecting: true,
                ruler: true,
                chained_links: true,
                appearance: Appearance::default(),
                detector: DetectorSettings::default(),
                language: None,
                theme: Theme::System,
            }
        );
    }

    #[test]
    fn stored_percentages_are_pulled_into_range() {
        assert_eq!(percent(-5), 0);
        assert_eq!(percent(40), 40);
        assert_eq!(percent(1_000_000), 100);
        assert_eq!(count(-1), 0);
        assert_eq!(count(1500), 1500);
        assert_eq!(count(i64::MAX), u32::MAX);
    }
}
