//! Options remembered between runs, kept in wxWidgets' per-user config
//! store (the registry on Windows, a file in the home or config directory
//! elsewhere).
//!
//! Only on/off choices from the menus live here; everything else about a
//! session (the open file, the edits) is deliberately not remembered.

use wxdragon::config::{Config, ConfigStyle};

/// Key of [`Options::keep_pitch`].
const KEEP_PITCH: &str = "/Playback/KeepPitch";
/// Key of [`Options::scrub_while_selecting`].
const SCRUB_WHILE_SELECTING: &str = "/Playback/ScrubWhileSelecting";

/// The remembered options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Play > Keep pitch: other speeds keep the pitch.
    pub keep_pitch: bool,
    /// Play > Scrub while selecting: dragging a selection plays the edge
    /// being dragged.
    pub scrub_while_selecting: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            keep_pitch: true,
            scrub_while_selecting: true,
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
        }
    }

    /// Stores the options. Failing to store them only means they are not
    /// remembered, which is not worth bothering the user with.
    pub fn save(&self) {
        let c = config();
        c.write_bool(KEEP_PITCH, self.keep_pitch);
        c.write_bool(SCRUB_WHILE_SELECTING, self.scrub_while_selecting);
        c.flush(false);
    }
}

/// The application's config store.
fn config() -> Config {
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
    fn both_options_are_on_by_default() {
        // Decided with the user: hearing the dragged edge is what the tool
        // is for, and speech should stay intelligible at other speeds.
        assert_eq!(
            Options::default(),
            Options {
                keep_pitch: true,
                scrub_while_selecting: true,
            }
        );
    }
}
