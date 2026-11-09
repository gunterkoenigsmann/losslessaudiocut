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
//! - [`lanes`] (here, also tested) turns that into pixel rows;
//! - [`lac_app::track_list`] says what the track list under the waveform
//!   shows and applies the user's tag edits ([`track_list`] here puts it
//!   into a grid).
//!
//! What remains here is wiring: menus, events, a timer that polls the
//! background jobs, and the paint handler.
//!
//! The waveform always shows the source timeline. Deleted parts stay
//! visible, shaded and dimmed, so they can be selected again and restored.
//!
//! Usage: `losslessaudiocut [FILE]`.

mod icons;
mod lanes;
mod log_pane;
mod logo;
mod minimap;
mod options;
mod panes;
mod position_bar;
mod recent;
mod toolbar;
mod track_list;

use std::cell::RefCell;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use icons::Icon;
use lac_analysis::{DetectParams, Detection, RegionKind};
use lac_app::analysis_job::{AnalysisJob, AnalysisStatus};
use lac_app::chapters::{Chapter, SuggestedStart, suggested_starts};
use lac_app::context_menu::{self, Effect, Entry, RightPress};
use lac_app::detail::{Detail, DetailLoader};
use lac_app::editor::{Editor, Edits};
use lac_app::export_job::{ExportJob, default_output, describe_outcomes, track_tasks};
use lac_app::format_samples;
use lac_app::hover::{self, Zone};
use lac_app::launch::GuiArgs;
use lac_app::layout::Pane;
use lac_app::minimap::Minimap;
use lac_app::peak_cache::PeakCache;
use lac_app::peak_job::{JobStatus, PeakJob};
use lac_app::playback::End;
use lac_app::recent_files::RecentFiles;
use lac_app::track_list::Bound;
use lac_app::waveform::{DrawMode, WaveformView};
use lac_core::{SamplePos, Tags};
use lac_peaks::PeakSummary;
use lac_playback::scrub::Scrubber;
use lac_playback::{Player, Speed, Timeline};
use options::Options;
use panes::Panes;
use position_bar::PositionBar;
use toolbar::{Tool, Toolbar};
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
const ID_ALL_TRACKS: Id = ID_HIGHEST + 23;
const ID_RULER: Id = ID_HIGHEST + 24;
const ID_SUGGESTED_SPLITS: Id = ID_HIGHEST + 25;
const ID_SUGGESTED_PAUSES: Id = ID_HIGHEST + 26;
const ID_CHAINED_LINKS: Id = ID_HIGHEST + 27;
const ID_PLAY_TRACK_START: Id = ID_HIGHEST + 28;
const ID_PLAY_TRACK_END: Id = ID_HIGHEST + 29;
/// The waveform's right-click menu: entry `i` has id `ID_CONTEXT_FIRST + i`
/// (above the panes' ids, `ID_HIGHEST + 100` on).
const ID_CONTEXT_FIRST: Id = ID_HIGHEST + 200;
/// More entries than the right-click menu ever has.
const CONTEXT_MAX: usize = 50;

/// The toolbar: the most used menu commands. Tooltips name the shortcut,
/// which is the faster way once known.
const TOOLS: &[Tool] = &[
    Tool::Button {
        id: ID_OPEN,
        icon: Icon::Open,
        tip: "Open an audio file (Ctrl+O)",
    },
    Tool::Button {
        id: ID_EXPORT,
        icon: Icon::Export,
        tip: "Export the kept parts (Ctrl+E)",
    },
    Tool::Gap,
    Tool::Button {
        id: ID_UNDO,
        icon: Icon::Undo,
        tip: "Undo (Ctrl+Z)",
    },
    Tool::Button {
        id: ID_REDO,
        icon: Icon::Redo,
        tip: "Redo (Ctrl+Y)",
    },
    Tool::Gap,
    Tool::Button {
        id: ID_DELETE,
        icon: Icon::Delete,
        tip: "Delete the selection (Del)",
    },
    Tool::Button {
        id: ID_RESTORE,
        icon: Icon::Restore,
        tip: "Restore deleted parts in the selection (Ctrl+R)",
    },
    Tool::Button {
        id: ID_SPLIT,
        icon: Icon::Split,
        tip: "Start a new track here (Ctrl+K)",
    },
    Tool::Gap,
    Tool::Button {
        id: ID_TO_START,
        icon: Icon::ToStart,
        tip: "Cursor to the start (Home)",
    },
    Tool::Button {
        id: ID_PLAY,
        icon: Icon::Play,
        tip: "Play or stop (Space)",
    },
    Tool::Button {
        id: ID_PLAY_TRACK_START,
        icon: Icon::PlayTrackStart,
        tip: "Play the first 3 seconds of the track (Ctrl+Shift+B)",
    },
    Tool::Button {
        id: ID_PLAY_TRACK_END,
        icon: Icon::PlayTrackEnd,
        tip: "Play the last 3 seconds of the track (Ctrl+Shift+E)",
    },
    Tool::Speed {
        tip: "Playback speed (Ctrl+Up, Ctrl+Down; Ctrl+1 for normal)",
    },
    Tool::Gap,
    Tool::Button {
        id: ID_ZOOM_IN,
        icon: Icon::ZoomIn,
        tip: "Zoom in (Ctrl++)",
    },
    Tool::Button {
        id: ID_ZOOM_OUT,
        icon: Icon::ZoomOut,
        tip: "Zoom out (Ctrl+-)",
    },
    Tool::Button {
        id: ID_ZOOM_FIT,
        icon: Icon::ZoomFit,
        tip: "Show the whole file (Ctrl+0)",
    },
];

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
/// Background of the time ruler.
const RULER_BACKGROUND: Colour = Colour::rgb(0x2a, 0x2f, 0x36);
/// Tick marks of the time ruler.
const RULER_TICK: Colour = Colour::rgb(0x8a, 0x94, 0x9e);
/// Labels of the time ruler.
const RULER_TEXT: Colour = Colour::rgb(0xc0, 0xc6, 0xcc);
/// Length of the ruler's labelled (major) and other (minor) ticks.
const RULER_MAJOR: i32 = 7;
const RULER_MINOR: i32 = 3;
/// Height of the ruler beyond its text.
const RULER_PADDING: i32 = 7;
/// The position being played.
const PLAYHEAD: Colour = Colour::rgb(0x6c, 0xe0, 0x7c);
/// Strip under a suggested pause that is silent.
const SUGGESTED_SILENCE: Colour = Colour::rgb(0x5c, 0x9c, 0x8c);
/// Strip under a suggested pause that is only noise.
const SUGGESTED_NOISE: Colour = Colour::rgb(0xc0, 0x90, 0x50);
/// Tick at a suggested track start (a dimmer [`TRACK`]).
const SUGGESTED_SPLIT: Colour = Colour::rgb(0x90, 0x60, 0xa8);
/// Height of the suggestion strip and ticks, in pixels.
const SUGGESTION_MARK: i32 = 6;

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
    /// Looks for pauses and track starts; dropping it cancels.
    analysis: Option<AnalysisJob>,
    /// Generation of the last status taken from `analysis`.
    analysis_seen: Option<u64>,
    /// How far the analysis is, in percent, while it runs.
    analysis_percent: u64,
    /// Why the analysis failed, if it did.
    analysis_error: Option<String>,
    /// The suggested pauses and track starts, once the analysis is done.
    detection: Option<Arc<Detection>>,
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
    /// The toolbar, whose speed selector shows `speed`.
    toolbar: Option<Toolbar>,
    /// Menu options, remembered between runs.
    options: Options,
    /// File > Open recent, remembered between runs.
    recent: RecentFiles,
    /// Whether the current left-button drag has tried to start a scrub
    /// (at most once per drag, so a failing audio device is reported once).
    drag_scrub_tried: bool,
    /// The file's tags, once the peak job has opened it.
    tags: Option<Arc<Tags>>,
    /// The parts the file marks as tracks (the links of a chained Ogg
    /// file), with their tags; offered as suggested track starts.
    chapters: Arc<[Chapter]>,
    /// The track list under the waveform.
    grid: Option<Grid>,
    /// The dockable panes around the waveform.
    panes: Option<Panes>,
    /// Selection fields and play clock.
    position_bar: Option<PositionBar>,
    /// The minimap's view model; exists together with `view`.
    minimap: Option<Minimap>,
    /// Width of the minimap pane, remembered for minimaps created later.
    minimap_width: u32,
    /// The minimap pane, redrawn whenever the waveform is.
    minimap_panel: Option<Panel>,
    /// Height of the time ruler as last painted (0 when hidden), so the
    /// tooltip knows where the ruler is.
    ruler_height: std::cell::Cell<i32>,
    /// The waveform's tooltip as last set, so it is set only on changes.
    tooltip: String,
    /// The right button is down, and it is not yet known whether this is
    /// a click (the menu) or a scrub.
    right_press: Option<RightPress>,
    /// What the entries of the last right-click menu do, by id
    /// `ID_CONTEXT_FIRST + index`.
    menu_actions: Vec<context_menu::Action>,
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
        let path_for_analysis = path.clone();
        self.path = Some(path);
        self.seen = None;
        self.peaks = None;
        self.view = None;
        self.minimap = None;
        self.detail_seen = None;
        self.detail = None;
        self.editor = None;
        self.rate = None;
        self.player = None;
        self.timeline = Timeline::default();
        self.scrub = None;
        self.analysis = Some(AnalysisJob::start(
            &path_for_analysis,
            DetectParams::default(),
        ));
        self.analysis_seen = None;
        self.analysis_percent = 0;
        self.analysis_error = None;
        self.detection = None;
        self.tags = None;
        self.chapters = Arc::default();
    }

    /// The suggested track starts: the file's own (chained Ogg links, with
    /// their tags) and the analysis' (once it is done) that are not the
    /// same boundary; see [`lac_app::chapters::suggested_starts`].
    fn suggested_starts(&self) -> Vec<SuggestedStart> {
        let detected = self
            .detection
            .iter()
            .flat_map(|d| d.splits.iter().map(|s| s.pos));
        suggested_starts(self.chapters_in_use(), detected, self.rate.unwrap_or(1))
    }

    /// The chained file's links if they are to be offered as track starts
    /// (Edit > Suggest chained streams as tracks), else none.
    fn chapters_in_use(&self) -> &[Chapter] {
        if self.options.chained_links {
            &self.chapters
        } else {
            &[]
        }
    }

    /// For the status bar after reading a chained file: its links are
    /// offered as tracks, or are not because the user turned that off.
    fn chapter_note(&self) -> String {
        match (self.chapters.len(), self.options.chained_links) {
            (0 | 1, _) => String::new(),
            (n, true) => format!(
                "; {n} chained streams, offered as track starts (Edit > Split at suggested track starts)"
            ),
            (n, false) => format!(
                "; {n} chained streams, not offered as track starts (Edit > Suggest chained streams as tracks)"
            ),
        }
    }

    /// The track list's rows, empty until the file is read.
    fn track_rows(&self) -> Vec<lac_app::track_list::Row> {
        match (&self.editor, &self.tags, self.rate) {
            (Some(e), Some(t), Some(rate)) => lac_app::track_list::rows(e, t, rate),
            _ => Vec::new(),
        }
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
        let Some(editor) = &self.editor else {
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
        self.play_timeline(tl, start)
    }

    /// Plays the first or last seconds (`end`) of the track where
    /// playback would start (the selection or the cursor). Returns an
    /// error message.
    fn play_track_end(&mut self, end: End) -> Option<String> {
        let (Some(ed), Some(rate)) = (&self.editor, self.rate) else {
            return Some("open a file first".to_string());
        };
        let tl = lac_app::playback::track_excerpt(ed, ed.play_from(), end, rate);
        self.play_excerpt(tl).err()
    }

    /// Plays `tl` from its start and stops at its end: an excerpt from
    /// the right-click menu. Space plays the edits again afterwards.
    fn play_excerpt(&mut self, tl: Timeline) -> Result<(), String> {
        if tl.is_empty() {
            return Err("all of it is deleted; nothing to play".to_string());
        }
        self.play_timeline(tl, 0)
    }

    /// Plays timeline `tl` from timeline position `start`, opening the
    /// player on first use.
    fn play_timeline(&mut self, tl: Timeline, start: SamplePos) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Err("open a file first".to_string());
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
        if let Some(t) = &self.toolbar {
            t.show_speed(speed);
        }
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

    /// The right button, held down, turned out not to be a click (it was
    /// held or dragged): scrubs from where it went down. Returns an error
    /// message.
    fn begin_right_scrub(&mut self) -> Option<String> {
        let press = self.right_press.take()?;
        let pos = scrub_source_at(self, press.x())?;
        self.begin_scrub(pos, false)
    }

    /// Timer: a right button held too long for a click starts scrubbing
    /// (listening at the pointer). Returns an error message.
    fn poll_right_press(&mut self) -> Option<String> {
        let held = self.right_press.is_some_and(|p| p.held(Instant::now()));
        if held { self.begin_right_scrub() } else { None }
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

    /// Fills the position bar.
    fn show_positions(&self) {
        let (Some(bar), Some(e), Some(rate)) = (&self.position_bar, &self.editor, self.rate) else {
            return;
        };
        bar.show(&lac_app::positions::positions(e, rate, self.playhead()));
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

    /// Takes the analysis result when it is there. Returns a message for
    /// the status bar when the analysis ends.
    fn poll_analysis(&mut self) -> Option<String> {
        let (g, status) = self.analysis.as_ref()?.poll(self.analysis_seen)?;
        self.analysis_seen = Some(g);
        match status {
            AnalysisStatus::Running { done, total } => {
                self.analysis_percent = (done * 100).checked_div(total).unwrap_or(0);
                None
            }
            AnalysisStatus::Done { detection, .. } => {
                self.analysis = None;
                let pauses = detection.regions.len();
                self.detection = Some(detection);
                // Counts a chained file's link starts too.
                Some(format!(
                    "found {pauses} pause(s), {} track start(s) (Edit menu)",
                    self.suggested_starts().len()
                ))
            }
            AnalysisStatus::Failed(e) => {
                self.analysis = None;
                let msg = format!("could not look for pauses and track starts: {e}");
                self.analysis_error = Some(e);
                Some(msg)
            }
        }
    }

    /// Edit > Split at suggested track starts / Delete suggested pauses,
    /// each one undoable step. Returns the message for the status bar.
    fn apply_suggestions(&mut self, id: Id) -> String {
        if self.editor.is_none() {
            return "open a file first".to_string();
        }
        // A chained file's links are known from the start; the analysis'
        // suggestions join them when it is done.
        let have_chapters = id == ID_SUGGESTED_SPLITS && !self.chapters_in_use().is_empty();
        if self.detection.is_none() && !have_chapters {
            return match &self.analysis_error {
                Some(err) => format!("no suggestions: {err}"),
                None => format!(
                    "still looking for pauses and track starts ({} %)",
                    self.analysis_percent
                ),
            };
        }
        let starts = self.suggested_starts();
        let tags = self.tags.clone().unwrap_or_default();
        let Some(e) = &mut self.editor else {
            return "open a file first".to_string();
        };
        if id == ID_SUGGESTED_SPLITS {
            match e.split_at_suggestions(&starts, &tags) {
                0 => "no new track starts suggested".to_string(),
                n => format!("added {n} track start(s); Ctrl+Z takes them back"),
            }
        } else if let Some(d) = &self.detection {
            match e.delete_ranges(d.regions.iter().map(|r| r.range.clone())) {
                0 => "no suggested pauses left to delete".to_string(),
                n => format!(
                    "deleted {} of pauses; Ctrl+Z takes them back",
                    self.rate
                        .map_or_else(|| format!("{n} samples"), |r| format_samples(n, r))
                ),
            }
        } else {
            // Ruled out above: pauses come from the analysis only.
            "no suggested pauses left to delete".to_string()
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
        if u.tags.is_some() {
            self.tags = u.tags;
        }
        if let Some(c) = u.chapters {
            self.chapters = c;
        }
        if let Some(p) = u.peaks {
            if self.view.is_none() {
                self.view = Some(WaveformView::new(p.total_samples(), self.width));
                self.minimap = Some(Minimap::new(p.total_samples(), self.minimap_width));
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
                let note = self.chapter_note();
                if u.cached {
                    format!("{name} (waveform from cache){note}")
                } else {
                    format!("{name}{note}")
                }
            }
            JobStatus::Done(report) => {
                self.job = None;
                format!(
                    "{name}: {}{}",
                    report.warnings.join("; "),
                    self.chapter_note()
                )
            }
            JobStatus::Failed(msg) => {
                self.job = None;
                msg
            }
        })
    }
}

fn main() {
    // Before any window: --help and --version print and exit, a malformed
    // command line exits with clap's message.
    let args = GuiArgs::try_parse_os(std::env::args_os()).unwrap_or_else(|e| e.exit());
    let _ = wxdragon::main(move |app| {
        // macOS hands files opened from the Finder (double-click, "Open
        // With", drop on the Dock icon) to the running program as an event,
        // also at startup; other systems pass them on the command line. The
        // handler must be `Send`, so it only queues the paths; the timer
        // below opens them on the GUI thread.
        let (open_tx, open_rx) = std::sync::mpsc::channel::<PathBuf>();
        app.on_open_files(move |files| {
            for f in files {
                let _ = open_tx.send(PathBuf::from(f));
            }
        });
        let frame = Frame::builder()
            .with_title("losslessaudiocut")
            .with_size({
                let g = panes::stored_geometry();
                Size::new(g.width, g.height)
            })
            .build();
        logo::set_window_icon(&frame);
        let status = frame.create_status_bar(3, 0, -1, "status");
        status.set_status_widths(&[-4, -4, -2]);
        let options = Options::load();
        frame.set_menu_bar(menu_bar(options));
        let recent_files = recent::load();
        if let Some(bar) = frame.get_menu_bar() {
            recent::refresh(&bar, &recent_files);
        }

        let panel = Panel::builder(&frame).build();
        // We paint every pixel ourselves; letting the system erase first
        // would flicker.
        panel.set_background_style(BackgroundStyle::Paint);
        let grid = track_list::create(&frame);

        let map = minimap::create(&frame);
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
            minimap_width: 1,
            minimap_panel: Some(map),
            cache,
            speed: Speed::new(100, options.keep_pitch),
            options,
            recent: recent_files,
            grid: Some(grid),
            ..State::default()
        }));
        bind_track_list(&grid, &frame, &panel, &state);
        bind_paint(&panel, &state);
        bind_mouse(&panel, &state);
        bind_select(&panel, &frame, &state);
        let tools = {
            let (state, f, p) = (Rc::clone(&state), frame, panel);
            let speed_state = Rc::clone(&state);
            toolbar::create(
                &frame,
                TOOLS,
                move |id| {
                    command(&f, &p, &state, id, None);
                    // Keys belong to the waveform, not to the clicked button
                    // (Space would press it again rather than stop).
                    p.set_focus();
                },
                move |step| {
                    let msg = {
                        let mut st = speed_state.borrow_mut();
                        let keep_pitch = st.speed.keep_pitch;
                        st.set_speed(Speed::at_step(step, keep_pitch))
                    };
                    report(&f, &msg);
                    // As above; and Space should not open the list again.
                    p.set_focus();
                },
            )
        };
        let bar = {
            let (state, f, p) = (Rc::clone(&state), frame, panel);
            PositionBar::create(&frame, move |field, text| {
                type_position(&f, &p, &state, field, &text);
            })
        };
        bind_minimap(&map, &panel, &state);
        let log = log_pane::create(&frame);
        let panes = Panes::new(
            &frame,
            &panel,
            &tools.panel,
            &map,
            &bar.panel(),
            &grid,
            &log,
        );
        panes.restore();
        {
            let mut st = state.borrow_mut();
            st.panes = Some(panes);
            st.toolbar = Some(tools);
            st.position_bar = Some(bar);
        }
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
                // One window shows one file: of several opened at once,
                // the first wins.
                let opened: Vec<PathBuf> = open_rx.try_iter().collect();
                if let Some((first, rest)) = opened.split_first() {
                    if !rest.is_empty() {
                        report(&frame, "only one file at a time; opening the first");
                    }
                    if confirm_discard(&frame, &state) {
                        open_file(&frame, &state, first.clone());
                    }
                }
                let held = state.borrow_mut().poll_right_press();
                if let Some(msg) = held {
                    if panel.has_capture() {
                        panel.release_mouse();
                    }
                    report(&frame, &msg);
                }
                let (status, detail, export, playing, analysis) = {
                    let mut st = state.borrow_mut();
                    (
                        st.poll(),
                        st.poll_detail(),
                        st.poll_export(),
                        st.poll_player(),
                        st.poll_analysis(),
                    )
                };
                if playing {
                    let st = state.borrow();
                    frame.set_status_text(&st.time_status(), FIELD_TIME);
                    st.show_positions();
                }
                if let Some(s) = &status {
                    // Progress ("reading… 40 %") only goes to the status
                    // bar; the log gets how reading ended.
                    if state.borrow().job.is_some() {
                        frame.set_status_text(s, FIELD_FILE);
                    } else {
                        report(&frame, s);
                    }
                }
                if let Some(s) = &export {
                    report(&frame, s);
                }
                if let Some(s) = &analysis {
                    frame.set_status_text(s, FIELD_FILE);
                }
                if status.is_some() || export.is_some() {
                    update_chrome(&frame, &state.borrow());
                }
                if status.is_some() || detail || playing || analysis.is_some() {
                    panel.refresh(false, None);
                }
            });
        }
        timer.start(POLL_MS, false);
        // The timer must live as long as the window; the closure of
        // `wxdragon::main` returns right away, so leak this one handle.
        std::mem::forget(timer);

        if let Some(path) = args.file {
            open_file(&frame, &state, path);
        }
        frame.show(true);
        frame.centre();
        if panes::stored_geometry().maximized {
            frame.maximize(true);
        }
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
    // The builder cannot add submenus; right below Open, like elsewhere.
    file.insert_submenu(
        1,
        recent::submenu(),
        "Open &recent",
        "Open one of the recently opened files",
    );
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
        .append_item(
            ID_ALL_TRACKS,
            "Use for all trac&ks\tCtrl+Shift+A",
            "Give every track the value of the selected cell of the track list",
        )
        .append_separator()
        .append_item(
            ID_SUGGESTED_SPLITS,
            "Split at suggested &track starts\tCtrl+Shift+K",
            "Start new tracks where the analysis found pauses or changes of sound",
        )
        .append_item(
            ID_SUGGESTED_PAUSES,
            "Delete suggested &pauses\tCtrl+Shift+D",
            "Delete the silent and noise-only stretches the analysis found",
        )
        .append_check_item(
            ID_CHAINED_LINKS,
            "Suggest &chained streams as tracks",
            "Offer the start of each stream of a chained Ogg file as a track start; \
             turn off for recorded web radio that changes its title every few seconds",
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
        .append_item(
            ID_PLAY_TRACK_START,
            "Play track &beginning\tCtrl+Shift+B",
            "Play the first 3 seconds of the track at the cursor or selection",
        )
        .append_item(
            ID_PLAY_TRACK_END,
            "Play track &end\tCtrl+Shift+E",
            "Play the last 3 seconds of the track at the cursor or selection",
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
        .append_check_item(
            ID_RULER,
            "Time &ruler",
            "Show the time scale along the bottom of the waveform",
        )
        .append_separator();
    let view = Pane::ALL
        .into_iter()
        .fold(view, |m, p| {
            m.append_check_item(
                panes::menu_id(p),
                &panes::menu_label(p),
                panes::menu_help(p),
            )
        })
        .build();
    let bar = MenuBar::builder()
        .append(file, "&File")
        .append(edit, "&Edit")
        .append(play, "&Play")
        .append(view, "&View")
        .build();
    bar.check_item(ID_KEEP_PITCH, options.keep_pitch);
    bar.check_item(ID_SCRUB_SELECTING, options.scrub_while_selecting);
    bar.check_item(ID_RULER, options.ruler);
    bar.check_item(ID_CHAINED_LINKS, options.chained_links);
    bar
}

/// Menu commands.
fn bind_menu(frame: &Frame, panel: &Panel, state: &Rc<RefCell<State>>) {
    let state = Rc::clone(state);
    let (f, panel) = (*frame, *panel);
    frame.on_menu(move |e| command(&f, &panel, &state, e.get_id(), e.is_checked()));
}

/// Runs command `id`, from the menu or the toolbar. `checked` is the new
/// check mark of a check item chosen from the menu; `None` toggles.
fn command(f: &Frame, panel: &Panel, state: &Rc<RefCell<State>>, id: Id, checked: Option<bool>) {
    // Handles are cheap copies; the body was written for owned ones.
    let f = *f;
    if let Some(pane) = panes::pane_for(id) {
        let panes = state.borrow().panes;
        if let Some(p) = panes {
            p.toggle(pane);
        }
        return;
    }
    if id == ID_OPEN {
        if !confirm_discard(&f, state) {
            return;
        }
        let dialog = FileDialog::builder(&f)
            .with_message("Open an audio file")
            .with_wildcard(&lac_app::file_types::dialog_wildcard())
            .with_style(FileDialogStyle::Open | FileDialogStyle::FileMustExist)
            .build();
        if dialog.show_modal() == ID_OK
            && let Some(path) = dialog.get_path()
        {
            open_file(&f, state, PathBuf::from(path));
        }
    } else if let Some(i) = context_index(id) {
        let msg = context_command(&mut state.borrow_mut(), i);
        if let Some(msg) = msg {
            report(&f, &msg);
        }
    } else if let Some(i) = recent::index_of(id) {
        open_recent(&f, state, i);
        return;
    } else if id == recent::ID_CLEAR {
        let mut st = state.borrow_mut();
        st.recent.clear();
        remember_recent(&f, &st.recent);
        return;
    } else if id == ID_EXPORT {
        export(&f, state);
    } else if id == ID_RENAME {
        rename_track(&f, state);
    } else if id == ID_ALL_TRACKS {
        use_for_all_tracks(&f, state);
    } else if id == ID_CLEAR_CACHE {
        let cache = state.borrow().cache.clone();
        let msg = match cache.map(|c| c.clear()) {
            Some(Ok(n)) => format!("waveform cache cleared ({n} entries)"),
            Some(Err(e)) => format!("could not clear the waveform cache: {e}"),
            None => "there is no waveform cache on this system".to_string(),
        };
        report(&f, &msg);
        return;
    } else if id == ID_EXIT {
        f.close(false);
        return;
    } else if id == ID_PLAY_TRACK_START || id == ID_PLAY_TRACK_END {
        let end = if id == ID_PLAY_TRACK_START {
            End::Start
        } else {
            End::End
        };
        let msg = state.borrow_mut().play_track_end(end);
        if let Some(msg) = msg {
            report(&f, &msg);
        }
    } else if id == ID_PLAY {
        let msg = state.borrow_mut().toggle_play();
        if let Some(msg) = msg {
            report(&f, &msg);
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
                    let keep = checked.unwrap_or(!st.speed.keep_pitch);
                    st.options.keep_pitch = keep;
                    st.options.save();
                    Speed::new(st.speed.percent(), keep)
                }
            };
            st.set_speed(speed)
        };
        report(&f, &msg);
    } else if id == ID_SCRUB_SELECTING {
        let mut st = state.borrow_mut();
        let on = checked.unwrap_or(!st.options.scrub_while_selecting);
        st.options.scrub_while_selecting = on;
        st.options.save();
        return;
    } else if id == ID_RULER {
        let mut st = state.borrow_mut();
        let on = checked.unwrap_or(!st.options.ruler);
        st.options.ruler = on;
        st.options.save();
    } else if id == ID_CHAINED_LINKS {
        // The suggestion marks change; splits already made stay.
        let mut st = state.borrow_mut();
        let on = checked.unwrap_or(!st.options.chained_links);
        st.options.chained_links = on;
        st.options.save();
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
    } else if id == ID_SUGGESTED_SPLITS || id == ID_SUGGESTED_PAUSES {
        let msg = {
            let mut st = state.borrow_mut();
            let msg = st.apply_suggestions(id);
            st.sync_timeline();
            msg
        };
        f.set_status_text(&msg, FIELD_FILE);
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
            report(&f, msg);
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

/// Shows `msg` in the status bar's message field and keeps it in the log
/// pane.
fn report(frame: &Frame, msg: &str) {
    frame.set_status_text(msg, FIELD_FILE);
    log_pane::append(msg);
}

/// Title and edit field of the status bar, after anything that may have
/// changed them.
fn update_chrome(frame: &Frame, st: &State) {
    frame.set_title(&st.title());
    frame.set_status_text(&st.edit_status(), FIELD_EDIT);
    frame.set_status_text(&st.time_status(), FIELD_TIME);
    if let Some(g) = &st.grid {
        track_list::refresh(g, &st.track_rows());
    }
    st.show_positions();
}

/// Enter in a field of the position bar: sets the selection (or cursor)
/// from the typed time, see [`lac_app::positions::apply`].
fn type_position(
    frame: &Frame,
    panel: &Panel,
    state: &Rc<RefCell<State>>,
    field: lac_app::positions::Field,
    text: &str,
) {
    let error = {
        let mut st = state.borrow_mut();
        let rate = st.rate;
        let result = match (&mut st.editor, rate) {
            (Some(e), Some(rate)) => lac_app::positions::apply(e, field, text, rate)
                .err()
                .map(|e| e.to_string()),
            _ => Some("open a file first".to_string()),
        };
        if result.is_none() {
            st.seek_to_cursor();
            // Show where the selection now starts.
            let from = st.editor.as_ref().map(Editor::play_from);
            if let (Some(from), Some(v)) = (from, &mut st.view)
                && !v.visible().contains(&from)
            {
                v.scroll_to(from);
            }
        }
        result
    };
    if let Some(msg) = &error {
        report(frame, msg);
    }
    // Back to the waveform, whose keys (Space, Del) the field would eat;
    // also lets the field show the accepted (or the old) value again.
    panel.set_focus();
    update_chrome(frame, &state.borrow());
    panel.refresh(false, None);
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

/// Starts reading `path` (from File > Open, the command line or the
/// macOS Finder). Unexported edits must have been confirmed before.
fn open_file(frame: &Frame, state: &Rc<RefCell<State>>, path: PathBuf) {
    // Absolute, so that a file opened from the command line in different
    // directories is listed once and still found from elsewhere. If that
    // fails (the file is gone), opening it reports the error anyway.
    let listed = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    {
        let mut st = state.borrow_mut();
        st.recent.add(listed);
        remember_recent(frame, &st.recent);
        st.open(path);
    }
    report(frame, "reading…");
    update_chrome(frame, &state.borrow());
}

/// File > Open recent > entry `index`: opens that file, or takes it off
/// the list if it is gone (moved, deleted, on an unplugged drive).
fn open_recent(frame: &Frame, state: &Rc<RefCell<State>>, index: usize) {
    let Some(path) = state.borrow().recent.get(index).map(Path::to_path_buf) else {
        return;
    };
    if !path.is_file() {
        let mut st = state.borrow_mut();
        st.recent.remove(&path);
        remember_recent(frame, &st.recent);
        drop(st);
        report(
            frame,
            &format!(
                "{} no longer exists; removed it from the list",
                path.display()
            ),
        );
        return;
    }
    if confirm_discard(frame, state) {
        open_file(frame, state, path);
    }
}

/// Stores the recent files and shows them in the menu.
fn remember_recent(frame: &Frame, list: &RecentFiles) {
    recent::save(list);
    if let Some(bar) = frame.get_menu_bar() {
        recent::refresh(&bar, list);
    }
}

/// File > Export: asks for the output file and starts writing it.
fn export(frame: &Frame, state: &Rc<RefCell<State>>) {
    let (source, editor) = {
        let st = state.borrow();
        if st.export.is_some() {
            report(frame, "an export is still running");
            return;
        }
        match (&st.path, &st.editor) {
            (Some(p), Some(e)) => (p.clone(), e.clone()),
            _ => {
                report(frame, "open a file first");
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
        report(frame, "everything is deleted; nothing to export");
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
    report(frame, &msg);
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
        report(frame, "open a file first");
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

/// Edits in the track list: a changed cell becomes a tag edit of its
/// track, selecting a cell puts the cursor at the start of its track, and
/// double-clicking a track's row selects the track and zooms to it.
fn bind_track_list(grid: &Grid, frame: &Frame, panel: &Panel, state: &Rc<RefCell<State>>) {
    // Double-clicking the row label or any cell of the row. On a tag cell
    // wxGrid then also opens the cell editor, as a double-click there
    // always did; wxdragon's grid events cannot be consumed to prevent it.
    {
        let (state, f, panel) = (Rc::clone(state), *frame, *panel);
        grid.on_label_left_dclick(move |e| show_track(e.get_row(), &f, &panel, &state));
    }
    {
        let (state, f, panel) = (Rc::clone(state), *frame, *panel);
        grid.on_cell_left_dclick(move |e| show_track(e.get_row(), &f, &panel, &state));
    }
    // Right-clicking a row label or a cell: the track's menu (issue #90).
    {
        let (state, g) = (Rc::clone(state), *grid);
        grid.on_label_right_click(move |e| show_track_menu(&g, &state, e.get_row()));
    }
    {
        let (state, g) = (Rc::clone(state), *grid);
        grid.on_cell_right_click(move |e| show_track_menu(&g, &state, e.get_row()));
    }
    {
        let (state, f, panel, g) = (Rc::clone(state), *frame, *panel, *grid);
        grid.on_cell_changed(move |e| {
            let (row, col) = (e.get_row(), e.get_col());
            let text = g.get_cell_value(row, col);
            let msg = {
                let Ok(mut st) = state.try_borrow_mut() else {
                    return;
                };
                let tags = st.tags.clone().unwrap_or_default();
                let rate = st.rate.unwrap_or(1);
                let Ok(index) = usize::try_from(row) else {
                    return;
                };
                if let (Some(ed), Some(bound)) = (&mut st.editor, track_list::bound(col)) {
                    // A typed start or end moves the track boundary, and
                    // the cursor jumps there (issues #88, #89).
                    match lac_app::track_list::edit_time(ed, index, bound, &text, rate) {
                        Ok(Some(pos)) => {
                            jump_to(&mut st, pos);
                            None
                        }
                        Ok(None) => None,
                        Err(e) => Some(e.to_string()),
                    }
                } else if let (Some(ed), Some(field)) = (&mut st.editor, track_list::field(col)) {
                    lac_app::track_list::edit(ed, &tags, index, field, &text, false)
                        .err()
                        .map(|e| e.to_string())
                } else {
                    return;
                }
            };
            if let Some(m) = &msg {
                report(&f, m);
            }
            // Puts the accepted value back, or the old one after an error.
            update_chrome(&f, &state.borrow());
            panel.refresh(false, None);
        });
    }
    {
        let (state, f, panel) = (Rc::clone(state), *frame, *panel);
        grid.on_select_cell(move |e| {
            // Refreshing the grid selects cells too; the state is borrowed
            // then, and those are no user's choice anyway.
            let Ok(mut st) = state.try_borrow_mut() else {
                return;
            };
            if let (Ok(index), Some(ed)) = (usize::try_from(e.get_row()), &st.editor) {
                // The end cell goes to the track's end; any other cell to
                // its start, unless the cursor is in that track already
                // (moving between its tag cells keeps the cursor).
                let target = if track_list::bound(e.get_col()) == Some(Bound::End) {
                    lac_app::track_list::track_end(ed, index)
                        .filter(|&end| ed.selection().is_some() || ed.cursor() != end)
                } else {
                    lac_app::track_list::track_start(ed, index)
                        .filter(|_| ed.track_at(ed.play_from()) != index)
                };
                if let Some(pos) = target {
                    jump_to(&mut st, pos);
                    f.set_status_text(&st.time_status(), FIELD_TIME);
                    f.set_status_text(&st.edit_status(), FIELD_EDIT);
                    panel.refresh(false, None);
                }
            }
            // Not vetoed, so the grid moves its cursor there.
        });
    }
}

/// Clears the selection and puts the cursor (and the playback position)
/// at `pos`, scrolling the waveform there if it is out of view.
fn jump_to(st: &mut State, pos: SamplePos) {
    if let Some(ed) = &mut st.editor {
        ed.clear_selection();
        ed.set_cursor(pos);
    }
    st.seek_to_cursor();
    if let Some(v) = &mut st.view
        && !v.visible().contains(&pos)
    {
        v.scroll_to(pos);
    }
}

/// Selects track `row` of the track list (0-based; anything else, such as
/// -1 for a column label, is ignored), puts the cursor at its start and
/// zooms the waveform to it.
fn show_track(row: i32, frame: &Frame, panel: &Panel, state: &Rc<RefCell<State>>) {
    let Ok(index) = usize::try_from(row) else {
        return;
    };
    // Busy elsewhere (e.g. a refresh in progress): the click is dropped.
    let Ok(mut st) = state.try_borrow_mut() else {
        return;
    };
    let Some(span) = st
        .editor
        .as_ref()
        .and_then(|ed| lac_app::track_list::track_span(ed, index))
    else {
        return;
    };
    if let Some(ed) = &mut st.editor {
        ed.select(span.start, span.end);
        ed.set_cursor(span.start);
    }
    st.seek_to_cursor();
    if let Some(v) = &mut st.view {
        v.zoom_to(&span);
    }
    // Title, status bar (selection, time) and the menus' enabled state.
    update_chrome(frame, &st);
    drop(st);
    panel.refresh(false, None);
}

/// Edit > Use for all tracks: gives every track the value of the track
/// list's selected cell.
fn use_for_all_tracks(frame: &Frame, state: &Rc<RefCell<State>>) {
    let msg = {
        let mut st = state.borrow_mut();
        let Some(g) = st.grid else { return };
        let (row, col) = (g.get_grid_cursor_row(), g.get_grid_cursor_col());
        let tags = st.tags.clone().unwrap_or_default();
        match (&mut st.editor, track_list::field(col), usize::try_from(row)) {
            (None, _, _) => Some("open a file first".to_string()),
            (Some(ed), Some(field), Ok(index)) => {
                let text = g.get_cell_value(row, col);
                match lac_app::track_list::edit(ed, &tags, index, field, &text, true) {
                    Ok(_) => Some(format!("{field} of every track: {text}")),
                    Err(e) => Some(e.to_string()),
                }
            }
            _ => Some("select a tag in the track list first".to_string()),
        }
    };
    if let Some(m) = msg {
        report(frame, &m);
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
            report(&f, "finishing the export…");
            // Usually seconds; the file would be broken otherwise.
            let _ = r.job.wait();
        }
        let panes = state.borrow().panes;
        if let Some(p) = panes {
            p.save();
            p.uninit();
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

/// The pointer's y position in a mouse event.
fn e_y(e: &WindowEventData) -> i32 {
    mouse_event(e)
        .and_then(|(ev, _)| ev.get_position())
        .map_or(0, |pt| pt.y)
}

/// How close (in pixels) a mark must be to the pointer for its tooltip.
const TOOLTIP_REACH: f64 = 3.0;

/// Sets the waveform's tooltip to what is at pixel (`x`, `y`) (issue #93;
/// the texts come from [`lac_app::hover`]). Only changes are passed on,
/// so the tooltip does not flicker while the pointer moves over one mark.
fn update_tooltip(st: &mut State, panel: &Panel, x: f64, y: i32) {
    let tip = with_scene(st, |scene, view| {
        let height = panel.get_client_size().height;
        let ruler = st.ruler_height.get();
        let zone = if y >= height - ruler {
            Zone::Ruler
        } else if y >= height - ruler - SUGGESTION_MARK {
            Zone::Bottom
        } else if y < 2 * SUGGESTION_MARK {
            Zone::Top
        } else {
            Zone::Lanes
        };
        let (pos, near) = pointer_at(view, x);
        hover::tooltip(scene, pos, zone, near)
    })
    .flatten()
    .unwrap_or_default();
    if tip != st.tooltip {
        panel.set_tooltip(&tip);
        st.tooltip = tip;
    }
}

/// Calls `f` with what the waveform shows (for its tooltips and menu)
/// and the view; `None` until a file is read.
fn with_scene<R>(st: &State, f: impl FnOnce(&hover::Scene, &WaveformView) -> R) -> Option<R> {
    let (view, ed, rate) = (st.view.as_ref()?, st.editor.as_ref()?, st.rate?);
    let starts: Vec<SamplePos> = st
        .suggested_starts()
        .iter()
        .map(|s| s.pos)
        .filter(|p| !ed.splits().contains(p))
        .collect();
    let scene = hover::Scene {
        editor: ed,
        pauses: st.detection.as_ref().map_or(&[][..], |d| &d.regions),
        suggested_starts: &starts,
        playhead: st.playhead(),
        rate,
    };
    Some(f(&scene, view))
}

/// The source position under the pointer at `x`, and how far from it
/// (in samples) a mark still counts as under the pointer.
fn pointer_at(view: &WaveformView, x: f64) -> (SamplePos, SamplePos) {
    let pos = view.x_to_sample(x.clamp(0.0, f64::from(view.width())));
    let near = (TOOLTIP_REACH * view.samples_per_pixel()).ceil() as SamplePos;
    (pos, near)
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
            {
                let mut st = state.borrow_mut();
                if let Some(press) = st.right_press {
                    if !press.moved(x, f64::from(e_y(&e))) {
                        return;
                    }
                    // Too far for a click: scrub from where the button
                    // went down; the code below follows the pointer.
                    if let Some(msg) = st.begin_right_scrub() {
                        drop(st);
                        if p.has_capture() {
                            p.release_mouse();
                        }
                        report(&f, &msg);
                        return;
                    }
                }
            }
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
                if !st.editor.as_ref().is_some_and(Editor::dragging) {
                    // Just moving: explain what is under the pointer.
                    let y = e_y(&e);
                    update_tooltip(st, &p, x, y);
                    return;
                }
                let (Some(v), Some(ed)) = (&mut st.view, &mut st.editor) else {
                    return;
                };
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
                report(&f, &msg);
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

/// Right button: a click opens the waveform's menu (see
/// [`show_context_menu`]); held or dragged, it scrubs. While scrubbing,
/// playback follows the pointer in short pieces (the motion is handled in
/// [`bind_select`]); see [`State::begin_scrub`] and [`State::end_scrub`].
/// Which of the two a press is stays open until the pointer moves or the
/// button is held a moment ([`RightPress`]), so a click stays silent and
/// does not move playback.
fn bind_scrub(panel: &Panel, frame: &Frame, state: &Rc<RefCell<State>>) {
    let (p, f) = (*panel, *frame);
    {
        let state = Rc::clone(state);
        panel.on_mouse_right_down(move |e| {
            let Some((_, x)) = mouse_event(&e) else {
                return;
            };
            {
                let mut st = state.borrow_mut();
                if st.scrub.is_some()
                    || st.view.is_none()
                    || st.editor.as_ref().is_some_and(Editor::dragging)
                {
                    // Already scrubbing a selection being dragged, or
                    // dragging one silently: the left button has the mouse.
                    return;
                }
                let y = f64::from(e_y(&e));
                st.right_press = Some(RightPress::new(x, y, Instant::now()));
            }
            p.capture_mouse();
        });
    }
    {
        let state = Rc::clone(state);
        panel.on_mouse_right_up(move |_| {
            let press = state.borrow_mut().right_press.take();
            if let Some(press) = press {
                // A click.
                if p.has_capture() {
                    p.release_mouse();
                }
                show_context_menu(&f, &p, &state, press.x());
                return;
            }
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
    {
        // The menu key (or Shift+F10): the menu for where playback would
        // start. A right click sends this event too, with a position; that
        // one is handled above.
        let state = Rc::clone(state);
        panel.on_context_menu(move |e| {
            if e.get_context_position().is_some() {
                return;
            }
            let x = {
                let st = state.borrow();
                let (Some(v), Some(ed)) = (&st.view, &st.editor) else {
                    return;
                };
                v.sample_to_x(ed.play_from())
                    .clamp(0.0, f64::from(v.width()))
            };
            show_context_menu(&f, &p, &state, x);
        });
    }
}

/// Opens the waveform's right-click menu for the pointer at `x` (see
/// [`context_menu::waveform`]); the chosen entry arrives as a menu command
/// ([`context_command`]).
fn show_context_menu(frame: &Frame, panel: &Panel, state: &Rc<RefCell<State>>, x: f64) {
    let entries = with_scene(&state.borrow(), |scene, view| {
        let (pos, near) = pointer_at(view, x);
        context_menu::waveform(scene, pos, near)
    });
    match entries {
        Some(entries) => popup(panel, state, entries),
        None => report(frame, "open a file first"),
    }
}

/// Opens the track list's right-click menu for row `row` (see
/// [`context_menu::track_list`]); anything but a track's row (such as -1
/// for a column label) is ignored.
fn show_track_menu(grid: &Grid, state: &Rc<RefCell<State>>, row: i32) {
    let Ok(index) = usize::try_from(row) else {
        return;
    };
    let entries = match &state.borrow().editor {
        Some(ed) => context_menu::track_list(ed, index),
        None => return,
    };
    if !entries.is_empty() {
        popup(grid, state, entries);
    }
}

/// Shows `entries` as a popup menu at the mouse pointer over `window`;
/// the chosen entry arrives at the frame as a menu command
/// ([`context_command`]).
fn popup(window: &impl WxWidget, state: &Rc<RefCell<State>>, entries: Vec<Entry>) {
    let mut menu = Menu::builder().build();
    {
        let mut st = state.borrow_mut();
        st.menu_actions.clear();
        for entry in entries {
            match entry {
                Entry::Separator => menu.append_separator(),
                Entry::Item(item) if st.menu_actions.len() < CONTEXT_MAX => {
                    let id = ID_CONTEXT_FIRST + st.menu_actions.len() as Id;
                    menu.append(id, &item.label, "", ItemKind::Normal);
                    menu.enable_item(id, item.enabled);
                    st.menu_actions.push(item.action);
                }
                Entry::Item(_) => {}
            }
        }
    }
    // No borrow while the menu is open: its command borrows the state.
    window.popup_menu(&mut menu, None);
}

/// The index into [`State::menu_actions`] of right-click menu command `id`.
fn context_index(id: Id) -> Option<usize> {
    let i = usize::try_from(id.checked_sub(ID_CONTEXT_FIRST)?).ok()?;
    (i < CONTEXT_MAX).then_some(i)
}

/// Runs entry `index` of the last right-click menu. Returns an error
/// message.
fn context_command(st: &mut State, index: usize) -> Option<String> {
    let action = st.menu_actions.get(index)?.clone();
    let rate = st.rate?;
    let ed = st.editor.as_mut()?;
    match context_menu::apply(ed, &action, rate) {
        Effect::Edited(_) => {
            st.sync_timeline();
            None
        }
        Effect::Play(tl) => st.play_excerpt(tl).err(),
        Effect::PlayFrom(pos) => st.play_at(pos, false).err(),
        Effect::ZoomTo(span) => {
            st.seek_to_cursor();
            if let Some(v) = &mut st.view {
                v.zoom_to(&span);
            }
            None
        }
        Effect::JumpTo(pos) => {
            jump_to(st, pos);
            None
        }
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
        // Whatever moved the waveform (zoom, scroll, edits, playback)
        // moves the minimap's frame too.
        if let Some(m) = st.minimap_panel {
            m.refresh(false, None);
        }
        let (Some(view), Some(peaks)) = (&st.view, &st.peaks) else {
            return;
        };
        // The ruler takes a strip at the bottom; the lanes get the rest.
        let ruler_height = if st.options.ruler {
            dc.get_text_extent("0:00").1 + RULER_PADDING
        } else {
            0
        };
        st.ruler_height.set(ruler_height);
        let lanes = lanes::lanes(peaks.channels(), size.height - ruler_height);
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
        // Suggestions: a strip along the bottom of the lanes (just above
        // the time ruler, if shown) under each suggested pause, and a tick
        // at the top of each suggested track start not split at yet.
        if let Some(d) = &st.detection {
            for r in &d.regions {
                let Some(span) = view.pixel_span(&r.range) else {
                    continue;
                };
                let colour = match r.kind {
                    RegionKind::Silence => SUGGESTED_SILENCE,
                    RegionKind::Noise => SUGGESTED_NOISE,
                };
                dc.set_pen(colour, 1, PenStyle::Solid);
                dc.set_brush(colour, BrushStyle::Solid);
                dc.draw_rectangle(
                    px(span.start),
                    size.height - ruler_height - SUGGESTION_MARK,
                    px(span.end - span.start),
                    SUGGESTION_MARK,
                );
            }
        }
        // The chained links' starts are there before the analysis is done.
        let split = editor.map_or(&[][..], Editor::splits);
        dc.set_pen(SUGGESTED_SPLIT, 2, PenStyle::Solid);
        for s in st
            .suggested_starts()
            .iter()
            .filter(|s| !split.contains(&s.pos))
        {
            let x = view.sample_to_x(s.pos).round();
            if (0.0..f64::from(view.width())).contains(&x) {
                dc.draw_line(x as i32, 0, x as i32, 2 * SUGGESTION_MARK);
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
        if let Some(rate) = st.rate.filter(|_| ruler_height > 0) {
            draw_ruler(&dc, view, rate, size, ruler_height);
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

/// The minimap pane: painting, resizing, and dragging its frame to scroll
/// the waveform (`waveform`, which is redrawn after a scroll; its paint
/// handler redraws the minimap in turn).
fn bind_minimap(map: &Panel, waveform: &Panel, state: &Rc<RefCell<State>>) {
    let (m, w) = (*map, *waveform);
    {
        let state = Rc::clone(state);
        map.on_paint(move |_| {
            let dc = AutoBufferedPaintDC::new(&m);
            let size = m.get_client_size();
            let st = state.borrow();
            if let (Some(map), Some(peaks), Some(view)) = (&st.minimap, &st.peaks, &st.view) {
                let scene = minimap::Scene {
                    map,
                    peaks,
                    editor: st.editor.as_ref(),
                    visible: view.visible(),
                    playhead: st.playhead(),
                };
                minimap::draw(&dc, size, &scene);
            } else {
                dc.set_background(BACKGROUND);
                dc.clear();
            }
        });
    }
    {
        let state = Rc::clone(state);
        map.on_size(move |e| {
            if let WindowEventData::Size(s) = &e
                && let Some(size) = s.get_size()
            {
                let mut st = state.borrow_mut();
                st.minimap_width = u32::try_from(size.width).unwrap_or(1).max(1);
                let w = st.minimap_width;
                if let Some(map) = &mut st.minimap {
                    map.set_width(w);
                }
            }
            m.refresh(false, None);
            e.skip(true);
        });
    }
    // Moves the waveform to start at what the minimap says.
    let scroll = move |st: &mut State, start: Option<SamplePos>| {
        if let (Some(start), Some(v)) = (start, &mut st.view) {
            v.scroll_to(start);
            w.refresh(false, None);
        }
    };
    {
        let state = Rc::clone(state);
        map.on_mouse_left_down(move |e| {
            let Some((_, x)) = mouse_event(&e) else {
                return;
            };
            let mut guard = state.borrow_mut();
            let st = &mut *guard;
            let (Some(map), Some(v)) = (&mut st.minimap, &st.view) else {
                return;
            };
            let start = map.press(x, &v.visible());
            scroll(st, Some(start));
            m.capture_mouse();
        });
    }
    {
        let state = Rc::clone(state);
        map.on_mouse_motion(move |e| {
            let Some((_, x)) = mouse_event(&e) else {
                return;
            };
            let mut guard = state.borrow_mut();
            let st = &mut *guard;
            let start = st.minimap.as_mut().and_then(|map| map.drag(x));
            scroll(st, start);
        });
    }
    {
        let state = Rc::clone(state);
        map.on_mouse_left_up(move |_| {
            if m.has_capture() {
                m.release_mouse();
            }
            if let Some(map) = &mut state.borrow_mut().minimap {
                map.release();
            }
        });
    }
}

/// Draws the time ruler into the bottom `height` rows of the waveform
/// window (`size`): ticks and labels from [`lac_app::ruler::ticks`].
fn draw_ruler(dc: &impl DeviceContext, view: &WaveformView, rate: u32, size: Size, height: i32) {
    let top = size.height - height;
    dc.set_pen(RULER_BACKGROUND, 1, PenStyle::Solid);
    dc.set_brush(RULER_BACKGROUND, BrushStyle::Solid);
    dc.draw_rectangle(0, top, size.width, height);
    dc.set_pen(RULER_TICK, 1, PenStyle::Solid);
    dc.draw_line(0, top, size.width, top);
    dc.set_text_foreground(RULER_TEXT);
    dc.set_background_mode(BackgroundMode::Transparent);
    // Room for the widest usual label and a gap.
    let min_gap = f64::from(dc.get_text_extent("0:00:00.000").0 + 16);
    for t in lac_app::ruler::ticks(view, rate, min_gap) {
        let x = t.x.round() as i32;
        let len = if t.label.is_some() {
            RULER_MAJOR
        } else {
            RULER_MINOR
        };
        // draw_line leaves out the end point, so go one past.
        dc.draw_line(x, top, x, top + len + 1);
        if let Some(label) = &t.label {
            dc.draw_text(label, x + 3, top + RULER_MINOR + 1);
        }
    }
}

/// A pixel coordinate for the device context.
fn px(x: u32) -> i32 {
    i32::try_from(x).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions_apply_as_one_undo_step_each() {
        use lac_analysis::{Region, SplitReason, SplitSuggestion, Thresholds};
        let mut st = State::default();
        assert_eq!(
            st.apply_suggestions(ID_SUGGESTED_SPLITS),
            "open a file first"
        );
        st.editor = Some(Editor::new(10_000));
        st.rate = Some(1000);
        st.analysis_percent = 42;
        assert_eq!(
            st.apply_suggestions(ID_SUGGESTED_PAUSES),
            "still looking for pauses and track starts (42 %)"
        );
        st.detection = Some(Arc::new(Detection {
            thresholds: Thresholds {
                floor_db: -80.0,
                loud_db: -10.0,
                quiet_db: -60.0,
            },
            regions: vec![
                Region {
                    range: 1000..1500,
                    kind: RegionKind::Silence,
                    level_db: -70.0,
                },
                Region {
                    range: 6000..7000,
                    kind: RegionKind::Noise,
                    level_db: -50.0,
                },
            ],
            splits: vec![SplitSuggestion {
                pos: 7000,
                reason: SplitReason::Gap { gap: 6000..7000 },
                confidence: 0.8,
            }],
        }));
        assert_eq!(
            st.apply_suggestions(ID_SUGGESTED_SPLITS),
            "added 1 track start(s); Ctrl+Z takes them back"
        );
        assert_eq!(
            st.apply_suggestions(ID_SUGGESTED_SPLITS),
            "no new track starts suggested"
        );
        assert_eq!(
            st.apply_suggestions(ID_SUGGESTED_PAUSES),
            "deleted 0:01.500 of pauses; Ctrl+Z takes them back"
        );
        let e = st.editor.as_mut().expect("the editor was set above");
        assert_eq!(e.deleted(), [1000..1500, 6000..7000]);
        assert_eq!(e.splits(), [7000]);
        assert!(e.undo());
        assert!(e.deleted().is_empty(), "pauses come back in one step");
        assert_eq!(e.splits(), [7000]);
    }

    #[test]
    fn chained_links_are_suggested_before_the_analysis_is_done() {
        let titled = |t: &str| Tags {
            title: Some(t.into()),
            ..Tags::default()
        };
        let mut st = State {
            editor: Some(Editor::new(10_000)),
            rate: Some(1000),
            tags: Some(Arc::new(titled("One"))),
            chapters: Arc::from(vec![
                Chapter {
                    start: 0,
                    tags: titled("One"),
                },
                Chapter {
                    start: 4000,
                    tags: titled("Two"),
                },
            ]),
            ..State::default()
        };
        assert!(st.chapter_note().contains("2 chained streams"));
        assert_eq!(
            st.apply_suggestions(ID_SUGGESTED_PAUSES),
            "still looking for pauses and track starts (0 %)",
            "pauses need the analysis"
        );
        assert_eq!(
            st.apply_suggestions(ID_SUGGESTED_SPLITS),
            "added 1 track start(s); Ctrl+Z takes them back"
        );
        let e = st.editor.as_ref().expect("set above");
        assert_eq!(e.splits(), [4000]);
        assert_eq!(e.tracks()[1].name.as_deref(), Some("Two"));
    }

    #[test]
    fn chained_links_are_not_suggested_when_turned_off() {
        // Recorded web radio: every title change starts a new link.
        let mut st = State {
            editor: Some(Editor::new(10_000)),
            rate: Some(1000),
            tags: Some(Arc::new(Tags::default())),
            chapters: Arc::from(vec![
                Chapter {
                    start: 0,
                    tags: Tags::default(),
                },
                Chapter {
                    start: 4000,
                    tags: Tags::default(),
                },
            ]),
            options: Options {
                chained_links: false,
                ..Options::default()
            },
            ..State::default()
        };
        assert!(st.suggested_starts().is_empty());
        assert!(
            st.chapter_note().contains("not offered"),
            "{}",
            st.chapter_note()
        );
        // Without the links, splitting waits for the analysis as usual.
        assert_eq!(
            st.apply_suggestions(ID_SUGGESTED_SPLITS),
            "still looking for pauses and track starts (0 %)"
        );
        assert!(st.editor.as_ref().expect("set above").splits().is_empty());
        // Turned back on, they are offered again.
        st.options.chained_links = true;
        assert_eq!(st.suggested_starts().len(), 1);
    }

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
    fn toolbar_buttons_run_distinct_menu_commands() {
        let ids: Vec<Id> = TOOLS
            .iter()
            .filter_map(|t| match t {
                Tool::Button { id, .. } => Some(*id),
                Tool::Speed { .. } | Tool::Gap => None,
            })
            .collect();
        for (i, id) in ids.iter().enumerate() {
            assert!(!ids[i + 1..].contains(id), "{id} twice");
            assert!(*id > ID_HIGHEST, "{id} is no command of ours");
            assert_eq!(panes::pane_for(*id), None, "{id} toggles a pane");
        }
    }

    #[test]
    fn right_click_menu_ids_are_their_own() {
        assert_eq!(context_index(ID_CONTEXT_FIRST), Some(0));
        let last = ID_CONTEXT_FIRST + CONTEXT_MAX as Id - 1;
        assert_eq!(context_index(last), Some(CONTEXT_MAX - 1));
        assert_eq!(context_index(last + 1), None);
        assert_eq!(context_index(ID_CHAINED_LINKS), None);
        for id in ID_CONTEXT_FIRST..=last {
            assert_eq!(panes::pane_for(id), None, "{id} toggles a pane");
            assert_eq!(recent::index_of(id), None, "{id} opens a recent file");
        }
    }

    #[test]
    fn right_click_menu_commands_edit_and_play() {
        let mut st = State {
            editor: Some(Editor::new(60_000)),
            rate: Some(1000),
            ..State::default()
        };
        if let Some(e) = &mut st.editor {
            e.select(10_000, 20_000);
        }
        st.menu_actions = vec![
            context_menu::Action::Split(vec![10_000, 20_000]),
            context_menu::Action::PlayFrom(5_000),
        ];
        assert_eq!(context_command(&mut st, 0), None);
        assert_eq!(
            st.editor.as_ref().map(|e| e.splits().to_vec()),
            Some(vec![10_000, 20_000])
        );
        // Playing needs a file; the message says so instead of panicking.
        assert_eq!(
            context_command(&mut st, 1).as_deref(),
            Some("open a file first")
        );
        assert_eq!(context_command(&mut st, 2), None, "no such entry");
    }

    #[test]
    fn playing_a_track_end_explains_why_nothing_plays() {
        let mut st = State::default();
        assert_eq!(
            st.play_track_end(End::Start).as_deref(),
            Some("open a file first")
        );
        st.editor = Some(Editor::new(60_000));
        st.rate = Some(1000);
        if let Some(e) = &mut st.editor {
            e.select_all();
            e.delete_selection();
        }
        assert_eq!(
            st.play_track_end(End::End).as_deref(),
            Some("all of it is deleted; nothing to play")
        );
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

/// The Windows resource script that `build.rs` compiles, included here so
/// that its unit tests run on every platform.
#[cfg(test)]
#[path = "../build/resources.rs"]
mod resources;
