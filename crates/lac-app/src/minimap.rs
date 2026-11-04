//! The minimap's view model: the whole recording squeezed into one strip,
//! with a frame showing which part the waveform window shows, and
//! dragging that frame (or clicking beside it) to scroll there.
//!
//! The strip itself is drawn like the main waveform, through a
//! [`WaveformView`] that always shows the whole timeline; the frame is the
//! main view's visible range mapped into that view.

use std::ops::Range;

use lac_core::SamplePos;

use crate::waveform::WaveformView;

/// Narrowest frame drawn, in pixels, so that a deep zoom stays visible.
pub const MIN_FRAME_WIDTH: f64 = 3.0;

/// The minimap of a recording.
#[derive(Debug, Clone)]
pub struct Minimap {
    /// Always zoomed to fit.
    view: WaveformView,
    /// While the button is held: how far after the main view's start the
    /// pointer grabbed it (frames), so that the frame does not jump under
    /// the pointer.
    grab: Option<SamplePos>,
}

impl Minimap {
    /// The minimap of `total` frames, `width` pixels wide.
    pub fn new(total: SamplePos, width: u32) -> Self {
        Minimap {
            view: WaveformView::new(total, width),
            grab: None,
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
    /// frame was grabbed, else centred on the pointer.
    pub fn press(&mut self, x: f64, visible: &Range<SamplePos>) -> SamplePos {
        let pos = self.view.x_to_sample(x);
        let grab = if self.frame(visible).contains(&x) {
            pos.saturating_sub(visible.start)
        } else {
            (visible.end - visible.start) / 2
        };
        self.grab = Some(grab);
        pos.saturating_sub(grab)
    }

    /// The pointer moved to pixel `x`. While the button is held, returns
    /// where the main view should start (the main view clamps it to the
    /// timeline).
    pub fn drag(&mut self, x: f64) -> Option<SamplePos> {
        let grab = self.grab?;
        Some(self.view.x_to_sample(x).saturating_sub(grab))
    }

    /// Button released.
    pub fn release(&mut self) {
        self.grab = None;
    }

    /// Whether the button is held.
    pub fn dragging(&self) -> bool {
        self.grab.is_some()
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
        assert_eq!(m.drag(45.0), Some(350));
        assert_eq!(m.drag(0.0), Some(0), "not before the beginning");
        m.release();
        assert_eq!(m.drag(60.0), None);
    }

    #[test]
    fn clicking_beside_the_frame_centres_it_there() {
        let mut m = map();
        assert_eq!(m.press(80.0, &(200..500)), 650);
        assert_eq!(m.drag(90.0), Some(750));
        assert_eq!(m.press(5.0, &(200..500)), 0, "clamped at the start");
    }

    #[test]
    fn resizing_keeps_the_whole_recording_in_view() {
        let mut m = map();
        m.set_width(200);
        assert_eq!(m.view().visible(), 0..1000);
        assert_eq!(m.frame(&(200..500)), 40.0..100.0);
    }
}
