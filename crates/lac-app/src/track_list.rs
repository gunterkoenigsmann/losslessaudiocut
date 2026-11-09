//! The track list's view model: one row per track with its tags, as the
//! export will write them, and the editing of a cell.
//!
//! A cell shows the value the track's file will get: the user's change if
//! there is one, else the source's value; with several tracks, track
//! number and total are counted among the files written unless the user
//! set them (see [`crate::export_job::track_numbers`]). Cells the user changed are
//! marked, so the list can show them differently.
//!
//! Typing a value equal to what the cell would show without a change
//! (the source's value, or the counted number) removes the change, so the
//! track follows the source and the counting again.

use lac_core::{SamplePos, TagField, TagValueError, Tags};

use crate::editor::Editor;
use crate::export_job::track_numbers;
use crate::format_samples;

/// The tag columns, in display order.
pub const COLUMNS: [TagField; 9] = TagField::ALL;

/// One cell of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// The value the track's file will get (empty: none).
    pub text: String,
    /// Whether the user changed it (for this track or for all tracks).
    pub edited: bool,
}

/// One row: a track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Where the track starts in the source, e.g. `1:02.500`.
    pub start: String,
    /// How long its file will be.
    pub length: String,
    /// Whether it is deleted entirely and so gets no file.
    pub deleted: bool,
    /// One cell per entry of [`COLUMNS`].
    pub cells: Vec<Cell>,
}

/// The rows for `editor`'s tracks, with `source` as the source's tags and
/// `rate` its sample rate.
pub fn rows(editor: &Editor, source: &Tags, rate: u32) -> Vec<Row> {
    // One track is exported as one file that keeps the source's numbers
    // (see `export_job::single_task`), so nothing is counted then.
    let numbers = if editor.splits().is_empty() {
        vec![None]
    } else {
        track_numbers(editor)
    };
    editor
        .tracks()
        .into_iter()
        .zip(numbers)
        .map(|(t, n)| Row {
            start: format_samples(t.span.start, rate),
            length: format_samples(t.kept_len(), rate),
            deleted: t.kept.is_empty(),
            cells: COLUMNS
                .iter()
                .map(|&f| {
                    let counted = match f {
                        TagField::TrackNumber => n.as_ref().map(|n| n.0.clone()),
                        TagField::TrackTotal => n.as_ref().map(|n| n.1.clone()),
                        _ => None,
                    };
                    let edited = t.tags.get(f);
                    let text = match (counted, edited) {
                        (Some(c), _) => c,
                        (None, Some(v)) => v.to_string(),
                        (None, None) => source.get(f).unwrap_or_default(),
                    };
                    Cell {
                        text,
                        edited: edited.is_some(),
                    }
                })
                .collect(),
        })
        .collect()
}

/// What the cell of track `index` and column `field` shows without the
/// track's own change: the all-tracks value, the source's value or the
/// counted number.
fn unchanged_value(editor: &Editor, source: &Tags, index: usize, field: TagField) -> String {
    let mut e = editor.clone();
    e.reset_track_tag(index, field);
    let col = COLUMNS.iter().position(|&f| f == field);
    // The rate only affects the times, which are not needed here.
    rows(&e, source, 1)
        .into_iter()
        .nth(index)
        .zip(col)
        .map(|(r, c)| r.cells[c].text.clone())
        .unwrap_or_default()
}

/// The user typed `text` into the cell of track `index` (0-based) and
/// column `field`; with `all_tracks` it applies to every track. Returns
/// whether anything changed, or why the value is not accepted.
pub fn edit(
    editor: &mut Editor,
    source: &Tags,
    index: usize,
    field: TagField,
    text: &str,
    all_tracks: bool,
) -> Result<bool, TagValueError> {
    let text = text.trim();
    // Reject before comparing, so "x" is an error even where it would
    // equal nothing.
    Tags::default().set(field, text)?;
    if all_tracks {
        return editor.set_all_tracks_tag(field, text);
    }
    if text == unchanged_value(editor, source, index, field) {
        return Ok(editor.reset_track_tag(index, field));
    }
    editor.set_track_tag(index, field, text)
}

/// Where track `index` starts in the source, to move the cursor there
/// when its row is selected.
pub fn track_start(editor: &Editor, index: usize) -> Option<SamplePos> {
    editor.tracks().get(index).map(|t| t.span.start)
}

/// The source span of track `index`, to select it and zoom to it when
/// its row is double-clicked.
pub fn track_span(editor: &Editor, index: usize) -> Option<std::ops::Range<SamplePos>> {
    editor.tracks().get(index).map(|t| t.span.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> Tags {
        Tags {
            title: Some("Concert".into()),
            artist: Some("Band".into()),
            ..Tags::default()
        }
    }

    fn texts(e: &Editor, field: TagField) -> Vec<String> {
        let col = COLUMNS.iter().position(|&f| f == field).expect("column");
        rows(e, &source(), 1000)
            .into_iter()
            .map(|r| r.cells[col].text.clone())
            .collect()
    }

    fn edited(e: &Editor, field: TagField) -> Vec<bool> {
        let col = COLUMNS.iter().position(|&f| f == field).expect("column");
        rows(e, &source(), 1000)
            .into_iter()
            .map(|r| r.cells[col].edited)
            .collect()
    }

    #[test]
    fn cells_show_what_the_files_will_get() {
        let mut e = Editor::new(3000);
        e.split_at(1000);
        e.split_at(2000);
        e.select(1000, 2000);
        e.delete_selection();
        let r = rows(&e, &source(), 1000);
        assert_eq!(r.len(), 3);
        assert_eq!((r[1].start.as_str(), r[1].deleted), ("0:01.000", true));
        assert_eq!(r[0].length, "0:01.000");
        // Counted among the files written; no number for the deleted one.
        assert_eq!(texts(&e, TagField::TrackNumber), ["1", "", "2"]);
        assert_eq!(texts(&e, TagField::TrackTotal), ["2", "", "2"]);
        assert_eq!(texts(&e, TagField::Title), ["Concert"; 3]);
        assert_eq!(edited(&e, TagField::Title), [false; 3]);
    }

    #[test]
    fn editing_a_cell_and_typing_the_old_value_back() {
        let s = source();
        let mut e = Editor::new(3000);
        e.split_at(1500);
        assert_eq!(
            edit(&mut e, &s, 1, TagField::Title, " Encore ", false),
            Ok(true)
        );
        assert_eq!(texts(&e, TagField::Title), ["Concert", "Encore"]);
        assert_eq!(edited(&e, TagField::Title), [false, true]);
        assert_eq!(e.tracks()[1].name.as_deref(), Some("Encore"));
        // The source's value again: the change is gone.
        assert_eq!(
            edit(&mut e, &s, 1, TagField::Title, "Concert", false),
            Ok(true)
        );
        assert_eq!(edited(&e, TagField::Title), [false, false]);
        assert!(e.edits().track_tags.is_empty());
        // Clearing is a change.
        assert_eq!(edit(&mut e, &s, 0, TagField::Artist, "", false), Ok(true));
        assert_eq!(texts(&e, TagField::Artist), ["", "Band"]);
        // Numbers: the counted value is no change, others are.
        assert_eq!(
            edit(&mut e, &s, 0, TagField::TrackNumber, "1", false),
            Ok(false)
        );
        assert_eq!(
            edit(&mut e, &s, 0, TagField::TrackNumber, "7", false),
            Ok(true)
        );
        assert_eq!(texts(&e, TagField::TrackNumber), ["7", "2"]);
        assert!(edit(&mut e, &s, 0, TagField::TrackNumber, "seven", false).is_err());
    }

    #[test]
    fn editing_for_all_tracks() {
        let s = source();
        let mut e = Editor::new(3000);
        e.split_at(1500);
        edit(&mut e, &s, 1, TagField::Album, "Own", false).expect("valid");
        assert_eq!(edit(&mut e, &s, 0, TagField::Album, "Live", true), Ok(true));
        assert_eq!(texts(&e, TagField::Album), ["Live", "Live"]);
        assert_eq!(edited(&e, TagField::Album), [true, true]);
        // A track can still differ, even back to the source's value.
        assert_eq!(edit(&mut e, &s, 1, TagField::Album, "", false), Ok(true));
        assert_eq!(texts(&e, TagField::Album), ["Live", ""]);
        assert_eq!(track_start(&e, 1), Some(1500));
    }

    #[test]
    fn a_single_track_shows_the_source_numbers() {
        let s = Tags {
            track_number: Some(4),
            ..source()
        };
        let e = Editor::new(3000);
        let r = rows(&e, &s, 1000);
        let col = |f| COLUMNS.iter().position(|&c| c == f).expect("column");
        assert_eq!(r[0].cells[col(TagField::TrackNumber)].text, "4");
        assert_eq!(r[0].cells[col(TagField::TrackTotal)].text, "");
        assert_eq!(track_start(&e, 2), None);
    }

    #[test]
    fn track_spans_cover_the_source_from_split_to_split() {
        let mut e = Editor::new(3000);
        e.split_at(1000);
        e.split_at(2500);
        assert_eq!(track_span(&e, 0), Some(0..1000));
        assert_eq!(track_span(&e, 1), Some(1000..2500));
        assert_eq!(track_span(&e, 2), Some(2500..3000));
        assert_eq!(track_span(&e, 3), None);
    }
}
