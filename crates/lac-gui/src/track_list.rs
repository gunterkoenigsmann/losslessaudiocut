//! The track list under the waveform: one row per track, one column per
//! tag, editable in place. The rows come from
//! [`lac_app::track_list::rows`]; this module only puts them into a
//! `wxGrid` and maps grid columns to tag fields.
//!
//! The grid is refreshed by comparing every cell with the wanted rows and
//! setting only those that differ, so it needs no state of its own and a
//! refresh while nothing changed costs no redraw.

use lac_app::appearance::Appearance;
use lac_app::track_list::{Bound, COLUMNS, Row};
use lac_core::TagField;
use wxdragon::prelude::*;

use crate::tints;

/// Grid columns before the tag columns: start, end and length of the
/// track. Start and end can be typed into (except the recording's own
/// start and end), length is computed.
const FIXED_COLUMNS: i32 = 3;
/// Grid column of the track's start.
const START_COLUMN: i32 = 0;
/// Grid column of the track's end.
const END_COLUMN: i32 = 1;
/// Grid column of the track's length.
const LENGTH_COLUMN: i32 = 2;

/// Text of cells the user changed.
const EDITED: Colour = Colour::rgb(0x10, 0x50, 0xb0);
/// Text of tracks that are deleted entirely (they get no file).
const DELETED: Colour = Colour::rgb(0x90, 0x90, 0x90);
/// Text of everything else.
const NORMAL: Colour = Colour::rgb(0x10, 0x10, 0x10);

/// Creates the (empty) grid with its column labels.
pub fn create(parent: &dyn WxWidget) -> Grid {
    let grid = Grid::builder(parent).build();
    grid.create_grid(
        0,
        FIXED_COLUMNS + COLUMNS.len() as i32,
        GridSelectionMode::Cells,
    );
    grid.set_col_label_value(START_COLUMN, "Start");
    grid.set_col_label_value(END_COLUMN, "End");
    grid.set_col_label_value(LENGTH_COLUMN, "Length");
    for (i, f) in COLUMNS.iter().enumerate() {
        grid.set_col_label_value(FIXED_COLUMNS + i as i32, f.label());
    }
    grid.set_row_label_size(36);
    // Wide text columns, narrow number columns.
    for (i, f) in COLUMNS.iter().enumerate() {
        let w = if f.is_numeric() { 56 } else { 150 };
        grid.set_col_size(FIXED_COLUMNS + i as i32, w);
    }
    grid
}

/// The tag field of grid column `col`, `None` for the fixed columns.
pub fn field(col: i32) -> Option<TagField> {
    usize::try_from(col - FIXED_COLUMNS)
        .ok()
        .and_then(|i| COLUMNS.get(i).copied())
}

/// The track boundary grid column `col` shows, `None` for the others.
pub fn bound(col: i32) -> Option<Bound> {
    match col {
        START_COLUMN => Some(Bound::Start),
        END_COLUMN => Some(Bound::End),
        _ => None,
    }
}

/// Shows `rows`, tinted as `appearance` says. Does nothing while a cell
/// is being edited, so that a refresh from the timer never throws away
/// what the user is typing.
pub fn refresh(grid: &Grid, rows: &[Row], appearance: &Appearance) {
    if grid.is_cell_edit_control_enabled() {
        return;
    }
    let want = i32::try_from(rows.len()).unwrap_or(i32::MAX);
    let have = grid.get_number_rows();
    if have == want && rows_match(grid, rows, appearance) {
        return;
    }
    grid.begin_batch();
    if have < want {
        grid.append_rows(want - have, true);
    } else if have > want {
        grid.delete_rows(want, have - want, true);
    }
    let bold = grid.get_default_cell_font().map(|f| {
        let mut b = f.to_owned();
        b.make_bold();
        b
    });
    let normal = grid.get_default_cell_font();
    for (r, row) in rows.iter().enumerate() {
        let r = r as i32;
        // The recording's own start and end cannot be moved.
        let first = r == 0;
        let last = r + 1 == want;
        set(grid, r, START_COLUMN, &row.start, first);
        set(grid, r, END_COLUMN, &row.end, last);
        set(grid, r, LENGTH_COLUMN, &row.length, true);
        for (c, cell) in row.cells.iter().enumerate() {
            let c = FIXED_COLUMNS + c as i32;
            set(grid, r, c, &cell.text, false);
            let colour = match (row.deleted, cell.edited) {
                (true, _) => DELETED,
                (false, true) => EDITED,
                (false, false) => NORMAL,
            };
            grid.set_cell_text_colour(r, c, &colour);
            if let (Some(b), Some(n)) = (&bold, &normal) {
                grid.set_cell_font(r, c, if cell.edited { b } else { n });
            }
        }
        for c in 0..FIXED_COLUMNS {
            let colour = if row.deleted { DELETED } else { NORMAL };
            grid.set_cell_text_colour(r, c, &colour);
        }
        let tint = row_tint(appearance, r as usize, rows.len());
        for c in 0..grid.get_number_cols() {
            grid.set_cell_background_colour(r, c, &tint);
        }
    }
    grid.end_batch();
    grid.force_refresh();
}

/// Background of row `row` of `rows`: the track's tint, the same hue as
/// behind its part of the waveform (issue #98); plain with one track,
/// which the waveform does not tint either.
fn row_tint(appearance: &Appearance, row: usize, rows: usize) -> Colour {
    if rows > 1 {
        tints::list(appearance, row)
    } else {
        tints::LIST_PLAIN
    }
}

/// Sets one cell's text, read-only or editable.
fn set(grid: &Grid, row: i32, col: i32, text: &str, read_only: bool) {
    grid.set_cell_value(row, col, text);
    grid.set_read_only(row, col, read_only);
}

/// Whether the grid shows `rows` already (texts and edit marks).
fn rows_match(grid: &Grid, rows: &[Row], appearance: &Appearance) -> bool {
    rows.iter().enumerate().all(|(r, row)| {
        let r = r as i32;
        grid.get_cell_value(r, START_COLUMN) == row.start
            && grid.get_cell_value(r, END_COLUMN) == row.end
            && grid.get_cell_value(r, LENGTH_COLUMN) == row.length
            && row.cells.iter().enumerate().all(|(c, cell)| {
                let c = FIXED_COLUMNS + c as i32;
                let colour = match (row.deleted, cell.edited) {
                    (true, _) => DELETED,
                    (false, true) => EDITED,
                    (false, false) => NORMAL,
                };
                grid.get_cell_value(r, c) == cell.text && grid.get_cell_text_colour(r, c) == colour
            })
            // Going from one track to two tints the first row too.
            && grid.get_cell_background_colour(r, 0) == row_tint(appearance, r as usize, rows.len())
    })
}
