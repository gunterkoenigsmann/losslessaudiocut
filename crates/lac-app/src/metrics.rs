//! Sizes of the waveform window's own marks, scaled to the screen
//! (issue #128).
//!
//! The marks are designed in device-independent pixels (DIPs: 1/96 inch on
//! Windows, a point on macOS). Where wxWidgets draws in logical points
//! (macOS, GTK) the scale is 1 and the toolkit enlarges everything on a
//! Retina/2x screen. On Windows a device context counts physical pixels, so
//! at 200 % a hard-coded size shrinks to half its visual size while the
//! text, which the system scales, grows. The GUI therefore asks its window
//! for the scale (`FromDIP`) and builds [`Metrics`] from it on each paint;
//! the hit tests use the same [`Metrics`], so what is drawn and where the
//! pointer finds it always agree.

use crate::hover::Zone;

/// Height of the suggestion strip and length of the suggested-start tick
/// (the tick is twice as long), in DIPs.
const SUGGESTION_MARK: f64 = 6.0;
/// Length of the ruler's labelled (major) ticks, in DIPs.
const RULER_MAJOR: f64 = 7.0;
/// Length of the ruler's other (minor) ticks, in DIPs.
const RULER_MINOR: f64 = 3.0;
/// Height of the ruler beyond its text, in DIPs.
const RULER_PADDING: f64 = 7.0;
/// Room between two ruler labels beyond the widest usual label, in DIPs.
const RULER_LABEL_GAP: f64 = 16.0;
/// Distance of a ruler label from its tick, in DIPs.
const RULER_LABEL_INSET: f64 = 3.0;
/// Distance of a track's label from its left boundary, in DIPs.
const TRACK_LABEL_INSET: f64 = 4.0;
/// Distance of a track's label from the window's top, in DIPs.
const TRACK_LABEL_TOP: f64 = 2.0;
/// How close a mark must be to the pointer for its tooltip, in DIPs.
const TOOLTIP_REACH: f64 = 3.0;
/// Height of the strip of keyframe pictures of a video file, in DIPs.
const THUMBNAIL_STRIP: f64 = 48.0;
/// Room between two keyframe pictures, in DIPs.
const THUMBNAIL_GAP: f64 = 4.0;
/// How far the pointer may move while the right button is down for it to
/// still be a click rather than a scrub, in DIPs.
const CLICK_SLOP: f64 = 4.0;

/// The waveform window's mark sizes in device pixels for one scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    /// Device pixels per DIP (1 for 100 %, 2 for 200 %).
    scale: f64,
}

impl Default for Metrics {
    /// Unscaled: 100 %, or a toolkit that scales by itself.
    fn default() -> Self {
        Metrics { scale: 1.0 }
    }
}

impl Metrics {
    /// The sizes for `scale` device pixels per DIP. A scale that is not a
    /// positive finite number (a broken report from the system) counts as 1.
    pub fn new(scale: f64) -> Self {
        Metrics {
            scale: if scale.is_finite() && scale > 0.0 {
                scale
            } else {
                1.0
            },
        }
    }

    /// The sizes for a window that turns `dips` DIPs into `pixels` device
    /// pixels (`FromDIP(dips)`); a large `dips` keeps fractional scales
    /// such as 125 % exact.
    pub fn from_dip(dips: i32, pixels: i32) -> Self {
        if dips <= 0 {
            return Metrics::default();
        }
        Metrics::new(f64::from(pixels) / f64::from(dips))
    }

    /// Device pixels per DIP.
    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// `dips` in whole device pixels, rounded, and at least 1: a mark
    /// never vanishes on a small scale.
    fn px(&self, dips: f64) -> i32 {
        ((dips * self.scale).round() as i32).max(1)
    }

    /// Height of the suggested-pause strip at the bottom of the lanes.
    pub fn suggestion_strip(&self) -> i32 {
        self.px(SUGGESTION_MARK)
    }

    /// Length of the suggested-start tick at the top of the window, which
    /// is also the height of the [`Zone::Top`] strip.
    pub fn suggestion_tick(&self) -> i32 {
        self.px(2.0 * SUGGESTION_MARK)
    }

    /// Height of the strip of keyframe pictures above the ruler, shown for
    /// video files.
    pub fn thumbnail_strip(&self) -> i32 {
        self.px(THUMBNAIL_STRIP)
    }

    /// Least room between two keyframe pictures in their strip.
    pub fn thumbnail_gap(&self) -> f64 {
        THUMBNAIL_GAP * self.scale
    }

    /// Length of a labelled ruler tick.
    pub fn ruler_major(&self) -> i32 {
        self.px(RULER_MAJOR)
    }

    /// Length of an unlabelled ruler tick.
    pub fn ruler_minor(&self) -> i32 {
        self.px(RULER_MINOR)
    }

    /// Height of the ruler for labels `text_height` device pixels high.
    pub fn ruler_height(&self, text_height: i32) -> i32 {
        text_height + self.px(RULER_PADDING)
    }

    /// The least distance between two ruler labels, for labels at most
    /// `label_width` device pixels wide.
    pub fn ruler_label_spacing(&self, label_width: i32) -> f64 {
        f64::from(label_width + self.px(RULER_LABEL_GAP))
    }

    /// Where a ruler label starts relative to its tick (`dx`) and the
    /// ruler's top (`dy`).
    pub fn ruler_label_offset(&self) -> (i32, i32) {
        (
            self.px(RULER_LABEL_INSET),
            self.ruler_minor() + self.px(1.0),
        )
    }

    /// Where a track's label starts relative to the track's left boundary
    /// (`dx`) and the window's top (`dy`).
    pub fn track_label_offset(&self) -> (i32, i32) {
        (self.px(TRACK_LABEL_INSET), self.px(TRACK_LABEL_TOP))
    }

    /// How close (in device pixels) a mark must be to the pointer for its
    /// tooltip.
    pub fn tooltip_reach(&self) -> f64 {
        TOOLTIP_REACH * self.scale
    }

    /// How far (in device pixels) the pointer may move during a right
    /// click.
    pub fn click_slop(&self) -> f64 {
        CLICK_SLOP * self.scale
    }

    /// The strip that row `y` of a window `height` rows high is in, with a
    /// ruler `ruler` rows high (0 when hidden) at the bottom and the strip
    /// of keyframe pictures, `thumbnails` rows high (0 for audio files),
    /// above it. The strips are where the marks are drawn: the
    /// suggested-start ticks at the top, the suggested-pause strip just
    /// above the keyframe pictures.
    pub fn zone(&self, y: i32, height: i32, ruler: i32, thumbnails: i32) -> Zone {
        if y >= height - ruler {
            Zone::Ruler
        } else if y >= height - ruler - thumbnails {
            Zone::Thumbnails
        } else if y >= height - ruler - thumbnails - self.suggestion_strip() {
            Zone::Bottom
        } else if y < self.suggestion_tick() {
            Zone::Top
        } else {
            Zone::Lanes
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// At 100 % the marks keep the sizes they had before #128.
    #[test]
    fn unscaled_sizes_are_the_old_ones() {
        let m = Metrics::default();
        assert_eq!(m.suggestion_strip(), 6);
        assert_eq!(m.suggestion_tick(), 12);
        assert_eq!(m.ruler_major(), 7);
        assert_eq!(m.ruler_minor(), 3);
        assert_eq!(m.ruler_height(13), 20);
        assert_eq!(m.ruler_label_spacing(70), 86.0);
        assert_eq!(m.ruler_label_offset(), (3, 4));
        assert_eq!(m.track_label_offset(), (4, 2));
        assert_eq!(m.tooltip_reach(), 3.0);
        assert_eq!(m.click_slop(), 4.0);
        assert_eq!(m.thumbnail_strip(), 48);
        assert_eq!(m.thumbnail_gap(), 4.0);
    }

    /// At 200 % every mark is twice as many device pixels; text sizes come
    /// from the (already scaled) font and are not scaled again.
    #[test]
    fn marks_grow_with_the_scale() {
        let one = Metrics::default();
        let two = Metrics::from_dip(1000, 2000);
        assert_eq!(two.scale(), 2.0);
        assert_eq!(two.suggestion_strip(), 2 * one.suggestion_strip());
        assert_eq!(two.suggestion_tick(), 2 * one.suggestion_tick());
        assert_eq!(two.ruler_major(), 2 * one.ruler_major());
        assert_eq!(two.ruler_minor(), 2 * one.ruler_minor());
        assert_eq!(two.ruler_height(26), 26 + 14);
        assert_eq!(two.ruler_label_spacing(140), 172.0);
        assert_eq!(two.track_label_offset(), (8, 4));
        assert_eq!(two.tooltip_reach(), 6.0);
        assert_eq!(two.click_slop(), 8.0);
        assert_eq!(two.thumbnail_strip(), 96);
        assert_eq!(two.thumbnail_gap(), 8.0);
    }

    /// Fractional scales round to whole pixels, and no mark disappears.
    #[test]
    fn fractional_and_tiny_scales() {
        let m = Metrics::from_dip(1000, 1250);
        assert_eq!(m.suggestion_strip(), 8); // 7.5
        assert_eq!(m.ruler_minor(), 4); // 3.75
        let tiny = Metrics::new(0.01);
        assert_eq!(tiny.ruler_minor(), 1);
        assert_eq!(tiny.suggestion_strip(), 1);
    }

    /// Nonsense from the system falls back to 100 %.
    #[test]
    fn broken_scales_count_as_one() {
        for s in [0.0, -2.0, f64::NAN, f64::INFINITY] {
            assert_eq!(Metrics::new(s), Metrics::default(), "{s}");
        }
        assert_eq!(Metrics::from_dip(0, 5), Metrics::default());
        assert_eq!(Metrics::from_dip(1000, 0), Metrics::default());
    }

    /// The hit test's strips are exactly where the marks are drawn, at any
    /// scale: the pause strip ends at the keyframe pictures (or the ruler),
    /// the tick zone at the tick's end.
    #[test]
    fn zones_match_the_drawn_marks() {
        for m in [Metrics::default(), Metrics::new(1.5), Metrics::new(2.0)] {
            let (height, ruler) = (300, m.ruler_height(13));
            for thumbs in [0, m.thumbnail_strip()] {
                let zone = |y| m.zone(y, height, ruler, thumbs);
                let pictures_top = height - ruler - thumbs;
                let strip_top = pictures_top - m.suggestion_strip();
                assert_eq!(zone(height - 1), Zone::Ruler);
                assert_eq!(zone(height - ruler), Zone::Ruler);
                if thumbs > 0 {
                    assert_eq!(zone(height - ruler - 1), Zone::Thumbnails);
                    assert_eq!(zone(pictures_top), Zone::Thumbnails);
                }
                assert_eq!(zone(pictures_top - 1), Zone::Bottom);
                assert_eq!(zone(strip_top), Zone::Bottom);
                assert_eq!(zone(strip_top - 1), Zone::Lanes);
                assert_eq!(zone(0), Zone::Top);
                assert_eq!(zone(m.suggestion_tick() - 1), Zone::Top);
                assert_eq!(zone(m.suggestion_tick()), Zone::Lanes);
            }
            // Without a ruler the pause strip is at the very bottom.
            assert_eq!(m.zone(height - 1, height, 0, 0), Zone::Bottom);
        }
    }
}
