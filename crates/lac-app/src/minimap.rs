//! The minimap's view model: the whole recording squeezed into one strip,
//! with a frame showing which part the waveform window shows. Dragging
//! that frame (or clicking beside it) scrolls there; pressing beside it
//! and dragging zooms the waveform to the dragged range (issue #178).
//!
//! The strip itself is drawn like the main waveform, through a
//! [`WaveformView`] that always shows the whole timeline; the frame is the
//! main view's visible range mapped into that view.

use std::ops::Range;

use lac_core::SamplePos;

use crate::waveform::WaveformView;

/// Narrowest frame drawn, in pixels, so that a deep zoom stays visible.
pub const MIN_FRAME_WIDTH: f64 = 3.0;

/// How far (pixels) the pointer has to move after a press beside the
/// frame before the press becomes a range drag rather than a click, so
/// that a slightly shaky click still just centres the view.
pub const DRAG_THRESHOLD: f64 = 3.0;

/// What the main view should do after a pointer event on the minimap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapAction {
    /// Scroll so that the view starts here (the main view clamps it to
    /// the timeline).
    ScrollTo(SamplePos),
    /// Zoom so that exactly this range fills the window.
    ZoomTo(Range<SamplePos>),
}

/// What a held button is doing.
#[derive(Debug, Clone, PartialEq)]
enum Gesture {
    /// Moving the frame. `grab` is how far after the main view's start the
    /// pointer holds it (frames), so that the frame does not jump under
    /// the pointer.
    Pan { grab: SamplePos },
    /// Pressed beside the frame at pixel `x` (frame `anchor`), not moved
    /// further than [`DRAG_THRESHOLD`] yet: still a click, which centred
    /// the view there. Becomes [`Gesture::Range`] once the pointer moves
    /// further.
    Pressed { x: f64, anchor: SamplePos },
    /// Dragging out a range from `anchor` to zoom to.
    Range { anchor: SamplePos },
}

/// The minimap of a recording.
#[derive(Debug, Clone)]
pub struct Minimap {
    /// Always zoomed to fit.
    view: WaveformView,
    /// While the button is held: what it does.
    gesture: Option<Gesture>,
}

impl Minimap {
    /// The minimap of `total` frames, `width` pixels wide.
    pub fn new(total: SamplePos, width: u32) -> Self {
        Minimap {
            view: WaveformView::new(total, width),
            gesture: None,
        }
    }

    /// The view that draws the strip: the whole timeline.
    pub fn view(&self) -> &WaveformView {
        &self.view
    }

    /// The window was resized.
    pub fn set_width(&mut self, width: u32) {
        self.view.set_width(width);
        self.view.zoom_to_fit();
    }

    /// The frame around `visible` (the main view's visible range) as
    /// pixel columns `left..right`, at least [`MIN_FRAME_WIDTH`] wide.
    pub fn frame(&self, visible: &Range<SamplePos>) -> Range<f64> {
        let (l, r) = (
            self.view.sample_to_x(visible.start),
            self.view.sample_to_x(visible.end),
        );
        let grow = (MIN_FRAME_WIDTH - (r - l)).max(0.0) / 2.0;
        (l - grow)..(r + grow)
    }

    /// Button pressed at pixel `x` while the main view shows `visible`.
    /// Returns where the main view should start now: unchanged when the
    /// frame was grabbed (dragging then moves it), else centred on the
    /// pointer (dragging then zooms to the dragged range, see
    /// [`Minimap::drag`]).
    pub fn press(&mut self, x: f64, visible: &Range<SamplePos>) -> SamplePos {
        let pos = self.view.x_to_sample(x);
        if self.frame(visible).contains(&x) {
            let grab = pos.saturating_sub(visible.start);
            self.gesture = Some(Gesture::Pan { grab });
            visible.start
        } else {
            self.gesture = Some(Gesture::Pressed { x, anchor: pos });
            pos.saturating_sub((visible.end - visible.start) / 2)
        }
    }

    /// The pointer moved to pixel `x`. While the button is held, returns
    /// what the main view should do: follow the grabbed frame, or show the
    /// range dragged out since a press beside the frame. A press beside
    /// the frame that has moved no further than [`DRAG_THRESHOLD`] is
    /// still a click and returns `None`.
    pub fn drag(&mut self, x: f64) -> Option<MapAction> {
        let pos = self.view.x_to_sample(x);
        match *self.gesture.as_ref()? {
            Gesture::Pan { grab } => Some(MapAction::ScrollTo(pos.saturating_sub(grab))),
            Gesture::Pressed { x: x0, anchor } => {
                if (x - x0).abs() <= DRAG_THRESHOLD {
                    return None;
                }
                self.gesture = Some(Gesture::Range { anchor });
                Some(MapAction::ZoomTo(anchor.min(pos)..anchor.max(pos)))
            }
            // The range may be empty when the pointer came back to the
            // anchor; the main view ignores an empty range.
            Gesture::Range { anchor } => Some(MapAction::ZoomTo(anchor.min(pos)..anchor.max(pos))),
        }
    }

    /// Button released.
    pub fn release(&mut self) {
        self.gesture = None;
    }

    /// Whether the button is held.
    pub fn dragging(&self) -> bool {
        self.gesture.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1000 frames over 100 pixels: 10 frames per pixel.
    fn map() -> Minimap {
        Minimap::new(1000, 100)
    }

    #[test]
    fn the_frame_shows_the_visible_part() {
        let m = map();
        assert_eq!(m.frame(&(200..500)), 20.0..50.0);
        assert_eq!(m.frame(&(0..1000)), 0.0..100.0);
        // Two frames are a fifth of a pixel: widened around the middle.
        let f = m.frame(&(500..502));
        assert!((f.end - f.start - MIN_FRAME_WIDTH).abs() < 1e-9);
        assert!(((f.start + f.end) / 2.0 - 50.1).abs() < 1e-9);
    }

    #[test]
    fn grabbing_the_frame_moves_it_with_the_pointer() {
        let mut m = map();
        // Grab the frame 200..500 at x = 30 (frame 300), 100 frames in.
        assert_eq!(m.press(30.0, &(200..500)), 200, "nothing moves yet");
        assert!(m.dragging());
        assert_eq!(m.drag(45.0), Some(MapAction::ScrollTo(350)));
        assert_eq!(
            m.drag(0.0),
            Some(MapAction::ScrollTo(0)),
            "not before the beginning"
        );
        m.release();
        assert_eq!(m.drag(60.0), None);
    }

    #[test]
    fn clicking_beside_the_frame_centres_it_there() {
        let mut m = map();
        assert_eq!(m.press(80.0, &(200..500)), 650);
        // A shaky click is still a click.
        assert_eq!(m.drag(80.0 + DRAG_THRESHOLD), None);
        m.release();
        assert!(!m.dragging());
        assert_eq!(m.press(5.0, &(200..500)), 0, "clamped at the start");
    }

    #[test]
    fn dragging_beside_the_frame_zooms_to_the_dragged_range() {
        let mut m = map();
        m.press(60.0, &(200..500));
        assert_eq!(m.drag(75.0), Some(MapAction::ZoomTo(600..750)));
        // Back across the press point: the range turns round.
        assert_eq!(m.drag(40.0), Some(MapAction::ZoomTo(400..600)));
        // Within the threshold again, it stays a range drag.
        assert_eq!(m.drag(61.0), Some(MapAction::ZoomTo(600..610)));
        assert_eq!(m.drag(60.0), Some(MapAction::ZoomTo(600..600)));
        // Beyond the strip: clamped to the timeline.
        assert_eq!(m.drag(150.0), Some(MapAction::ZoomTo(600..1000)));
        m.release();
        assert_eq!(m.drag(70.0), None);
    }

    #[test]
    fn dragging_the_frame_never_zooms() {
        let mut m = map();
        m.press(30.0, &(200..500));
        assert_eq!(m.drag(90.0), Some(MapAction::ScrollTo(800)));
    }

    #[test]
    fn resizing_keeps_the_whole_recording_in_view() {
        let mut m = map();
        m.set_width(200);
        assert_eq!(m.view().visible(), 0..1000);
        assert_eq!(m.frame(&(200..500)), 40.0..100.0);
    }
}
