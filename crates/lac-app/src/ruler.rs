//! The time ruler's view model: where the ticks under the waveform go and
//! what their labels say, for any zoom from hours to single samples.
//!
//! Major ticks get a label and come at a "round" interval (1, 2 or 5 of
//! some unit, or the usual clock steps of 15 s, 30 s, 15 min…), the
//! smallest one that leaves room for the labels; minor ticks divide it.
//! Labels show as many decimals as the interval needs, so neighbouring
//! labels always differ.
//!
//! Times are kept in microseconds (an integer) so that tick positions do
//! not drift over hours of audio; only the final pixel position is a
//! floating point number.

use crate::waveform::WaveformView;

/// One tick of the ruler.
#[derive(Debug, Clone, PartialEq)]
pub struct Tick {
    /// Pixel column (may be fractional; the GUI rounds).
    pub x: f64,
    /// The label of a major tick, `None` for a minor one.
    pub label: Option<String>,
}

/// The intervals between major ticks, in microseconds, each with the
/// number of minor intervals it is divided into.
const STEPS: [(u64, u64); 27] = [
    (10, 5),
    (20, 4),
    (50, 5),
    (100, 5),
    (200, 4),
    (500, 5),
    (1_000, 5),
    (2_000, 4),
    (5_000, 5),
    (10_000, 5),
    (20_000, 4),
    (50_000, 5),
    (100_000, 5),
    (200_000, 4),
    (500_000, 5),
    (1_000_000, 5),
    (2_000_000, 4),
    (5_000_000, 5),
    (10_000_000, 5),
    (15_000_000, 3),
    (30_000_000, 3),
    (60_000_000, 4),
    (120_000_000, 4),
    (300_000_000, 5),
    (600_000_000, 5),
    (900_000_000, 3),
    (1_800_000_000, 3),
];
/// One hour; above the table the interval is a whole number of hours.
const HOUR_US: u64 = 3_600_000_000;

/// The interval between major ticks (µs) and its number of minor parts,
/// for a ruler showing `px_per_us` pixels per microsecond with at least
/// `min_px` pixels between labels.
fn step(px_per_us: f64, min_px: f64) -> (u64, u64) {
    if let Some(&s) = STEPS
        .iter()
        .find(|(us, _)| *us as f64 * px_per_us >= min_px)
    {
        return s;
    }
    // Whole hours: 1, 2, 5, 10, 20, 50… of them. Twenty of these reach
    // millions of hours, more than any recording has.
    for i in 0..20u32 {
        let lead = [1, 2, 5][i as usize % 3];
        let us = (lead * 10u64.pow(i / 3)).saturating_mul(HOUR_US);
        if us as f64 * px_per_us >= min_px || i == 19 {
            return (us, if lead == 2 { 4 } else { 5 });
        }
    }
    unreachable!("the loop returns at the latest in its last round")
}

/// Formats `us` microseconds as `[h:]m:ss` with `decimals` (0 to 5)
/// decimals of a second.
fn label(us: u64, decimals: u32) -> String {
    let secs = us / 1_000_000;
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    let clock = if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    };
    if decimals == 0 {
        return clock;
    }
    let frac = us % 1_000_000 / 10u64.pow(6 - decimals);
    format!("{clock}.{frac:0width$}", width = decimals as usize)
}

/// Decimals of a second needed to tell labels `step_us` apart.
fn decimals(step_us: u64) -> u32 {
    (0..6)
        .find(|&d| step_us.is_multiple_of(10u64.pow(6 - d)))
        .unwrap_or(6)
        .min(5)
}

/// The ticks for `view` of a recording at `rate` frames per second, with
/// at least `min_label_px` pixels between labelled ticks. Only ticks
/// inside the window (`0..width`) are returned, left to right.
pub fn ticks(view: &WaveformView, rate: u32, min_label_px: f64) -> Vec<Tick> {
    let rate = f64::from(rate.max(1));
    let spp = view.samples_per_pixel();
    if spp.is_nan() || spp <= 0.0 || view.width() == 0 {
        return Vec::new();
    }
    // Pixels per microsecond: (frames per µs) / (frames per pixel).
    let px_per_us = rate / 1e6 / spp;
    let (major, parts) = step(px_per_us, min_label_px.max(1.0));
    let minor = major / parts;
    let decimals = decimals(major);
    let start_us = view.start() as f64 / rate * 1e6;
    let width = f64::from(view.width());
    let end_us = start_us + width / px_per_us;
    // First minor tick at or before the left edge.
    let mut t = (start_us.max(0.0) as u64) / minor * minor;
    let mut out = Vec::new();
    while (t as f64) <= end_us {
        let x = (t as f64 - start_us) * px_per_us;
        if (0.0..width).contains(&x) {
            out.push(Tick {
                x,
                label: t.is_multiple_of(major).then(|| label(t, decimals)),
            });
        }
        t += minor;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A view of `seconds` of audio at 48 kHz over 1000 pixels, zoomed to
    /// show all of it.
    fn view(seconds: f64) -> WaveformView {
        WaveformView::new((seconds * 48_000.0) as u64, 1000)
    }

    fn labels(ticks: &[Tick]) -> Vec<&str> {
        ticks.iter().filter_map(|t| t.label.as_deref()).collect()
    }

    #[test]
    fn ten_seconds_get_a_label_every_second() {
        let t = ticks(&view(10.0), 48_000, 80.0);
        assert_eq!(
            labels(&t),
            [
                "0:00", "0:01", "0:02", "0:03", "0:04", "0:05", "0:06", "0:07", "0:08", "0:09"
            ]
        );
        // Five parts per second; the first tick is at the left edge.
        assert_eq!(t.len(), 50);
        assert_eq!(t[0].x, 0.0);
        assert!((t[5].x - 100.0).abs() < 1e-9);
    }

    #[test]
    fn labels_have_room() {
        for seconds in [0.001, 0.37, 1.0, 59.0, 600.0, 3600.0 * 3.0, 3600.0 * 500.0] {
            let t = ticks(&view(seconds), 48_000, 80.0);
            let xs: Vec<f64> = t
                .iter()
                .filter(|t| t.label.is_some())
                .map(|t| t.x)
                .collect();
            assert!(!xs.is_empty(), "{seconds} s: no label");
            for w in xs.windows(2) {
                assert!(w[1] - w[0] >= 80.0 - 1e-9, "{seconds} s: {w:?}");
            }
            let l = labels(&t);
            for w in l.windows(2) {
                assert_ne!(w[0], w[1], "{seconds} s: same label twice");
            }
        }
    }

    #[test]
    fn long_recordings_count_hours() {
        let t = ticks(&view(3600.0 * 3.0), 48_000, 80.0);
        assert_eq!(labels(&t)[..3], ["0:00", "15:00", "30:00"]);
        assert!(labels(&t).contains(&"1:00:00"));
    }

    #[test]
    fn deep_zoom_shows_fractions() {
        // 3 ms over 1000 pixels: a label every 500 µs.
        let mut v = WaveformView::new(48_000, 1000);
        v.zoom_at(0.0, 48_000.0 / 144.0);
        let t = ticks(&v, 48_000, 80.0);
        assert_eq!(labels(&t)[..3], ["0:00.0000", "0:00.0005", "0:00.0010"]);
    }

    #[test]
    fn scrolled_views_start_at_the_window() {
        let mut v = view(100.0);
        v.zoom_at(0.0, 10.0); // 10 s over 1000 px
        v.scroll_to(48_000 * 42 + 24_000); // 42.5 s at the left edge
        let t = ticks(&v, 48_000, 80.0);
        assert!(t.iter().all(|t| (0.0..1000.0).contains(&t.x)));
        assert_eq!(labels(&t)[0], "0:43");
        // 42.6 s: the first minor tick (a fifth of a second).
        assert!((t[0].x - 10.0).abs() < 1e-6, "{:?}", t[0]);
    }

    #[test]
    fn decimals_follow_the_step() {
        assert_eq!(decimals(1_000_000), 0);
        assert_eq!(decimals(15_000_000), 0);
        assert_eq!(decimals(500_000), 1);
        assert_eq!(decimals(20_000), 2);
        assert_eq!(decimals(5_000), 3);
        assert_eq!(decimals(10), 5);
        assert_eq!(label(3_723_450_000, 2), "1:02:03.45");
        assert_eq!(label(59_999_999, 0), "0:59");
    }

    #[test]
    fn hour_steps_go_on_one_two_five() {
        let px_per_hour = |px: f64| px / HOUR_US as f64;
        assert_eq!(step(px_per_hour(100.0), 80.0), (HOUR_US, 5));
        assert_eq!(step(px_per_hour(50.0), 80.0), (2 * HOUR_US, 4));
        assert_eq!(step(px_per_hour(20.0), 80.0), (5 * HOUR_US, 5));
        assert_eq!(step(px_per_hour(10.0), 80.0), (10 * HOUR_US, 5));
        assert_eq!(step(px_per_hour(5.0), 80.0), (20 * HOUR_US, 4));
    }
}
