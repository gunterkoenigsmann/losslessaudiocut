//! The editing state of one opened recording: what is selected, what is
//! deleted, where it is split into tracks, and the undo history. The view
//! model behind the GUI's Edit menu and mouse selection, testable without a
//! display.
//!
//! Edits are the "delete parts, join the rest, split into tracks" model of
//! `docs/architecture.md`: the source is never changed, the editor only
//! keeps [`Edits`] in source sample frames. The waveform keeps showing the
//! source timeline with the deleted parts and track boundaries marked, so
//! that a deletion can be seen, selected again and restored. Export writes
//! one file per [`Track`] that keeps anything.
//!
//! Split points live on the source timeline, not on the kept audio, so
//! deleting or restoring around them never moves them. A track is the
//! source span between two split points, minus what is deleted in it.
//!
//! Tag changes ([`TagEdits`]) are kept on two levels: changes for all
//! tracks (album, artist, ...) and changes per track, keyed by where the
//! track starts. A track's changes are both, the per-track ones winning;
//! the export applies them to the source's tags. A track's title is its
//! name: it names the file too.
//!
//! Undo and redo keep whole snapshots of the [`Edits`]; they are small (one
//! entry per deletion or split), so this is simpler and safer than inverse
//! operations.

use std::collections::BTreeMap;
use std::ops::{Range, RangeInclusive};

use lac_core::{SamplePos, TagField, TagValueError, Tags};

use crate::chapters::SuggestedStart;
use crate::i18n::{n_, tr, trf};
use crate::{CutRequest, KeptRange, TagEdits, format_samples, ranges};

/// Everything the user changed, as one undoable value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Edits {
    /// Deleted ranges, normalised (sorted, disjoint, non-adjacent).
    pub deleted: Vec<Range<SamplePos>>,
    /// Where new tracks start, sorted, strictly inside the recording.
    pub splits: Vec<SamplePos>,
    /// Tag changes that apply to every track.
    pub all_tags: TagEdits,
    /// Tag changes per track, keyed by where the track starts (0 for the
    /// first); they win over [`Edits::all_tags`].
    pub track_tags: BTreeMap<SamplePos, TagEdits>,
}

/// One track: a stretch of the source between split points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    /// 1-based position among all tracks.
    pub number: usize,
    /// The source span from its split point to the next one.
    pub span: Range<SamplePos>,
    /// What the track's file will contain: the span minus deletions.
    pub kept: Vec<Range<SamplePos>>,
    /// The name the user gave it: its title, if not empty.
    pub name: Option<String>,
    /// Its tag changes: those for all tracks, overridden by its own.
    pub tags: TagEdits,
}

impl Track {
    /// Sample frames the track's file will contain.
    pub fn kept_len(&self) -> SamplePos {
        self.kept.iter().map(|r| r.end - r.start).sum()
    }
}

/// Editing state of one recording of `total` sample frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    total: SamplePos,
    edits: Edits,
    selection: Option<Range<SamplePos>>,
    /// Where a click without dragging put the cursor; playback starts
    /// here when nothing is selected.
    cursor: SamplePos,
    /// Where the current mouse drag started.
    anchor: Option<SamplePos>,
    undo: Vec<Edits>,
    redo: Vec<Edits>,
    /// The edits at the last export, to tell whether they are saved
    /// anywhere.
    exported: Option<Edits>,
    /// A track start being dragged with the mouse: the track's index and
    /// the edits before the drag, which become one undo step at its end.
    moving: Option<(usize, Edits)>,
}

impl Editor {
    /// A recording of `total` sample frames with nothing deleted.
    pub fn new(total: SamplePos) -> Self {
        Editor {
            total,
            edits: Edits::default(),
            selection: None,
            cursor: 0,
            anchor: None,
            undo: Vec::new(),
            redo: Vec::new(),
            exported: None,
            moving: None,
        }
    }

    /// A recording of `total` sample frames with `edits` from a saved
    /// project. Whatever does not fit the recording (it may have been
    /// shortened since) is dropped: deletions and splits outside it, tag
    /// changes of tracks that no longer start anywhere. The result counts
    /// as saved ([`Editor::has_unexported_changes`] is false) and has no
    /// undo history.
    pub fn with_edits(total: SamplePos, edits: Edits) -> Self {
        let mut deleted: Vec<Range<SamplePos>> = edits
            .deleted
            .into_iter()
            .map(|r| r.start.min(total)..r.end.min(total))
            .filter(|r| r.start < r.end)
            .collect();
        ranges::normalize(&mut deleted);
        let mut splits: Vec<SamplePos> = edits
            .splits
            .into_iter()
            .filter(|&p| p > 0 && p < total)
            .collect();
        splits.sort_unstable();
        splits.dedup();
        let track_tags = edits
            .track_tags
            .into_iter()
            .filter(|(start, t)| {
                (*start == 0 || splits.binary_search(start).is_ok()) && !t.is_empty()
            })
            .collect();
        let edits = Edits {
            deleted,
            splits,
            all_tags: edits.all_tags,
            track_tags,
        };
        let mut editor = Editor::new(total);
        editor.exported = Some(edits.clone());
        editor.edits = edits;
        editor
    }

    /// Length of the recording.
    pub fn total(&self) -> SamplePos {
        self.total
    }

    /// The selected range, never empty.
    pub fn selection(&self) -> Option<Range<SamplePos>> {
        self.selection.clone()
    }

    /// Selects `a..b` or `b..a`, clamped to the recording; an empty range
    /// clears the selection.
    pub fn select(&mut self, a: SamplePos, b: SamplePos) {
        let (a, b) = (a.min(b).min(self.total), a.max(b).min(self.total));
        self.selection = (a < b).then_some(a..b);
    }

    /// Selects the whole recording.
    pub fn select_all(&mut self) {
        self.select(0, self.total);
    }

    /// Clears the selection.
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Mouse button pressed at `pos`: starts a new selection there and
    /// puts the cursor there.
    pub fn begin_drag(&mut self, pos: SamplePos) {
        let pos = pos.min(self.total);
        self.anchor = Some(pos);
        self.cursor = pos;
        self.selection = None;
    }

    /// The cursor position (source frames).
    pub fn cursor(&self) -> SamplePos {
        self.cursor
    }

    /// Moves the cursor, clamped to the recording.
    pub fn set_cursor(&mut self, pos: SamplePos) {
        self.cursor = pos.min(self.total);
    }

    /// Where playback starts: the start of the selection, else the
    /// cursor.
    pub fn play_from(&self) -> SamplePos {
        self.selection.as_ref().map_or(self.cursor, |s| s.start)
    }

    /// Shift-click at `pos`: moves the nearer end of the selection there and
    /// keeps dragging it. Without a selection this is a plain
    /// [`Editor::begin_drag`].
    pub fn begin_extend(&mut self, pos: SamplePos) {
        let Some(sel) = self.selection.clone() else {
            return self.begin_drag(pos);
        };
        let pos = pos.min(self.total);
        // The far end stays put.
        let anchor = if pos.abs_diff(sel.start) < pos.abs_diff(sel.end) {
            sel.end
        } else {
            sel.start
        };
        self.anchor = Some(anchor);
        self.select(anchor, pos);
    }

    /// Mouse moved to `pos` with the button held: the selection spans from
    /// the press position to here. Ignored without a drag.
    pub fn drag_to(&mut self, pos: SamplePos) {
        if let Some(a) = self.anchor {
            self.select(a, pos);
        }
    }

    /// Mouse button released. A click without movement leaves no selection.
    pub fn end_drag(&mut self) {
        self.anchor = None;
    }

    /// Whether a drag is in progress.
    pub fn dragging(&self) -> bool {
        self.anchor.is_some()
    }

    /// The deleted ranges, sorted and disjoint.
    pub fn deleted(&self) -> &[Range<SamplePos>] {
        &self.edits.deleted
    }

    /// All edits, e.g. to remember what an export contained.
    pub fn edits(&self) -> &Edits {
        &self.edits
    }

    /// Where tracks start, besides 0.
    pub fn splits(&self) -> &[SamplePos] {
        &self.edits.splits
    }

    /// The tracks, in order; one if there are no splits. Tracks whose span
    /// is deleted entirely are included (with nothing kept), so numbers
    /// stay stable while editing.
    pub fn tracks(&self) -> Vec<Track> {
        let kept = self.kept();
        let starts = std::iter::once(0).chain(self.edits.splits.iter().copied());
        let ends = self.edits.splits.iter().copied().chain([self.total]);
        starts
            .zip(ends)
            .enumerate()
            .map(|(i, (a, b))| Track {
                number: i + 1,
                span: a..b,
                kept: kept
                    .iter()
                    .map(|k| k.start.max(a)..k.end.min(b))
                    .filter(|k| k.start < k.end)
                    .collect(),
                tags: self.track_edits(a),
                name: None,
            })
            .map(|mut t| {
                t.name = t
                    .tags
                    .get(TagField::Title)
                    .filter(|n| !n.is_empty())
                    .map(String::from);
                t
            })
            .collect()
    }

    /// The tag changes of the track starting at `start`.
    fn track_edits(&self, start: SamplePos) -> TagEdits {
        let mut t = self.edits.all_tags.clone();
        if let Some(own) = self.edits.track_tags.get(&start) {
            for (f, v) in own.iter() {
                // Stored values are valid, so this cannot fail.
                let _ = t.set(f, v);
            }
        }
        t
    }

    /// Index (0-based) of the track containing source frame `pos`.
    pub fn track_at(&self, pos: SamplePos) -> usize {
        self.edits.splits.partition_point(|&s| s <= pos)
    }

    /// Starts a new track at `pos`. Returns false if `pos` is the start or
    /// end of the recording or already a split point.
    pub fn split_at(&mut self, pos: SamplePos) -> bool {
        if pos == 0 || pos >= self.total {
            return false;
        }
        let i = self.edits.splits.partition_point(|&s| s < pos);
        if self.edits.splits.get(i) == Some(&pos) {
            return false;
        }
        let mut next = self.edits.clone();
        next.splits.insert(i, pos);
        self.commit(next)
    }

    /// Starts new tracks at all of `positions` in one undoable step, e.g.
    /// the track starts suggested by `lac_analysis::detect`. Positions at
    /// the start or end of the recording, beyond it, or already split at
    /// are skipped. Returns how many splits were added.
    pub fn split_at_all(&mut self, positions: impl IntoIterator<Item = SamplePos>) -> usize {
        let mut next = self.edits.clone();
        let before = next.splits.len();
        next.splits
            .extend(positions.into_iter().filter(|&p| p > 0 && p < self.total));
        next.splits.sort_unstable();
        next.splits.dedup();
        let added = next.splits.len() - before;
        self.commit(next);
        added
    }

    /// Splits at each of `starts` in one undoable step, like
    /// [`Editor::split_at_all`], and gives each track that starts at a
    /// suggestion with tags (a chained Ogg link's, see
    /// [`crate::chapters`]) those tags: every field where they differ from
    /// `source` (the file's tags, which every track starts with), unless
    /// the user already set that field for the track. A field the link
    /// lacks is cleared, so a song without an artist does not inherit the
    /// first song's. Returns the number of new track starts.
    pub fn split_at_suggestions(&mut self, starts: &[SuggestedStart], source: &Tags) -> usize {
        let mut next = self.edits.clone();
        let before = next.splits.len();
        next.splits.extend(
            starts
                .iter()
                .map(|s| s.pos)
                .filter(|&p| p > 0 && p < self.total),
        );
        next.splits.sort_unstable();
        next.splits.dedup();
        let added = next.splits.len() - before;
        for s in starts.iter().filter(|s| s.pos > 0 && s.pos < self.total) {
            let Some(tags) = &s.tags else { continue };
            let edits = next.track_tags.entry(s.pos).or_default();
            for field in TagField::ALL {
                let want = tags.get(field);
                if edits.get(field).is_none() && want != source.get(field) {
                    // Values read from a tag are valid; a numeric one that
                    // is not (cannot happen) is simply not taken over.
                    let _ = edits.set(field, want.as_deref().unwrap_or(""));
                }
            }
            if edits.is_empty() {
                next.track_tags.remove(&s.pos);
            }
        }
        self.commit(next);
        added
    }

    /// Moves the start of track `index` (0-based, so split point
    /// `index - 1`) to `to`, keeping the track's own tags with it. The
    /// order of the tracks cannot change: `to` must lie strictly after the
    /// previous track's start and strictly before the next one's (or the
    /// end of the recording). Returns false for the first track, an index
    /// out of range, a position outside those bounds, or no change.
    pub fn move_track_start(&mut self, index: usize, to: SamplePos) -> bool {
        let Some(range) = self.track_start_range(index) else {
            return false;
        };
        if !range.contains(&to) {
            return false;
        }
        let mut next = self.edits.clone();
        let from = std::mem::replace(&mut next.splits[index - 1], to);
        if let Some(tags) = next.track_tags.remove(&from) {
            next.track_tags.insert(to, tags);
        }
        self.commit(next)
    }

    /// Where the start of track `index` may be moved to with
    /// [`Editor::move_track_start`]: between the previous track's start
    /// and the next track's start (both excluded). `None` for the first
    /// track, which always starts at 0, and for indices out of range.
    pub fn track_start_range(&self, index: usize) -> Option<Range<SamplePos>> {
        let splits = &self.edits.splits;
        if index == 0 || index > splits.len() {
            return None;
        }
        let prev = if index >= 2 { splits[index - 2] } else { 0 };
        let next = splits.get(index).copied().unwrap_or(self.total);
        Some(prev + 1..next)
    }

    /// Makes track `index` (0-based) span exactly `span`, moving its start
    /// and its end (the next track's start) in one undoable step; each
    /// track keeps its own tags. The first track's start is fixed at 0 and
    /// the last track's end at the end of the recording, so for those
    /// `span` must keep that bound. Returns false if `span` is empty,
    /// would change the order of the tracks or make one empty, or changes
    /// nothing.
    pub fn set_track_span(&mut self, index: usize, span: Range<SamplePos>) -> bool {
        match self.with_track_span(index, span) {
            Some(next) => self.commit(next),
            None => false,
        }
    }

    /// Whether [`Editor::set_track_span`] would change something.
    pub fn can_set_track_span(&self, index: usize, span: Range<SamplePos>) -> bool {
        self.with_track_span(index, span)
            .is_some_and(|next| next != self.edits)
    }

    /// The edits with track `index` spanning `span`, or `None` if that is
    /// not possible (see [`Editor::set_track_span`]).
    fn with_track_span(&self, index: usize, span: Range<SamplePos>) -> Option<Edits> {
        let n = self.edits.splits.len();
        if index > n || span.start >= span.end {
            return None;
        }
        let mut next = self.edits.clone();
        // Split point `index - 1` is the track's start, split point
        // `index` its end; (old, new) for each one that moves.
        let mut moved = Vec::new();
        if index == 0 {
            if span.start != 0 {
                return None;
            }
        } else {
            moved.push((
                std::mem::replace(&mut next.splits[index - 1], span.start),
                span.start,
            ));
        }
        if index == n {
            if span.end != self.total {
                return None;
            }
        } else {
            moved.push((
                std::mem::replace(&mut next.splits[index], span.end),
                span.end,
            ));
        }
        // Strictly increasing inside the recording: no track reordered or
        // empty.
        let inside = next.splits.first().is_none_or(|&s| s > 0)
            && next.splits.last().is_none_or(|&s| s < self.total)
            && next.splits.windows(2).all(|w| w[0] < w[1]);
        if !inside {
            return None;
        }
        // Take all moved tags out first: a new key may be another one's
        // old key.
        let tags: Vec<_> = moved
            .iter()
            .map(|&(from, to)| (to, next.track_tags.remove(&from)))
            .collect();
        for (to, t) in tags {
            if let Some(t) = t {
                next.track_tags.insert(to, t);
            }
        }
        Some(next)
    }

    /// The track whose start lies within `near` frames of `pos` (the
    /// nearest one), as an index for [`Editor::begin_moving_track_start`].
    /// The first track's start is fixed at 0, so it is never returned.
    pub fn track_start_near(&self, pos: SamplePos, near: SamplePos) -> Option<usize> {
        self.edits
            .splits
            .iter()
            .enumerate()
            .filter(|(_, s)| s.abs_diff(pos) <= near)
            .min_by_key(|(_, s)| s.abs_diff(pos))
            .map(|(i, _)| i + 1)
    }

    /// Starts dragging the start of track `index` (the end of the one
    /// before). While dragging, [`Editor::move_track_start_to`] moves it
    /// at once (tracks, tags and the track list follow), and
    /// [`Editor::end_moving_track_start`] makes the whole drag one undo
    /// step. Returns false for the first track or an index out of range.
    pub fn begin_moving_track_start(&mut self, index: usize) -> bool {
        if self.track_start_range(index).is_none() {
            return false;
        }
        self.moving = Some((index, self.edits.clone()));
        true
    }

    /// The track whose start is being dragged.
    pub fn moving_track_start(&self) -> Option<usize> {
        self.moving.as_ref().map(|(i, _)| *i)
    }

    /// Moves the start being dragged to `pos`, clamped between its
    /// neighbours so the tracks keep their order and none becomes empty.
    /// Not recorded for undo by itself. Ignored without a drag.
    pub fn move_track_start_to(&mut self, pos: SamplePos) {
        let Some(index) = self.moving_track_start() else {
            return;
        };
        let Some(range) = self.track_start_range(index) else {
            return;
        };
        let to = pos.clamp(range.start, range.end - 1);
        let from = std::mem::replace(&mut self.edits.splits[index - 1], to);
        if let Some(tags) = self.edits.track_tags.remove(&from) {
            self.edits.track_tags.insert(to, tags);
        }
    }

    /// Ends dragging a track start. Returns whether it moved; if so, the
    /// drag is one undo step.
    pub fn end_moving_track_start(&mut self) -> bool {
        let Some((_, before)) = self.moving.take() else {
            return false;
        };
        if before == self.edits {
            return false;
        }
        self.undo.push(before);
        self.redo.clear();
        true
    }

    /// Joins track `index` (0-based) with the one before it: removes the
    /// split point it starts at, and its name. Returns false for the first
    /// track or an index out of range.
    pub fn join_with_previous(&mut self, index: usize) -> bool {
        if index == 0 || index > self.edits.splits.len() {
            return false;
        }
        let mut next = self.edits.clone();
        let at = next.splits.remove(index - 1);
        next.track_tags.remove(&at);
        self.commit(next)
    }

    /// Merges tracks `tracks` (0-based, inclusive) into one: removes the
    /// split points between them, and the names and tags of all but the
    /// first, whose name and tags the merged track keeps (issue #96).
    /// One undo step. Returns false unless the range holds at least two
    /// existing tracks.
    pub fn merge_tracks(&mut self, tracks: RangeInclusive<usize>) -> bool {
        let (first, last) = (*tracks.start(), *tracks.end());
        // Track i + 1 starts at splits[i]: the splits inside the merged
        // track are those of tracks first + 1 ..= last.
        if first >= last || last > self.edits.splits.len() {
            return false;
        }
        let mut next = self.edits.clone();
        for at in next.splits.drain(first..last) {
            next.track_tags.remove(&at);
        }
        self.commit(next)
    }

    /// Names track `index` (0-based), i.e. sets its title; an empty or
    /// blank name removes the name, so the title is the source's again.
    /// Returns false if nothing changed.
    pub fn rename_track(&mut self, index: usize, name: &str) -> bool {
        if name.trim().is_empty() {
            return self.reset_track_tag(index, TagField::Title);
        }
        self.set_track_tag(index, TagField::Title, name)
            .unwrap_or(false)
    }

    /// Sets tag `field` of track `index` (0-based) to `value`; an empty
    /// value clears the field. Returns whether anything changed (false
    /// also for an index out of range), or an error for a numeric field
    /// that is not a number.
    pub fn set_track_tag(
        &mut self,
        index: usize,
        field: TagField,
        value: &str,
    ) -> Result<bool, TagValueError> {
        let Some(start) = self.track_start(index) else {
            return Ok(false);
        };
        let mut next = self.edits.clone();
        next.track_tags
            .entry(start)
            .or_default()
            .set(field, value)?;
        Ok(self.commit(next))
    }

    /// Makes tag `field` of track `index` the source's (or the all-tracks
    /// value) again. Returns whether anything changed.
    pub fn reset_track_tag(&mut self, index: usize, field: TagField) -> bool {
        let Some(start) = self.track_start(index) else {
            return false;
        };
        let mut next = self.edits.clone();
        if let Some(t) = next.track_tags.get_mut(&start) {
            t.reset(field);
            if t.is_empty() {
                next.track_tags.remove(&start);
            }
        }
        self.commit(next)
    }

    /// Sets tag `field` of every track to `value` (empty clears it),
    /// replacing their own values, in one undoable step. Tracks added
    /// later get it too. Returns whether anything changed.
    pub fn set_all_tracks_tag(
        &mut self,
        field: TagField,
        value: &str,
    ) -> Result<bool, TagValueError> {
        let mut next = self.edits.clone();
        next.all_tags.set(field, value)?;
        for t in next.track_tags.values_mut() {
            t.reset(field);
        }
        next.track_tags.retain(|_, t| !t.is_empty());
        Ok(self.commit(next))
    }

    /// Where track `index` starts, if it exists.
    fn track_start(&self, index: usize) -> Option<SamplePos> {
        match index {
            0 => Some(0),
            i => self.edits.splits.get(i - 1).copied(),
        }
    }

    /// The parts that will be exported, in order (the complement of
    /// [`Editor::deleted`]).
    pub fn kept(&self) -> Vec<Range<SamplePos>> {
        let mut out = Vec::new();
        let mut pos = 0;
        for d in &self.edits.deleted {
            if d.start > pos {
                out.push(pos..d.start);
            }
            pos = d.end;
        }
        if pos < self.total {
            out.push(pos..self.total);
        }
        out
    }

    /// Sample frames that will be exported.
    pub fn kept_len(&self) -> SamplePos {
        self.total
            - self
                .edits
                .deleted
                .iter()
                .map(|d| d.end - d.start)
                .sum::<SamplePos>()
    }

    /// Whether `pos` lies in a deleted range.
    pub fn is_deleted(&self, pos: SamplePos) -> bool {
        let deleted = &self.edits.deleted;
        let i = deleted.partition_point(|d| d.end <= pos);
        deleted.get(i).is_some_and(|d| d.start <= pos)
    }

    /// Deletes the selection. Returns false (and changes nothing) if there
    /// is none or it is deleted already.
    pub fn delete_selection(&mut self) -> bool {
        let Some(sel) = self.selection.clone() else {
            return false;
        };
        let mut next = self.edits.clone();
        next.deleted.push(sel);
        ranges::normalize(&mut next.deleted);
        self.commit(next)
    }

    /// Deletes all of `ranges` in one undoable step, e.g. the pauses
    /// suggested by `lac_analysis::detect`. Parts beyond the end are
    /// clipped. Returns how many sample frames were newly deleted (0 if
    /// everything was deleted already, and then nothing is recorded).
    pub fn delete_ranges(
        &mut self,
        ranges: impl IntoIterator<Item = Range<SamplePos>>,
    ) -> SamplePos {
        let before = self.deleted_len();
        let mut next = self.edits.clone();
        next.deleted.extend(
            ranges
                .into_iter()
                .map(|r| r.start.min(self.total)..r.end.min(self.total))
                .filter(|r| r.start < r.end),
        );
        ranges::normalize(&mut next.deleted);
        self.commit(next);
        self.deleted_len() - before
    }

    /// Sample frames deleted in total.
    fn deleted_len(&self) -> SamplePos {
        self.edits.deleted.iter().map(|r| r.end - r.start).sum()
    }

    /// Restores (un-deletes) everything inside the selection. Returns false
    /// if nothing in it was deleted.
    pub fn restore_selection(&mut self) -> bool {
        let Some(sel) = self.selection.clone() else {
            return false;
        };
        let mut deleted = Vec::new();
        for d in &self.edits.deleted {
            // Keep the parts of `d` outside the selection.
            if d.start < sel.start {
                deleted.push(d.start..d.end.min(sel.start));
            }
            if d.end > sel.end {
                deleted.push(d.start.max(sel.end)..d.end);
            }
        }
        ranges::normalize(&mut deleted);
        let next = Edits {
            deleted,
            ..self.edits.clone()
        };
        self.commit(next)
    }

    /// Whether there is anything to undo.
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Whether there is anything to redo.
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Undoes the last change (delete, restore, split, join, tag change).
    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else {
            return false;
        };
        self.redo.push(std::mem::replace(&mut self.edits, prev));
        true
    }

    /// Redoes the last undone change.
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(std::mem::replace(&mut self.edits, next));
        true
    }

    /// Whether anything is edited, i.e. an export would differ from the
    /// source.
    pub fn is_modified(&self) -> bool {
        self.edits != Edits::default()
    }

    /// Remembers `edits` as exported or saved in a project: the
    /// [`Editor::edits`] taken when an export started or the project was
    /// written, so edits made since still count as unexported.
    pub fn mark_exported(&mut self, edits: Edits) {
        self.exported = Some(edits);
    }

    /// Whether there are edits that no export or saved project contains,
    /// i.e. closing the file would lose work. Undoing back to the exported state, or to no
    /// edits at all, counts as nothing to lose.
    pub fn has_unexported_changes(&self) -> bool {
        self.is_modified() && self.exported.as_ref() != Some(&self.edits)
    }

    /// What to hand to [`crate::Source::export`] for a single output file
    /// (all tracks joined).
    pub fn cut_request(&self) -> CutRequest {
        CutRequest::Delete(self.edits.deleted.clone())
    }

    /// Makes `next` the edits if they differ, with an undo step.
    fn commit(&mut self, next: Edits) -> bool {
        if next == self.edits {
            return false;
        }
        self.undo.push(std::mem::replace(&mut self.edits, next));
        self.redo.clear();
        true
    }
}

impl Editor {
    /// The status bar line about the edits: the selection, and once
    /// something is deleted how much is kept, e.g.
    /// `selected 0:01.500 .. 0:02.000 (0:00.500) · keeping 0:42.000 of 1:00.000`.
    pub fn describe(&self, rate: u32) -> String {
        let mut parts = Vec::new();
        if let Some(sel) = &self.selection {
            parts.push(describe_selection(sel, rate));
        }
        if !self.edits.splits.is_empty() {
            parts.push(trf(
                "track {n} of {count}",
                &[
                    ("n", &(self.track_at(self.play_from()) + 1)),
                    ("count", &(self.edits.splits.len() + 1)),
                ],
            ));
        }
        if !self.edits.deleted.is_empty() {
            parts.push(trf(
                "keeping {kept} of {total}",
                &[
                    ("kept", &format_samples(self.kept_len(), rate)),
                    ("total", &format_samples(self.total, rate)),
                ],
            ));
        }
        parts.join(" · ")
    }
}

/// One line for the status bar about the selection, e.g.
/// `selected 0:01.500 .. 0:02.000 (0:00.500)`.
pub fn describe_selection(sel: &Range<SamplePos>, rate: u32) -> String {
    trf(
        "selected {start} .. {end} ({length})",
        &[
            ("start", &format_samples(sel.start, rate)),
            ("end", &format_samples(sel.end, rate)),
            ("length", &format_samples(sel.end - sel.start, rate)),
        ],
    )
}

/// A short report of an export for the status bar: how many parts, the
/// output length, and how much longer it is than selected, because cuts
/// moved out to packet boundaries (lossy codecs) or the codec plays
/// padding at a join. One line, however many parts there are.
pub fn describe_export(kept: &[KeptRange], rate: u32) -> String {
    let len: SamplePos = kept.iter().map(KeptRange::output_len).sum();
    // How far each kept range reaches beyond what was asked for, on both
    // sides. A planner never narrows a range, but stay safe if one did.
    let widened: SamplePos = kept
        .iter()
        .map(|k| {
            k.requested.start.saturating_sub(k.actual.start)
                + k.actual.end.saturating_sub(k.requested.end)
        })
        .sum();
    let padding: SamplePos = kept.iter().map(|k| k.padding).sum();
    let template = if kept.len() == 1 {
        n_("exported {n} part, {length}")
    } else {
        n_("exported {n} parts, {length}")
    };
    let mut out = trf(
        template,
        &[("n", &kept.len()), ("length", &format_samples(len, rate))],
    );
    match (widened, padding) {
        (0, 0) => out += &tr(", all cuts exact"),
        (w, 0) => {
            out += &trf(
                "; cuts at packet boundaries add {added}",
                &[("added", &format_samples(w, rate))],
            )
        }
        (0, p) => {
            out += &trf(
                "; codec padding at joins adds {padding}",
                &[("padding", &format_samples(p, rate))],
            )
        }
        (w, p) => {
            out += &trf(
                "; cuts at packet boundaries add {added}, codec padding {padding}",
                &[
                    ("added", &format_samples(w, rate)),
                    ("padding", &format_samples(p, rate)),
                ],
            )
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_edits_are_fitted_to_the_recording() {
        let mut title = TagEdits::new();
        title.set(TagField::Title, "Two").expect("title");
        let edits = Edits {
            deleted: vec![80..120, 10..20, 15..30, 200..300],
            splits: vec![50, 0, 50, 150, 30],
            all_tags: TagEdits::new(),
            track_tags: [
                (50, title.clone()),
                (60, title.clone()),
                (150, title.clone()),
            ]
            .into_iter()
            .collect(),
        };
        let e = Editor::with_edits(100, edits);
        assert_eq!(e.deleted(), [10..30, 80..100]);
        assert_eq!(e.splits(), [30, 50]);
        assert_eq!(
            e.edits().track_tags.keys().copied().collect::<Vec<_>>(),
            [50],
            "tags of tracks that no longer start anywhere are dropped"
        );
        assert!(!e.has_unexported_changes(), "loaded edits count as saved");
        assert!(!e.can_undo());
    }

    #[test]
    fn moving_a_track_start_keeps_its_tags_and_the_order() {
        let mut e = Editor::new(10_000);
        e.split_at_all([3000, 6000]);
        assert!(e.rename_track(1, "Middle"));
        assert_eq!(e.track_start_range(0), None, "track 1 starts at 0");
        assert_eq!(e.track_start_range(1), Some(1..6000));
        assert_eq!(e.track_start_range(2), Some(3001..10_000));
        assert_eq!(e.track_start_range(3), None);
        assert!(e.move_track_start(1, 2500));
        assert_eq!(e.splits(), [2500, 6000]);
        assert_eq!(e.tracks()[1].name.as_deref(), Some("Middle"));
        // Not past a neighbour, not onto one, not the first track.
        assert!(!e.move_track_start(1, 6000));
        assert!(!e.move_track_start(2, 2500));
        assert!(!e.move_track_start(0, 100));
        assert!(!e.move_track_start(1, 2500), "no change");
        // One undo step.
        assert!(e.undo());
        assert_eq!(e.splits(), [3000, 6000]);
        assert_eq!(e.tracks()[1].name.as_deref(), Some("Middle"));
    }

    #[test]
    fn dragging_a_track_start_is_one_undo_step() {
        let mut e = Editor::new(10_000);
        e.split_at_all([3000, 6000]);
        assert!(e.rename_track(1, "Middle"));
        assert_eq!(e.track_start_near(3010, 20), Some(1));
        assert_eq!(e.track_start_near(5990, 20), Some(2));
        assert_eq!(e.track_start_near(4500, 20), None);
        assert_eq!(e.track_start_near(5, 20), None, "track 1 starts at 0");
        let undo_depth = e.undo.len();

        assert!(e.begin_moving_track_start(1));
        assert_eq!(e.moving_track_start(), Some(1));
        e.move_track_start_to(2000);
        assert_eq!(e.splits(), [2000, 6000], "moves at once");
        assert_eq!(e.tracks()[1].name.as_deref(), Some("Middle"));
        // Clamped: not onto or past a neighbour.
        e.move_track_start_to(9000);
        assert_eq!(e.splits(), [5999, 6000]);
        e.move_track_start_to(0);
        assert_eq!(e.splits(), [1, 6000]);
        e.move_track_start_to(2500);
        assert!(e.end_moving_track_start());
        assert_eq!(e.moving_track_start(), None);
        assert_eq!(e.undo.len(), undo_depth + 1, "one step for the drag");
        assert!(e.undo());
        assert_eq!(e.splits(), [3000, 6000]);
        assert_eq!(e.tracks()[1].name.as_deref(), Some("Middle"));

        // A drag that ends where it began records nothing.
        assert!(e.begin_moving_track_start(2));
        e.move_track_start_to(7000);
        e.move_track_start_to(6000);
        assert!(!e.end_moving_track_start());
        assert!(!e.begin_moving_track_start(0), "track 1 starts at 0");
        assert!(!e.begin_moving_track_start(3));
        e.move_track_start_to(100);
        assert_eq!(e.splits(), [3000, 6000], "no drag, no move");
    }

    #[test]
    fn setting_a_track_span_moves_both_ends_in_one_step() {
        let mut e = Editor::new(10_000);
        e.split_at_all([3000, 6000]);
        assert!(e.rename_track(1, "Middle"));
        assert!(e.rename_track(2, "Last"));
        assert!(e.can_set_track_span(1, 2000..7000));
        assert!(e.set_track_span(1, 2000..7000));
        assert_eq!(e.splits(), [2000, 7000]);
        let names: Vec<_> = e.tracks().into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            [None, Some("Middle".to_string()), Some("Last".to_string())]
        );
        assert!(e.undo());
        assert_eq!(e.splits(), [3000, 6000], "one undo step");

        // The first track starts at 0 and the last ends at the end.
        assert!(e.set_track_span(0, 0..4000));
        assert_eq!(e.splits(), [4000, 6000]);
        assert!(!e.can_set_track_span(0, 100..4000));
        assert!(!e.can_set_track_span(2, 6000..9000));
        assert!(e.set_track_span(2, 5000..10_000));
        assert_eq!(e.splits(), [4000, 5000]);
        // Moving a boundary onto or past a neighbour, an empty span, an
        // index out of range and no change are refused.
        assert!(!e.can_set_track_span(1, 4000..10_000), "last track empty");
        assert!(!e.can_set_track_span(1, 4500..4500));
        assert!(!e.can_set_track_span(3, 0..1));
        assert!(!e.can_set_track_span(1, 4000..5000), "no change");
        assert!(!e.set_track_span(1, 0..5000), "first track would be empty");
        assert_eq!(e.splits(), [4000, 5000]);
    }

    #[test]
    fn delete_ranges_is_one_undo_step_and_counts_only_new_frames() {
        let mut e = Editor::new(1000);
        e.select(100, 200);
        e.delete_selection();
        // Overlapping, out of order, partly beyond the end.
        assert_eq!(
            e.delete_ranges([900..1200, 150..300, 500..600]),
            100 + 100 + 100
        );
        assert_eq!(e.deleted(), [100..300, 500..600, 900..1000]);
        assert_eq!(
            e.delete_ranges(std::iter::once(120..280)),
            0,
            "deleted already"
        );
        assert!(e.undo());
        assert_eq!(
            e.deleted(),
            std::slice::from_ref(&(100..200)),
            "the whole batch at once"
        );
        assert_eq!(e.delete_ranges(std::iter::empty()), 0);
    }

    #[test]
    fn split_at_all_is_one_undo_step_and_skips_invalid_positions() {
        let mut e = Editor::new(1000);
        assert!(e.split_at(300));
        assert_eq!(e.split_at_all([0, 700, 300, 100, 1000, 5000, 700]), 2);
        assert_eq!(e.splits(), &[100, 300, 700]);
        assert!(e.undo());
        assert_eq!(e.splits(), &[300]);
        assert_eq!(e.split_at_all([300, 0]), 0);
        assert!(e.can_redo(), "a no-op leaves the redo history alone");
    }

    #[test]
    fn splitting_at_chained_links_takes_over_their_tags() {
        let tags = |title: &str, artist: Option<&str>| Tags {
            title: Some(title.into()),
            artist: artist.map(Into::into),
            album: Some("Radio".into()),
            ..Tags::default()
        };
        let source = tags("One", Some("A"));
        let mut e = Editor::new(1000);
        // The user split at 600 already and named that track.
        assert!(e.split_at(600));
        assert!(e.rename_track(1, "Mine"));
        let starts = [
            SuggestedStart {
                pos: 0,
                tags: Some(source.clone()),
            },
            SuggestedStart {
                pos: 200,
                tags: None,
            },
            SuggestedStart {
                pos: 400,
                tags: Some(tags("Two", Some("A"))),
            },
            SuggestedStart {
                pos: 600,
                tags: Some(tags("Three", None)),
            },
        ];
        assert_eq!(e.split_at_suggestions(&starts, &source), 2);
        assert_eq!(e.splits(), [200, 400, 600]);
        let edits: Vec<Vec<(TagField, String)>> = e
            .tracks()
            .iter()
            .map(|t| t.tags.iter().map(|(f, v)| (f, v.to_string())).collect())
            .collect();
        assert_eq!(
            edits,
            [
                vec![],
                vec![],
                // Only what differs from the file's tags.
                vec![(TagField::Title, "Two".to_string())],
                // The user's title stays; the missing artist is cleared.
                vec![
                    (TagField::Title, "Mine".to_string()),
                    (TagField::Artist, String::new())
                ],
            ]
        );
        assert!(e.undo(), "one undo step");
        assert_eq!(e.splits(), [600]);
        assert_eq!(e.tracks()[1].name.as_deref(), Some("Mine"));
    }

    #[test]
    fn dragging_selects_in_either_direction() {
        let mut e = Editor::new(1000);
        e.begin_drag(500);
        assert!(e.dragging());
        e.drag_to(200);
        assert_eq!(e.selection(), Some(200..500));
        e.drag_to(2000);
        assert_eq!(e.selection(), Some(500..1000), "clamped");
        e.end_drag();
        assert!(!e.dragging());
        // A click without movement clears the selection.
        e.begin_drag(300);
        e.end_drag();
        assert_eq!(e.selection(), None);
        // Moving without a drag does nothing.
        e.drag_to(10);
        assert_eq!(e.selection(), None);
    }

    #[test]
    fn delete_merges_and_keeps_the_complement() {
        let mut e = Editor::new(1000);
        e.select(100, 200);
        assert!(e.delete_selection());
        e.select(150, 300);
        assert!(e.delete_selection());
        e.select(900, 1000);
        assert!(e.delete_selection());
        assert_eq!(e.deleted(), &[100..300, 900..1000]);
        assert_eq!(e.kept(), vec![0..100, 300..900]);
        assert_eq!(e.kept_len(), 700);
        assert!(e.is_deleted(100) && e.is_deleted(299) && !e.is_deleted(300) && !e.is_deleted(99));
        assert_eq!(
            e.cut_request(),
            CutRequest::Delete(vec![100..300, 900..1000])
        );
        // Deleting what is deleted already is no change and no undo step.
        e.select(120, 130);
        assert!(!e.delete_selection());
        e.clear_selection();
        assert!(!e.delete_selection());
    }

    #[test]
    fn restore_cuts_holes_into_deletions() {
        let mut e = Editor::new(1000);
        e.select(100, 500);
        e.delete_selection();
        e.select(200, 300);
        assert!(e.restore_selection());
        assert_eq!(e.deleted(), &[100..200, 300..500]);
        e.select(0, 1000);
        assert!(e.restore_selection());
        assert!(!e.is_modified());
        assert!(!e.restore_selection(), "nothing left to restore");
    }

    #[test]
    fn undo_and_redo_walk_the_history() {
        let mut e = Editor::new(1000);
        assert!(!e.can_undo() && !e.undo());
        e.select(0, 10);
        e.delete_selection();
        e.select(20, 30);
        e.delete_selection();
        assert!(e.undo());
        assert_eq!(e.deleted(), std::slice::from_ref(&(0..10)));
        assert!(e.can_redo());
        assert!(e.redo());
        assert_eq!(e.deleted(), &[0..10, 20..30]);
        assert!(!e.redo());
        // A new change after an undo drops the redo history.
        e.undo();
        e.select(50, 60);
        e.delete_selection();
        assert!(!e.can_redo());
        assert_eq!(e.deleted(), &[0..10, 50..60]);
        e.undo();
        e.undo();
        assert!(!e.is_modified());
    }

    #[test]
    fn exporting_saves_the_edits() {
        let mut e = Editor::new(1000);
        assert!(!e.has_unexported_changes(), "nothing to lose yet");
        e.select(0, 10);
        e.delete_selection();
        assert!(e.has_unexported_changes());
        e.mark_exported(e.edits().clone());
        assert!(!e.has_unexported_changes());
        e.select(20, 30);
        e.delete_selection();
        assert!(e.has_unexported_changes());
        // Back to the exported state.
        e.undo();
        assert!(!e.has_unexported_changes());
        // Back to no edits at all.
        e.undo();
        assert!(!e.has_unexported_changes());
    }

    #[test]
    fn edits_during_an_export_stay_unexported() {
        let mut e = Editor::new(1000);
        e.select(0, 10);
        e.delete_selection();
        let started = e.edits().clone();
        e.select(20, 30);
        e.delete_selection();
        e.mark_exported(started);
        assert!(e.has_unexported_changes());
    }

    #[test]
    fn shift_click_moves_the_nearer_end() {
        let mut e = Editor::new(1000);
        e.begin_extend(300);
        assert_eq!(e.selection(), None, "no selection: a plain drag");
        e.drag_to(400);
        e.end_drag();
        assert_eq!(e.selection(), Some(300..400));
        e.begin_extend(450);
        assert_eq!(e.selection(), Some(300..450));
        e.drag_to(500);
        e.end_drag();
        assert_eq!(e.selection(), Some(300..500));
        e.begin_extend(310);
        e.end_drag();
        assert_eq!(e.selection(), Some(310..500));
        // Past the far end the selection flips around the anchor.
        e.begin_extend(320);
        e.drag_to(900);
        assert_eq!(e.selection(), Some(500..900));
    }

    #[test]
    fn the_cursor_follows_clicks_and_playback_starts_at_the_selection() {
        let mut e = Editor::new(1000);
        assert_eq!((e.cursor(), e.play_from()), (0, 0));
        e.begin_drag(400);
        e.end_drag();
        assert_eq!((e.cursor(), e.play_from()), (400, 400));
        e.begin_drag(700);
        e.drag_to(600);
        e.end_drag();
        assert_eq!(e.play_from(), 600, "selection start, not the press point");
        e.clear_selection();
        assert_eq!(e.play_from(), 700);
        e.set_cursor(5000);
        assert_eq!(e.cursor(), 1000, "clamped");
    }

    #[test]
    fn splitting_makes_tracks_that_survive_deletions() {
        let mut e = Editor::new(1000);
        assert_eq!(e.tracks().len(), 1);
        assert!(e.split_at(600));
        assert!(e.split_at(300));
        assert!(!e.split_at(300), "already a split");
        assert!(!e.split_at(0) && !e.split_at(1000), "not at the ends");
        assert_eq!(e.splits(), &[300, 600]);
        // A deletion across a split shortens both tracks; the split stays.
        e.select(250, 350);
        e.delete_selection();
        let t = e.tracks();
        assert_eq!(
            t.iter().map(|t| t.span.clone()).collect::<Vec<_>>(),
            vec![0..300, 300..600, 600..1000]
        );
        assert_eq!(t[0].kept, std::slice::from_ref(&(0..250)));
        assert_eq!(t[1].kept, std::slice::from_ref(&(350..600)));
        assert_eq!(t[2].kept_len(), 400);
        assert_eq!(t.iter().map(|t| t.number).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(
            (
                e.track_at(0),
                e.track_at(299),
                e.track_at(300),
                e.track_at(999)
            ),
            (0, 0, 1, 2)
        );
        // A track deleted entirely is still listed, with nothing kept.
        e.select(600, 1000);
        e.delete_selection();
        assert!(e.tracks()[2].kept.is_empty());
    }

    #[test]
    fn track_tags_override_the_all_tracks_tags() {
        let mut e = Editor::new(1000);
        e.split_at(500);
        assert_eq!(e.set_all_tracks_tag(TagField::Album, "Live"), Ok(true));
        assert_eq!(e.set_track_tag(1, TagField::Artist, "Guest"), Ok(true));
        assert_eq!(e.set_track_tag(1, TagField::Album, "Bonus"), Ok(true));
        assert_eq!(e.set_track_tag(1, TagField::Album, "Bonus"), Ok(false));
        assert!(e.set_track_tag(0, TagField::TrackNumber, "x").is_err());
        assert_eq!(e.set_track_tag(7, TagField::Title, "x"), Ok(false));
        let t = e.tracks();
        assert_eq!(t[0].tags.get(TagField::Album), Some("Live"));
        assert_eq!(t[0].tags.get(TagField::Artist), None);
        assert_eq!(t[1].tags.get(TagField::Album), Some("Bonus"));
        assert_eq!(t[1].tags.get(TagField::Artist), Some("Guest"));

        // A new track gets the all-tracks values.
        e.split_at(800);
        assert_eq!(e.tracks()[2].tags.get(TagField::Album), Some("Live"));
        // Resetting brings the all-tracks value back.
        assert!(e.reset_track_tag(1, TagField::Album));
        assert_eq!(e.tracks()[1].tags.get(TagField::Album), Some("Live"));
        assert!(!e.reset_track_tag(1, TagField::Album), "nothing to reset");
        // Setting for all tracks replaces the tracks' own values.
        assert_eq!(e.set_all_tracks_tag(TagField::Artist, ""), Ok(true));
        assert!(
            e.tracks()
                .iter()
                .all(|t| t.tags.get(TagField::Artist) == Some(""))
        );
        assert!(e.edits().track_tags.is_empty());
        // One undo step each.
        assert!(e.undo());
        assert_eq!(e.tracks()[1].tags.get(TagField::Artist), Some("Guest"));
    }

    #[test]
    fn merging_tracks_keeps_the_first_name_and_is_one_undo_step() {
        let mut e = Editor::new(1000);
        for at in [200, 400, 600, 800] {
            e.split_at(at);
        }
        for (i, name) in ["A", "B", "C", "D", "E"].iter().enumerate() {
            assert!(e.rename_track(i, name));
        }
        // Tracks 2 to 4 (B, C, D) become one, named B.
        assert!(e.merge_tracks(1..=3));
        assert_eq!(e.splits(), &[200, 800]);
        let names: Vec<_> = e.tracks().into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            [Some("A".into()), Some("B".into()), Some("E".into())]
        );
        assert_eq!(e.tracks()[1].span, 200..800);
        // C's and D's names went with their split points: splitting there
        // again gives unnamed tracks.
        e.split_at(400);
        assert_eq!(e.tracks()[2].name, None);
        assert!(e.undo());
        // One undo step brings all five back.
        assert!(e.undo());
        assert_eq!(e.tracks().len(), 5);
        assert_eq!(e.tracks()[3].name.as_deref(), Some("D"));
        // Fewer than two tracks, or tracks that do not exist.
        assert!(!e.merge_tracks(2..=2));
        let (from, to) = (3, 1);
        assert!(!e.merge_tracks(from..=to), "reversed");
        assert!(!e.merge_tracks(3..=5));
        assert!(e.merge_tracks(0..=4));
        assert_eq!(e.tracks().len(), 1);
    }

    #[test]
    fn names_follow_their_track_and_everything_undoes() {
        let mut e = Editor::new(1000);
        assert!(e.rename_track(0, "  Intro "));
        assert_eq!(e.tracks()[0].name.as_deref(), Some("Intro"));
        assert!(!e.rename_track(0, "Intro"), "no change");
        assert!(!e.rename_track(5, "x"), "no such track");
        e.split_at(500);
        assert!(e.rename_track(1, "Talk"));
        let names: Vec<_> = e.tracks().into_iter().map(|t| t.name).collect();
        assert_eq!(names, [Some("Intro".to_string()), Some("Talk".to_string())]);
        // Splitting a named track: the first part keeps the name.
        e.split_at(200);
        let names: Vec<_> = e.tracks().into_iter().map(|t| t.name).collect();
        assert_eq!(names, [Some("Intro".into()), None, Some("Talk".into())]);
        // Joining drops the joined track's name.
        assert!(e.join_with_previous(2));
        assert_eq!(e.splits(), &[200]);
        assert_eq!(e.tracks()[1].name, None);
        assert!(!e.join_with_previous(0) && !e.join_with_previous(9));
        // Blank names remove the name.
        assert!(e.rename_track(0, " "));
        assert_eq!(e.tracks()[0].name, None);
        assert!(e.has_unexported_changes());
        while e.undo() {}
        assert!(!e.is_modified());
        assert_eq!(e.tracks().len(), 1);
    }

    #[test]
    fn status_line() {
        let mut e = Editor::new(60_000);
        assert_eq!(e.describe(1000), "");
        e.select(1500, 2000);
        assert_eq!(e.describe(1000), "selected 0:01.500 .. 0:02.000 (0:00.500)");
        e.delete_selection();
        e.clear_selection();
        assert_eq!(e.describe(1000), "keeping 0:59.500 of 1:00.000");
        e.select(0, 1000);
        assert_eq!(
            e.describe(1000),
            "selected 0:00.000 .. 0:01.000 (0:01.000) · keeping 0:59.500 of 1:00.000"
        );
        e.split_at(30_000);
        e.select(40_000, 41_000);
        assert_eq!(
            e.describe(1000),
            "selected 0:40.000 .. 0:41.000 (0:01.000) · track 2 of 2 · keeping 0:59.500 of 1:00.000"
        );
    }

    #[test]
    fn select_all_and_empty_recordings() {
        let mut e = Editor::new(0);
        e.select_all();
        assert_eq!(e.selection(), None);
        assert_eq!(e.kept(), Vec::<Range<SamplePos>>::new());
        let mut e = Editor::new(5);
        e.select_all();
        assert_eq!(e.selection(), Some(0..5));
    }

    #[test]
    fn descriptions() {
        assert_eq!(
            describe_selection(&(1500..2000), 1000),
            "selected 0:01.500 .. 0:02.000 (0:00.500)"
        );
        let exact = KeptRange {
            requested: 0..1000,
            actual: 0..1000,
            padding: 0,
        };
        assert_eq!(
            describe_export(std::slice::from_ref(&exact), 1000),
            "exported 1 part, 0:01.000, all cuts exact"
        );
        let widened = KeptRange {
            requested: 2000..3000,
            actual: 1990..3010,
            padding: 5,
        };
        assert_eq!(
            describe_export(&[exact.clone(), widened.clone()], 1000),
            "exported 2 parts, 0:02.025; cuts at packet boundaries add 0:00.020, codec padding 0:00.005"
        );
        let unpadded = KeptRange {
            padding: 0,
            ..widened.clone()
        };
        assert_eq!(
            describe_export(&[unpadded], 1000),
            "exported 1 part, 0:01.020; cuts at packet boundaries add 0:00.020"
        );
        let padded_only = KeptRange {
            actual: 2000..3000,
            ..widened
        };
        assert_eq!(
            describe_export(&[padded_only, exact], 1000),
            "exported 2 parts, 0:02.005; codec padding at joins adds 0:00.005"
        );
    }
}
