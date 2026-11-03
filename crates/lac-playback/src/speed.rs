//! Playback speed (`docs/architecture.md`, section "Playback speed").
//!
//! Speed only affects listening; exports never pass through playback. It
//! is a whole number of percent, so the engine can turn it into exact
//! integer rate ratios and positions never drift.

/// The speeds that [`Speed::faster`] and [`Speed::slower`] step through,
/// in percent.
pub const STEPS: [u32; 9] = [50, 75, 100, 125, 150, 175, 200, 250, 300];

/// How fast playback runs and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Speed {
    /// Percent of normal speed, within the range of [`STEPS`].
    percent: u32,
    /// Keep the pitch (time-stretching, [`crate::stretch`]); otherwise the
    /// pitch follows the speed like a tape played faster ("varispeed").
    pub keep_pitch: bool,
}

impl Default for Speed {
    fn default() -> Self {
        Speed::NORMAL
    }
}

impl Speed {
    /// Normal speed, pitch kept.
    pub const NORMAL: Speed = Speed {
        percent: 100,
        keep_pitch: true,
    };

    /// `percent` of normal speed, clamped to the range of [`STEPS`].
    pub fn new(percent: u32, keep_pitch: bool) -> Self {
        Speed {
            percent: percent.clamp(STEPS[0], STEPS[STEPS.len() - 1]),
            keep_pitch,
        }
    }

    /// Percent of normal speed.
    pub fn percent(self) -> u32 {
        self.percent
    }

    /// Whether this is normal speed, where audio passes through unchanged.
    pub fn is_normal(self) -> bool {
        self.percent == 100
    }

    /// The next faster step (the same at the fastest).
    pub fn faster(self) -> Self {
        let percent = STEPS
            .iter()
            .copied()
            .find(|&s| s > self.percent)
            .unwrap_or(self.percent);
        Speed { percent, ..self }
    }

    /// The next slower step (the same at the slowest).
    pub fn slower(self) -> Self {
        let percent = STEPS
            .iter()
            .rev()
            .copied()
            .find(|&s| s < self.percent)
            .unwrap_or(self.percent);
        Speed { percent, ..self }
    }

    /// For display, e.g. `1.25×`.
    pub fn label(self) -> String {
        let s = format!("{:.2}", f64::from(self.percent) / 100.0);
        // 1.50 -> 1.5, 2.00 -> 2
        let s = s.trim_end_matches('0').trim_end_matches('.');
        format!("{s}×")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_up_and_down_and_stops_at_the_ends() {
        let mut s = Speed::NORMAL;
        let mut seen = vec![s.percent()];
        while s.faster() != s {
            s = s.faster();
            seen.push(s.percent());
        }
        assert_eq!(seen, [100, 125, 150, 175, 200, 250, 300]);
        let mut s = Speed::NORMAL;
        s = s.slower().slower().slower();
        assert_eq!(s.percent(), 50);
        // A speed between steps goes to the neighbouring ones.
        assert_eq!(Speed::new(110, true).faster().percent(), 125);
        assert_eq!(Speed::new(110, true).slower().percent(), 100);
        assert!(!Speed::new(110, false).faster().keep_pitch);
    }

    #[test]
    fn clamps_and_labels() {
        assert_eq!(Speed::new(10, true).percent(), 50);
        assert_eq!(Speed::new(1000, true).percent(), 300);
        assert_eq!(Speed::NORMAL.label(), "1×");
        assert_eq!(Speed::new(150, true).label(), "1.5×");
        assert_eq!(Speed::new(125, true).label(), "1.25×");
        assert_eq!(Speed::new(50, true).label(), "0.5×");
        assert!(Speed::NORMAL.is_normal() && !Speed::new(75, true).is_normal());
    }
}
