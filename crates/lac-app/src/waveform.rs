//! The view model of the waveform display: which part of the timeline is
//! visible, at which zoom, and what to draw in each pixel column.
//!
//! It knows nothing about the toolkit. The GUI forwards size changes,
//! scroll and zoom gestures here and draws what [`WaveformView::columns`]
//! returns, so all of this is unit-tested without a display.
//!
//! Positions are integer sample frames ([`SamplePos`]). The zoom factor,
//! samples per pixel, is the one floating-point quantity: it only scales the
//! display, and every pixel is mapped back to a whole sample frame.

use std::ops::Range;

use lac_core::SamplePos;
use lac_peaks::{Peak, PeakSummary};

use crate::detail::Detail;

/// Most zoomed-in view: this many pixels per sample frame.
pub const MAX_PIXELS_PER_SAMPLE: f64 = 32.0;

/// Room left of and right of a range [`WaveformView::zoom_to`] shows, as a
/// fraction of the range's length.
pub const ZOOM_TO_MARGIN: f64 = 0.05;

/// How the visible part should be drawn (see `docs/architecture.md`,
/// "Rendering rule").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawMode {
    /// One vertical min..max line per pixel column, from the summary.
    MinMax,
    /// Columns are narrower than the packets: the summary shows whole
    /// packets, decoded samples would show more detail.
    MinMaxCoarse,
    /// Less than one sample frame per pixel: a polyline through the
    /// samples is the right picture (needs decoded samples).
    Polyline,
}

/// Visible range and zoom of a waveform of `total` sample frames.
#[derive(Debug, Clone, PartialEq)]
pub struct WaveformView {
    total: SamplePos,
    width: u32,
    /// First visible sample frame (left edge of pixel 0).
    start: SamplePos,
    /// Samples per pixel.
    spp: f64,
}

impl WaveformView {
    /// A view of `total` sample frames, `width` pixels wide, showing the
    /// whole timeline.
    pub fn new(total: SamplePos, width: u32) -> Self {
        let mut v = WaveformView {
            total,
            width: width.max(1),
            start: 0,
            spp: 1.0,
        };
        v.zoom_to_fit();
        v
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Samples per pixel.
    pub fn samples_per_pixel(&self) -> f64 {
        self.spp
    }

    /// First visible sample frame.
    pub fn start(&self) -> SamplePos {
        self.start
    }

    /// Visible sample frames (clipped to the timeline).
    pub fn visible(&self) -> Range<SamplePos> {
        let end = self.start as f64 + self.spp * f64::from(self.width);
        self.start..(end.ceil() as SamplePos).min(self.total)
    }

    /// Changes the width (window resized), keeping the left edge and the
    /// zoom unless that would show beyond the end.
    pub fn set_width(&mut self, width: u32) {
        self.width = width.max(1);
        self.spp = self.spp.min(self.max_spp());
        self.clamp_start();
    }

    /// Shows the whole timeline.
    pub fn zoom_to_fit(&mut self) {
        self.spp = self.max_spp();
        self.start = 0;
    }

    /// Zooms so that `r` fills the window, with a margin of
    /// [`ZOOM_TO_MARGIN`] of its length on either side so its edges stay
    /// visible. Never zooms in beyond [`MAX_PIXELS_PER_SAMPLE`] (a short
    /// range is then centred) nor out beyond the whole timeline. An empty
    /// `r` changes nothing.
    pub fn zoom_to(&mut self, r: &Range<SamplePos>) {
        let r = r.start.min(self.total)..r.end.min(self.total);
        if r.is_empty() {
            return;
        }
        let len = (r.end - r.start) as f64;
        let shown = len * (1.0 + 2.0 * ZOOM_TO_MARGIN);
        self.spp =
            (shown / f64::from(self.width)).clamp(1.0 / MAX_PIXELS_PER_SAMPLE, self.max_spp());
        // Centre the range in the window, whatever the zoom became.
        let centre = r.start as f64 + len / 2.0;
        let half = self.spp * f64::from(self.width) / 2.0;
        self.start = (centre - half).max(0.0).round() as SamplePos;
        self.clamp_start();
    }

    /// Zooms by `factor` (> 1 zooms in) keeping the sample under pixel `x`
    /// where it is, like zooming at the mouse pointer.
    pub fn zoom_at(&mut self, x: f64, factor: f64) {
        if !(factor.is_finite() && factor > 0.0) {
            return;
        }
        let anchor = self.start as f64 + x * self.spp;
        self.spp = (self.spp / factor).clamp(1.0 / MAX_PIXELS_PER_SAMPLE, self.max_spp());
        self.start = (anchor - x * self.spp).max(0.0).round() as SamplePos;
        self.clamp_start();
    }

    /// Scrolls by `dx` pixels (positive = later in time).
    pub fn scroll_by(&mut self, dx: f64) {
        let s = (self.start as f64 + dx * self.spp).max(0.0);
        self.start = s.round() as SamplePos;
        self.clamp_start();
    }

    /// Scrolls so that the view starts at `pos`, as far as possible.
    pub fn scroll_to(&mut self, pos: SamplePos) {
        self.start = pos;
        self.clamp_start();
    }

    /// The sample frame at pixel `x` (rounded down), clamped to the
    /// timeline. Selections and cut points are set through this.
    pub fn x_to_sample(&self, x: f64) -> SamplePos {
        let s = self.start as f64 + x.max(0.0) * self.spp;
        (s.floor() as SamplePos).min(self.total)
    }

    /// The pixel position of sample frame `pos` (may lie outside the
    /// window).
    pub fn sample_to_x(&self, pos: SamplePos) -> f64 {
        (pos as f64 - self.start as f64) / self.spp
    }

    /// The pixel columns that show any part of `r`, clipped to the window:
    /// at least one column even if `r` is narrower than a pixel, so a
    /// short selection or deletion stays visible. `None` if `r` is empty
    /// or not in view.
    pub fn pixel_span(&self, r: &Range<SamplePos>) -> Option<Range<u32>> {
        if r.start >= r.end {
            return None;
        }
        let width = f64::from(self.width);
        let x0 = self.sample_to_x(r.start).floor().max(0.0);
        let x1 = self.sample_to_x(r.end).ceil().min(width);
        // Off screen to the left (x1 <= 0) or right (x0 >= width).
        if x1 <= 0.0 || x0 >= width {
            return None;
        }
        let (x0, x1) = (x0 as u32, x1 as u32);
        Some(x0..x1.max(x0 + 1))
    }

    /// How to draw at the current zoom, given the length of the packets in
    /// view (use the shortest one for a conservative choice).
    pub fn draw_mode(&self, packet_len: SamplePos) -> DrawMode {
        if self.spp < 1.0 {
            DrawMode::Polyline
        } else if self.spp < packet_len as f64 {
            DrawMode::MinMaxCoarse
        } else {
            DrawMode::MinMax
        }
    }

    /// The min/max/RMS per channel of every pixel column, from `peaks`.
    /// A column is `None` beyond the end of the timeline and while its
    /// packets are not summarised yet.
    pub fn columns(&self, peaks: &PeakSummary) -> Vec<Option<Vec<Peak>>> {
        (0..self.width)
            .map(|x| {
                let a = self.x_to_sample(f64::from(x));
                let b = self
                    .x_to_sample(f64::from(x + 1))
                    .max(a + 1)
                    .min(self.total);
                if a >= self.total {
                    return None;
                }
                peaks.summarize(peaks.packets_overlapping(a..b))
            })
            .collect()
    }

    /// The samples the current view needs decoded, if any: `None` while
    /// the summary is detailed enough ([`DrawMode::MinMax`] for packets of
    /// `packet_len` frames). Otherwise the visible range with one screen
    /// width on either side, so that scrolling a little needs no new
    /// decoding.
    pub fn detail_wanted(&self, packet_len: SamplePos) -> Option<Range<SamplePos>> {
        if self.draw_mode(packet_len) == DrawMode::MinMax {
            return None;
        }
        let v = self.visible();
        let pad = v.end - v.start;
        Some(v.start.saturating_sub(pad)..(v.end + pad).min(self.total))
    }

    /// Like [`WaveformView::columns`], but from decoded samples: exact
    /// min/max/RMS per pixel column. `None` unless `detail` covers the
    /// visible range.
    pub fn detail_columns(&self, detail: &Detail) -> Option<Vec<Option<Vec<Peak>>>> {
        if !detail.covers(&self.visible()) {
            return None;
        }
        Some(
            (0..self.width)
                .map(|x| {
                    let a = self.x_to_sample(f64::from(x));
                    let b = self
                        .x_to_sample(f64::from(x + 1))
                        .max(a + 1)
                        .min(self.total);
                    (0..detail.channels)
                        .map(|ch| Peak::of_samples(detail.channel(ch, a..b)))
                        .collect()
                })
                .collect(),
        )
    }

    /// For [`DrawMode::Polyline`]: per channel, the points `(x, value)` of
    /// every visible sample frame plus one on either side, so that the line
    /// runs through the window edges. `None` unless `detail` covers them.
    pub fn polyline(&self, detail: &Detail) -> Option<Vec<Vec<(f64, f32)>>> {
        let v = self.visible();
        let r = v.start.saturating_sub(1)..(v.end + 1).min(self.total);
        if !detail.covers(&r) {
            return None;
        }
        Some(
            (0..detail.channels)
                .map(|ch| {
                    r.clone()
                        .zip(detail.channel(ch, r.clone()))
                        .map(|(pos, value)| (self.sample_to_x(pos), value))
                        .collect()
                })
                .collect(),
        )
    }

    /// Largest useful zoom-out: the whole timeline fills the width.
    fn max_spp(&self) -> f64 {
        (self.total as f64 / f64::from(self.width)).max(1.0 / MAX_PIXELS_PER_SAMPLE)
    }

    /// Keeps the view inside the timeline where possible.
    fn clamp_start(&mut self) {
        let span = (self.spp * f64::from(self.width)).floor() as SamplePos;
        self.start = self.start.min(self.total.saturating_sub(span));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lac_core::{AudioStreamInfo, PacketFlags, PacketIndex, PacketInfo};
    use lac_peaks::PeakBuilder;

    #[test]
    fn pixel_spans_are_clipped_and_never_vanish() {
        // 10 samples per pixel, 100 pixels.
        let mut v = WaveformView::new(10_000, 100);
        assert_eq!(v.samples_per_pixel(), 100.0);
        v.zoom_at(0.0, 10.0);
        assert_eq!(v.samples_per_pixel(), 10.0);
        assert_eq!(v.pixel_span(&(100..200)), Some(10..20));
        // Partly covered columns count.
        assert_eq!(v.pixel_span(&(105..201)), Some(10..21));
        // Narrower than a pixel: still one column.
        assert_eq!(v.pixel_span(&(503..504)), Some(50..51));
        // Clipped at the right edge, off screen, empty.
        assert_eq!(v.pixel_span(&(900..5000)), Some(90..100));
        assert_eq!(v.pixel_span(&(1000..5000)), None);
        assert_eq!(v.pixel_span(&(300..300)), None);
        // Clipped at the left edge after scrolling.
        v.scroll_to(500);
        assert_eq!(v.pixel_span(&(0..600)), Some(0..10));
        assert_eq!(v.pixel_span(&(0..500)), None);
    }

    #[test]
    fn zoom_to_shows_the_range_with_a_margin() {
        let mut v = WaveformView::new(1_000_000, 1100);
        v.zoom_to(&(200_000..300_000));
        // 100 000 samples plus 5 % on either side in 1100 pixels.
        assert!((v.samples_per_pixel() - 100.0).abs() < 1e-9);
        assert_eq!(v.visible(), 195_000..305_000);
        // The range's edges are inside the window.
        assert!(v.sample_to_x(200_000) > 0.0);
        assert!(v.sample_to_x(300_000) < 1100.0);
    }

    #[test]
    fn zoom_to_stays_inside_the_timeline_and_the_zoom_limits() {
        let mut v = WaveformView::new(1_000_000, 1000);
        // At the start: the margin cannot go before sample 0.
        v.zoom_to(&(0..100_000));
        assert_eq!(v.start(), 0);
        assert!(v.visible().contains(&99_999));
        // At the end: the view ends at the timeline's end.
        v.zoom_to(&(900_000..1_000_000));
        assert_eq!(v.visible().end, 1_000_000);
        assert!(v.visible().contains(&900_000));
        // Longer than the file (with the margin): everything is shown.
        v.zoom_to(&(0..1_000_000));
        assert_eq!(v.visible(), 0..1_000_000);
        // Two samples: zoomed in only as far as allowed, centred.
        v.zoom_to(&(500_000..500_002));
        assert_eq!(v.samples_per_pixel(), 1.0 / MAX_PIXELS_PER_SAMPLE);
        let x = v.sample_to_x(500_001);
        assert!((x - 500.0).abs() <= 32.0, "{x}");
        // Empty or beyond the end: nothing changes.
        let before = v.clone();
        v.zoom_to(&(10..10));
        v.zoom_to(&(2_000_000..3_000_000));
        assert_eq!(v, before);
    }

    #[test]
    fn starts_showing_everything() {
        let v = WaveformView::new(48_000 * 60, 1000);
        assert_eq!(v.visible(), 0..48_000 * 60);
        assert_eq!(v.samples_per_pixel(), 2880.0);
    }

    #[test]
    fn zoom_keeps_the_sample_under_the_pointer() {
        let mut v = WaveformView::new(1_000_000, 1000);
        let before = v.x_to_sample(250.0);
        v.zoom_at(250.0, 8.0);
        assert_eq!(v.samples_per_pixel(), 125.0);
        assert_eq!(v.x_to_sample(250.0), before);
        // Zooming back out restores the full view.
        v.zoom_at(250.0, 1.0 / 8.0);
        assert_eq!(v.visible(), 0..1_000_000);
    }

    #[test]
    fn zoom_is_limited_both_ways() {
        let mut v = WaveformView::new(10_000, 100);
        v.zoom_at(0.0, 1e9);
        assert_eq!(v.samples_per_pixel(), 1.0 / MAX_PIXELS_PER_SAMPLE);
        assert_eq!(v.draw_mode(1152), DrawMode::Polyline);
        v.zoom_at(0.0, 1e-9);
        assert_eq!(v.samples_per_pixel(), 100.0);
        // Invalid factors change nothing.
        let before = v.clone();
        v.zoom_at(10.0, f64::NAN);
        v.zoom_at(10.0, 0.0);
        assert_eq!(v, before);
    }

    #[test]
    fn scrolling_stays_inside_the_timeline() {
        let mut v = WaveformView::new(100_000, 100);
        v.zoom_at(0.0, 10.0); // 100 spp, 10 000 samples visible
        v.scroll_by(-50.0);
        assert_eq!(v.start(), 0);
        v.scroll_by(1e9);
        assert_eq!(v.visible(), 90_000..100_000);
        v.scroll_to(50_000);
        assert_eq!(v.start(), 50_000);
    }

    #[test]
    fn resizing_keeps_zoom_until_it_would_overshoot() {
        let mut v = WaveformView::new(100_000, 100);
        v.zoom_at(0.0, 10.0);
        v.scroll_to(90_000);
        v.set_width(200); // 20 000 samples would be visible
        assert_eq!(v.samples_per_pixel(), 100.0);
        assert_eq!(v.visible(), 80_000..100_000);
        v.set_width(5000); // more than the whole file at this zoom
        assert_eq!(v.visible(), 0..100_000);
    }

    #[test]
    fn pixel_sample_mapping_round_trips() {
        let mut v = WaveformView::new(1_000_000, 800);
        v.zoom_at(400.0, 3.0);
        for x in [0.0, 1.0, 123.0, 799.0] {
            let s = v.x_to_sample(x);
            assert!((v.sample_to_x(s) - x).abs() < 1.0, "{x}");
        }
    }

    #[test]
    fn draw_mode_follows_the_rendering_rule() {
        let mut v = WaveformView::new(10_000_000, 1000); // 10 000 spp
        assert_eq!(v.draw_mode(1152), DrawMode::MinMax);
        v.zoom_at(0.0, 20.0); // 500 spp
        assert_eq!(v.draw_mode(1152), DrawMode::MinMaxCoarse);
        v.zoom_at(0.0, 1000.0); // 0.5 spp
        assert_eq!(v.draw_mode(1152), DrawMode::Polyline);
    }

    /// A mono detail whose sample n has the value n / 1000.
    fn ramp_detail(range: Range<SamplePos>) -> Detail {
        Detail {
            samples: range.clone().map(|n| n as f32 / 1000.0).collect(),
            range,
            channels: 1,
        }
    }

    #[test]
    fn detail_is_wanted_only_below_packet_resolution() {
        let mut v = WaveformView::new(1_000_000, 100); // 10 000 spp
        assert_eq!(v.detail_wanted(1152), None);
        v.scroll_to(0);
        v.zoom_at(0.0, 100.0); // 100 spp, 10 000 visible
        v.scroll_to(500_000);
        assert_eq!(v.detail_wanted(1152), Some(490_000..520_000));
        v.scroll_to(0);
        assert_eq!(
            v.detail_wanted(1152),
            Some(0..20_000),
            "clamped at the start"
        );
    }

    #[test]
    fn detail_columns_are_exact_per_pixel() {
        let mut v = WaveformView::new(1000, 10); // 100 spp
        assert!(
            v.detail_columns(&ramp_detail(0..999)).is_none(),
            "must cover the view"
        );
        let cols = v.detail_columns(&ramp_detail(0..1000)).expect("covered");
        let p = cols[3].as_ref().expect("column")[0];
        let q = |x: f32| (x * 32767.0).round() as i16;
        assert_eq!((p.min, p.max), (q(0.3), q(0.399)));
        v.zoom_at(0.0, 50.0); // 2 spp
        let cols = v.detail_columns(&ramp_detail(0..1000)).expect("covered");
        assert_eq!(cols[1].as_ref().expect("column")[0].min, q(0.002));
    }

    #[test]
    fn polyline_runs_through_every_visible_sample() {
        let mut v = WaveformView::new(1000, 100);
        v.zoom_at(0.0, 1e9); // 1/32 spp: 3 or 4 samples visible
        v.scroll_to(500);
        let line = v.polyline(&ramp_detail(0..1000)).expect("covered");
        let pts = &line[0];
        assert_eq!(pts.first(), Some(&(v.sample_to_x(499), 0.499)));
        assert!(
            pts.windows(2)
                .all(|w| w[1].0 - w[0].0 == MAX_PIXELS_PER_SAMPLE)
        );
        assert!(
            pts.last().expect("points").0 >= 100.0,
            "reaches past the right edge"
        );
        assert!(
            v.polyline(&ramp_detail(500..1000)).is_none(),
            "needs the sample before"
        );
    }

    #[test]
    fn columns_come_from_the_summary() {
        // 10 packets of 100 frames; packet k holds the value k / 10.
        let packets = (0..10u64)
            .map(|k| PacketInfo {
                byte_offset: k,
                byte_len: 1,
                first_sample: k * 100,
                n_samples: 100,
                hidden_before: 0,
                hidden_after: 0,
                flags: PacketFlags::default(),
            })
            .collect();
        let info = AudioStreamInfo {
            sample_rate: 1000,
            channels: 1,
        };
        let idx = PacketIndex::new(info, packets).expect("valid");
        let mut b = PeakBuilder::new(&idx);
        for k in 0..6 {
            b.push(&[k as f32 / 10.0; 100]);
        }
        let v = WaveformView::new(1000, 10);
        let cols = v.columns(b.summary());
        let max: Vec<Option<i16>> = cols.iter().map(|c| c.as_ref().map(|p| p[0].max)).collect();
        let q = |x: f32| (x * 32767.0).round() as i16;
        assert_eq!(
            &max[..6],
            &(0..6).map(|k| Some(q(k as f32 / 10.0))).collect::<Vec<_>>()[..]
        );
        assert_eq!(&max[6..], &[None; 4], "packets 6..10 are not decoded yet");
    }
}
