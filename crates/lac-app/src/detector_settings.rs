//! The detector settings the user can change (issue #92), on the
//! Detection page of Edit > Settings.
//!
//! [`DetectParams`] has some twenty knobs, most of which only make sense
//! to someone who knows how the detector works. These are the ones that
//! answer questions a user has: how long a pause must be to be offered
//! for deletion, how long one must be to start a new track, how short a
//! track may get, and whether a change of sound without a pause starts a
//! new track. Times are whole milliseconds so the settings compare
//! exactly and store as integers.

use std::ops::RangeInclusive;

use lac_analysis::{DetectParams, KEY_DISTANCE};

/// Allowed [`DetectorSettings::min_pause_ms`]: a tenth of a second finds
/// breaths between words, ten seconds only the longest silences.
pub const MIN_PAUSE_RANGE: RangeInclusive<u32> = 100..=10_000;
/// Allowed [`DetectorSettings::track_gap_ms`].
pub const TRACK_GAP_RANGE: RangeInclusive<u32> = 500..=30_000;
/// Allowed [`DetectorSettings::min_track_secs`]: up to ten minutes, for
/// recordings of long pieces.
pub const MIN_TRACK_RANGE: RangeInclusive<u32> = 0..=600;

/// What the detector looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetectorSettings {
    /// Pauses shorter than this (ms) are not suggested for deletion.
    pub min_pause_ms: u32,
    /// Pauses at least this long (ms) suggest a new track after them;
    /// never shorter than [`DetectorSettings::min_pause_ms`], as such a
    /// gap would not be a pause at all.
    pub track_gap_ms: u32,
    /// Shortest track (seconds): of two suggested track starts closer
    /// than this only the more confident one is kept.
    pub min_track_secs: u32,
    /// A change of timbre without a pause suggests a new track.
    pub timbre: bool,
    /// A change of key or harmony without a pause suggests a new track.
    pub key: bool,
}

impl Default for DetectorSettings {
    /// The detector's own defaults.
    fn default() -> Self {
        let d = DetectParams::default();
        DetectorSettings {
            min_pause_ms: ms(d.min_pause_secs),
            track_gap_ms: ms(d.min_track_gap_secs),
            min_track_secs: d.min_track_secs.round() as u32,
            timbre: d.min_novelty_db.is_finite(),
            key: d.min_key_distance.is_finite(),
        }
    }
}

impl DetectorSettings {
    /// These settings moved into their ranges, the track gap at least the
    /// shortest pause. Stored or typed values may be anything.
    pub fn normalized(self) -> Self {
        let clamp = |v: u32, r: &RangeInclusive<u32>| v.clamp(*r.start(), *r.end());
        let min_pause_ms = clamp(self.min_pause_ms, &MIN_PAUSE_RANGE);
        DetectorSettings {
            min_pause_ms,
            track_gap_ms: clamp(self.track_gap_ms.max(min_pause_ms), &TRACK_GAP_RANGE),
            min_track_secs: clamp(self.min_track_secs, &MIN_TRACK_RANGE),
            ..self
        }
    }

    /// The detector's parameters: its defaults with these settings.
    pub fn params(&self) -> DetectParams {
        let s = self.normalized();
        let d = DetectParams::default();
        DetectParams {
            min_pause_secs: secs(s.min_pause_ms),
            min_track_gap_secs: secs(s.track_gap_ms),
            min_track_secs: s.min_track_secs as f32,
            min_novelty_db: if s.timbre {
                d.min_novelty_db
            } else {
                f32::INFINITY
            },
            min_key_distance: if s.key { KEY_DISTANCE } else { f32::INFINITY },
            ..d
        }
    }
}

/// `secs` seconds as whole milliseconds.
fn ms(secs: f32) -> u32 {
    (secs * 1000.0).round() as u32
}

/// `ms` milliseconds as seconds.
fn secs(ms: u32) -> f32 {
    ms as f32 / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_detectors_own() {
        let s = DetectorSettings::default();
        assert_eq!(s, s.normalized(), "the defaults are in range");
        assert_eq!(s.params(), DetectParams::default());
    }

    #[test]
    fn settings_reach_the_detector() {
        let p = DetectorSettings {
            min_pause_ms: 1500,
            track_gap_ms: 4000,
            min_track_secs: 90,
            timbre: false,
            key: true,
        }
        .params();
        assert_eq!(p.min_pause_secs, 1.5);
        assert_eq!(p.min_track_gap_secs, 4.0);
        assert_eq!(p.min_track_secs, 90.0);
        assert_eq!(p.min_novelty_db, f32::INFINITY, "timbre off");
        assert_eq!(p.min_key_distance, KEY_DISTANCE, "key on");
        // Everything else stays as it was.
        assert_eq!(p.min_content_secs, DetectParams::default().min_content_secs);
    }

    #[test]
    fn normalizing_clamps_and_keeps_the_track_gap_a_pause() {
        let s = DetectorSettings {
            min_pause_ms: 0,
            track_gap_ms: u32::MAX,
            min_track_secs: u32::MAX,
            ..DetectorSettings::default()
        }
        .normalized();
        assert_eq!(s.min_pause_ms, *MIN_PAUSE_RANGE.start());
        assert_eq!(s.track_gap_ms, *TRACK_GAP_RANGE.end());
        assert_eq!(s.min_track_secs, *MIN_TRACK_RANGE.end());
        let s = DetectorSettings {
            min_pause_ms: 5000,
            track_gap_ms: 1000,
            ..DetectorSettings::default()
        }
        .normalized();
        assert_eq!(s.track_gap_ms, 5000, "a track gap is a pause");
    }
}
