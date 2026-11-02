//! The non-destructive edit model.
//!
//! An [`EditList`] describes the result of editing as an ordered list of
//! output [`Track`]s, each of which is an ordered list of kept [`Segment`]s
//! of a source. The source files themselves are never modified; deleting,
//! splitting and undoing only rewrite these (small) lists.
//!
//! Positions passed to editing operations are *timeline* positions, i.e.
//! positions in the track as it currently looks after earlier edits, which
//! is what a user sees and selects.

use std::ops::Range;

use crate::{SamplePos, Tags};

/// Identifies an opened source file within a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(pub u32);

/// A kept region of a source, as a half-open range of source sample frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// The source the samples come from.
    pub source: SourceId,
    /// Sample frames of the source that are kept, `start..end`.
    pub range: Range<SamplePos>,
}

impl Segment {
    /// Length in sample frames.
    pub fn len(&self) -> SamplePos {
        self.range.end - self.range.start
    }

    /// True if the segment contains no samples.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Errors of editing operations.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EditError {
    /// The given range or position lies outside the track.
    #[error("range {start}..{end} is outside the track (length {len})")]
    OutOfRange {
        /// Start of the requested range.
        start: SamplePos,
        /// End of the requested range.
        end: SamplePos,
        /// Current length of the track.
        len: SamplePos,
    },
    /// The track index does not exist.
    #[error("no track with index {0}")]
    NoSuchTrack(usize),
}

/// One output file: kept segments in playback order plus its own tags.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Track {
    /// Kept segments, played one after the other. Never contains empty
    /// segments.
    pub segments: Vec<Segment>,
    /// Metadata written into the exported file.
    pub tags: Tags,
}

impl Track {
    /// Length of the track in sample frames.
    pub fn len(&self) -> SamplePos {
        self.segments.iter().map(Segment::len).sum()
    }

    /// True if nothing is left of the track.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Removes the timeline range `range` from the track; the remaining
    /// parts are joined.
    ///
    /// An empty range is a no-op. A range reaching beyond the end of the
    /// track is an error, so that a UI bug cannot silently delete less than
    /// the user selected.
    pub fn delete(&mut self, range: Range<SamplePos>) -> Result<(), EditError> {
        let len = self.len();
        if range.start > range.end || range.end > len {
            return Err(EditError::OutOfRange {
                start: range.start,
                end: range.end,
                len,
            });
        }
        let mut result = Vec::with_capacity(self.segments.len() + 1);
        // Timeline position of the current segment's first sample.
        let mut t = 0;
        for seg in self.segments.drain(..) {
            let seg_t = t..t + seg.len();
            t = seg_t.end;
            // Part of the segment before the deleted range.
            if seg_t.start < range.start {
                let keep = seg_t.end.min(range.start) - seg_t.start;
                result.push(Segment {
                    source: seg.source,
                    range: seg.range.start..seg.range.start + keep,
                });
            }
            // Part of the segment after the deleted range.
            if seg_t.end > range.end {
                let skip = range.end.saturating_sub(seg_t.start);
                result.push(Segment {
                    source: seg.source,
                    range: seg.range.start + skip..seg.range.end,
                });
            }
        }
        // A segment entirely before or after the range is pushed unchanged
        // by one of the two branches above; a segment containing the range
        // is pushed as two pieces. For an empty range those two pieces are
        // adjacent in the source, so merge them back into one segment.
        result.dedup_by(|b, a| {
            if a.source == b.source && a.range.end == b.range.start {
                a.range.end = b.range.end;
                true
            } else {
                false
            }
        });
        self.segments = result;
        Ok(())
    }

    /// Splits the track at timeline position `pos` and returns the second
    /// half. `self` keeps `0..pos`. The new track gets a copy of the tags.
    pub fn split_off(&mut self, pos: SamplePos) -> Result<Track, EditError> {
        let len = self.len();
        if pos > len {
            return Err(EditError::OutOfRange {
                start: pos,
                end: pos,
                len,
            });
        }
        let mut first = Vec::new();
        let mut second = Vec::new();
        let mut t = 0;
        for seg in self.segments.drain(..) {
            let seg_t = t..t + seg.len();
            t = seg_t.end;
            if seg_t.end <= pos {
                first.push(seg);
            } else if seg_t.start >= pos {
                second.push(seg);
            } else {
                let at = seg.range.start + (pos - seg_t.start);
                first.push(Segment {
                    source: seg.source,
                    range: seg.range.start..at,
                });
                second.push(Segment {
                    source: seg.source,
                    range: at..seg.range.end,
                });
            }
        }
        self.segments = first;
        Ok(Track {
            segments: second,
            tags: self.tags.clone(),
        })
    }
}

/// The whole editing result: the tracks to export.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EditList {
    /// Output tracks in order. Deleting parts of a single recording is the
    /// one-track case.
    pub tracks: Vec<Track>,
}

impl EditList {
    /// An edit list that keeps all of `source` (of length `len`) as a
    /// single track with the given tags.
    pub fn whole(source: SourceId, len: SamplePos, tags: Tags) -> Self {
        let segments = if len == 0 {
            Vec::new()
        } else {
            vec![Segment {
                source,
                range: 0..len,
            }]
        };
        Self {
            tracks: vec![Track { segments, tags }],
        }
    }

    /// Splits track `track` at timeline position `pos` into two tracks and
    /// renumbers all tracks (see [`EditList::renumber_tracks`]).
    pub fn split_track(&mut self, track: usize, pos: SamplePos) -> Result<(), EditError> {
        let t = self
            .tracks
            .get_mut(track)
            .ok_or(EditError::NoSuchTrack(track))?;
        let second = t.split_off(pos)?;
        self.tracks.insert(track + 1, second);
        self.renumber_tracks();
        Ok(())
    }

    /// Sets `track_number` (1-based) and `track_total` of every track.
    pub fn renumber_tracks(&mut self) {
        let total = u32::try_from(self.tracks.len()).unwrap_or(u32::MAX);
        for (i, t) in self.tracks.iter_mut().enumerate() {
            t.tags.track_number = Some(u32::try_from(i + 1).unwrap_or(u32::MAX));
            t.tags.track_total = Some(total);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const SRC: SourceId = SourceId(0);

    /// Expands a track into the list of source positions it plays, which is
    /// the simplest possible reference model for editing operations.
    fn expand(t: &Track) -> Vec<SamplePos> {
        t.segments.iter().flat_map(|s| s.range.clone()).collect()
    }

    fn track(len: SamplePos) -> Track {
        EditList::whole(SRC, len, Tags::default()).tracks.remove(0)
    }

    #[test]
    fn delete_middle_joins_rest() {
        let mut t = track(100);
        t.delete(10..20).expect("in range");
        assert_eq!(
            t.segments,
            vec![
                Segment {
                    source: SRC,
                    range: 0..10
                },
                Segment {
                    source: SRC,
                    range: 20..100
                },
            ]
        );
        assert_eq!(t.len(), 90);
    }

    #[test]
    fn delete_beyond_end_is_error() {
        let mut t = track(100);
        assert!(t.delete(90..101).is_err());
        assert_eq!(t.len(), 100, "a failed delete must not change the track");
    }

    #[test]
    fn delete_everything_leaves_empty_track() {
        let mut t = track(100);
        t.delete(0..100).expect("in range");
        assert!(t.segments.is_empty());
    }

    #[test]
    fn split_renumbers_and_copies_tags() {
        let tags = Tags {
            album: Some("Live".into()),
            ..Tags::default()
        };
        let mut e = EditList::whole(SRC, 100, tags);
        e.split_track(0, 40).expect("in range");
        assert_eq!(e.tracks.len(), 2);
        assert_eq!(e.tracks[0].len(), 40);
        assert_eq!(e.tracks[1].len(), 60);
        assert_eq!(e.tracks[1].tags.album.as_deref(), Some("Live"));
        assert_eq!(e.tracks[1].tags.track_number, Some(2));
        assert_eq!(e.tracks[0].tags.track_total, Some(2));
    }

    proptest! {
        /// Any sequence of deletes matches the reference model of removing
        /// elements from the expanded position list.
        #[test]
        fn delete_matches_reference_model(
            len in 0u64..200,
            ops in proptest::collection::vec((0u64..200, 0u64..50), 0..8),
        ) {
            let mut t = track(len);
            let mut model: Vec<SamplePos> = (0..len).collect();
            for (start, n) in ops {
                let cur = model.len() as u64;
                let start = start.min(cur);
                let end = (start + n).min(cur);
                t.delete(start..end).expect("clamped range");
                model.drain(start as usize..end as usize);
                prop_assert_eq!(expand(&t), model.clone());
                prop_assert!(t.segments.iter().all(|s| !s.is_empty()));
            }
        }

        /// Splitting never loses or duplicates samples.
        #[test]
        fn split_preserves_content(len in 0u64..200, del in 0u64..200, pos in 0u64..200) {
            let mut t = track(len);
            let del = del.min(len);
            t.delete(del / 2..del).expect("in range");
            let before = expand(&t);
            let pos = pos.min(t.len());
            let second = t.split_off(pos).expect("in range");
            let mut joined = expand(&t);
            joined.extend(expand(&second));
            prop_assert_eq!(joined, before);
            prop_assert_eq!(t.len(), pos);
        }
    }
}
