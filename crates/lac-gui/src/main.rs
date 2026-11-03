//! # losslessaudiocut (GUI)
//!
//! The graphical front end: a window that shows the waveform of an audio
//! file and lets the user zoom and scroll through it. Cutting from the GUI
//! comes next; until then `lac cut` does it.
//!
//! This crate is deliberately thin. Everything that can be decided without
//! a display lives in `lac-app` and is tested there:
//!
//! - [`lac_app::peak_job::PeakJob`] decodes the file in a background
//!   thread and publishes the growing waveform summary;
//! - [`lac_app::waveform::WaveformView`] maps pixels to samples, zooms and
//!   scrolls, and says what each pixel column shows;
//! - [`lanes`] (here, also tested) turns that into pixel rows.
//!
//! What remains here is wiring: menus, events, a timer that polls the job,
//! and the paint handler.
//!
//! Usage: `losslessaudiocut [FILE]`.

mod lanes;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use lac_app::detail::{Detail, DetailLoader};
use lac_app::peak_job::{JobStatus, PeakJob};
use lac_app::waveform::{DrawMode, WaveformView};
use lac_peaks::PeakSummary;
use wxdragon::prelude::*;

/// Menu command ids (above wxWidgets' reserved range).
const ID_OPEN: Id = ID_HIGHEST + 1;
const ID_ZOOM_IN: Id = ID_HIGHEST + 2;
const ID_ZOOM_OUT: Id = ID_HIGHEST + 3;
const ID_ZOOM_FIT: Id = ID_HIGHEST + 4;

/// How often the timer looks for new waveform data, in milliseconds.
const POLL_MS: i32 = 50;
/// The job publishes partial summaries at most this often.
const PUBLISH_INTERVAL: Duration = Duration::from_millis(100);
/// Zoom factor of one menu step or one wheel notch.
const ZOOM_STEP: f64 = 1.25;
/// Pixels scrolled per wheel notch.
const SCROLL_STEP: f64 = 40.0;

/// File types offered by the open dialog.
const WILDCARD: &str = "Audio files|*.mp3;*.ogg;*.oga;*.opus;*.spx;*.flac;*.wav|All files|*";

/// Background of the waveform area.
const BACKGROUND: Colour = Colour::rgb(0x1e, 0x22, 0x28);
/// Zero line of each lane.
const ZERO_LINE: Colour = Colour::rgb(0x40, 0x48, 0x52);
/// Min..max lines.
const PEAK: Colour = Colour::rgb(0x4f, 0x9d, 0xde);
/// RMS band drawn over the peaks.
const RMS: Colour = Colour::rgb(0xa8, 0xd4, 0xf5);

/// What the window currently shows.
#[derive(Default)]
struct State {
    /// The file being shown.
    path: Option<PathBuf>,
    /// Builds the summary; dropping it cancels the decoder.
    job: Option<PeakJob>,
    /// Generation of the last update taken from `job`.
    seen: Option<u64>,
    /// The latest (possibly partial) summary.
    peaks: Option<Arc<PeakSummary>>,
    /// Zoom and scroll; exists once the length of the file is known.
    view: Option<WaveformView>,
    /// Width of the drawing area, remembered for views created later.
    width: u32,
    /// Decodes samples for zoomed-in views.
    detail_loader: Option<DetailLoader>,
    /// Generation of the last result taken from `detail_loader`.
    detail_seen: Option<u64>,
    /// The latest decoded samples.
    detail: Option<Arc<Detail>>,
}

impl State {
    /// Starts showing `path`, dropping (and so cancelling) any previous job.
    fn open(&mut self, path: PathBuf) {
        self.job = Some(PeakJob::start(&path, PUBLISH_INTERVAL));
        self.detail_loader = Some(DetailLoader::start(&path));
        self.path = Some(path);
        self.seen = None;
        self.peaks = None;
        self.view = None;
        self.detail_seen = None;
        self.detail = None;
    }

    /// Average packet length: what the summary resolves (see
    /// [`WaveformView::draw_mode`]).
    fn packet_len(&self) -> u64 {
        self.peaks.as_ref().map_or(1, |p| {
            (p.total_samples() / p.packets().max(1) as u64).max(1)
        })
    }

    /// Asks for the samples the view needs and takes new ones. Returns
    /// whether the display must be redrawn.
    fn poll_detail(&mut self) -> bool {
        let packet_len = self.packet_len();
        let wanted = self.view.as_ref().and_then(|v| v.detail_wanted(packet_len));
        let Some(loader) = &mut self.detail_loader else {
            return false;
        };
        if let Some(r) = wanted {
            loader.request(r);
        }
        match loader.poll(self.detail_seen) {
            Some((g, Ok(d))) => {
                self.detail_seen = Some(g);
                self.detail = Some(d);
                true
            }
            // Keep showing the summary; the status line belongs to the
            // file as a whole.
            Some((g, Err(_))) => {
                self.detail_seen = Some(g);
                false
            }
            None => false,
        }
    }

    /// Takes new data from the job. Returns the status line to show if
    /// anything changed.
    fn poll(&mut self) -> Option<String> {
        let u = self.job.as_ref()?.poll(self.seen)?;
        self.seen = Some(u.generation);
        if let Some(p) = u.peaks {
            if self.view.is_none() {
                self.view = Some(WaveformView::new(p.total_samples(), self.width));
            }
            self.peaks = Some(p);
        }
        let name = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Some(match u.status {
            JobStatus::Running => {
                let pct = self.peaks.as_ref().map_or(0, |p| {
                    (p.packets_done() * 100)
                        .checked_div(p.packets())
                        .unwrap_or(100)
                });
                format!("{name}: reading… {pct} %")
            }
            JobStatus::Done(report) if report.warnings.is_empty() => {
                self.job = None;
                name
            }
            JobStatus::Done(report) => {
                self.job = None;
                format!("{name}: {}", report.warnings.join("; "))
            }
            JobStatus::Failed(msg) => {
                self.job = None;
                msg
            }
        })
    }
}

fn main() {
    let _ = wxdragon::main(|_| {
        let frame = Frame::builder()
            .with_title("losslessaudiocut")
            .with_size(Size::new(1000, 400))
            .build();
        frame.create_status_bar(1, 0, -1, "status");
        frame.set_menu_bar(menu_bar());

        let panel = Panel::builder(&frame).build();
        // We paint every pixel ourselves; letting the system erase first
        // would flicker.
        panel.set_background_style(BackgroundStyle::Paint);
        let sizer = BoxSizer::builder(Orientation::Vertical).build();
        sizer.add(&panel, 1, SizerFlag::Expand, 0);
        frame.set_sizer(sizer, true);

        let state = Rc::new(RefCell::new(State {
            width: 1,
            ..State::default()
        }));
        bind_paint(&panel, &state);
        bind_mouse(&panel, &state);
        bind_menu(&frame, &panel, &state);

        {
            let state = Rc::clone(&state);
            panel.on_size(move |e| {
                if let WindowEventData::Size(s) = &e
                    && let Some(size) = s.get_size()
                {
                    let mut st = state.borrow_mut();
                    st.width = u32::try_from(size.width).unwrap_or(1).max(1);
                    let w = st.width;
                    if let Some(v) = &mut st.view {
                        v.set_width(w);
                    }
                }
                e.skip(true);
            });
        }

        // The job runs on its own thread; the timer brings its results to
        // the GUI thread, where all widget calls must happen.
        let timer = Timer::new(&frame);
        {
            let state = Rc::clone(&state);
            timer.on_tick(move |_| {
                let (status, detail) = {
                    let mut st = state.borrow_mut();
                    (st.poll(), st.poll_detail())
                };
                if let Some(s) = &status {
                    frame.set_status_text(s, 0);
                }
                if status.is_some() || detail {
                    panel.refresh(false, None);
                }
            });
        }
        timer.start(POLL_MS, false);
        // The timer must live as long as the window; the closure of
        // `wxdragon::main` returns right away, so leak this one handle.
        std::mem::forget(timer);

        if let Some(path) = std::env::args_os().nth(1) {
            state.borrow_mut().open(PathBuf::from(path));
        }
        frame.show(true);
        frame.centre();
    });
}

/// File and View menus.
fn menu_bar() -> MenuBar {
    let file = Menu::builder()
        .append_item(ID_OPEN, "&Open…\tCtrl+O", "Open an audio file")
        .append_separator()
        .append_item(ID_EXIT, "&Quit\tCtrl+Q", "Quit losslessaudiocut")
        .build();
    let view = Menu::builder()
        .append_item(
            ID_ZOOM_IN,
            "Zoom &in\tCtrl++",
            "Show less time, more detail",
        )
        .append_item(ID_ZOOM_OUT, "Zoom &out\tCtrl+-", "Show more time")
        .append_item(ID_ZOOM_FIT, "Zoom to &fit\tCtrl+0", "Show the whole file")
        .build();
    MenuBar::builder()
        .append(file, "&File")
        .append(view, "&View")
        .build()
}

/// Menu commands.
fn bind_menu(frame: &Frame, panel: &Panel, state: &Rc<RefCell<State>>) {
    let state = Rc::clone(state);
    let (f, panel) = (*frame, *panel);
    frame.on_menu(move |e| {
        let id = e.get_id();
        if id == ID_OPEN {
            let dialog = FileDialog::builder(&f)
                .with_message("Open an audio file")
                .with_wildcard(WILDCARD)
                .with_style(FileDialogStyle::Open | FileDialogStyle::FileMustExist)
                .build();
            if dialog.show_modal() == ID_OK
                && let Some(path) = dialog.get_path()
            {
                state.borrow_mut().open(PathBuf::from(path));
                f.set_status_text("reading…", 0);
            }
        } else if id == ID_EXIT {
            f.close(false);
            return;
        } else {
            let mut st = state.borrow_mut();
            let Some(v) = &mut st.view else { return };
            let centre = f64::from(v.width()) / 2.0;
            match id {
                ID_ZOOM_IN => v.zoom_at(centre, ZOOM_STEP),
                ID_ZOOM_OUT => v.zoom_at(centre, 1.0 / ZOOM_STEP),
                ID_ZOOM_FIT => v.zoom_to_fit(),
                _ => return,
            }
        }
        panel.refresh(false, None);
    });
}

/// Wheel: zoom at the pointer; with Shift (or a horizontal wheel) scroll.
fn bind_mouse(panel: &Panel, state: &Rc<RefCell<State>>) {
    let state = Rc::clone(state);
    let p = *panel;
    panel.on_mouse_wheel(move |e| {
        let ev = match &e {
            WindowEventData::MouseMotion(m) => &m.event.event,
            WindowEventData::MouseButton(m) => &m.event.event,
            WindowEventData::General(g) => g,
            _ => return,
        };
        let delta = ev.get_wheel_delta();
        if delta == 0 {
            return;
        }
        let notches = f64::from(ev.get_wheel_rotation()) / f64::from(delta);
        let x = ev.get_position().map_or(0.0, |pt| f64::from(pt.x));
        let horizontal = matches!(ev.get_wheel_axis(), MouseWheelAxis::Horizontal);
        let mut st = state.borrow_mut();
        let Some(v) = &mut st.view else { return };
        if horizontal || ev.shift_down() {
            v.scroll_by(-notches * SCROLL_STEP);
        } else {
            v.zoom_at(x, ZOOM_STEP.powf(notches));
        }
        p.refresh(false, None);
    });
}

/// Draws the waveform from the view model.
fn bind_paint(panel: &Panel, state: &Rc<RefCell<State>>) {
    let state = Rc::clone(state);
    let p = *panel;
    panel.on_paint(move |_| {
        let dc = AutoBufferedPaintDC::new(&p);
        let size = p.get_client_size();
        dc.set_background(BACKGROUND);
        dc.clear();
        let st = state.borrow();
        let (Some(view), Some(peaks)) = (&st.view, &st.peaks) else {
            return;
        };
        let lanes = lanes::lanes(peaks.channels(), size.height);
        dc.set_pen(ZERO_LINE, 1, PenStyle::Solid);
        for l in &lanes {
            dc.draw_line(0, l.centre(), size.width, l.centre());
        }
        // Zoomed in below packet resolution: draw from decoded samples once
        // they are there, until then from the summary.
        let mode = view.draw_mode(st.packet_len());
        let detail = st.detail.as_deref().filter(|_| mode != DrawMode::MinMax);
        if mode == DrawMode::Polyline
            && let Some(lines) = detail.and_then(|d| view.polyline(d))
        {
            dc.set_pen(PEAK, 1, PenStyle::Solid);
            for (lane, line) in lanes.iter().zip(&lines) {
                let points: Vec<wxdragon::dc::Point> = line
                    .iter()
                    .map(|&(x, v)| wxdragon::dc::Point::new(x.round() as i32, lane.y(v)))
                    .collect();
                dc.draw_lines(&points, 0, 0);
            }
            return;
        }
        let columns = detail
            .and_then(|d| view.detail_columns(d))
            .unwrap_or_else(|| view.columns(peaks));
        for (pass, colour) in [(0, PEAK), (1, RMS)] {
            dc.set_pen(colour, 1, PenStyle::Solid);
            for (x, col) in columns.iter().enumerate() {
                let Some(col) = col else { continue };
                let x = i32::try_from(x).unwrap_or(i32::MAX);
                for (lane, peak) in lanes.iter().zip(col) {
                    let rows = lanes::column_rows(lane, *peak);
                    let (top, bottom) = if pass == 0 { rows.peak } else { rows.rms };
                    // draw_line leaves out the end point, so go one past.
                    dc.draw_line(x, top, x, bottom + 1);
                }
            }
        }
    });
}
