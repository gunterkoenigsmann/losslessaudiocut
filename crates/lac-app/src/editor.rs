//! The editing state of one opened recording: what is selected, what is
//! deleted, and the undo history. The view model behind the GUI's Edit
//! menu and mouse selection, testable without a display.
//!
//! Edits are the "delete parts, join the rest" model of
//! `docs/architecture.md`: the source is never changed, the editor only
//! keeps the list of deleted ranges (in source sample frames). The waveform
//! keeps showing the source timeline with the deleted parts marked, so
//! that a deletion can be seen, selected again and restored. Export turns
//! the list into a [`CutRequest::Delete`].
//!
//! Undo and redo keep whole snapshots of the deleted list; it is small
//! (one range per deletion), so this is simpler and safer than inverse
//! operations.

use std::ops::Range;

use lac_core::SamplePos;

use crate::{CutRequest, KeptRange, format_samples, ranges};

/// Editing state of one recording of `total` sample frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    total: SamplePos,
    /// Deleted ranges, normalised (sorted, disjoint, non-adjacent).
    deleted: Vec<Range<SamplePos>>,
    selection: Option<Range<SamplePos>>,
    /// Where the current mouse drag started.
    anchor: Option<SamplePos>,
    undo: Vec<Vec<Range<SamplePos>>>,
    redo: Vec<Vec<Range<SamplePos>>>,
    /// The deleted list at the last export, to tell whether the edits are
    /// saved anywhere.
    exported: Option<Vec<Range<SamplePos>>>,
}

impl Editor {
    /// A recording of `total` sample frames with nothing deleted.
    pub fn new(total: SamplePos) -> Self {
        Editor {
            total,
            deleted: Vec::new(),
            selection: None,
            anchor: None,
            undo: Vec::new(),
            redo: Vec::new(),
            exported: None,
        }
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

    /// Mouse button pressed at `pos`: starts a new selection there.
    pub fn begin_drag(&mut self, pos: SamplePos) {
        self.anchor = Some(pos.min(self.total));
        self.selection = None;
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
        &self.deleted
    }

    /// The parts that will be exported, in order (the complement of
    /// [`Editor::deleted`]).
    pub fn kept(&self) -> Vec<Range<SamplePos>> {
        let mut out = Vec::new();
        let mut pos = 0;
        for d in &self.deleted {
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
                .deleted
                .iter()
                .map(|d| d.end - d.start)
                .sum::<SamplePos>()
    }

    /// Whether `pos` lies in a deleted range.
    pub fn is_deleted(&self, pos: SamplePos) -> bool {
        let i = self.deleted.partition_point(|d| d.end <= pos);
        self.deleted.get(i).is_some_and(|d| d.start <= pos)
    }

    /// Deletes the selection. Returns false (and changes nothing) if there
    /// is none or it is deleted already.
    pub fn delete_selection(&mut self) -> bool {
        let Some(sel) = self.selection.clone() else {
            return false;
        };
        let mut next = self.deleted.clone();
        next.push(sel);
        ranges::normalize(&mut next);
        self.commit(next)
    }

    /// Restores (un-deletes) everything inside the selection. Returns false
    /// if nothing in it was deleted.
    pub fn restore_selection(&mut self) -> bool {
        let Some(sel) = self.selection.clone() else {
            return false;
        };
        let mut next = Vec::new();
        for d in &self.deleted {
            // Keep the parts of `d` outside the selection.
            if d.start < sel.start {
                next.push(d.start..d.end.min(sel.start));
            }
            if d.end > sel.end {
                next.push(d.start.max(sel.end)..d.end);
            }
        }
        ranges::normalize(&mut next);
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

    /// Undoes the last delete or restore.
    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else {
            return false;
        };
        self.redo.push(std::mem::replace(&mut self.deleted, prev));
        true
    }

    /// Redoes the last undone change.
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(std::mem::replace(&mut self.deleted, next));
        true
    }

    /// Whether anything is deleted, i.e. an export would differ from the
    /// source.
    pub fn is_modified(&self) -> bool {
        !self.deleted.is_empty()
    }

    /// Remembers `deleted` as exported: the [`Editor::deleted`] list taken
    /// when an export started, so edits made while it ran still count as
    /// unexported.
    pub fn mark_exported(&mut self, deleted: Vec<Range<SamplePos>>) {
        self.exported = Some(deleted);
    }

    /// Whether there are edits that no export contains, i.e. closing the
    /// file would lose work. Undoing back to the exported state, or to no
    /// edits at all, counts as nothing to lose.
    pub fn has_unexported_changes(&self) -> bool {
        self.is_modified() && self.exported.as_ref() != Some(&self.deleted)
    }

    /// What to hand to [`crate::Source::export`].
    pub fn cut_request(&self) -> CutRequest {
        CutRequest::Delete(self.deleted.clone())
    }

    /// Makes `next` the deleted list if it differs, with an undo step.
    fn commit(&mut self, next: Vec<Range<SamplePos>>) -> bool {
        if next == self.deleted {
            return false;
        }
        self.undo.push(std::mem::replace(&mut self.deleted, next));
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
        if self.is_modified() {
            parts.push(format!(
                "keeping {} of {}",
                format_samples(self.kept_len(), rate),
                format_samples(self.total, rate)
            ));
        }
        parts.join(" · ")
    }
}

/// One line for the status bar about the selection, e.g.
/// `selected 0:01.500 .. 0:02.000 (0:00.500)`.
pub fn describe_selection(sel: &Range<SamplePos>, rate: u32) -> String {
    format!(
        "selected {} .. {} ({})",
        format_samples(sel.start, rate),
        format_samples(sel.end, rate),
        format_samples(sel.end - sel.start, rate)
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
    let parts = if kept.len() == 1 { "part" } else { "parts" };
    let mut out = format!(
        "exported {} {parts}, {}",
        kept.len(),
        format_samples(len, rate)
    );
    match (widened, padding) {
        (0, 0) => out += ", all cuts exact",
        (w, 0) => {
            out += &format!(
                "; cuts at packet boundaries add {}",
                format_samples(w, rate)
            )
        }
        (0, p) => out += &format!("; codec padding at joins adds {}", format_samples(p, rate)),
        (w, p) => {
            out += &format!(
                "; cuts at packet boundaries add {}, codec padding {}",
                format_samples(w, rate),
                format_samples(p, rate)
            )
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        e.mark_exported(e.deleted().to_vec());
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
        let started = e.deleted().to_vec();
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
