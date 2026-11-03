//! Geometry of the waveform drawing: one horizontal lane per channel, and
//! where a column's min/max/RMS land inside it.
//!
//! Pure arithmetic on pixel coordinates, kept apart from the drawing calls
//! so that it is unit-tested without a display.

use lac_peaks::Peak;

/// Vertical gap between two channel lanes, in pixels.
pub const LANE_GAP: i32 = 2;

/// A channel's horizontal strip of the drawing area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lane {
    /// First pixel row.
    pub top: i32,
    /// Height in pixels (at least 1).
    pub height: i32,
}

impl Lane {
    /// The row of a sample value (`-1.0..=1.0`, full scale at the lane's
    /// edges, positive values up). Out-of-range values are clamped.
    pub fn y(&self, value: f32) -> i32 {
        let v = if value.is_nan() {
            0.0
        } else {
            value.clamp(-1.0, 1.0)
        };
        let half = f64::from(self.height - 1) / 2.0;
        self.top + (half * (1.0 - f64::from(v))).round() as i32
    }

    /// The row of the zero line.
    pub fn centre(&self) -> i32 {
        self.y(0.0)
    }
}

/// Splits `height` pixel rows into `channels` equal lanes separated by
/// [`LANE_GAP`]. Rows that do not divide evenly go to the last lane.
pub fn lanes(channels: usize, height: i32) -> Vec<Lane> {
    if channels == 0 {
        return Vec::new();
    }
    let n = i32::try_from(channels).unwrap_or(i32::MAX);
    let usable = (height - LANE_GAP * (n - 1)).max(n);
    let each = usable / n;
    (0..n)
        .map(|i| Lane {
            top: i * (each + LANE_GAP),
            height: if i == n - 1 {
                usable - each * (n - 1)
            } else {
                each
            },
        })
        .collect()
}

/// Rows of one column of one channel: the min..max line and the RMS band
/// drawn over it, both as `(top, bottom)` with `top <= bottom`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnRows {
    /// From the maximum (top) to the minimum (bottom).
    pub peak: (i32, i32),
    /// ±RMS around zero, clipped to the peak line so that it never sticks
    /// out of it.
    pub rms: (i32, i32),
}

/// Where `p` lands in `lane`.
pub fn column_rows(lane: &Lane, p: Peak) -> ColumnRows {
    let max = f32::from(p.max) / 32767.0;
    let min = f32::from(p.min) / 32767.0;
    let peak = (lane.y(max), lane.y(min));
    let r = p.rms_f32();
    let rms = (lane.y(r).max(peak.0), lane.y(-r).min(peak.1));
    ColumnRows { peak, rms }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lanes_fill_the_height() {
        assert!(lanes(0, 100).is_empty());
        assert_eq!(
            lanes(1, 100),
            vec![Lane {
                top: 0,
                height: 100
            }]
        );
        let l = lanes(2, 101);
        assert_eq!(l[0], Lane { top: 0, height: 49 });
        assert_eq!(
            l[1],
            Lane {
                top: 51,
                height: 50
            }
        );
        // Too small: every lane still gets a row.
        assert!(lanes(4, 3).iter().all(|l| l.height >= 1));
    }

    #[test]
    fn full_scale_hits_the_lane_edges() {
        let l = Lane {
            top: 10,
            height: 101,
        };
        assert_eq!(l.y(1.0), 10);
        assert_eq!(l.y(-1.0), 110);
        assert_eq!(l.centre(), 60);
        assert_eq!(l.y(7.0), 10, "clamped");
        assert_eq!(l.y(f32::NAN), 60);
    }

    #[test]
    fn rms_band_stays_inside_the_peak_line() {
        let l = Lane {
            top: 0,
            height: 201,
        };
        let c = column_rows(
            &l,
            Peak {
                min: -16384,
                max: 32767,
                rms: 65535 / 4,
            },
        );
        assert_eq!(c.peak, (0, 150));
        assert_eq!(c.rms, (75, 125));
        // A one-sided signal: the band is clipped to the peak line.
        let c = column_rows(
            &l,
            Peak {
                min: 16384,
                max: 32767,
                rms: 65535 / 2,
            },
        );
        assert_eq!(c.peak, (0, 50));
        assert_eq!(c.rms, (50, 50));
    }
}
