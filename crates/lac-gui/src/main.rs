//! # losslessaudiocut (GUI)
//!
//! The graphical front end: a window that shows the waveform of an audio
//! file, lets the user zoom and scroll through it, select parts with the
//! mouse, delete and restore them (with undo), and export the result
//! without re-encoding.
//!
//! This crate is deliberately thin. Everything that can be decided without
//! a display lives in `lac-app` and is tested there:
//!
//! - [`lac_app::peak_job::PeakJob`] decodes the file in a background
//!   thread and publishes the growing waveform summary;
//! - [`lac_app::waveform::WaveformView`] maps pixels to samples, zooms and
//!   scrolls, and says what each pixel column shows;
//! - [`lac_app::editor::Editor`] holds the selection, the deleted parts and
//!   the undo history;
//! - [`lac_app::export_job::ExportJob`] writes the cut in the background;
//! - [`lanes`] (here, also tested) turns that into pixel rows.
//!
//! What remains here is wiring: menus, events, a timer that polls the
//! background jobs, and the paint handler.
//!
//! The waveform always shows the source timeline. Deleted parts stay
//! visible, shaded and dimmed, so they can be selected again and restored.
//!
//! Usage: `losslessaudiocut [FILE]`.

mod lanes;

use std::cell::RefCell;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use lac_app::detail::{Detail, DetailLoader};
use lac_app::editor::{Editor, describe_export};
use lac_app::export_job::{ExportJob, default_output};
use lac_app::peak_cache::PeakCache;
use lac_app::peak_job::{JobStatus, PeakJob};
use lac_app::waveform::{DrawMode, WaveformView};
use lac_core::SamplePos;
use lac_peaks::PeakSummary;
use wxdragon::prelude::*;

/// Menu command ids (above wxWidgets' reserved range).
const ID_OPEN: Id = ID_HIGHEST + 1;
const ID_ZOOM_IN: Id = ID_HIGHEST + 2;
const ID_ZOOM_OUT: Id = ID_HIGHEST + 3;
const ID_ZOOM_FIT: Id = ID_HIGHEST + 4;
const ID_CLEAR_CACHE: Id = ID_HIGHEST + 5;
const ID_EXPORT: Id = ID_HIGHEST + 6;
const ID_UNDO: Id = ID_HIGHEST + 7;
const ID_REDO: Id = ID_HIGHEST + 8;
const ID_DELETE: Id = ID_HIGHEST + 9;
const ID_RESTORE: Id = ID_HIGHEST + 10;
const ID_SELECT_ALL: Id = ID_HIGHEST + 11;
const ID_SELECT_NONE: Id = ID_HIGHEST + 12;

/// Status bar fields: file and messages, then selection and edits.
const FIELD_FILE: i32 = 0;
const FIELD_EDIT: i32 = 1;

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
/// Background of deleted parts.
const DELETED_BACKGROUND: Colour = Colour::rgb(0x3a, 0x1e, 0x22);
/// Min..max lines in deleted parts.
const DELETED_PEAK: Colour = Colour::rgb(0x6a, 0x58, 0x5c);
/// RMS band in deleted parts.
const DELETED_RMS: Colour = Colour::rgb(0x8c, 0x78, 0x7c);
/// Background of the selection.
const SELECTION: Colour = Colour::rgb(0x34, 0x40, 0x52);
/// Left and right edge of the selection.
const SELECTION_EDGE: Colour = Colour::rgb(0xe8, 0xc0, 0x50);

/// An export in progress.
struct RunningExport {
    job: ExportJob,
    /// The file being cut; another one may be open by the time it ends.
    source: PathBuf,
    /// The deleted list that is being exported (see
    /// [`Editor::mark_exported`]).
    deleted: Vec<Range<SamplePos>>,
}

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
    /// Where waveform summaries are kept between runs (`None` if the
    /// system has no per-user cache directory).
    cache: Option<PeakCache>,
    /// Selection and deletions; exists together with `view`.
    editor: Option<Editor>,
    /// Sample rate of the file, for showing times.
    rate: Option<u32>,
    /// The export being written, if any (at most one at a time).
    export: Option<RunningExport>,
}

impl State {
    /// Starts showing `path`, dropping (and so cancelling) any previous job.
    fn open(&mut self, path: PathBuf) {
        self.job = Some(PeakJob::start(&path, PUBLISH_INTERVAL, self.cache.clone()));
        self.detail_loader = Some(DetailLoader::start(&path));
        self.path = Some(path);
        self.seen = None;
        self.peaks = None;
        self.view = None;
        self.detail_seen = None;
        self.detail = None;
        self.editor = None;
        self.rate = None;
    }

    /// File name of the open file, for messages and the title.
    fn name(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Window title; a `*` marks edits that no export contains.
    fn title(&self) -> String {
        if self.path.is_none() {
            return "losslessaudiocut".to_string();
        }
        let star = if self.unexported() { "*" } else { "" };
        format!("{}{star} – losslessaudiocut", self.name())
    }

    /// Whether closing the file would lose edits.
    fn unexported(&self) -> bool {
        self.editor
            .as_ref()
            .is_some_and(Editor::has_unexported_changes)
    }

    /// The status bar text about selection and edits.
    fn edit_status(&self) -> String {
        match (&self.editor, self.rate) {
            (Some(e), Some(rate)) => e.describe(rate),
            _ => String::new(),
        }
    }

    /// Takes the result of a finished export. Returns the message to show.
    fn poll_export(&mut self) -> Option<String> {
        let result = self.export.as_ref()?.job.poll()?;
        let done = self.export.take()?;
        let name = done
            .job
            .output()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Some(match result {
            Ok(kept) => {
                // Only the file that was exported has its edits saved.
                if self.path.as_deref() == Some(done.source.as_path())
                    && let Some(e) = &mut self.editor
                {
                    e.mark_exported(done.deleted);
                }
                let rate = self.rate.unwrap_or(1);
                format!("{name}: {}", describe_export(&kept, rate))
            }
            Err(msg) => format!("export to {name} failed: {msg}"),
        })
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
        if let Some(info) = u.info {
            self.rate = Some(info.sample_rate);
        }
        if let Some(p) = u.peaks {
            if self.view.is_none() {
                self.view = Some(WaveformView::new(p.total_samples(), self.width));
                self.editor = Some(Editor::new(p.total_samples()));
            }
            self.peaks = Some(p);
        }
        let name = self.name();
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
                if u.cached {
                    format!("{name} (waveform from cache)")
                } else {
                    name
                }
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
        let status = frame.create_status_bar(2, 0, -1, "status");
        status.set_status_widths(&[-1, -1]);
        frame.set_menu_bar(menu_bar());

        let panel = Panel::builder(&frame).build();
        // We paint every pixel ourselves; letting the system erase first
        // would flicker.
        panel.set_background_style(BackgroundStyle::Paint);
        let sizer = BoxSizer::builder(Orientation::Vertical).build();
        sizer.add(&panel, 1, SizerFlag::Expand, 0);
        frame.set_sizer(sizer, true);

        let cache = PeakCache::default_dir().map(PeakCache::new);
        if let Some(c) = cache.clone() {
            // Startup eviction (orphans, old and excess entries) off the
            // GUI thread; its result only matters for disk space.
            std::thread::spawn(move || {
                let _ = c.evict();
            });
        }
        let state = Rc::new(RefCell::new(State {
            width: 1,
            cache,
            ..State::default()
        }));
        bind_paint(&panel, &state);
        bind_mouse(&panel, &state);
        bind_select(&panel, &frame, &state);
        bind_menu(&frame, &panel, &state);
        bind_close(&frame, &state);

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
                let (status, detail, export) = {
                    let mut st = state.borrow_mut();
                    (st.poll(), st.poll_detail(), st.poll_export())
                };
                if let Some(s) = &status {
                    frame.set_status_text(s, FIELD_FILE);
                }
                if let Some(s) = &export {
                    frame.set_status_text(s, FIELD_FILE);
                }
                if status.is_some() || export.is_some() {
                    update_chrome(&frame, &state.borrow());
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
            update_chrome(&frame, &state.borrow());
        }
        frame.show(true);
        frame.centre();
    });
}

/// File and View menus.
fn menu_bar() -> MenuBar {
    let file = Menu::builder()
        .append_item(ID_OPEN, "&Open…\tCtrl+O", "Open an audio file")
        .append_item(
            ID_EXPORT,
            "&Export…\tCtrl+E",
            "Write the kept parts to a new file, without re-encoding",
        )
        .append_separator()
        .append_item(
            ID_CLEAR_CACHE,
            "&Clear waveform cache",
            "Delete the stored waveforms; files are decoded again when opened",
        )
        .append_separator()
        .append_item(ID_EXIT, "&Quit\tCtrl+Q", "Quit losslessaudiocut")
        .build();
    let edit = Menu::builder()
        .append_item(ID_UNDO, "&Undo\tCtrl+Z", "Undo the last delete or restore")
        .append_item(ID_REDO, "&Redo\tCtrl+Y", "Redo what was undone")
        .append_separator()
        .append_item(
            ID_DELETE,
            "&Delete selection\tDel",
            "Leave the selection out of the export",
        )
        .append_item(
            ID_RESTORE,
            "Re&store selection\tCtrl+R",
            "Take deleted parts inside the selection back in",
        )
        .append_separator()
        .append_item(
            ID_SELECT_ALL,
            "Select &all\tCtrl+A",
            "Select the whole file",
        )
        .append_item(ID_SELECT_NONE, "Select &none\tEsc", "Clear the selection")
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
        .append(edit, "&Edit")
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
            if !confirm_discard(&f, &state) {
                return;
            }
            let dialog = FileDialog::builder(&f)
                .with_message("Open an audio file")
                .with_wildcard(WILDCARD)
                .with_style(FileDialogStyle::Open | FileDialogStyle::FileMustExist)
                .build();
            if dialog.show_modal() == ID_OK
                && let Some(path) = dialog.get_path()
            {
                state.borrow_mut().open(PathBuf::from(path));
                f.set_status_text("reading…", FIELD_FILE);
            }
        } else if id == ID_EXPORT {
            export(&f, &state);
        } else if id == ID_CLEAR_CACHE {
            let cache = state.borrow().cache.clone();
            let msg = match cache.map(|c| c.clear()) {
                Some(Ok(n)) => format!("waveform cache cleared ({n} entries)"),
                Some(Err(e)) => format!("could not clear the waveform cache: {e}"),
                None => "there is no waveform cache on this system".to_string(),
            };
            f.set_status_text(&msg, FIELD_FILE);
            return;
        } else if id == ID_EXIT {
            f.close(false);
            return;
        } else if let Some(msg) = edit(&mut state.borrow_mut(), id) {
            if !msg.is_empty() {
                f.set_status_text(msg, FIELD_FILE);
            }
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
        update_chrome(&f, &state.borrow());
        panel.refresh(false, None);
    });
}

/// Runs Edit menu command `id`. Returns `None` if `id` is not one, else a
/// message for the status bar (empty if there is nothing to say).
fn edit(st: &mut State, id: Id) -> Option<&'static str> {
    if !matches!(
        id,
        ID_UNDO | ID_REDO | ID_DELETE | ID_RESTORE | ID_SELECT_ALL | ID_SELECT_NONE
    ) {
        return None;
    }
    let Some(e) = &mut st.editor else {
        return Some("open a file first");
    };
    Some(match id {
        ID_UNDO if !e.undo() => "nothing to undo",
        ID_REDO if !e.redo() => "nothing to redo",
        ID_DELETE if e.selection().is_none() => "select a part with the mouse first",
        ID_DELETE if !e.delete_selection() => "the selection is deleted already",
        ID_RESTORE if e.selection().is_none() => "select a deleted part first",
        ID_RESTORE if !e.restore_selection() => "nothing in the selection is deleted",
        ID_SELECT_ALL => {
            e.select_all();
            ""
        }
        ID_SELECT_NONE => {
            e.clear_selection();
            ""
        }
        _ => "",
    })
}

/// Title and edit field of the status bar, after anything that may have
/// changed them.
fn update_chrome(frame: &Frame, st: &State) {
    frame.set_title(&st.title());
    frame.set_status_text(&st.edit_status(), FIELD_EDIT);
}

/// If the open file has edits that no export contains, asks whether to
/// throw them away. Returns whether to go on.
fn confirm_discard(frame: &Frame, state: &Rc<RefCell<State>>) -> bool {
    // Copy what is needed: the dialog runs the event loop, and the timer
    // and paint handlers borrow the state meanwhile.
    let (unexported, name) = {
        let st = state.borrow();
        (st.unexported(), st.name())
    };
    if !unexported {
        return true;
    }
    let dialog = MessageDialog::builder(
        frame,
        &format!("{name} has edits that are not exported yet. Discard them?"),
        "Discard edits?",
    )
    .with_style(
        MessageDialogStyle::YesNo | MessageDialogStyle::NoDefault | MessageDialogStyle::IconWarning,
    )
    .build();
    dialog.show_modal() == ID_YES
}

/// File > Export: asks for the output file and starts writing it.
fn export(frame: &Frame, state: &Rc<RefCell<State>>) {
    let (source, deleted) = {
        let st = state.borrow();
        if st.export.is_some() {
            frame.set_status_text("an export is still running", FIELD_FILE);
            return;
        }
        match (&st.path, &st.editor) {
            (Some(p), Some(e)) => (p.clone(), e.deleted().to_vec()),
            _ => {
                frame.set_status_text("open a file first", FIELD_FILE);
                return;
            }
        }
    };
    let suggestion = default_output(&source);
    let file_name = |p: &Path| {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let dir = suggestion
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dialog = FileDialog::builder(frame)
        .with_message("Export the kept parts")
        .with_default_dir(&dir)
        .with_default_file(&file_name(&suggestion))
        .with_style(FileDialogStyle::Save | FileDialogStyle::OverwritePrompt)
        .build();
    if dialog.show_modal() != ID_OK {
        return;
    }
    let Some(output) = dialog.get_path().map(PathBuf::from) else {
        return;
    };
    frame.set_status_text(&format!("exporting to {}…", file_name(&output)), FIELD_FILE);
    let job = ExportJob::start(
        &source,
        lac_app::CutRequest::Delete(deleted.clone()),
        output,
    );
    state.borrow_mut().export = Some(RunningExport {
        job,
        source,
        deleted,
    });
}

/// Closing the window: offers to keep unexported edits, and lets a running
/// export finish so that it does not leave a truncated file.
fn bind_close(frame: &Frame, state: &Rc<RefCell<State>>) {
    let state = Rc::clone(state);
    let f = *frame;
    frame.on_close(move |e| {
        let ev = match &e {
            WindowEventData::General(g) => Some(g),
            _ => None,
        };
        if !confirm_discard(&f, &state)
            && let Some(ev) = ev.filter(|ev| ev.can_veto())
        {
            ev.veto();
            return;
        }
        let running = state.borrow_mut().export.take();
        if let Some(r) = running {
            f.set_status_text("finishing the export…", FIELD_FILE);
            // Usually seconds; the file would be broken otherwise.
            let _ = r.job.wait();
        }
        e.skip(true);
    });
}

/// The event and pointer x position of a mouse event.
fn mouse_event(e: &WindowEventData) -> Option<(&Event, f64)> {
    let ev = match e {
        WindowEventData::MouseMotion(m) => &m.event.event,
        WindowEventData::MouseButton(m) => &m.event.event,
        WindowEventData::General(g) => g,
        _ => return None,
    };
    let x = ev.get_position().map_or(0.0, |pt| f64::from(pt.x));
    Some((ev, x))
}

/// Left button: drag to select, Shift+click to move the nearer edge. The
/// view scrolls when the pointer leaves the window while dragging.
fn bind_select(panel: &Panel, frame: &Frame, state: &Rc<RefCell<State>>) {
    let (p, f) = (*panel, *frame);
    {
        let state = Rc::clone(state);
        panel.on_mouse_left_down(move |e| {
            let Some((ev, x)) = mouse_event(&e) else {
                return;
            };
            let mut guard = state.borrow_mut();
            let st = &mut *guard;
            let (Some(v), Some(ed)) = (&st.view, &mut st.editor) else {
                return;
            };
            let pos = v.x_to_sample(x);
            if ev.shift_down() {
                ed.begin_extend(pos);
            } else {
                ed.begin_drag(pos);
            }
            p.capture_mouse();
            f.set_status_text(&st.edit_status(), FIELD_EDIT);
            p.refresh(false, None);
        });
    }
    {
        let state = Rc::clone(state);
        panel.on_mouse_motion(move |e| {
            let Some((_, x)) = mouse_event(&e) else {
                return;
            };
            let mut guard = state.borrow_mut();
            let st = &mut *guard;
            let (Some(v), Some(ed)) = (&mut st.view, &mut st.editor) else {
                return;
            };
            if !ed.dragging() {
                return;
            }
            let width = f64::from(v.width());
            if x < 0.0 {
                v.scroll_by(x);
            } else if x > width {
                v.scroll_by(x - width);
            }
            ed.drag_to(v.x_to_sample(x.clamp(0.0, width)));
            f.set_status_text(&st.edit_status(), FIELD_EDIT);
            p.refresh(false, None);
        });
    }
    {
        let state = Rc::clone(state);
        panel.on_mouse_left_up(move |e| {
            let Some((_, x)) = mouse_event(&e) else {
                return;
            };
            if p.has_capture() {
                p.release_mouse();
            }
            let mut guard = state.borrow_mut();
            let st = &mut *guard;
            let (Some(v), Some(ed)) = (&st.view, &mut st.editor) else {
                return;
            };
            if ed.dragging() {
                // A click without movement ends with an empty range, i.e.
                // no selection.
                ed.drag_to(v.x_to_sample(x.clamp(0.0, f64::from(v.width()))));
                ed.end_drag();
            }
            f.set_status_text(&st.edit_status(), FIELD_EDIT);
            p.refresh(false, None);
        });
    }
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
        let editor = st.editor.as_ref();
        // Backgrounds first, the waveform goes on top.
        let spans = |ranges: &mut dyn Iterator<Item = Range<SamplePos>>| -> Vec<Range<u32>> {
            ranges.filter_map(|r| view.pixel_span(&r)).collect()
        };
        let deleted = editor.map_or_else(Vec::new, |e| spans(&mut e.deleted().iter().cloned()));
        let selected = editor.map_or_else(Vec::new, |e| spans(&mut e.selection().into_iter()));
        for (colour, list) in [(DELETED_BACKGROUND, &deleted), (SELECTION, &selected)] {
            dc.set_pen(colour, 1, PenStyle::Solid);
            dc.set_brush(colour, BrushStyle::Solid);
            for r in list {
                dc.draw_rectangle(px(r.start), 0, px(r.end - r.start), size.height);
            }
        }
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
        } else {
            let columns = detail
                .and_then(|d| view.detail_columns(d))
                .unwrap_or_else(|| view.columns(peaks));
            // Deleted columns are drawn dimmed, in a pass of their own so
            // the pen changes only four times.
            let is_deleted = |x: usize| deleted.iter().any(|r| r.contains(&(x as u32)));
            for (pass, colour, dim) in [
                (0, PEAK, false),
                (1, RMS, false),
                (0, DELETED_PEAK, true),
                (1, DELETED_RMS, true),
            ] {
                dc.set_pen(colour, 1, PenStyle::Solid);
                for (x, col) in columns.iter().enumerate() {
                    let Some(col) = col else { continue };
                    if is_deleted(x) != dim {
                        continue;
                    }
                    let x = px(x as u32);
                    for (lane, peak) in lanes.iter().zip(col) {
                        let rows = lanes::column_rows(lane, *peak);
                        let (top, bottom) = if pass == 0 { rows.peak } else { rows.rms };
                        // draw_line leaves out the end point, so go one past.
                        dc.draw_line(x, top, x, bottom + 1);
                    }
                }
            }
        }
        // Selection edges on top, where they lie in view.
        if let Some(sel) = editor.and_then(Editor::selection) {
            dc.set_pen(SELECTION_EDGE, 1, PenStyle::Solid);
            for pos in [sel.start, sel.end] {
                let x = view.sample_to_x(pos).round();
                if (0.0..f64::from(view.width())).contains(&x) {
                    let x = x as i32;
                    dc.draw_line(x, 0, x, size.height);
                }
            }
        }
    });
}

/// A pixel coordinate for the device context.
fn px(x: u32) -> i32 {
    i32::try_from(x).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_commands_explain_why_nothing_happened() {
        let mut st = State::default();
        assert_eq!(edit(&mut st, ID_UNDO), Some("open a file first"));
        assert_eq!(edit(&mut st, ID_ZOOM_IN), None, "not an edit command");
        st.editor = Some(Editor::new(1000));
        assert_eq!(edit(&mut st, ID_UNDO), Some("nothing to undo"));
        assert_eq!(
            edit(&mut st, ID_DELETE),
            Some("select a part with the mouse first")
        );
        assert_eq!(edit(&mut st, ID_SELECT_ALL), Some(""));
        assert_eq!(
            edit(&mut st, ID_RESTORE),
            Some("nothing in the selection is deleted")
        );
        assert_eq!(edit(&mut st, ID_DELETE), Some(""));
        assert_eq!(
            edit(&mut st, ID_DELETE),
            Some("the selection is deleted already")
        );
        assert!(st.unexported());
        assert_eq!(edit(&mut st, ID_UNDO), Some(""));
        assert!(!st.unexported());
        assert_eq!(edit(&mut st, ID_SELECT_NONE), Some(""));
        assert_eq!(st.editor.as_ref().and_then(Editor::selection), None);
    }

    #[test]
    fn title_marks_unexported_edits() {
        let mut st = State {
            path: Some(PathBuf::from("/rec/talk.mp3")),
            editor: Some(Editor::new(1000)),
            ..State::default()
        };
        assert_eq!(st.title(), "talk.mp3 – losslessaudiocut");
        edit(&mut st, ID_SELECT_ALL);
        edit(&mut st, ID_DELETE);
        assert_eq!(st.title(), "talk.mp3* – losslessaudiocut");
        assert_eq!(State::default().title(), "losslessaudiocut");
    }
}
