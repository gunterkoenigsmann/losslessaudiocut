//! The playback timeline: the kept parts of a recording, played one after
//! the other.
//!
//! Editing never changes the source; it only decides which source ranges
//! are kept (`lac_app::editor::Editor::kept`). Playback plays exactly
//! those, so one hears the result of the cuts before exporting. Positions
//! on this timeline are sample frames counted from its start; the GUI maps
//! them back to the source timeline it draws ([`Timeline::to_source`]).

use std::ops::Range;

use lac_core::SamplePos;

/// Source ranges played in sequence.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Timeline {
    /// Non-empty source ranges, in playing order.
    segments: Vec<Range<SamplePos>>,
    /// Timeline position where each segment starts.
    starts: Vec<SamplePos>,
    /// Total length.
    len: SamplePos,
}

impl Timeline {
    /// A timeline playing `segments` in order; empty ones are dropped.
    /// Segments may be in any order and may even overlap, but the editor
    /// produces sorted, disjoint ones, which [`Timeline::from_source`]
    /// relies on.
    pub fn new(segments: impl IntoIterator<Item = Range<SamplePos>>) -> Self {
        let mut t = Timeline::default();
        for s in segments.into_iter().filter(|s| s.start < s.end) {
            t.starts.push(t.len);
            t.len += s.end - s.start;
            t.segments.push(s);
        }
        t
    }

    /// A timeline playing the whole source of `total` frames.
    pub fn whole(total: SamplePos) -> Self {
        Timeline::new(std::iter::once(0..total))
    }

    /// Length in sample frames.
    pub fn len(&self) -> SamplePos {
        self.len
    }

    /// Whether there is nothing to play.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The source ranges, in playing order.
    pub fn segments(&self) -> &[Range<SamplePos>] {
        &self.segments
    }

    /// Index of the segment playing at timeline position `t < len`.
    fn segment_at(&self, t: SamplePos) -> Option<usize> {
        // The last segment starting at or before `t`.
        let i = self.starts.partition_point(|&s| s <= t).checked_sub(1)?;
        (t < self.len).then_some(i)
    }

    /// The source frame played at timeline position `t`. At or after the
    /// end this is the end of the last segment, so a cursor at the end of
    /// the timeline is drawn after the last kept frame.
    pub fn to_source(&self, t: SamplePos) -> SamplePos {
        match self.segment_at(t) {
            Some(i) => self.segments[i].start + (t - self.starts[i]),
            None => self.segments.last().map_or(0, |s| s.end),
        }
    }

    /// The timeline position of source frame `s`. A frame that is not
    /// played (deleted) maps to where playback continues after it, i.e.
    /// the start of the next kept segment.
    pub fn from_source(&self, s: SamplePos) -> SamplePos {
        // First segment that ends after `s`.
        let i = self.segments.partition_point(|seg| seg.end <= s);
        match self.segments.get(i) {
            Some(seg) if s >= seg.start => self.starts[i] + (s - seg.start),
            Some(_) => self.starts[i],
            None => self.len,
        }
    }

    /// The source range to play from timeline position `t`: at most `max`
    /// frames, never crossing the end of a segment (the next segment
    /// continues elsewhere in the source). `None` at the end.
    pub fn chunk(&self, t: SamplePos, max: SamplePos) -> Option<Range<SamplePos>> {
        let i = self.segment_at(t)?;
        let seg = &self.segments[i];
        let start = seg.start + (t - self.starts[i]);
        Some(start..seg.end.min(start.saturating_add(max.max(1))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cut() -> Timeline {
        // Keeps 0..100, 300..350 and 900..1000 of a 1000 frame source.
        Timeline::new([0..100, 300..350, 500..500, 900..1000])
    }

    #[test]
    fn lengths_and_empty_segments() {
        let t = cut();
        assert_eq!(t.len(), 250);
        assert_eq!(t.segments().len(), 3, "the empty segment is dropped");
        assert!(Timeline::new([]).is_empty());
        assert_eq!(Timeline::whole(7).len(), 7);
    }

    #[test]
    fn maps_to_the_source() {
        let t = cut();
        assert_eq!(t.to_source(0), 0);
        assert_eq!(t.to_source(99), 99);
        assert_eq!(t.to_source(100), 300);
        assert_eq!(t.to_source(149), 349);
        assert_eq!(t.to_source(150), 900);
        assert_eq!(t.to_source(249), 999);
        assert_eq!(t.to_source(250), 1000);
        assert_eq!(t.to_source(10_000), 1000);
        assert_eq!(Timeline::new([]).to_source(5), 0);
    }

    #[test]
    fn maps_from_the_source() {
        let t = cut();
        assert_eq!(t.from_source(0), 0);
        assert_eq!(t.from_source(99), 99);
        // Deleted frames map to where playback continues.
        assert_eq!(t.from_source(100), 100);
        assert_eq!(t.from_source(299), 100);
        assert_eq!(t.from_source(300), 100);
        assert_eq!(t.from_source(360), 150);
        assert_eq!(t.from_source(950), 200);
        assert_eq!(t.from_source(1000), 250);
        // Round trip for every kept frame.
        for s in (0..100).chain(300..350).chain(900..1000) {
            assert_eq!(t.to_source(t.from_source(s)), s);
        }
    }

    #[test]
    fn chunks_stop_at_segment_ends() {
        let t = cut();
        assert_eq!(t.chunk(0, 64), Some(0..64));
        assert_eq!(t.chunk(64, 64), Some(64..100));
        assert_eq!(t.chunk(100, 64), Some(300..350));
        assert_eq!(t.chunk(150, 64), Some(900..964));
        assert_eq!(t.chunk(249, 64), Some(999..1000));
        assert_eq!(t.chunk(250, 64), None);
        assert_eq!(t.chunk(0, 0), Some(0..1), "always progresses");
    }
}
