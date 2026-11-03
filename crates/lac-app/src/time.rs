//! Parsing and formatting of user-facing time values.
//!
//! This is the only place where seconds are converted to sample frames.
//! Fractions are parsed as exact decimals (no floating point), and the
//! result is rounded to the nearest sample frame.

use std::ops::Range;
use std::str::FromStr;

use lac_core::SamplePos;

/// Errors from parsing or resolving user-typed times.
///
/// The messages are meant to be shown to the user as they are.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TimeError {
    /// The text is not a valid time or range; the string says why.
    #[error("{0}")]
    Syntax(String),
    /// The range ends before it starts.
    #[error("range starts after it ends")]
    Reversed,
    /// The range extends past the end of the stream.
    #[error("range ends at {end} but the file is only {total} long")]
    PastEnd {
        /// Formatted end of the requested range.
        end: String,
        /// Formatted length of the stream.
        total: String,
    },
}

/// Shorthand for returning a [`TimeError::Syntax`].
macro_rules! syntax {
    ($($arg:tt)*) => {
        return Err(TimeError::Syntax(format!($($arg)*)))
    };
}

/// A point in time as typed by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeSpec {
    /// An exact sample frame (`@N`).
    Samples(SamplePos),
    /// `numerator / denominator` seconds, e.g. `90.25` = 9025 / 100.
    Seconds {
        /// Numerator.
        num: u128,
        /// Denominator, a power of ten.
        den: u128,
    },
}

impl TimeSpec {
    /// Converts to a sample frame at `rate`, rounding to nearest.
    pub fn to_samples(self, rate: u32) -> SamplePos {
        match self {
            TimeSpec::Samples(n) => n,
            TimeSpec::Seconds { num, den } => {
                let s = (num * u128::from(rate) + den / 2) / den;
                SamplePos::try_from(s).unwrap_or(SamplePos::MAX)
            }
        }
    }
}

impl FromStr for TimeSpec {
    type Err = TimeError;

    fn from_str(s: &str) -> Result<Self, TimeError> {
        let s = s.trim();
        if let Some(n) = s.strip_prefix('@') {
            return n
                .parse()
                .map(TimeSpec::Samples)
                .map_err(|_| TimeError::Syntax(format!("invalid sample frame {s:?}")));
        }
        let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
        if !frac.chars().all(|c| c.is_ascii_digit()) || frac.len() > 9 {
            syntax!("invalid fraction in time {s:?}");
        }
        // [[h:]m:]s, each part an unsigned integer.
        let parts: Vec<&str> = whole.split(':').collect();
        if parts.len() > 3 || parts.iter().any(|p| p.is_empty()) {
            syntax!("invalid time {s:?}; use seconds, m:ss, h:mm:ss or @samples");
        }
        let mut secs: u128 = 0;
        for (i, p) in parts.iter().enumerate() {
            let v: u128 = p
                .parse()
                .map_err(|_| TimeError::Syntax(format!("invalid time {s:?}")))?;
            if i > 0 && v >= 60 {
                syntax!("minutes and seconds must be below 60 in {s:?}");
            }
            secs = secs * 60 + v;
        }
        let den = 10u128.pow(frac.len() as u32);
        // At most nine ASCII digits (checked above), so this cannot fail.
        let frac_num: u128 = frac.parse().unwrap_or(0);
        Ok(TimeSpec::Seconds {
            num: secs * den + frac_num,
            den,
        })
    }
}

/// `START..END`, either side optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRange {
    /// Start, or the beginning of the file.
    pub start: Option<TimeSpec>,
    /// End, or the end of the file.
    pub end: Option<TimeSpec>,
}

impl TimeRange {
    /// Resolves to sample frames for a stream of `total` frames at `rate`.
    pub fn resolve(&self, rate: u32, total: SamplePos) -> Result<Range<SamplePos>, TimeError> {
        let start = self.start.map_or(0, |t| t.to_samples(rate));
        let end = self.end.map_or(total, |t| t.to_samples(rate));
        if start > end {
            return Err(TimeError::Reversed);
        }
        if end > total {
            return Err(TimeError::PastEnd {
                end: format_samples(end, rate),
                total: format_samples(total, rate),
            });
        }
        Ok(start..end)
    }
}

impl FromStr for TimeRange {
    type Err = TimeError;

    fn from_str(s: &str) -> Result<Self, TimeError> {
        let Some((a, b)) = s.split_once("..") else {
            syntax!("a range is written START..END, got {s:?}");
        };
        let opt = |x: &str| -> Result<Option<TimeSpec>, TimeError> {
            if x.trim().is_empty() {
                Ok(None)
            } else {
                x.parse().map(Some)
            }
        };
        Ok(TimeRange {
            start: opt(a)?,
            end: opt(b)?,
        })
    }
}

/// Formats a sample position as `h:mm:ss.mmm` (hours omitted when zero).
pub fn format_samples(pos: SamplePos, rate: u32) -> String {
    let rate = u64::from(rate.max(1));
    let ms_total = (u128::from(pos) * 1000 / u128::from(rate)) as u64;
    let (h, rem) = (ms_total / 3_600_000, ms_total % 3_600_000);
    let (m, rem) = (rem / 60_000, rem % 60_000);
    let (s, ms) = (rem / 1000, rem % 1000);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}.{ms:03}")
    } else {
        format!("{m}:{s:02}.{ms:03}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(s: &str, rate: u32) -> SamplePos {
        s.parse::<TimeSpec>().expect("valid").to_samples(rate)
    }

    #[test]
    fn parses_time_formats() {
        assert_eq!(samples("90", 1000), 90_000);
        assert_eq!(samples("90.25", 1000), 90_250);
        assert_eq!(samples("1:30", 1000), 90_000);
        assert_eq!(samples("1:01:30.5", 10), 36_905);
        assert_eq!(samples("@12345", 44_100), 12_345);
    }

    #[test]
    fn rounds_to_nearest_sample() {
        // 0.00001 s at 44.1 kHz = 0.441 samples -> 0; 0.00002 s = 0.882 -> 1.
        assert_eq!(samples("0.00001", 44_100), 0);
        assert_eq!(samples("0.00002", 44_100), 1);
    }

    #[test]
    fn rejects_bad_times() {
        for bad in ["", "1:", ":5", "1:60", "1.2.3", "abc", "1:2:3:4", "-5"] {
            assert!(
                bad.parse::<TimeSpec>().is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn ranges_with_open_ends() {
        let r: TimeRange = "1..".parse().expect("valid");
        assert_eq!(r.resolve(10, 100).expect("in range"), 10..100);
        let r: TimeRange = "..2".parse().expect("valid");
        assert_eq!(r.resolve(10, 100).expect("in range"), 0..20);
        assert!("5".parse::<TimeRange>().is_err());
        assert!(
            "3..2"
                .parse::<TimeRange>()
                .expect("syntax ok")
                .resolve(10, 100)
                .is_err()
        );
        assert!(
            "..11"
                .parse::<TimeRange>()
                .expect("syntax ok")
                .resolve(10, 100)
                .is_err()
        );
    }

    #[test]
    fn formats_positions() {
        assert_eq!(format_samples(0, 44_100), "0:00.000");
        assert_eq!(format_samples(44_100 * 61 + 22_050, 44_100), "1:01.500");
        assert_eq!(format_samples(48_000 * 3_723, 48_000), "1:02:03.000");
    }
}
