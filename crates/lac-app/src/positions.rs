//! The position bar's view model: selection start, end and length, and
//! the play clock, as text; and what typing a time into one of the
//! selection fields does.
//!
//! All times are on the source timeline (the one the waveform shows), so
//! the numbers match what the user sees under the pointer. The status bar
//! keeps showing the position on the edited timeline while playing.

use lac_core::SamplePos;

use crate::editor::Editor;
use crate::time::{TimeError, TimeSpec, format_samples};

/// An editable field of the position bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// Start of the selection (the cursor without a selection).
    Start,
    /// End of the selection (the cursor without a selection).
    End,
    /// Length of the selection.
    Length,
}

/// What the position bar shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Positions {
    /// Start of the selection, or the cursor.
    pub start: String,
    /// End of the selection, or the cursor.
    pub end: String,
    /// Length of the selection; zero without one.
    pub length: String,
    /// The play clock: where playback is, or would start.
    pub clock: String,
}

impl Positions {
    /// The text of `field`.
    pub fn get(&self, field: Field) -> &str {
        match field {
            Field::Start => &self.start,
            Field::End => &self.end,
            Field::Length => &self.length,
        }
    }
}

/// The widest text the play clock is laid out for: [`format_samples`] of
/// 99:59:59.999. The clock uses a fixed-width font, so every label of up to
/// 99 hours fits into the space this text takes; the GUI reserves that
/// space once instead of resizing the bar while playback runs (issue #94).
pub const CLOCK_WIDEST: &str = "00:00:00.000";

/// What the bar shows for `editor` at sample rate `rate`, with playback at
/// source position `playhead` (`None` while stopped). Like the
/// selection, a missing selection shows the cursor as an empty range.
pub fn positions(editor: &Editor, rate: u32, playhead: Option<SamplePos>) -> Positions {
    let sel = editor
        .selection()
        .unwrap_or(editor.cursor()..editor.cursor());
    let fmt = |p| format_samples(p, rate);
    Positions {
        start: fmt(sel.start),
        end: fmt(sel.end),
        length: fmt(sel.end - sel.start),
        clock: fmt(playhead.unwrap_or(editor.play_from())),
    }
}

/// The user typed `text` into `field` and pressed Enter. Changes the
/// selection (times beyond the end are taken as the end):
///
/// - start: the selection starts there and keeps its end; without a
///   selection the cursor moves there;
/// - end: the selection ends there and keeps its start (the cursor
///   without one);
/// - length: the selection keeps its start (or begins at the cursor) and
///   gets that length.
///
/// A start after the end swaps the two, as dragging does. Returns an
/// error to show for text that is no time.
pub fn apply(editor: &mut Editor, field: Field, text: &str, rate: u32) -> Result<(), TimeError> {
    let t = text.parse::<TimeSpec>()?.to_samples(rate);
    let sel = editor.selection();
    match (field, sel) {
        (Field::Start, Some(s)) => editor.select(t, s.end),
        (Field::Start, None) => editor.set_cursor(t),
        (Field::End, Some(s)) => editor.select(s.start, t),
        (Field::End, None) => editor.select(editor.cursor(), t),
        (Field::Length, s) => {
            let from = s.map_or(editor.cursor(), |s| s.start);
            editor.select(from, from.saturating_add(t));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1000 frames per second makes the numbers easy to read.
    const RATE: u32 = 1000;

    fn editor() -> Editor {
        Editor::new(60_000)
    }

    #[test]
    fn without_a_selection_every_field_is_the_cursor() {
        let mut e = editor();
        e.set_cursor(1_500);
        let p = positions(&e, RATE, None);
        assert_eq!(p.start, "0:01.500");
        assert_eq!(p.end, "0:01.500");
        assert_eq!(p.length, "0:00.000");
        assert_eq!(p.clock, "0:01.500");
    }

    #[test]
    fn clock_widest_is_as_long_as_the_longest_label() {
        // The last millisecond before 100 hours, at a common and an odd rate.
        for rate in [44_100, 7] {
            let last = (100 * 3600 * u64::from(rate)) - 1;
            let longest = format_samples(last, rate);
            assert_eq!(
                longest.chars().count(),
                CLOCK_WIDEST.chars().count(),
                "{longest}"
            );
        }
        // Shorter labels, e.g. issue #94's 14:00.375, are no longer.
        assert!(format_samples(840_375, 1000).len() < CLOCK_WIDEST.len());
    }

    #[test]
    fn the_clock_runs_with_playback() {
        let mut e = editor();
        e.select(2_000, 5_000);
        let p = positions(&e, RATE, Some(3_250));
        assert_eq!(
            (p.get(Field::Start), p.get(Field::End), p.get(Field::Length)),
            ("0:02.000", "0:05.000", "0:03.000")
        );
        assert_eq!(p.clock, "0:03.250");
        assert_eq!(positions(&e, RATE, None).clock, "0:02.000", "starts there");
    }

    #[test]
    fn typing_a_start_keeps_the_end() {
        let mut e = editor();
        e.select(2_000, 5_000);
        apply(&mut e, Field::Start, "1.5", RATE).expect("valid");
        assert_eq!(e.selection(), Some(1_500..5_000));
        // Past the end: the two swap.
        apply(&mut e, Field::Start, "0:07", RATE).expect("valid");
        assert_eq!(e.selection(), Some(5_000..7_000));
    }

    #[test]
    fn typing_a_start_without_selection_moves_the_cursor() {
        let mut e = editor();
        apply(&mut e, Field::Start, "@1234", RATE).expect("valid");
        assert_eq!((e.selection(), e.cursor()), (None, 1_234));
    }

    #[test]
    fn typing_an_end_selects_from_the_cursor() {
        let mut e = editor();
        e.set_cursor(1_000);
        apply(&mut e, Field::End, "4", RATE).expect("valid");
        assert_eq!(e.selection(), Some(1_000..4_000));
        apply(&mut e, Field::End, "2", RATE).expect("valid");
        assert_eq!(e.selection(), Some(1_000..2_000));
    }

    #[test]
    fn typing_a_length_keeps_the_start() {
        let mut e = editor();
        e.select(2_000, 5_000);
        apply(&mut e, Field::Length, "0.5", RATE).expect("valid");
        assert_eq!(e.selection(), Some(2_000..2_500));
        e.clear_selection();
        e.set_cursor(10_000);
        apply(&mut e, Field::Length, "1:00", RATE).expect("valid");
        assert_eq!(e.selection(), Some(10_000..60_000), "clamped to the end");
        apply(&mut e, Field::Length, "0", RATE).expect("valid");
        assert_eq!(e.selection(), None, "an empty selection is none");
    }

    #[test]
    fn what_is_shown_can_be_typed_back() {
        let mut e = Editor::new(48_000 * 4000);
        // Whole milliseconds (48 frames), which the fields show exactly.
        e.select(48_000 * 3700 + 48 * 17, 48_000 * 3725 + 48 * 999);
        let before = positions(&e, 48_000, None);
        for f in [Field::Start, Field::End, Field::Length] {
            apply(&mut e, f, before.get(f), 48_000).expect("valid");
        }
        assert_eq!(
            e.selection(),
            Some(48_000 * 3700 + 48 * 17..48_000 * 3725 + 48 * 999)
        );
        assert_eq!(positions(&e, 48_000, None), before);
    }

    #[test]
    fn nonsense_is_an_error_and_changes_nothing() {
        let mut e = editor();
        e.select(2_000, 5_000);
        for text in ["", "abc", "1:99", "-3"] {
            assert!(apply(&mut e, Field::End, text, RATE).is_err(), "{text:?}");
        }
        assert_eq!(e.selection(), Some(2_000..5_000));
    }
}
