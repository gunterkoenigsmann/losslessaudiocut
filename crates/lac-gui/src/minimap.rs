//! The minimap pane: the whole recording in one strip, with deleted parts
//! shaded, track boundaries, the playhead, and a frame around the part the
//! waveform window shows. Dragging the frame, or clicking beside it,
//! scrolls the waveform there ([`lac_app::minimap`] decides how).

use std::ops::Range;

use lac_app::editor::Editor;
use lac_app::minimap::Minimap;
use lac_core::SamplePos;
use lac_peaks::PeakSummary;
use wxdragon::prelude::*;

use crate::lanes;

/// Background of the strip.
const BACKGROUND: Colour = Colour::rgb(0x1a, 0x1d, 0x22);
/// The frame around the visible part, and its tinted inside.
const FRAME: Colour = Colour::rgb(0xe8, 0xc0, 0x50);
const FRAME_INSIDE: Colour = Colour::rgb(0x2c, 0x33, 0x3e);

/// Creates the (empty) minimap panel.
pub fn create(parent: &Frame) -> Panel {
    let panel = Panel::builder(parent).build();
    // Painted completely by `draw`; erasing first would flicker.
    panel.set_background_style(BackgroundStyle::Paint);
    panel
}

/// What the minimap shows.
pub struct Scene<'a> {
    /// The strip's view model.
    pub map: &'a Minimap,
    /// The waveform summary.
    pub peaks: &'a PeakSummary,
    /// Deletions and tracks, once known.
    pub editor: Option<&'a Editor>,
    /// What the waveform window shows.
    pub visible: Range<SamplePos>,
    /// Where playback is, while playing.
    pub playhead: Option<SamplePos>,
}

/// Paints `scene` onto `dc`, `size` being the panel's client size and
/// `line` the pen width of the marker lines (track splits, playhead, the
/// frame) in device units; see `marker_width` in `main.rs`.
pub fn draw(dc: &impl DeviceContext, size: Size, line: i32, scene: &Scene) {
    let view = scene.map.view();
    dc.set_background(BACKGROUND);
    dc.clear();
    let px = |x: f64| x.round() as i32;
    // The visible part first, so the waveform shows on top of it.
    let frame = scene.map.frame(&scene.visible);
    dc.set_pen(FRAME_INSIDE, 1, PenStyle::Solid);
    dc.set_brush(FRAME_INSIDE, BrushStyle::Solid);
    dc.draw_rectangle(
        px(frame.start),
        0,
        px(frame.end - frame.start).max(1),
        size.height,
    );
    if let Some(e) = scene.editor {
        dc.set_pen(crate::DELETED_BACKGROUND, 1, PenStyle::Solid);
        dc.set_brush(crate::DELETED_BACKGROUND, BrushStyle::Solid);
        for r in e.deleted().iter().filter_map(|r| view.pixel_span(r)) {
            let w = i32::try_from(r.end - r.start).unwrap_or(1);
            dc.draw_rectangle(crate::px(r.start), 0, w, size.height);
        }
    }
    let lanes = lanes::lanes(scene.peaks.channels(), size.height);
    let deleted = |x: f64| {
        let pos = view.x_to_sample(x);
        scene.editor.is_some_and(|e| e.is_deleted(pos))
    };
    for (x, col) in view.columns(scene.peaks).iter().enumerate() {
        let Some(col) = col else { continue };
        let colour = if deleted(x as f64) {
            crate::DELETED_PEAK
        } else {
            crate::PEAK
        };
        dc.set_pen(colour, 1, PenStyle::Solid);
        let x = crate::px(x as u32);
        for (lane, peak) in lanes.iter().zip(col) {
            let (top, bottom) = lanes::column_rows(lane, *peak).peak;
            dc.draw_line(x, top, x, bottom + 1);
        }
    }
    if let Some(e) = scene.editor {
        dc.set_pen(crate::TRACK, line, PenStyle::ShortDash);
        for &s in e.splits() {
            let x = px(view.sample_to_x(s));
            dc.draw_line(x, 0, x, size.height);
        }
    }
    if let Some(pos) = scene.playhead {
        dc.set_pen(crate::PLAYHEAD, line, PenStyle::Solid);
        let x = px(view.sample_to_x(pos));
        dc.draw_line(x, 0, x, size.height);
    }
    // The frame's outline last, over everything.
    dc.set_pen(FRAME, line, PenStyle::Solid);
    dc.set_brush(FRAME, BrushStyle::Transparent);
    dc.draw_rectangle(
        px(frame.start),
        0,
        px(frame.end - frame.start).max(1),
        size.height,
    );
}
