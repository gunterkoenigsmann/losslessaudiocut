//! The right-click menus of the waveform (issue #82: what can be done with
//! the selection, the playback position or the place under the pointer)
//! and of the track list (issue #90: what can be done with a track).
//!
//! [`waveform`] and [`track_list`] list the entries; each carries an
//! [`Action`] with the concrete positions it was offered for, so the menu
//! can say what an entry does and grey out what would change nothing.
//! [`apply`] runs an action on the [`Editor`]; playing is left to the GUI,
//! which owns the player.
//!
//! The right button also scrubs (hold it, or drag); [`RightPress`] tells a
//! click, which opens the menu, from the start of a scrub.

use std::ops::{Range, RangeInclusive};
use std::time::{Duration, Instant};

use lac_core::SamplePos;
use lac_playback::Timeline;

use crate::editor::Editor;
use crate::hover::Scene;
use crate::i18n::{tr, trf};
use crate::playback::{self, EXCERPT_SECONDS, End};
use crate::time::format_samples;
use crate::track_list::Bound;

/// How far (in pixels) the pointer may move while the right button is
/// down for it to still be a click rather than a scrub.
pub const CLICK_SLOP: f64 = 4.0;

/// How long the right button may be held for a click: holding it longer
/// listens at the pointer (a scrub without moving).
pub const CLICK_TIME: Duration = Duration::from_millis(250);

/// How far (in seconds) "Fine-tune selection" looks for a detected cutting
/// point to move each edge of the selection to.
pub const FINE_TUNE_SECONDS: u32 = 2;

/// The right button went down and it is not yet known whether this is a
/// click (menu) or a scrub.
#[derive(Debug, Clone, Copy)]
pub struct RightPress {
    x: f64,
    y: f64,
    at: Instant,
}

impl RightPress {
    /// The button went down at window position (`x`, `y`) at time `at`.
    pub fn new(x: f64, y: f64, at: Instant) -> Self {
        RightPress { x, y, at }
    }

    /// Where the button went down.
    pub fn x(&self) -> f64 {
        self.x
    }

    /// Whether the pointer at (`x`, `y`) has left the click's reach, which
    /// makes the press a scrub.
    pub fn moved(&self, x: f64, y: f64) -> bool {
        (x - self.x).abs() > CLICK_SLOP || (y - self.y).abs() > CLICK_SLOP
    }

    /// Whether the button has been held too long for a click at `now`,
    /// which makes the press a scrub.
    pub fn held(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.at) >= CLICK_TIME
    }
}

/// What a menu entry does, with the positions it was offered for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// New tracks start at these positions (one undo step).
    Split(Vec<SamplePos>),
    /// Track `track` (0-based) spans exactly `span`.
    SetTrackSpan {
        /// Index of the track.
        track: usize,
        /// Its new start and end.
        span: Range<SamplePos>,
    },
    /// The start or end of track `track` (0-based) moves to `to`.
    MoveBound {
        /// Index of the track.
        track: usize,
        /// Which end moves.
        bound: Bound,
        /// Where it moves to.
        to: SamplePos,
    },
    /// The selection becomes this range.
    Select(Range<SamplePos>),
    /// Deletes the selection.
    Delete,
    /// Restores the deleted parts in the selection.
    Restore,
    /// Plays the first or last [`EXCERPT_SECONDS`] of kept audio in the
    /// range.
    PlayExcerpt {
        /// The source range.
        range: Range<SamplePos>,
        /// Which end of it.
        end: End,
    },
    /// Plays from this source position on.
    PlayFrom(SamplePos),
    /// Selects this range (a track), puts the cursor at its start and
    /// zooms to it.
    ShowTrack(Range<SamplePos>),
    /// Clears the selection and puts the cursor (and, while playing, the
    /// playback position) here.
    JumpTo(SamplePos),
    /// Joins track `index` (0-based) with the one before it.
    JoinWithPrevious(usize),
    /// Merges these tracks (0-based, inclusive) into one.
    MergeTracks(RangeInclusive<usize>),
    /// Asks for a file name and writes what is kept of the selection to
    /// it (issue #101).
    ExportSelection,
    /// Selects this range (a track) and deletes it.
    DeleteRange(Range<SamplePos>),
    /// Selects this range (a track) and restores what is deleted in it.
    RestoreRange(Range<SamplePos>),
}

/// One entry of the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// The text shown.
    pub label: String,
    /// What choosing it does.
    pub action: Action,
    /// Whether it would do anything; disabled entries are shown greyed
    /// out, so the menu does not change shape under the user's hand.
    pub enabled: bool,
}

/// A line of the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A command.
    Item(Item),
    /// A separator between groups of commands.
    Separator,
}

/// What [`apply`] leaves for the GUI to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// The editor was changed (`true`) or nothing changed (`false`).
    Edited(bool),
    /// Play this, then stop.
    Play(Timeline),
    /// Play from this source position on.
    PlayFrom(SamplePos),
    /// Zoom the waveform to this range (selected already).
    ZoomTo(Range<SamplePos>),
    /// Move the cursor (and playback) here, see [`Action::JumpTo`].
    JumpTo(SamplePos),
    /// Export the selection: the GUI asks for the file (see
    /// [`crate::export_job::selection_task`]).
    ExportSelection,
}

/// The menu for a right-click at source position `pos`, with marks within
/// `near` samples counting as under the pointer (as for tooltips, see
/// [`crate::hover::tooltip`]).
///
/// Groups, separated: the selection's commands if the click is in (or at
/// an edge of) the selection, the playback position's if it is near the
/// playhead, and always the pointer's.
pub fn waveform(scene: &Scene, pos: SamplePos, near: SamplePos) -> Vec<Entry> {
    let ed = scene.editor;
    let mut groups = Vec::new();
    if let Some(sel) = ed
        .selection()
        .filter(|s| s.start.saturating_sub(near) <= pos && pos <= s.end + near)
    {
        groups.push(selection_items(scene, &sel, pos));
    }
    if let Some(ph) = scene.playhead.filter(|p| p.abs_diff(pos) <= near) {
        groups.push(playhead_items(scene, ph));
    }
    groups.push(pointer_items(scene, pos));
    let mut entries = Vec::new();
    for g in groups {
        if !entries.is_empty() {
            entries.push(Entry::Separator);
        }
        entries.extend(g.into_iter().map(Entry::Item));
    }
    entries
}

/// Commands for the selection `sel`, clicked at `pos`. Track commands are
/// about the track under the pointer.
fn selection_items(scene: &Scene, sel: &Range<SamplePos>, pos: SamplePos) -> Vec<Item> {
    let ed = scene.editor;
    let t = |p| format_samples(p, scene.rate);
    let track = ed.track_at(pos.min(ed.total().saturating_sub(1)));
    let n = track + 1;
    let bound = |bound, to, label: String| {
        // The end of track i is the start of track i + 1.
        let start_of = match bound {
            Bound::Start => track,
            Bound::End => track + 1,
        };
        let enabled = ed
            .track_start_range(start_of)
            .is_some_and(|r| r.contains(&to));
        Item {
            label,
            action: Action::MoveBound { track, bound, to },
            enabled,
        }
    };
    let excerpt = |end, label: &str| Item {
        label: trf(
            "{label} ({seconds} s)",
            &[("label", &label), ("seconds", &EXCERPT_SECONDS)],
        ),
        enabled: !playback::excerpt(ed, sel.clone(), end, 1).is_empty(),
        action: Action::PlayExcerpt {
            range: sel.clone(),
            end,
        },
    };
    let kept = ed.kept();
    let any_kept = kept.iter().any(|k| k.start < sel.end && sel.start < k.end);
    let any_deleted = ed
        .deleted()
        .iter()
        .any(|d| d.start < sel.end && sel.start < d.end);
    let tuned = fine_tuned(scene, sel);
    let mut items = vec![
        split_item(
            ed,
            vec![sel.start, sel.end],
            tr("New track from selection").into_owned(),
        ),
        Item {
            label: trf("Fit track {n} to selection", &[("n", &n)]),
            enabled: ed.can_set_track_span(track, sel.clone()),
            action: Action::SetTrackSpan {
                track,
                span: sel.clone(),
            },
        },
        bound(
            Bound::Start,
            sel.start,
            trf("Track {n} starts at selection start", &[("n", &n)]),
        ),
        bound(
            Bound::Start,
            sel.end,
            trf("Track {n} starts at selection end", &[("n", &n)]),
        ),
        bound(
            Bound::End,
            sel.start,
            trf("Track {n} ends at selection start", &[("n", &n)]),
        ),
        bound(
            Bound::End,
            sel.end,
            trf("Track {n} ends at selection end", &[("n", &n)]),
        ),
        excerpt(End::Start, &tr("Play selection start")),
        excerpt(End::End, &tr("Play selection end")),
        Item {
            label: match &tuned {
                Some(r) => trf(
                    "Fine-tune selection to detected cuts ({start}–{end})",
                    &[("start", &t(r.start)), ("end", &t(r.end))],
                ),
                None => tr("Fine-tune selection to detected cuts (none nearby)").into_owned(),
            },
            enabled: tuned.is_some(),
            action: Action::Select(tuned.unwrap_or_else(|| sel.clone())),
        },
        Item {
            label: tr("Export selection as…").into_owned(),
            action: Action::ExportSelection,
            enabled: any_kept,
        },
        Item {
            label: tr("Delete selection").into_owned(),
            action: Action::Delete,
            enabled: any_kept,
        },
        Item {
            label: tr("Restore selection").into_owned(),
            action: Action::Restore,
            enabled: any_deleted,
        },
    ];
    // Keep entries in a stable order, but drop the pointless "restore"
    // where nothing was ever deleted.
    if ed.deleted().is_empty() {
        items.pop();
    }
    // A selection reaching into several tracks selects them: offer to
    // merge them, right after "New track from selection".
    let last = ed.track_at(sel.end.saturating_sub(1).max(sel.start));
    if let Some(merge) = merge_item(ed.track_at(sel.start)..=last) {
        items.insert(1, merge);
    }
    items
}

/// Commands for the playback position `ph`.
fn playhead_items(scene: &Scene, ph: SamplePos) -> Vec<Item> {
    let ed = scene.editor;
    let at = format_samples(ph, scene.rate);
    let mut items = vec![split_item(
        ed,
        vec![ph],
        trf("Split at playback position {at}", &[("at", &at)]),
    )];
    if let Some(sel) = ed.selection() {
        items.push(Item {
            label: trf("Selection starts at playback position {at}", &[("at", &at)]),
            enabled: ph < sel.end && ph != sel.start,
            action: Action::Select(ph..sel.end),
        });
        items.push(Item {
            label: trf("Selection ends at playback position {at}", &[("at", &at)]),
            enabled: ph > sel.start && ph != sel.end,
            action: Action::Select(sel.start..ph),
        });
    }
    items
}

/// Commands for the place under the pointer.
fn pointer_items(scene: &Scene, pos: SamplePos) -> Vec<Item> {
    let ed = scene.editor;
    let at = format_samples(pos, scene.rate);
    vec![
        split_item(ed, vec![pos], trf("Split here ({at})", &[("at", &at)])),
        Item {
            label: tr("Play from here").into_owned(),
            action: Action::PlayFrom(pos),
            enabled: ed.kept().iter().any(|k| k.end > pos),
        },
    ]
}

/// An entry starting tracks at `at`, enabled if any of them is new.
fn split_item(ed: &Editor, at: Vec<SamplePos>, label: String) -> Item {
    let enabled = at
        .iter()
        .any(|&p| p > 0 && p < ed.total() && !ed.splits().contains(&p));
    Item {
        label,
        action: Action::Split(at),
        enabled,
    }
}

/// "Merge tracks …" for the tracks `tracks` (0-based, inclusive), if
/// they are at least two (issue #96); `None` for a single track, so the
/// entry appears only where there is something to merge.
fn merge_item(tracks: RangeInclusive<usize>) -> Option<Item> {
    let (first, last) = (*tracks.start() + 1, *tracks.end() + 1);
    let label = match last.checked_sub(first)? {
        0 => return None,
        1 => trf(
            "Merge tracks {first} and {last}",
            &[("first", &first), ("last", &last)],
        ),
        _ => trf(
            "Merge tracks {first} to {last}",
            &[("first", &first), ("last", &last)],
        ),
    };
    Some(Item {
        label,
        action: Action::MergeTracks(tracks),
        enabled: true,
    })
}

/// The selection with each edge moved to the nearest detected cutting
/// point (an edge of a suggested pause, or a suggested track start)
/// within [`FINE_TUNE_SECONDS`]; `None` if that changes nothing or would
/// leave nothing selected.
fn fine_tuned(scene: &Scene, sel: &Range<SamplePos>) -> Option<Range<SamplePos>> {
    let reach = SamplePos::from(FINE_TUNE_SECONDS) * SamplePos::from(scene.rate);
    let points: Vec<SamplePos> = scene
        .pauses
        .iter()
        .flat_map(|r| [r.range.start, r.range.end])
        .chain(scene.suggested_starts.iter().copied())
        .collect();
    let snap = |p: SamplePos| {
        points
            .iter()
            .copied()
            .filter(|q| q.abs_diff(p) <= reach)
            .min_by_key(|q| q.abs_diff(p))
            .unwrap_or(p)
    };
    let tuned = snap(sel.start)..snap(sel.end);
    (tuned.start < tuned.end && tuned != *sel).then_some(tuned)
}

/// Runs `action` on `editor` of a recording with `rate` frames per
/// second. Playing is left to the caller (see [`Effect`]).
pub fn apply(editor: &mut Editor, action: &Action, rate: u32) -> Effect {
    match action {
        Action::Split(at) => Effect::Edited(editor.split_at_all(at.iter().copied()) > 0),
        Action::SetTrackSpan { track, span } => {
            Effect::Edited(editor.set_track_span(*track, span.clone()))
        }
        Action::MoveBound { track, bound, to } => {
            let start_of = match bound {
                Bound::Start => *track,
                Bound::End => track + 1,
            };
            Effect::Edited(editor.move_track_start(start_of, *to))
        }
        Action::Select(r) => {
            editor.select(r.start, r.end);
            Effect::Edited(true)
        }
        Action::Delete => Effect::Edited(editor.delete_selection()),
        Action::Restore => Effect::Edited(editor.restore_selection()),
        Action::PlayExcerpt { range, end } => {
            let len = playback::excerpt_len(rate);
            Effect::Play(playback::excerpt(editor, range.clone(), *end, len))
        }
        Action::PlayFrom(p) => Effect::PlayFrom(*p),
        Action::ShowTrack(r) => {
            editor.select(r.start, r.end);
            editor.set_cursor(r.start);
            Effect::ZoomTo(r.clone())
        }
        Action::JumpTo(p) => Effect::JumpTo(*p),
        Action::JoinWithPrevious(i) => Effect::Edited(editor.join_with_previous(*i)),
        Action::MergeTracks(r) => Effect::Edited(editor.merge_tracks(r.clone())),
        Action::ExportSelection => Effect::ExportSelection,
        Action::DeleteRange(r) => {
            editor.select(r.start, r.end);
            Effect::Edited(editor.delete_selection())
        }
        Action::RestoreRange(r) => {
            editor.select(r.start, r.end);
            Effect::Edited(editor.restore_selection())
        }
    }
}

/// The menu for a right-click on row `index` (0-based) of the track list:
/// showing and playing the track, moving the cursor or the selection's
/// edges to its ends, joining and deleting it. Empty for a row that is no
/// track.
pub fn track_list(editor: &Editor, index: usize, selected: &[usize]) -> Vec<Entry> {
    let Some(track) = editor.tracks().into_iter().nth(index) else {
        return Vec::new();
    };
    let span = track.span.clone();
    let n = track.number;
    let any_kept = !track.kept.is_empty();
    let any_deleted = track
        .kept
        .iter()
        .map(|k| k.end - k.start)
        .sum::<SamplePos>()
        < span.end - span.start;
    let excerpt = |end, label: String| Item {
        label: trf(
            "{label} ({seconds} s)",
            &[("label", &label), ("seconds", &EXCERPT_SECONDS)],
        ),
        enabled: any_kept,
        action: Action::PlayExcerpt {
            range: span.clone(),
            end,
        },
    };
    let jump = |to: SamplePos, label: String| Item {
        enabled: editor.selection().is_some() || editor.cursor() != to,
        label,
        action: Action::JumpTo(to),
    };
    let mut groups = vec![
        vec![
            Item {
                label: trf("Zoom to track {n}", &[("n", &n)]),
                action: Action::ShowTrack(span.clone()),
                enabled: true,
            },
            excerpt(End::Start, trf("Play start of track {n}", &[("n", &n)])),
            excerpt(End::End, trf("Play end of track {n}", &[("n", &n)])),
        ],
        vec![
            jump(
                span.start,
                trf("Cursor to start of track {n}", &[("n", &n)]),
            ),
            jump(span.end, trf("Cursor to end of track {n}", &[("n", &n)])),
        ],
    ];
    if let Some(sel) = editor.selection() {
        // Each edge to each end of the track; the selection must keep
        // something selected.
        let select = |r: Range<SamplePos>, label: String| Item {
            enabled: r.start < r.end && r != sel,
            label,
            action: Action::Select(r),
        };
        groups.push(vec![
            select(
                span.start..sel.end,
                trf("Selection start to start of track {n}", &[("n", &n)]),
            ),
            select(
                span.end..sel.end,
                trf("Selection start to end of track {n}", &[("n", &n)]),
            ),
            select(
                sel.start..span.start,
                trf("Selection end to start of track {n}", &[("n", &n)]),
            ),
            select(
                sel.start..span.end,
                trf("Selection end to end of track {n}", &[("n", &n)]),
            ),
        ]);
    }
    // Rows selected in the list, if the clicked one is among them, select
    // the tracks from the first to the last of them (issue #96).
    let merge = match (selected.iter().min(), selected.iter().max()) {
        (Some(&first), Some(&last)) if (first..=last).contains(&index) => {
            merge_item(first..=last.min(editor.tracks().len() - 1))
        }
        _ => None,
    };
    groups.push(merge.into_iter().collect());
    groups.push(vec![
        Item {
            label: trf("Join track {n} with the previous one", &[("n", &n)]),
            action: Action::JoinWithPrevious(index),
            enabled: index > 0,
        },
        Item {
            label: trf("Delete track {n}", &[("n", &n)]),
            action: Action::DeleteRange(span.clone()),
            enabled: any_kept,
        },
        Item {
            label: trf("Restore track {n}", &[("n", &n)]),
            action: Action::RestoreRange(span.clone()),
            enabled: any_deleted,
        },
    ]);
    let mut entries = Vec::new();
    for g in groups.into_iter().filter(|g| !g.is_empty()) {
        if !entries.is_empty() {
            entries.push(Entry::Separator);
        }
        entries.extend(g.into_iter().map(Entry::Item));
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use lac_analysis::{Region, RegionKind};

    /// 1000 frames per second keeps the times readable.
    const RATE: u32 = 1000;

    fn scene<'a>(editor: &'a Editor, pauses: &'a [Region], starts: &'a [SamplePos]) -> Scene<'a> {
        Scene {
            editor,
            pauses,
            suggested_starts: starts,
            playhead: None,
            rate: RATE,
        }
    }

    /// The menu's items, without separators.
    fn items(entries: &[Entry]) -> Vec<&Item> {
        entries
            .iter()
            .filter_map(|e| match e {
                Entry::Item(i) => Some(i),
                Entry::Separator => None,
            })
            .collect()
    }

    /// The item labelled `label`.
    fn item<'a>(entries: &'a [Entry], label: &str) -> &'a Item {
        items(entries)
            .into_iter()
            .find(|i| i.label.starts_with(label))
            .unwrap_or_else(|| panic!("no {label:?} in {entries:#?}"))
    }

    #[test]
    fn a_click_stays_a_click_until_the_pointer_moves_or_is_held() {
        let t0 = Instant::now();
        let p = RightPress::new(100.0, 50.0, t0);
        assert!(!p.moved(103.0, 47.0));
        assert!(p.moved(105.0, 50.0));
        assert!(p.moved(100.0, 55.0));
        assert!(!p.held(t0 + Duration::from_millis(100)));
        assert!(p.held(t0 + CLICK_TIME));
        assert_eq!(p.x(), 100.0);
    }

    #[test]
    fn outside_the_selection_only_the_pointer_has_commands() {
        let e = Editor::new(60_000);
        let s = scene(&e, &[], &[]);
        let menu = waveform(&s, 5_000, 10);
        let labels: Vec<_> = items(&menu).iter().map(|i| i.label.clone()).collect();
        assert_eq!(labels, ["Split here (0:05.000)", "Play from here"]);
        assert!(items(&menu).iter().all(|i| i.enabled));
        // Not at the very start: track 1 always starts there.
        let menu = waveform(&s, 0, 10);
        assert!(!item(&menu, "Split here").enabled);
    }

    #[test]
    fn the_selection_can_become_a_track_or_move_its_bounds() {
        let mut e = Editor::new(60_000);
        e.split_at_all([10_000, 30_000]);
        e.select(15_000, 20_000);
        let s = scene(&e, &[], &[]);
        // Right-click in track 2 (10..30 s), inside the selection.
        let menu = waveform(&s, 17_000, 10);
        assert_eq!(menu.iter().filter(|e| **e == Entry::Separator).count(), 1);
        let new = item(&menu, "New track from selection");
        assert_eq!(new.action, Action::Split(vec![15_000, 20_000]));
        assert!(new.enabled);
        let fit = item(&menu, "Fit track 2 to selection");
        assert!(fit.enabled);
        assert!(item(&menu, "Track 2 starts at selection start").enabled);
        assert!(item(&menu, "Track 2 starts at selection end").enabled);
        assert!(item(&menu, "Track 2 ends at selection start").enabled);
        assert!(item(&menu, "Track 2 ends at selection end").enabled);
        assert!(item(&menu, "Delete selection").enabled);
        assert!(
            items(&menu).iter().all(|i| !i.label.starts_with("Restore")),
            "nothing was deleted"
        );

        let mut e2 = e.clone();
        assert_eq!(apply(&mut e2, &fit.action, RATE), Effect::Edited(true));
        assert_eq!(e2.splits(), [15_000, 20_000]);
        let mut e2 = e.clone();
        let ends = &item(&menu, "Track 2 ends at selection end").action;
        assert_eq!(apply(&mut e2, ends, RATE), Effect::Edited(true));
        assert_eq!(e2.splits(), [10_000, 20_000]);
        let mut e2 = e.clone();
        assert_eq!(apply(&mut e2, &new.action, RATE), Effect::Edited(true));
        assert_eq!(e2.splits(), [10_000, 15_000, 20_000, 30_000]);
        assert!(e2.undo(), "one undo step");
        assert_eq!(e2.splits(), [10_000, 30_000]);
    }

    #[test]
    fn bounds_that_cannot_move_are_greyed_out() {
        let mut e = Editor::new(60_000);
        e.select(15_000, 20_000);
        let s = scene(&e, &[], &[]);
        // One track: its start is 0, its end the end of the recording.
        let menu = waveform(&s, 17_000, 10);
        assert!(!item(&menu, "Fit track 1").enabled);
        assert!(!item(&menu, "Track 1 starts at selection start").enabled);
        assert!(!item(&menu, "Track 1 ends at selection end").enabled);
        assert!(item(&menu, "New track from selection").enabled);
    }

    #[test]
    fn excerpts_of_the_selection_play_its_kept_ends() {
        let mut e = Editor::new(60_000);
        e.select(10_000, 20_000);
        let s = scene(&e, &[], &[]);
        let menu = waveform(&s, 10_000, 10);
        let start = item(&menu, "Play selection start (3 s)");
        let end = item(&menu, "Play selection end (3 s)");
        let mut e2 = e.clone();
        let Effect::Play(tl) = apply(&mut e2, &start.action, RATE) else {
            panic!("plays");
        };
        assert_eq!(tl.segments(), std::slice::from_ref(&(10_000..13_000)));
        let Effect::Play(tl) = apply(&mut e2, &end.action, RATE) else {
            panic!("plays");
        };
        assert_eq!(tl.segments(), std::slice::from_ref(&(17_000..20_000)));

        // All deleted: nothing to play, but restoring is offered.
        e.delete_selection();
        let s = scene(&e, &[], &[]);
        let menu = waveform(&s, 15_000, 10);
        assert!(!item(&menu, "Play selection start").enabled);
        assert!(!item(&menu, "Delete selection").enabled);
        assert!(item(&menu, "Restore selection").enabled);
    }

    #[test]
    fn fine_tuning_moves_the_edges_to_nearby_detected_cuts() {
        let mut e = Editor::new(60_000);
        e.select(10_400, 20_000);
        let pauses = [Region {
            range: 9_000..10_000,
            kind: RegionKind::Silence,
            level_db: -70.0,
        }];
        // 25 s is too far from the end (20 s) to be "nearby".
        let starts = [21_500, 25_000];
        let s = scene(&e, &pauses, &starts);
        let menu = waveform(&s, 15_000, 10);
        let tune = item(&menu, "Fine-tune selection");
        assert_eq!(tune.action, Action::Select(10_000..21_500));
        assert!(
            tune.label.ends_with("(0:10.000–0:21.500)"),
            "{}",
            tune.label
        );
        assert!(tune.enabled);

        let s = scene(&e, &[], &[]);
        let menu = waveform(&s, 15_000, 10);
        let tune = item(&menu, "Fine-tune selection");
        assert!(!tune.enabled);
        assert!(tune.label.ends_with("(none nearby)"));
    }

    #[test]
    fn near_the_playhead_its_commands_come_after_the_selections() {
        let mut e = Editor::new(60_000);
        e.select(10_000, 20_000);
        let s = Scene {
            playhead: Some(15_000),
            ..scene(&e, &[], &[])
        };
        let menu = waveform(&s, 15_002, 10);
        assert_eq!(menu.iter().filter(|e| **e == Entry::Separator).count(), 2);
        let split = item(&menu, "Split at playback position 0:15.000");
        assert_eq!(split.action, Action::Split(vec![15_000]));
        let from = item(&menu, "Selection starts at playback position");
        assert_eq!(from.action, Action::Select(15_000..20_000));
        let to = item(&menu, "Selection ends at playback position");
        assert_eq!(to.action, Action::Select(10_000..15_000));
        let mut e2 = e.clone();
        apply(&mut e2, &to.action, RATE);
        assert_eq!(e2.selection(), Some(10_000..15_000));
        // Far from the playhead: no playhead commands.
        let menu = waveform(&s, 16_000, 10);
        assert!(
            items(&menu)
                .iter()
                .all(|i| !i.label.contains("playback position"))
        );
    }

    #[test]
    fn the_selection_menu_exports_what_is_kept_of_it() {
        let mut e = Editor::new(60_000);
        e.select(10_000, 20_000);
        let menu = waveform(&scene(&e, &[], &[]), 15_000, 10);
        let export = item(&menu, "Export selection as…");
        assert!(export.enabled);
        assert_eq!(export.action, Action::ExportSelection);
        assert_eq!(
            apply(&mut e.clone(), &export.action, RATE),
            Effect::ExportSelection
        );
        // All of it deleted: nothing to export.
        e.delete_selection();
        let menu = waveform(&scene(&e, &[], &[]), 15_000, 10);
        assert!(!item(&menu, "Export selection as…").enabled);
    }

    #[test]
    fn the_track_list_menu_shows_plays_and_moves_to_a_track() {
        let mut e = Editor::new(60_000);
        e.split_at_all([10_000, 30_000]);
        // Row 2 is track 2, 10..30 s.
        let menu = track_list(&e, 1, &[]);
        let zoom = item(&menu, "Zoom to track 2");
        let mut e2 = e.clone();
        assert_eq!(
            apply(&mut e2, &zoom.action, RATE),
            Effect::ZoomTo(10_000..30_000)
        );
        assert_eq!(e2.selection(), Some(10_000..30_000));
        assert_eq!(e2.cursor(), 10_000);

        let start = item(&menu, "Play start of track 2 (3 s)");
        let Effect::Play(tl) = apply(&mut e2, &start.action, RATE) else {
            panic!("plays");
        };
        assert_eq!(tl.segments(), std::slice::from_ref(&(10_000..13_000)));
        let end = item(&menu, "Play end of track 2 (3 s)");
        let Effect::Play(tl) = apply(&mut e2, &end.action, RATE) else {
            panic!("plays");
        };
        assert_eq!(tl.segments(), std::slice::from_ref(&(27_000..30_000)));

        let to_end = item(&menu, "Cursor to end of track 2");
        assert_eq!(to_end.action, Action::JumpTo(30_000));
        assert!(to_end.enabled);
        assert!(item(&menu, "Join track 2 with the previous one").enabled);
        assert!(!item(&track_list(&e, 0, &[]), "Join track 1").enabled);
        assert!(track_list(&e, 3, &[]).is_empty(), "no fourth track");
        // Without a selection there are no selection entries.
        assert!(
            items(&menu)
                .iter()
                .all(|i| !i.label.starts_with("Selection"))
        );
    }

    #[test]
    fn selected_rows_offer_to_merge_their_tracks() {
        let mut e = Editor::new(60_000);
        e.split_at_all([10_000, 30_000, 45_000]);
        let merge = |menu: &[Entry]| {
            items(menu)
                .into_iter()
                .find(|i| i.label.starts_with("Merge"))
                .cloned()
        };
        // One row selected, or none: nothing to merge.
        assert_eq!(merge(&track_list(&e, 1, &[])), None);
        assert_eq!(merge(&track_list(&e, 1, &[1])), None);
        // Rows 2 and 3 selected, right-clicking either of them.
        let m = merge(&track_list(&e, 2, &[1, 2])).expect("offered");
        assert_eq!(m.label, "Merge tracks 2 and 3");
        assert_eq!(m.action, Action::MergeTracks(1..=2));
        assert_eq!(merge(&track_list(&e, 1, &[2, 1])), Some(m.clone()));
        // Right-clicking a row outside the selected ones is about that
        // row only.
        assert_eq!(merge(&track_list(&e, 3, &[1, 2])), None);
        // Rows 1 and 3: track 2 between them is merged too.
        let all = merge(&track_list(&e, 0, &[0, 2])).expect("offered");
        assert_eq!(all.label, "Merge tracks 1 to 3");
        // Separators only between groups.
        let menu = track_list(&e, 1, &[1]);
        assert!(
            !menu
                .windows(2)
                .any(|w| w == [Entry::Separator, Entry::Separator])
        );

        let mut e2 = e.clone();
        assert_eq!(apply(&mut e2, &m.action, RATE), Effect::Edited(true));
        assert_eq!(e2.splits(), &[10_000, 45_000]);
        assert!(e2.undo());
        assert_eq!(e2.splits(), e.splits());
    }

    #[test]
    fn a_selection_across_tracks_offers_to_merge_them() {
        let mut e = Editor::new(60_000);
        e.split_at_all([10_000, 30_000, 45_000]);
        let merge = |e: &Editor| {
            let s = scene(e, &[], &[]);
            let pos = e.selection().map_or(0, |s| s.start);
            items(&waveform(&s, pos, 10))
                .into_iter()
                .find(|i| i.label.starts_with("Merge"))
                .cloned()
        };
        // Inside one track, or ending exactly at the next one's start.
        e.select(12_000, 20_000);
        assert_eq!(merge(&e), None);
        e.select(10_000, 30_000);
        assert_eq!(merge(&e), None);
        // Reaching from track 2 into track 4.
        e.select(25_000, 50_000);
        let m = merge(&e).expect("offered");
        assert_eq!(m.label, "Merge tracks 2 to 4");
        assert_eq!(m.action, Action::MergeTracks(1..=3));
        let mut e2 = e.clone();
        apply(&mut e2, &m.action, RATE);
        assert_eq!(e2.splits(), &[10_000]);
    }

    #[test]
    fn the_track_list_menu_moves_selection_edges_to_track_ends() {
        let mut e = Editor::new(60_000);
        e.split_at_all([10_000, 30_000]);
        e.select(15_000, 40_000);
        let menu = track_list(&e, 1, &[]);
        let a = item(&menu, "Selection start to start of track 2");
        assert_eq!(a.action, Action::Select(10_000..40_000));
        assert!(a.enabled);
        let b = item(&menu, "Selection end to end of track 2");
        assert_eq!(b.action, Action::Select(15_000..30_000));
        let c = item(&menu, "Selection end to start of track 2");
        assert!(!c.enabled, "would select nothing");
        let mut e2 = e.clone();
        apply(&mut e2, &b.action, RATE);
        assert_eq!(e2.selection(), Some(15_000..30_000));
    }

    #[test]
    fn tracks_are_deleted_and_restored_from_the_track_list() {
        let mut e = Editor::new(60_000);
        e.split_at_all([10_000, 30_000]);
        let menu = track_list(&e, 1, &[]);
        assert!(!item(&menu, "Restore track 2").enabled);
        let del = item(&menu, "Delete track 2");
        assert_eq!(apply(&mut e, &del.action, RATE), Effect::Edited(true));
        assert_eq!(e.deleted(), std::slice::from_ref(&(10_000..30_000)));
        let menu = track_list(&e, 1, &[]);
        assert!(!item(&menu, "Delete track 2").enabled);
        assert!(!item(&menu, "Play start of track 2").enabled);
        let restore = item(&menu, "Restore track 2");
        assert!(restore.enabled);
        assert_eq!(apply(&mut e, &restore.action, RATE), Effect::Edited(true));
        assert!(e.deleted().is_empty());
    }
}
