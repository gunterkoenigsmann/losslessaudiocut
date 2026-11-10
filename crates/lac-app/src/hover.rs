//! Tooltips for the waveform (issue #93): what the thing under the mouse
//! pointer is. The waveform shows many marks (selection edges, cursor,
//! playhead, track boundaries, suggested track starts and pauses, deleted
//! parts, the time ruler); this module names the one under the pointer,
//! with its time and, where there is one, the command that acts on it.
//!
//! The GUI says which horizontal strip of the window the pointer is in
//! ([`Zone`]) and how far apart in samples two things may be to count as
//! "under the pointer" (a few pixels, converted at the current zoom).

use std::ops::Range;

use lac_analysis::{Region, RegionKind};
use lac_core::SamplePos;

use crate::editor::Editor;
use crate::i18n::{tr, trf};
use crate::time::format_samples;

/// The horizontal strip of the waveform window the pointer is in. The
/// marks of suggestions live in strips of their own (see the GUI's
/// painting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    /// The top strip, where suggested track starts are ticked.
    Top,
    /// The bottom strip of the lanes, where suggested pauses are marked.
    Bottom,
    /// The time ruler under the lanes.
    Ruler,
    /// Anywhere else on the waveform.
    Lanes,
}

/// What the waveform shows, as far as tooltips care.
#[derive(Debug, Clone, Copy)]
pub struct Scene<'a> {
    /// The edits, selection and cursor.
    pub editor: &'a Editor,
    /// Suggested pauses (empty until the analysis is done).
    pub pauses: &'a [Region],
    /// Suggested track starts not split at yet.
    pub suggested_starts: &'a [SamplePos],
    /// Where playback is, if it runs.
    pub playhead: Option<SamplePos>,
    /// Sample rate, for the times shown.
    pub rate: u32,
}

/// The tooltip for the pointer at source position `pos` in `zone`, with
/// marks within `near` samples of it counting as under the pointer.
/// `None` where there is nothing to explain (plain waveform).
///
/// Marks the user can grab come first (track boundaries, which a press
/// drags, then selection edges, playhead, cursor), then suggestions in
/// their strips, then the area the pointer is in (deleted, selected).
pub fn tooltip(scene: &Scene, pos: SamplePos, zone: Zone, near: SamplePos) -> Option<String> {
    let t = |p| format_samples(p, scene.rate);
    let close = |p: SamplePos| p.abs_diff(pos) <= near;
    let ed = scene.editor;
    if zone == Zone::Ruler {
        return Some(trf(
            "Time ruler: {time} under the pointer (View > Time ruler hides it)",
            &[("time", &t(pos))],
        ));
    }
    // First, as a press there drags the boundary (see the GUI).
    if let Some(track) = ed
        .tracks()
        .into_iter()
        .skip(1)
        .find(|k| close(k.span.start))
    {
        let name = track.name.map(|n| format!(" \"{n}\"")).unwrap_or_default();
        return Some(trf(
            "Track {n}{name} starts at {time}; drag to move it",
            &[
                ("n", &track.number),
                ("name", &name),
                ("time", &t(track.span.start)),
            ],
        ));
    }
    if let Some(sel) = ed.selection() {
        if close(sel.start) {
            return Some(trf(
                "Selection start {time}; Shift+click moves the nearer edge",
                &[("time", &t(sel.start))],
            ));
        }
        if close(sel.end) {
            return Some(trf(
                "Selection end {time}; Shift+click moves the nearer edge",
                &[("time", &t(sel.end))],
            ));
        }
    }
    if let Some(p) = scene.playhead.filter(|&p| close(p)) {
        return Some(trf("Playback position {time}", &[("time", &t(p))]));
    }
    if ed.selection().is_none() && close(ed.cursor()) {
        return Some(trf(
            "Cursor {time}: playback starts here (Space)",
            &[("time", &t(ed.cursor()))],
        ));
    }
    if zone == Zone::Top
        && let Some(&s) = scene.suggested_starts.iter().find(|&&s| close(s))
    {
        return Some(trf(
            "Suggested track start {time} (Edit > Split at suggested track starts)",
            &[("time", &t(s))],
        ));
    }
    if zone == Zone::Bottom
        && let Some(r) = scene.pauses.iter().find(|r| r.range.contains(&pos))
    {
        let kind = match r.kind {
            RegionKind::Silence => tr("silence"),
            RegionKind::Noise => tr("noise only"),
        };
        return Some(trf(
            "Suggested pause ({kind}, {level} dBFS) {start}–{end}, {length} long (Edit > Delete suggested pauses)",
            &[
                ("kind", &kind),
                ("level", &format!("{:.0}", r.level_db)),
                ("start", &t(r.range.start)),
                ("end", &t(r.range.end)),
                ("length", &t(r.range.end - r.range.start)),
            ],
        ));
    }
    if let Some(d) = ed.deleted().iter().find(|d| d.contains(&pos)) {
        return Some(trf(
            "Deleted {start}–{end}; not exported (select it and Edit > Restore selection to keep it)",
            &[("start", &t(d.start)), ("end", &t(d.end))],
        ));
    }
    if let Some(sel) = ed.selection().filter(|s| s.contains(&pos)) {
        return Some(selected(&sel, scene.rate));
    }
    None
}

/// The tooltip inside the selection.
fn selected(sel: &Range<SamplePos>, rate: u32) -> String {
    trf(
        "Selection {start}–{end}, {length} long",
        &[
            ("start", &format_samples(sel.start, rate)),
            ("end", &format_samples(sel.end, rate)),
            ("length", &format_samples(sel.end - sel.start, rate)),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn plain_waveform_has_no_tooltip_but_the_ruler_has() {
        let mut e = Editor::new(60_000);
        e.set_cursor(50_000);
        let s = scene(&e, &[], &[]);
        assert_eq!(tooltip(&s, 10_000, Zone::Lanes, 10), None);
        let ruler = tooltip(&s, 10_000, Zone::Ruler, 10).expect("ruler");
        assert!(ruler.contains("0:10.000"), "{ruler}");
    }

    #[test]
    fn marks_near_the_pointer_are_named() {
        let mut e = Editor::new(60_000);
        e.set_cursor(5_000);
        let s = scene(&e, &[], &[]);
        let tip = tooltip(&s, 5_008, Zone::Lanes, 10).expect("cursor");
        assert!(tip.starts_with("Cursor 0:05.000"), "{tip}");
        assert_eq!(tooltip(&s, 5_020, Zone::Lanes, 10), None, "too far");

        e.select(1_000, 3_000);
        let s = scene(&e, &[], &[]);
        let start = tooltip(&s, 1_000, Zone::Lanes, 10).expect("edge");
        assert!(start.starts_with("Selection start 0:01.000"), "{start}");
        let end = tooltip(&s, 2_995, Zone::Lanes, 10).expect("edge");
        assert!(end.starts_with("Selection end 0:03.000"), "{end}");
        let inside = tooltip(&s, 2_000, Zone::Lanes, 10).expect("inside");
        assert_eq!(inside, "Selection 0:01.000–0:03.000, 0:02.000 long");

        let playing = Scene {
            playhead: Some(40_000),
            ..s
        };
        let tip = tooltip(&playing, 40_001, Zone::Lanes, 10).expect("playhead");
        assert_eq!(tip, "Playback position 0:40.000");
    }

    #[test]
    fn track_boundaries_and_deletions_are_explained() {
        let mut e = Editor::new(60_000);
        e.set_cursor(59_000);
        e.split_at(20_000);
        assert!(e.rename_track(1, "Encore"));
        e.select(30_000, 31_000);
        e.delete_selection();
        e.clear_selection();
        let s = scene(&e, &[], &[]);
        let tip = tooltip(&s, 20_005, Zone::Lanes, 10).expect("boundary");
        assert_eq!(
            tip,
            "Track 2 \"Encore\" starts at 0:20.000; drag to move it"
        );
        // A boundary at a selection edge is named as the boundary: a
        // press there drags it.
        e.select(19_000, 20_000);
        let s = scene(&e, &[], &[]);
        let tip = tooltip(&s, 20_000, Zone::Lanes, 10).expect("boundary");
        assert!(tip.starts_with("Track 2"), "{tip}");
        e.clear_selection();
        let s = scene(&e, &[], &[]);
        let tip = tooltip(&s, 30_500, Zone::Lanes, 10).expect("deleted");
        assert!(tip.starts_with("Deleted 0:30.000–0:31.000"), "{tip}");
    }

    #[test]
    fn suggestions_are_explained_only_in_their_strips() {
        let e = Editor::new(60_000);
        let pauses = [Region {
            range: 10_000..12_500,
            kind: RegionKind::Silence,
            level_db: -61.4,
        }];
        let starts = [12_500];
        let s = scene(&e, &pauses, &starts);
        let tip = tooltip(&s, 11_000, Zone::Bottom, 10).expect("pause");
        assert_eq!(
            tip,
            "Suggested pause (silence, -61 dBFS) 0:10.000–0:12.500, 0:02.500 long \
             (Edit > Delete suggested pauses)"
        );
        assert_eq!(tooltip(&s, 11_000, Zone::Lanes, 10), None);
        let tip = tooltip(&s, 12_497, Zone::Top, 10).expect("start");
        assert!(tip.starts_with("Suggested track start 0:12.500"), "{tip}");
        assert_eq!(tooltip(&s, 12_497, Zone::Lanes, 10), None);
    }
}
