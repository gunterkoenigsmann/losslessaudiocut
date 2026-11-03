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
//! - [`lac_app::editor::Editor`] holds the selection, the deleted parts,
//!   the track split points and names, and the undo history;
//! - [`lac_app::export_job::ExportJob`] writes the cut in the background;
//! - [`lac_playback::Player`] plays the kept parts (`lac_app::playback`
//!   connects it to the decoder), and [`lac_playback::scrub::Scrubber`]
//!   decides where it restarts while the user scrubs;
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
mod options;

use std::cell::RefCell;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lac_app::detail::{Detail, DetailLoader};
use lac_app::editor::{Editor, Edits};
use lac_app::export_job::{ExportJob, default_output, describe_outcomes, track_tasks};
use lac_app::format_samples;
use lac_app::peak_cache::PeakCache;
use lac_app::peak_job::{JobStatus, PeakJob};
use lac_app::waveform::{DrawMode, WaveformView};
use lac_core::SamplePos;
use lac_peaks::PeakSummary;
use lac_playback::scrub::Scrubber;
use lac_playback::{Player, Speed, Timeline};
use options::Options;
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
const ID_PLAY: Id = ID_HIGHEST + 13;
const ID_TO_START: Id = ID_HIGHEST + 14;
const ID_SPLIT: Id = ID_HIGHEST + 15;
const ID_JOIN: Id = ID_HIGHEST + 16;
const ID_RENAME: Id = ID_HIGHEST + 17;
const ID_FASTER: Id = ID_HIGHEST + 18;
const ID_SLOWER: Id = ID_HIGHEST + 19;
const ID_NORMAL_SPEED: Id = ID_HIGHEST + 20;
const ID_KEEP_PITCH: Id = ID_HIGHEST + 21;
const ID_SCRUB_SELECTING: Id = ID_HIGHEST + 22;

/// Status bar fields: file and messages, then selection and edits.
const FIELD_FILE: i32 = 0;
const FIELD_EDIT: i32 = 1;
/// Status bar field with the play or cursor position.
const FIELD_TIME: i32 = 2;

/// How often the timer looks for new waveform data, in milliseconds.
const POLL_MS: i32 = 50;
/// The job publishes partial summaries at most this often.
const PUBLISH_INTERVAL: Duration = Duration::from_millis(100);
/// Zoom factor of one menu step or one wheel notch.
const ZOOM_STEP: f64 = 1.25;
/// Pixels scrolled per wheel notch.
const SCROLL_STEP: f64 = 40.0;

/// File types offered by the open dialog.
const WILDCARD: &str =
    "Audio files|*.mp3;*.ogg;*.oga;*.opus;*.spx;*.webm;*.mka;*.mkv;*.flac;*.wav|All files|*";

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
/// The cursor where playback starts.
const CURSOR: Colour = Colour::rgb(0xc8, 0xcc, 0xd0);
/// Track boundaries and track labels.
const TRACK: Colour = Colour::rgb(0xd0, 0x8c, 0xe8);
/// The position being played.
const PLAYHEAD: Colour = Colour::rgb(0x6c, 0xe0, 0x7c);

/// An export in progress.
struct RunningExport {
    job: ExportJob,
    /// The file being cut; another one may be open by the time it ends.
    source: PathBuf,
    /// The edits being exported (see [`Editor::mark_exported`]).
    edits: Edits,
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
    /// Plays the kept parts; opened on the first play, because opening
    /// the audio device can fail and should only bother users who play.
    player: Option<Player>,
    /// The timeline `player` plays: the kept parts when it was last
    /// synchronised with the editor.
    timeline: Timeline,
    /// The scrub in progress while the right button is held.
    scrub: Option<Scrub>,
    /// Playback speed; kept when another file is opened.
    speed: Speed,
    /// Menu options, remembered between runs.
    options: Options,
    /// Whether the current left-button drag has tried to start a scrub
    /// (at most once per drag, so a failing audio device is reported once).
    drag_scrub_tried: bool,
}

/// A scrub: dragging with the right button plays short pieces of audio
/// at the pointer (see [`lac_playback::scrub`]).
struct Scrub {
    /// Decides when to restart playback at the pointer.
    scrubber: Scrubber,
    /// Whether playback was running before; if so it goes on from where
    /// the scrub ended, else it stops there (and with the right button the
    /// cursor goes there).
    was_playing: bool,
    /// Started by dragging a selection with the left button, rather than
    /// with the right button.
    selecting: bool,
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
        self.player = None;
        self.timeline = Timeline::default();
        self.scrub = None;
    }

    /// The source position being played, while playing.
    fn playhead(&self) -> Option<SamplePos> {
        let p = self.player.as_ref().filter(|p| p.engine().is_playing())?;
        Some(self.timeline.to_source(p.engine().position()))
    }

    /// Space: stops if playing, else plays the kept parts from the start of
    /// the selection or the cursor (from the beginning if that is at the
    /// end). Returns a message for the status bar.
    fn toggle_play(&mut self) -> Option<String> {
        if let Some(p) = self.player.as_ref().filter(|p| p.engine().is_playing()) {
            p.engine().pause();
            return None;
        }
        let from = self.editor.as_ref()?.play_from();
        self.play_at(from, true).err()
    }

    /// Plays the kept parts from source position `src`, opening the player
    /// on first use and bringing its timeline up to date with the edits.
    /// A position at (or after) the end of the kept parts plays from the
    /// beginning if `wrap`, else from the last frame.
    fn play_at(&mut self, src: SamplePos, wrap: bool) -> Result<(), String> {
        let (Some(path), Some(editor)) = (&self.path, &self.editor) else {
            return Err("open a file first".to_string());
        };
        let tl = lac_app::playback::timeline(editor);
        if tl.is_empty() {
            return Err("everything is deleted; nothing to play".to_string());
        }
        let start = match tl.from_source(src) {
            t if t >= tl.len() && wrap => 0,
            t => t.min(tl.len() - 1),
        };
        match &mut self.player {
            Some(p) => p.engine_mut().set_timeline(tl.clone(), start),
            None => match lac_app::playback::open_player(path, tl.clone()) {
                Ok(mut p) => {
                    p.engine_mut().set_speed(self.speed);
                    p.engine_mut().seek(start);
                    self.player = Some(p);
                }
                Err(e) => return Err(format!("cannot play: {e}")),
            },
        }
        self.timeline = tl;
        if let Some(p) = &self.player {
            p.engine().play();
        }
        Ok(())
    }

    /// Changes the playback speed, also while playing. Returns a message
    /// for the status bar.
    fn set_speed(&mut self, speed: Speed) -> String {
        self.speed = speed;
        if let Some(p) = &mut self.player {
            p.engine_mut().set_speed(speed);
        }
        let how = match (speed.is_normal(), speed.keep_pitch) {
            (true, _) => "",
            (false, true) => " (pitch kept)",
            (false, false) => " (pitch follows the speed)",
        };
        format!("speed {}{how}", speed.label())
    }

    /// The playback position for a pointer at source position `src`:
    /// within the timeline, so scrubbing over the end plays its last bit.
    fn scrub_target(&self, src: SamplePos) -> SamplePos {
        self.timeline
            .from_source(src)
            .min(self.timeline.len().saturating_sub(1))
    }

    /// Right button down, or a selection being dragged (`selecting`), at
    /// source position `src`: starts playing there and starts following
    /// the pointer. Returns an error message.
    fn begin_scrub(&mut self, src: SamplePos, selecting: bool) -> Option<String> {
        let was_playing = self
            .player
            .as_ref()
            .is_some_and(|p| p.engine().is_playing());
        if let Err(e) = self.play_at(src, false) {
            return Some(e);
        }
        let mut scrubber = Scrubber::default();
        // `play_at` already started there; this only starts the clock.
        scrubber.move_to(self.scrub_target(src), Instant::now());
        self.scrub = Some(Scrub {
            scrubber,
            was_playing,
            selecting,
        });
        None
    }

    /// The pointer moved to source position `src` while scrubbing.
    fn scrub_to(&mut self, src: SamplePos) {
        let target = self.scrub_target(src);
        let (Some(s), Some(p)) = (&mut self.scrub, &mut self.player) else {
            return;
        };
        if let Some(pos) = s.scrubber.move_to(target, Instant::now()) {
            p.engine_mut().seek(pos);
            // Playback stops at the end of the timeline; scrubbing back
            // must start it again.
            p.engine().play();
        }
    }

    /// Timer while scrubbing: catches up with a pointer that moved too
    /// soon after the last restart.
    fn poll_scrub(&mut self) {
        let (Some(s), Some(p)) = (&mut self.scrub, &mut self.player) else {
            return;
        };
        if let Some(pos) = s.scrubber.tick(Instant::now()) {
            p.engine_mut().seek(pos);
            p.engine().play();
        }
    }

    /// Button up: playback goes on if it was running before the scrub;
    /// else it stops, and after a right-button scrub the cursor moves to
    /// where the scrub ended (a selection keeps its cursor).
    fn end_scrub(&mut self) {
        let Some(s) = self.scrub.take() else { return };
        let end = s.scrubber.position();
        let Some(p) = &mut self.player else { return };
        if s.was_playing {
            // Go on from the pointer's last position, if the last move
            // came too soon to be played yet.
            if let Some(pos) = s.scrubber.finish() {
                p.engine_mut().seek(pos);
            }
            return;
        }
        p.engine().pause();
        if s.selecting {
            return;
        }
        if let (Some(pos), Some(ed)) = (end, &mut self.editor) {
            ed.set_cursor(self.timeline.to_source(pos));
        }
    }

    /// After an edit: lets playback continue on the new timeline at the
    /// same source position (or where playback goes on after it, if that
    /// was just deleted).
    fn sync_timeline(&mut self) {
        let (Some(p), Some(editor)) = (&mut self.player, &self.editor) else {
            return;
        };
        let tl = lac_app::playback::timeline(editor);
        if tl == self.timeline {
            return;
        }
        let src = self.timeline.to_source(p.engine().position());
        p.engine_mut().set_timeline(tl.clone(), tl.from_source(src));
        self.timeline = tl;
    }

    /// While playing, a click moves playback to the cursor.
    fn seek_to_cursor(&mut self) {
        let (Some(p), Some(editor)) = (&mut self.player, &self.editor) else {
            return;
        };
        if p.engine().is_playing() && editor.selection().is_none() {
            p.engine_mut()
                .seek(self.timeline.from_source(editor.cursor()));
        }
    }

    /// Timer: stops at the end and keeps the playhead in view. Returns
    /// whether to redraw.
    fn poll_player(&mut self) -> bool {
        self.poll_scrub();
        let Some(p) = &self.player else { return false };
        if !p.engine().is_playing() {
            return false;
        }
        if p.engine().at_end() {
            p.engine().pause();
            return true;
        }
        // While scrubbing the pointer leads; paging would pull the
        // waveform away under it.
        if let (None, Some(pos), Some(v)) = (&self.scrub, self.playhead(), &mut self.view)
            && !v.visible().contains(&pos)
        {
            // Page along, playhead at the left edge.
            v.scroll_to(pos);
        }
        true
    }

    /// The status bar text with the time: the play position on the
    /// edited timeline while playing, else the cursor.
    fn time_status(&self) -> String {
        let Some(rate) = self.rate else {
            return String::new();
        };
        // The speed only when it is not normal.
        let speed = if self.speed.is_normal() {
            String::new()
        } else {
            format!("{} ", self.speed.label())
        };
        match (&self.player, &self.editor) {
            (Some(p), _) if p.engine().is_playing() => format!(
                "▶ {speed}{} / {}",
                format_samples(p.engine().position(), rate),
                format_samples(self.timeline.len(), rate)
            ),
            (_, Some(e)) => format!("{speed}cursor {}", format_samples(e.cursor(), rate)),
            _ => String::new(),
        }
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
        let outcomes = self.export.as_ref()?.job.poll()?;
        let done = self.export.take()?;
        // Only a complete export of the file that is open saves its edits.
        if outcomes.iter().all(|o| o.result.is_ok())
            && self.path.as_deref() == Some(done.source.as_path())
            && let Some(e) = &mut self.editor
        {
            e.mark_exported(done.edits);
        }
        Some(describe_outcomes(&outcomes, self.rate.unwrap_or(1)))
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
        let status = frame.create_status_bar(3, 0, -1, "status");
        status.set_status_widths(&[-4, -4, -2]);
        let options = Options::load();
        frame.set_menu_bar(menu_bar(options));

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
            speed: Speed::new(100, options.keep_pitch),
            options,
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
                let (status, detail, export, playing) = {
                    let mut st = state.borrow_mut();
                    (
                        st.poll(),
                        st.poll_detail(),
                        st.poll_export(),
                        st.poll_player(),
                    )
                };
                if playing {
                    frame.set_status_text(&state.borrow().time_status(), FIELD_TIME);
                }
                if let Some(s) = &status {
                    frame.set_status_text(s, FIELD_FILE);
                }
                if let Some(s) = &export {
                    frame.set_status_text(s, FIELD_FILE);
                }
                if status.is_some() || export.is_some() {
                    update_chrome(&frame, &state.borrow());
                }
                if status.is_some() || detail || playing {
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
fn menu_bar(options: Options) -> MenuBar {
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
            ID_SPLIT,
            "S&plit track here\tCtrl+K",
            "Start a new track at the cursor or the selection start",
        )
        .append_item(
            ID_JOIN,
            "&Join with previous track\tCtrl+J",
            "Remove the split at the start of the track under the cursor",
        )
        .append_item(
            ID_RENAME,
            "Re&name track…\tF2",
            "Name the track under the cursor",
        )
        .append_separator()
        .append_item(
            ID_SELECT_ALL,
            "Select &all\tCtrl+A",
            "Select the whole file",
        )
        .append_item(ID_SELECT_NONE, "Select &none\tEsc", "Clear the selection")
        .build();
    let play = Menu::builder()
        .append_item(
            ID_PLAY,
            "&Play/Stop\tSpace",
            "Play the kept parts from the selection or the cursor, or stop",
        )
        .append_item(
            ID_TO_START,
            "Cursor to &start\tHome",
            "Put the cursor at the beginning",
        )
        .append_separator()
        .append_item(ID_FASTER, "&Faster\tCtrl+Up", "Play faster (up to 3×)")
        .append_item(
            ID_SLOWER,
            "S&lower\tCtrl+Down",
            "Play slower (down to 0.5×)",
        )
        .append_item(
            ID_NORMAL_SPEED,
            "&Normal speed\tCtrl+1",
            "Play at normal speed",
        )
        .append_check_item(
            ID_KEEP_PITCH,
            "&Keep pitch",
            "At other speeds keep the pitch; else it changes like a tape played faster",
        )
        .append_check_item(
            ID_SCRUB_SELECTING,
            "S&crub while selecting",
            "Play the edge of the selection while dragging it",
        )
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
    let bar = MenuBar::builder()
        .append(file, "&File")
        .append(edit, "&Edit")
        .append(play, "&Play")
        .append(view, "&View")
        .build();
    bar.check_item(ID_KEEP_PITCH, options.keep_pitch);
    bar.check_item(ID_SCRUB_SELECTING, options.scrub_while_selecting);
    bar
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
        } else if id == ID_RENAME {
            rename_track(&f, &state);
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
        } else if id == ID_PLAY {
            let msg = state.borrow_mut().toggle_play();
            if let Some(msg) = msg {
                f.set_status_text(&msg, FIELD_FILE);
            }
        } else if matches!(id, ID_FASTER | ID_SLOWER | ID_NORMAL_SPEED | ID_KEEP_PITCH) {
            let msg = {
                let mut st = state.borrow_mut();
                let speed = match id {
                    ID_FASTER => st.speed.faster(),
                    ID_SLOWER => st.speed.slower(),
                    ID_NORMAL_SPEED => Speed::new(100, st.speed.keep_pitch),
                    // The menu has toggled the check mark already.
                    _ => {
                        let keep = e.is_checked().unwrap_or(!st.speed.keep_pitch);
                        st.options.keep_pitch = keep;
                        st.options.save();
                        Speed::new(st.speed.percent(), keep)
                    }
                };
                st.set_speed(speed)
            };
            f.set_status_text(&msg, FIELD_FILE);
        } else if id == ID_SCRUB_SELECTING {
            let mut st = state.borrow_mut();
            let on = e.is_checked().unwrap_or(!st.options.scrub_while_selecting);
            st.options.scrub_while_selecting = on;
            st.options.save();
            return;
        } else if id == ID_TO_START {
            let mut st = state.borrow_mut();
            if let Some(e) = &mut st.editor {
                e.clear_selection();
                e.set_cursor(0);
            }
            st.seek_to_cursor();
            if let Some(v) = &mut st.view {
                v.scroll_to(0);
            }
        } else if is_edit(id) {
            // One borrow: the `if let` scrutinee's would last through the
            // whole block.
            let msg = {
                let mut st = state.borrow_mut();
                let msg = edit(&mut st, id);
                st.sync_timeline();
                msg
            };
            if let Some(msg) = msg.filter(|m| !m.is_empty()) {
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

/// Whether `id` is an Edit menu command.
fn is_edit(id: Id) -> bool {
    matches!(
        id,
        ID_UNDO
            | ID_REDO
            | ID_DELETE
            | ID_RESTORE
            | ID_SELECT_ALL
            | ID_SELECT_NONE
            | ID_SPLIT
            | ID_JOIN
    )
}

/// Runs Edit menu command `id`. Returns `None` if `id` is not one, else a
/// message for the status bar (empty if there is nothing to say).
fn edit(st: &mut State, id: Id) -> Option<&'static str> {
    if !is_edit(id) {
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
        // Tracks are split at, or joined around, where playback would
        // start: the selection start or the cursor.
        ID_SPLIT if !e.split_at(e.play_from()) => "click where the new track should start",
        ID_JOIN if !e.join_with_previous(e.track_at(e.play_from())) => {
            "the first track has no track before it"
        }
        _ => "",
    })
}

/// Title and edit field of the status bar, after anything that may have
/// changed them.
fn update_chrome(frame: &Frame, st: &State) {
    frame.set_title(&st.title());
    frame.set_status_text(&st.edit_status(), FIELD_EDIT);
    frame.set_status_text(&st.time_status(), FIELD_TIME);
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
    let (source, editor) = {
        let st = state.borrow();
        if st.export.is_some() {
            frame.set_status_text("an export is still running", FIELD_FILE);
            return;
        }
        match (&st.path, &st.editor) {
            (Some(p), Some(e)) => (p.clone(), e.clone()),
            _ => {
                frame.set_status_text("open a file first", FIELD_FILE);
                return;
            }
        }
    };
    let file_name = |p: &Path| {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let source_dir = source
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (tasks, skipped) = if editor.splits().is_empty() {
        // One file: ask for its name.
        let dialog = FileDialog::builder(frame)
            .with_message("Export the kept parts")
            .with_default_dir(&source_dir)
            .with_default_file(&file_name(&default_output(&source)))
            .with_style(FileDialogStyle::Save | FileDialogStyle::OverwritePrompt)
            .build();
        if dialog.show_modal() != ID_OK {
            return;
        }
        let Some(output) = dialog.get_path().map(PathBuf::from) else {
            return;
        };
        (
            vec![lac_app::export_job::single_task(&editor, output)],
            Vec::new(),
        )
    } else {
        // One file per track: ask for the folder, the names follow from
        // the track numbers and names.
        let dialog =
            DirDialog::builder(frame, "Export the tracks into this folder", &source_dir).build();
        if dialog.show_modal() != ID_OK {
            return;
        }
        let Some(dir) = dialog.get_path().map(PathBuf::from) else {
            return;
        };
        let (tasks, skipped) = track_tasks(&editor, &source, &dir);
        let existing = tasks.iter().filter(|t| t.output.exists()).count();
        if existing > 0 {
            let ask = MessageDialog::builder(
                frame,
                &format!(
                    "{existing} of the {} track files exist already in {}. Replace them?",
                    tasks.len(),
                    dir.display()
                ),
                "Replace files?",
            )
            .with_style(
                MessageDialogStyle::YesNo
                    | MessageDialogStyle::NoDefault
                    | MessageDialogStyle::IconWarning,
            )
            .build();
            if ask.show_modal() != ID_YES {
                return;
            }
        }
        (tasks, skipped)
    };
    if tasks.is_empty() {
        frame.set_status_text("everything is deleted; nothing to export", FIELD_FILE);
        return;
    }
    let mut msg = match tasks.as_slice() {
        [one] => format!("exporting to {}…", file_name(&one.output)),
        many => format!("exporting {} tracks…", many.len()),
    };
    if !skipped.is_empty() {
        let list: Vec<String> = skipped.iter().map(ToString::to_string).collect();
        msg += &format!(" (track {} deleted entirely, no file)", list.join(", "));
    }
    frame.set_status_text(&msg, FIELD_FILE);
    let job = ExportJob::start_all(&source, tasks);
    state.borrow_mut().export = Some(RunningExport {
        job,
        source,
        edits: editor.edits().clone(),
    });
}

/// Edit > Rename track: asks for the name of the track under the cursor.
fn rename_track(frame: &Frame, state: &Rc<RefCell<State>>) {
    let Some((index, current)) = state.borrow().editor.as_ref().map(|e| {
        let i = e.track_at(e.play_from());
        let name = e.tracks().get(i).and_then(|t| t.name.clone());
        (i, name.unwrap_or_default())
    }) else {
        frame.set_status_text("open a file first", FIELD_FILE);
        return;
    };
    // No borrow while the dialog runs the event loop.
    let dialog = TextEntryDialog::builder(
        frame,
        &format!("Name of track {} (also used in its file name):", index + 1),
        "Rename track",
    )
    .with_default_value(&current)
    .build();
    if dialog.show_modal() != ID_OK {
        return;
    }
    let name = dialog.get_value().unwrap_or_default();
    if let Some(e) = &mut state.borrow_mut().editor {
        e.rename_track(index, &name);
    }
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

/// Left button: drag to select, Shift+click to move the nearer edge, a
/// click puts the cursor (and moves playback there). The view scrolls when
/// the pointer leaves the window while dragging.
///
/// Each handler changes the state inside one mutable borrow and only then
/// touches widgets: on GTK, setting a status text can paint at once, and
/// the paint handler borrows the state too.
fn bind_select(panel: &Panel, frame: &Frame, state: &Rc<RefCell<State>>) {
    let (p, f) = (*panel, *frame);
    // After a change: status bar and waveform.
    let show = move |state: &RefCell<State>| {
        update_chrome(&f, &state.borrow());
        p.refresh(false, None);
    };
    {
        let state = Rc::clone(state);
        panel.on_mouse_left_down(move |e| {
            let Some((ev, x)) = mouse_event(&e) else {
                return;
            };
            {
                let mut guard = state.borrow_mut();
                let st = &mut *guard;
                let (Some(v), Some(ed)) = (&st.view, &mut st.editor) else {
                    return;
                };
                let pos = v.x_to_sample(x);
                st.drag_scrub_tried = false;
                if ev.shift_down() {
                    ed.begin_extend(pos);
                } else {
                    ed.begin_drag(pos);
                }
            }
            p.capture_mouse();
            show(&state);
        });
    }
    {
        let state = Rc::clone(state);
        panel.on_mouse_motion(move |e| {
            let Some((_, x)) = mouse_event(&e) else {
                return;
            };
            let error = {
                let mut guard = state.borrow_mut();
                let st = &mut *guard;
                if st.scrub.as_ref().is_some_and(|s| !s.selecting) {
                    // Right button held: the waveform does not change, the
                    // playhead follows on the next timer tick.
                    if let Some(pos) = scrub_source_at(st, x) {
                        st.scrub_to(pos);
                    }
                    return;
                }
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
                let pos = v.x_to_sample(x.clamp(0.0, width));
                ed.drag_to(pos);
                let moved = ed.selection().is_some();
                // Scrub the dragged edge, once the pointer has moved: a
                // plain click stays silent.
                if st.scrub.is_some() {
                    st.scrub_to(pos);
                    None
                } else if st.options.scrub_while_selecting && moved && !st.drag_scrub_tried {
                    st.drag_scrub_tried = true;
                    st.begin_scrub(pos, true)
                } else {
                    None
                }
            };
            if let Some(msg) = error {
                f.set_status_text(&msg, FIELD_FILE);
            }
            show(&state);
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
            {
                let mut guard = state.borrow_mut();
                let st = &mut *guard;
                let (Some(v), Some(ed)) = (&st.view, &mut st.editor) else {
                    return;
                };
                if ed.dragging() {
                    // A click without movement ends with an empty range,
                    // i.e. no selection.
                    ed.drag_to(v.x_to_sample(x.clamp(0.0, f64::from(v.width()))));
                    ed.end_drag();
                }
                if st.scrub.as_ref().is_some_and(|s| s.selecting) {
                    st.end_scrub();
                }
                st.seek_to_cursor();
            }
            show(&state);
        });
    }
    bind_scrub(panel, frame, state);
}

/// The source position under a scrubbing pointer at `x`, clamped to the
/// window (the mouse is captured, so `x` can lie outside).
fn scrub_source_at(st: &State, x: f64) -> Option<SamplePos> {
    let v = st.view.as_ref()?;
    Some(v.x_to_sample(x.clamp(0.0, f64::from(v.width()))))
}

/// Right button: scrub. While it is held, playback follows the pointer
/// in short pieces (the motion is handled in [`bind_select`]); see
/// [`State::begin_scrub`] and [`State::end_scrub`].
fn bind_scrub(panel: &Panel, frame: &Frame, state: &Rc<RefCell<State>>) {
    let (p, f) = (*panel, *frame);
    {
        let state = Rc::clone(state);
        panel.on_mouse_right_down(move |e| {
            let Some((_, x)) = mouse_event(&e) else {
                return;
            };
            let error = {
                let mut st = state.borrow_mut();
                if st.scrub.is_some() {
                    // Already scrubbing a selection being dragged.
                    return;
                }
                let Some(pos) = scrub_source_at(&st, x) else {
                    return;
                };
                st.begin_scrub(pos, false)
            };
            match error {
                Some(msg) => f.set_status_text(&msg, FIELD_FILE),
                None => p.capture_mouse(),
            }
            update_chrome(&f, &state.borrow());
            p.refresh(false, None);
        });
    }
    {
        let state = Rc::clone(state);
        panel.on_mouse_right_up(move |_| {
            if !state.borrow().scrub.as_ref().is_some_and(|s| !s.selecting) {
                // No right-button scrub (perhaps a selection being dragged,
                // which keeps the mouse).
                return;
            }
            if p.has_capture() {
                p.release_mouse();
            }
            state.borrow_mut().end_scrub();
            update_chrome(&f, &state.borrow());
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
        // Track boundaries (dashed) and labels at the top of each track.
        if let Some(e) = editor.filter(|e| !e.splits().is_empty()) {
            let width = f64::from(view.width());
            dc.set_pen(TRACK, 1, PenStyle::ShortDash);
            dc.set_text_foreground(TRACK);
            dc.set_background_mode(BackgroundMode::Transparent);
            for t in e.tracks() {
                let x0 = view.sample_to_x(t.span.start).round();
                let x1 = view.sample_to_x(t.span.end).round();
                if x1 < 0.0 || x0 >= width {
                    continue;
                }
                if t.span.start > 0 && x0 >= 0.0 {
                    dc.draw_line(x0 as i32, 0, x0 as i32, size.height);
                }
                let label = match &t.name {
                    Some(n) => format!("{} {n}", t.number),
                    None => t.number.to_string(),
                };
                // Keep the label of a track that starts left of the window
                // visible at the left edge.
                dc.draw_text(&label, x0.max(0.0) as i32 + 4, 2);
            }
        }
        // Vertical markers on top: selection edges (or the cursor), then
        // the playhead.
        let mut marks = Vec::new();
        match editor.and_then(Editor::selection) {
            Some(sel) => marks.extend([(sel.start, SELECTION_EDGE), (sel.end, SELECTION_EDGE)]),
            None => marks.extend(editor.map(|e| (e.cursor(), CURSOR))),
        }
        marks.extend(st.playhead().map(|pos| (pos, PLAYHEAD)));
        for (pos, colour) in marks {
            let x = view.sample_to_x(pos).round();
            if (0.0..f64::from(view.width())).contains(&x) {
                dc.set_pen(colour, 1, PenStyle::Solid);
                let x = x as i32;
                dc.draw_line(x, 0, x, size.height);
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
    fn split_and_join_at_the_cursor() {
        let mut st = State {
            editor: Some(Editor::new(1000)),
            ..State::default()
        };
        assert_eq!(
            edit(&mut st, ID_SPLIT),
            Some("click where the new track should start"),
            "not at the very start"
        );
        assert_eq!(
            edit(&mut st, ID_JOIN),
            Some("the first track has no track before it")
        );
        if let Some(e) = &mut st.editor {
            e.set_cursor(400);
        }
        assert_eq!(edit(&mut st, ID_SPLIT), Some(""));
        assert_eq!(
            st.editor.as_ref().map(|e| e.splits().to_vec()),
            Some(vec![400])
        );
        assert_eq!(edit(&mut st, ID_JOIN), Some(""));
        assert_eq!(st.editor.as_ref().map(|e| e.splits().len()), Some(0));
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
