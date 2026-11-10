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

#[cfg(test)]
mod hotkeys;
mod icons;
mod lanes;
mod log_pane;
mod logo;
mod minimap;
mod options;
mod panes;
mod position_bar;
mod recent;
mod settings;
mod surface;
mod tints;
mod toolbar;
mod track_list;

use std::cell::RefCell;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use icons::Icon;
use lac_analysis::{Detection, Features, RegionKind};
use lac_app::analysis_job::{AnalysisJob, AnalysisStatus};
use lac_app::autosave::{Autosaver, Found, Recovery, Step};
use lac_app::chapters::{Chapter, SuggestedStart, suggested_starts};
use lac_app::context_menu::{self, Effect, Entry, RightPress};
use lac_app::control_surface::{self, Command as SurfaceCommand};
use lac_app::detail::{Detail, DetailLoader};
use lac_app::editor::{Editor, Edits};
use lac_app::export_job::{ExportJob, default_output, describe_outcomes, track_tasks};
use lac_app::format_samples;
use lac_app::hover;
use lac_app::i18n::{n_, tr, tr_menu, trf};
use lac_app::launch::GuiArgs;
use lac_app::layout::Pane;
use lac_app::metrics::Metrics;
use lac_app::minimap::Minimap;
use lac_app::peak_cache::PeakCache;
use lac_app::peak_job::{JobStatus, PeakJob};
use lac_app::perspective::Perspective;
use lac_app::playback::End;
use lac_app::project::Project;
use lac_app::recent_files::RecentFiles;
use lac_app::theme::{ListPalette, Theme};
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
/// File > Export selection (issue #101); after the perspectives.
const ID_EXPORT_SELECTION: Id = ID_HIGHEST + 36;
/// File > Save project and Save project as (issue #97).
const ID_SAVE_PROJECT: Id = ID_HIGHEST + 37;
const ID_SAVE_PROJECT_AS: Id = ID_HIGHEST + 38;
/// Edit > Move track up and down in the list's order (issue #85).
const ID_MOVE_UP: Id = ID_HIGHEST + 39;
const ID_MOVE_DOWN: Id = ID_HIGHEST + 40;
/// Play > Control surface (issue #95).
const ID_CONTROL_SURFACE: Id = ID_HIGHEST + 41;
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
/// Edit > Settings (issue #103); its id was Edit > Suggest chained
/// streams', which moved into the dialog.
const ID_SETTINGS: Id = ID_HIGHEST + 27;
const ID_PLAY_TRACK_START: Id = ID_HIGHEST + 28;
const ID_PLAY_TRACK_END: Id = ID_HIGHEST + 29;
/// View > Perspective: entry `i` of [`Perspective::ALL`] has id
/// `ID_PERSPECTIVE_FIRST + i` (up to `ID_HIGHEST + 35`).
const ID_PERSPECTIVE_FIRST: Id = ID_HIGHEST + 30;
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
        tip: n_("Open an audio file (Ctrl+O)"),
    },
    Tool::Button {
        id: ID_EXPORT,
        icon: Icon::Export,
        tip: n_("Export the kept parts (Ctrl+E)"),
    },
    Tool::Gap,
    Tool::Button {
        id: ID_UNDO,
        icon: Icon::Undo,
        tip: n_("Undo (Ctrl+Z)"),
    },
    Tool::Button {
        id: ID_REDO,
        icon: Icon::Redo,
        tip: n_("Redo (Ctrl+Y)"),
    },
    Tool::Gap,
    Tool::Button {
        id: ID_DELETE,
        icon: Icon::Delete,
        tip: n_("Delete the selection (Del)"),
    },
    Tool::Button {
        id: ID_RESTORE,
        icon: Icon::Restore,
        tip: n_("Restore deleted parts in the selection (Ctrl+R)"),
    },
    Tool::Button {
        id: ID_SPLIT,
        icon: Icon::Split,
        tip: n_("Start a new track here (Ctrl+K)"),
    },
    Tool::Gap,
    Tool::Button {
        id: ID_TO_START,
        icon: Icon::ToStart,
        tip: n_("Cursor to the start (Home)"),
    },
    Tool::Button {
        id: ID_PLAY,
        icon: Icon::Play,
        tip: n_("Play or stop (Space)"),
    },
    Tool::Button {
        id: ID_PLAY_TRACK_START,
        icon: Icon::PlayTrackStart,
        tip: n_("Play the first 3 seconds of the track (Ctrl+Shift+B)"),
    },
    Tool::Button {
        id: ID_PLAY_TRACK_END,
        icon: Icon::PlayTrackEnd,
        tip: n_("Play the last 3 seconds of the track (Ctrl+Shift+E)"),
    },
    Tool::Speed {
        tip: n_("Playback speed (Ctrl+Up, Ctrl+Down; Ctrl+Shift+N for normal)"),
    },
    Tool::Gap,
    Tool::Button {
        id: ID_ZOOM_IN,
        icon: Icon::ZoomIn,
        tip: n_("Zoom in (Ctrl++)"),
    },
    Tool::Button {
        id: ID_ZOOM_OUT,
        icon: Icon::ZoomOut,
        tip: n_("Zoom out (Ctrl+-)"),
    },
    Tool::Button {
        id: ID_ZOOM_FIT,
        icon: Icon::ZoomFit,
        tip: n_("Show the whole file (Ctrl+0)"),
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
/// The position being played.
const PLAYHEAD: Colour = Colour::rgb(0x6c, 0xe0, 0x7c);
/// Strip under a suggested pause that is silent.
const SUGGESTED_SILENCE: Colour = Colour::rgb(0x5c, 0x9c, 0x8c);
/// Strip under a suggested pause that is only noise.
const SUGGESTED_NOISE: Colour = Colour::rgb(0xc0, 0x90, 0x50);
/// Tick at a suggested track start (a dimmer [`TRACK`]).
const SUGGESTED_SPLIT: Colour = Colour::rgb(0x90, 0x60, 0xa8);

/// An export in progress.
struct RunningExport {
    job: ExportJob,
    /// The file being cut; another one may be open by the time it ends.
    source: PathBuf,
    /// The edits being exported (see [`Editor::mark_exported`]); `None`
    /// for an export of the selection, which does not save the edits.
    edits: Option<Edits>,
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
    /// What the analysis measured, for detecting again with other
    /// settings without decoding again (issue #92).
    features: Option<Arc<Features>>,
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
    /// The track list's colours, for light or dark windows (fixed at
    /// start, like the theme).
    list_palette: ListPalette,
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
    /// The project file the edits were opened from or saved to last
    /// (issue #97); File > Save project writes there.
    project_path: Option<PathBuf>,
    /// A project being opened: its edits and view wait until the
    /// recording's length is known.
    pending_project: Option<Project>,
    /// Why the project's edits may not fit the recording, for the status
    /// bar once it is read.
    project_note: String,
    /// Where recovery copies of unsaved edits are kept (crash recovery,
    /// issue #97); `None` without a home folder, or in tests.
    recovery: Option<Recovery>,
    /// The open recording's absolute path: the recovery copy's key.
    audio_key: Option<PathBuf>,
    /// When the recovery copy is written or removed.
    autosaver: Autosaver,
    /// A recovery copy found for the recording just read, with other
    /// edits than it was opened with: offered by the timer.
    pending_recovery: Option<Found>,
    /// Restore the copy without asking: the user already chose to at
    /// start.
    restore_silently: bool,
    /// An autosave failed and was reported; not again for this file.
    autosave_failed: bool,
    /// MIDI control surfaces, connected while Play > Control surface is
    /// on (issue #95).
    surfaces: surface::Surfaces,
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
    /// Whether the pointer shows that the track boundary under it can be
    /// dragged, so its shape is only set on changes.
    pointer_on_boundary: bool,
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
        // The previous file's edits were saved or discarded (see
        // `confirm_discard`), so its recovery copy goes.
        self.drop_recovery();
        self.audio_key = Some(std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone()));
        self.autosaver = Autosaver::default();
        self.pending_recovery = None;
        self.autosave_failed = false;
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
            self.options.detector.params(),
        ));
        self.analysis_seen = None;
        self.analysis_percent = 0;
        self.analysis_error = None;
        self.detection = None;
        self.features = None;
        self.tags = None;
        self.chapters = Arc::default();
        self.project_path = None;
        self.pending_project = None;
        self.project_note = String::new();
    }

    /// Removes the open recording's recovery copy: its edits are saved or
    /// discarded.
    fn drop_recovery(&mut self) {
        if let (Some(r), Some(audio)) = (&self.recovery, &self.audio_key) {
            let _ = r.remove(audio);
        }
        self.autosaver = Autosaver::default();
    }

    /// The recovery copy of the recording just read, if it holds other
    /// edits than those it was opened with; else none is offered, and the
    /// saver knows about the copy.
    fn find_recovery(&mut self) {
        let (Some(r), Some(audio), Some(ed)) = (&self.recovery, &self.audio_key, &self.editor)
        else {
            return;
        };
        let found = r.load(audio);
        let mut restored = ed.clone();
        match found {
            Some(f) if restored.replace_edits(f.project.edits.clone()) => {
                self.pending_recovery = Some(f);
            }
            other => {
                if let Some(f) = other {
                    self.autosaver = Autosaver::with_copy(f.project.edits);
                }
                // Nothing to restore after all.
                self.restore_silently = false;
            }
        }
    }

    /// Restores the edits of a recovery copy (one undoable step; they
    /// count as unsaved) or, if not `restore`, throws the copy away.
    fn use_recovery(&mut self, found: Found, restore: bool) -> String {
        if !restore {
            self.drop_recovery();
            return tr("discarded the kept edits").into_owned();
        }
        let Some(ed) = &mut self.editor else {
            return String::new();
        };
        ed.replace_edits(found.project.edits.clone());
        self.autosaver = Autosaver::with_copy(found.project.edits);
        tr("restored the edits kept after the program ended unexpectedly").into_owned()
    }

    /// Timer: writes or removes the recovery copy of the open recording's
    /// edits (see [`Autosaver::step`]). Returns an error to show, once per
    /// file.
    fn autosave(&mut self) -> Option<String> {
        if self.pending_recovery.is_some() {
            // The copy is being offered; writing would overwrite it.
            return None;
        }
        let (Some(r), Some(audio), Some(ed)) = (&self.recovery, &self.audio_key, &self.editor)
        else {
            return None;
        };
        let result =
            match self
                .autosaver
                .step(ed.edits(), ed.has_unexported_changes(), Instant::now())
            {
                Step::Keep => Ok(()),
                Step::Remove => r.remove(audio),
                Step::Write(edits) => r.store(&Project {
                    audio: audio.clone(),
                    frames: Some(ed.total()),
                    edits,
                    view: self.view.as_ref().map(WaveformView::visible),
                }),
            };
        match result {
            Err(e) if !self.autosave_failed => {
                self.autosave_failed = true;
                Some(trf(
                    "cannot keep a recovery copy of the edits in {dir}: {e}",
                    &[("dir", &r.dir().display()), ("e", &e)],
                ))
            }
            _ => None,
        }
    }

    /// Opens the recording of `project` with its edits, which are applied
    /// once the recording's length is known ([`State::poll`]).
    fn open_project(&mut self, path: PathBuf, project: Project) {
        self.open(project.audio.clone());
        self.project_path = Some(path);
        self.pending_project = Some(project);
    }

    /// Notes for the status bar after reading a file: chained streams,
    /// a project that does not fit the recording.
    fn open_notes(&self) -> String {
        format!("{}{}", self.chapter_note(), self.project_note)
    }

    /// An editor for a recording of `total` frames with `project`'s edits;
    /// shows the project's part of the waveform. Notes a recording whose
    /// length changed since the project was saved.
    fn editor_from(&mut self, project: Project, total: SamplePos) -> Editor {
        if project.frames.is_some_and(|f| f != total) {
            self.project_note =
                tr("; the recording's length changed since the project was saved, check the edits")
                    .into_owned();
        }
        if let (Some(v), Some(range)) = (&mut self.view, &project.view) {
            v.zoom_exactly_to(&(range.start.min(total)..range.end.min(total)));
        }
        Editor::with_edits(total, project.edits)
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
            (n, true) => trf(
                "; {n} chained streams, offered as track starts (Edit > Split at suggested track starts)",
                &[("n", &n)],
            ),
            (n, false) => trf(
                "; {n} chained streams, not offered as track starts (Edit > Settings > Detection)",
                &[("n", &n)],
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
            return Err(tr("open a file first").into_owned());
        };
        let tl = lac_app::playback::timeline(editor);
        if tl.is_empty() {
            return Err(tr("everything is deleted; nothing to play").into_owned());
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
            return Some(tr("open a file first").into_owned());
        };
        let tl = lac_app::playback::track_excerpt(ed, ed.play_from(), end, rate);
        self.play_excerpt(tl).err()
    }

    /// Plays `tl` from its start and stops at its end: an excerpt from
    /// the right-click menu. Space plays the edits again afterwards.
    fn play_excerpt(&mut self, tl: Timeline) -> Result<(), String> {
        if tl.is_empty() {
            return Err(tr("all of it is deleted; nothing to play").into_owned());
        }
        self.play_timeline(tl, 0)
    }

    /// Plays timeline `tl` from timeline position `start`, opening the
    /// player on first use.
    fn play_timeline(&mut self, tl: Timeline, start: SamplePos) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Err(tr("open a file first").into_owned());
        };
        match &mut self.player {
            Some(p) => p.engine_mut().set_timeline(tl.clone(), start),
            None => match lac_app::playback::open_player(path, tl.clone()) {
                Ok(mut p) => {
                    p.engine_mut().set_speed(self.speed);
                    p.engine_mut().seek(start);
                    self.player = Some(p);
                }
                Err(e) => return Err(trf("cannot play: {e}", &[("e", &e)])),
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
            (true, _) => String::new(),
            (false, true) => tr(" (pitch kept)").into_owned(),
            (false, false) => tr(" (pitch follows the speed)").into_owned(),
        };
        trf(
            "speed {speed}{how}",
            &[("speed", &speed.label()), ("how", &how)],
        )
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
            (_, Some(e)) => trf(
                "{speed}cursor {pos}",
                &[
                    ("speed", &speed),
                    ("pos", &format_samples(e.cursor(), rate)),
                ],
            ),
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
            && let (Some(e), Some(edits)) = (&mut self.editor, done.edits)
        {
            e.mark_exported(edits);
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

    /// What the analysis suggests, for the status bar.
    fn suggestion_summary(&self) -> String {
        let pauses = self.detection.as_ref().map_or(0, |d| d.regions.len());
        // Counts a chained file's link starts too.
        trf(
            "found {pauses} pause(s), {starts} track start(s) (Edit menu)",
            &[
                ("pauses", &pauses),
                ("starts", &self.suggested_starts().len()),
            ],
        )
    }

    /// Detects pauses and track starts again from the stored features
    /// with the current settings; takes milliseconds. Nothing to do
    /// before the analysis is done.
    fn redetect(&mut self) {
        if let Some(f) = &self.features {
            self.detection = Some(Arc::new(lac_analysis::detect(
                f,
                &self.options.detector.params(),
            )));
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
            AnalysisStatus::Done {
                detection,
                features,
                ..
            } => {
                self.analysis = None;
                self.detection = Some(detection);
                self.features = Some(features);
                // The settings may have changed while it ran.
                self.redetect();
                Some(self.suggestion_summary())
            }
            AnalysisStatus::Failed(e) => {
                self.analysis = None;
                let msg = trf(
                    "could not look for pauses and track starts: {e}",
                    &[("e", &e)],
                );
                self.analysis_error = Some(e);
                Some(msg)
            }
        }
    }

    /// Edit > Split at suggested track starts / Delete suggested pauses,
    /// each one undoable step. Returns the message for the status bar.
    fn apply_suggestions(&mut self, id: Id) -> String {
        if self.editor.is_none() {
            return tr("open a file first").into_owned();
        }
        // A chained file's links are known from the start; the analysis'
        // suggestions join them when it is done.
        let have_chapters = id == ID_SUGGESTED_SPLITS && !self.chapters_in_use().is_empty();
        if self.detection.is_none() && !have_chapters {
            return match &self.analysis_error {
                Some(err) => trf("no suggestions: {err}", &[("err", err)]),
                None => trf(
                    "still looking for pauses and track starts ({percent} %)",
                    &[("percent", &self.analysis_percent)],
                ),
            };
        }
        let starts = self.suggested_starts();
        let tags = self.tags.clone().unwrap_or_default();
        let Some(e) = &mut self.editor else {
            return tr("open a file first").into_owned();
        };
        if id == ID_SUGGESTED_SPLITS {
            match e.split_at_suggestions(&starts, &tags) {
                0 => tr("no new track starts suggested").into_owned(),
                n => trf(
                    "added {n} track start(s); Ctrl+Z takes them back",
                    &[("n", &n)],
                ),
            }
        } else if let Some(d) = &self.detection {
            match e.delete_ranges(d.regions.iter().map(|r| r.range.clone())) {
                0 => tr("no suggested pauses left to delete").into_owned(),
                n => trf(
                    "deleted {length} of pauses; Ctrl+Z takes them back",
                    &[(
                        "length",
                        &self.rate.map_or_else(
                            || trf("{n} samples", &[("n", &n)]),
                            |r| format_samples(n, r),
                        ),
                    )],
                ),
            }
        } else {
            // Ruled out above: pauses come from the analysis only.
            tr("no suggested pauses left to delete").into_owned()
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
                self.editor = Some(match self.pending_project.take() {
                    Some(project) => self.editor_from(project, p.total_samples()),
                    None => Editor::new(p.total_samples()),
                });
                self.find_recovery();
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
                trf(
                    "{name}: reading… {percent} %",
                    &[("name", &name), ("percent", &pct)],
                )
            }
            JobStatus::Done(report) if report.warnings.is_empty() => {
                self.job = None;
                let note = self.open_notes();
                if u.cached {
                    trf(
                        "{name} (waveform from cache){note}",
                        &[("name", &name), ("note", &note)],
                    )
                } else {
                    format!("{name}{note}")
                }
            }
            JobStatus::Done(report) => {
                self.job = None;
                format!(
                    "{name}: {}{}",
                    report.warnings.join("; "),
                    self.open_notes()
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
        // The language first: every text from here on is translated.
        let options = Options::load();
        use_language(options.language);
        // Before the first window, which wxWidgets cannot switch later.
        let dark = use_theme(options.theme);
        icons::use_dark_ink(dark);
        let list_palette = ListPalette::new(dark);
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
        frame.set_menu_bar(menu_bar(options));
        let recent_files = recent::load();
        if let Some(bar) = frame.get_menu_bar() {
            recent::refresh(&bar, &recent_files);
        }

        let panel = Panel::builder(&frame).build();
        // We paint every pixel ourselves; letting the system erase first
        // would flicker.
        panel.set_background_style(BackgroundStyle::Paint);
        let grid = track_list::create(&frame, &list_palette);

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
            list_palette,
            recent: recent_files,
            grid: Some(grid),
            recovery: Recovery::default_dir().map(Recovery::new),
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
                        report(&frame, &tr("only one file at a time; opening the first"));
                    }
                    if confirm_discard(&frame, &state) {
                        open_file(&frame, &state, first.clone());
                    }
                }
                let surfaces = {
                    let mut st = state.borrow_mut();
                    let on = st.options.control_surface;
                    st.surfaces.poll(on)
                };
                for msg in &surfaces.messages {
                    report(&frame, msg);
                }
                for cmd in surfaces.commands {
                    surface_command(&frame, &panel, &state, cmd);
                }
                let held = state.borrow_mut().poll_right_press();
                if let Some(msg) = held {
                    if panel.has_capture() {
                        panel.release_mouse();
                    }
                    report(&frame, &msg);
                }
                let (status, detail, export, playing, analysis, autosave) = {
                    let mut st = state.borrow_mut();
                    (
                        st.poll(),
                        st.poll_detail(),
                        st.poll_export(),
                        st.poll_player(),
                        st.poll_analysis(),
                        st.autosave(),
                    )
                };
                if let Some(msg) = &autosave {
                    report(&frame, msg);
                }
                offer_recovery(&frame, &state);
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

        let opened = args.file.is_some();
        if let Some(path) = args.file {
            open_file(&frame, &state, path);
        }
        frame.show(true);
        frame.centre();
        if panes::stored_geometry().maximized {
            frame.maximize(true);
        }
        if !opened {
            offer_crashed_edits(&frame, &state);
        }
    });
}

/// File and View menus.
fn menu_bar(options: Options) -> MenuBar {
    let file = Menu::builder()
        .append_item(
            ID_OPEN,
            &tr_menu("&Open…\tCtrl+O"),
            &tr("Open an audio file or a project"),
        )
        .append_item(
            ID_SAVE_PROJECT,
            &tr_menu("Save &project\tCtrl+S"),
            &tr("Save the edits, tracks and tags to continue later"),
        )
        .append_item(
            ID_SAVE_PROJECT_AS,
            &tr_menu("Save project &as…\tCtrl+Shift+S"),
            &tr("Save the edits, tracks and tags in a new project file"),
        )
        .append_separator()
        .append_item(
            ID_EXPORT,
            &tr_menu("&Export…\tCtrl+E"),
            &tr("Write the kept parts to a new file, without re-encoding"),
        )
        .append_item(
            ID_EXPORT_SELECTION,
            &tr_menu("Export &selection…\tCtrl+Alt+E"),
            &tr("Write what is kept of the selection to a new file, without re-encoding"),
        )
        .append_separator()
        .append_item(
            ID_CLEAR_CACHE,
            &tr_menu("&Clear waveform cache"),
            &tr("Delete the stored waveforms; files are decoded again when opened"),
        )
        .append_separator()
        .append_item(
            ID_EXIT,
            &tr_menu("&Quit\tCtrl+Q"),
            &tr("Quit losslessaudiocut"),
        )
        .build();
    // The builder cannot add submenus; right below Open, like elsewhere.
    file.insert_submenu(
        1,
        recent::submenu(),
        &tr_menu("Open &recent"),
        &tr("Open one of the recently opened files"),
    );
    let edit = Menu::builder()
        .append_item(
            ID_UNDO,
            &tr_menu("&Undo\tCtrl+Z"),
            &tr("Undo the last delete or restore"),
        )
        .append_item(
            ID_REDO,
            &tr_menu("&Redo\tCtrl+Y"),
            &tr("Redo what was undone"),
        )
        .append_separator()
        .append_item(
            ID_DELETE,
            &tr_menu("&Delete selection\tDel"),
            &tr("Leave the selection out of the export"),
        )
        .append_item(
            ID_RESTORE,
            &tr_menu("Re&store selection\tCtrl+R"),
            &tr("Take deleted parts inside the selection back in"),
        )
        .append_separator()
        .append_item(
            ID_SPLIT,
            &tr_menu("S&plit track here\tCtrl+K"),
            &tr("Start a new track at the cursor or the selection start"),
        )
        .append_item(
            ID_JOIN,
            &tr_menu("&Join with previous track\tCtrl+J"),
            &tr("Remove the split at the start of the track under the cursor"),
        )
        .append_item(
            ID_RENAME,
            &tr_menu("Re&name track…\tF2"),
            &tr("Name the track under the cursor"),
        )
        .append_item(
            ID_MOVE_UP,
            &tr_menu("Move track &up\tCtrl+Shift+Up"),
            &tr("Give the track under the cursor the place before it in the track list and the export"),
        )
        .append_item(
            ID_MOVE_DOWN,
            &tr_menu("Move track do&wn\tCtrl+Shift+Down"),
            &tr("Give the track under the cursor the place after it in the track list and the export"),
        )
        .append_item(
            ID_ALL_TRACKS,
            &tr_menu("Use for all trac&ks\tCtrl+Shift+A"),
            &tr("Give every track the value of the selected cell of the track list"),
        )
        .append_separator()
        .append_item(
            ID_SUGGESTED_SPLITS,
            &tr_menu("Split at suggested &track starts\tCtrl+Shift+K"),
            &tr("Start new tracks where the analysis found pauses or changes of sound"),
        )
        .append_item(
            ID_SUGGESTED_PAUSES,
            &tr_menu("Delete suggested &pauses\tCtrl+Shift+D"),
            &tr("Delete the silent and noise-only stretches the analysis found"),
        )
        .append_item(
            ID_SETTINGS,
            &tr_menu("Se&ttings…\tCtrl+,"),
            &tr("What the analysis suggests and how the waveform is coloured"),
        )
        .append_separator()
        .append_item(
            ID_SELECT_ALL,
            &tr_menu("Select &all\tCtrl+A"),
            &tr("Select the whole file"),
        )
        .append_item(
            ID_SELECT_NONE,
            &tr_menu("Select &none\tEsc"),
            &tr("Clear the selection"),
        )
        .build();
    let play = Menu::builder()
        .append_item(
            ID_PLAY,
            &tr_menu("&Play/Stop\tSpace"),
            &tr("Play the kept parts from the selection or the cursor, or stop"),
        )
        .append_item(
            ID_TO_START,
            &tr_menu("Cursor to &start\tHome"),
            &tr("Put the cursor at the beginning"),
        )
        .append_item(
            ID_PLAY_TRACK_START,
            &tr_menu("Play track &beginning\tCtrl+Shift+B"),
            &tr("Play the first 3 seconds of the track at the cursor or selection"),
        )
        .append_item(
            ID_PLAY_TRACK_END,
            &tr_menu("Play track &end\tCtrl+Shift+E"),
            &tr("Play the last 3 seconds of the track at the cursor or selection"),
        )
        .append_separator()
        .append_item(
            ID_FASTER,
            &tr_menu("&Faster\tCtrl+Up"),
            &tr("Play faster (up to 3×)"),
        )
        .append_item(
            ID_SLOWER,
            &tr_menu("S&lower\tCtrl+Down"),
            &tr("Play slower (down to 0.5×)"),
        )
        .append_item(
            ID_NORMAL_SPEED,
            &tr_menu("&Normal speed\tCtrl+Shift+N"),
            &tr("Play at normal speed"),
        )
        .append_check_item(
            ID_KEEP_PITCH,
            &tr_menu("&Keep pitch"),
            &tr("At other speeds keep the pitch; else it changes like a tape played faster"),
        )
        .append_check_item(
            ID_SCRUB_SELECTING,
            &tr_menu("S&crub while selecting"),
            &tr("Play the edge of the selection while dragging it"),
        )
        .append_check_item(
            ID_CONTROL_SURFACE,
            &tr_menu("Control s&urface (Mackie Control)"),
            &tr("Let a MIDI controller in Mackie Control mode play, stop, move the cursor and split"),
        )
        .build();
    let view = Menu::builder()
        .append_item(
            ID_ZOOM_IN,
            &tr_menu("Zoom &in\tCtrl++"),
            &tr("Show less time, more detail"),
        )
        .append_item(
            ID_ZOOM_OUT,
            &tr_menu("Zoom &out\tCtrl+-"),
            &tr("Show more time"),
        )
        .append_item(
            ID_ZOOM_FIT,
            &tr_menu("Zoom to &fit\tCtrl+0"),
            &tr("Show the whole file"),
        )
        .build();
    // The builder takes no submenus; the rest is appended to the menu.
    view.append_submenu(
        perspective_menu(),
        &tr_menu("&Perspective"),
        &tr("Views of the selection's edges in context"),
    );
    view.append(
        ID_RULER,
        &tr_menu("Time &ruler"),
        &tr("Show the time scale along the bottom of the waveform"),
        ItemKind::Check,
    );
    view.append_separator();
    for p in Pane::ALL {
        view.append(
            panes::menu_id(p),
            &panes::menu_label(p),
            &panes::menu_help(p),
            ItemKind::Check,
        );
    }
    let bar = MenuBar::builder()
        .append(file, &tr_menu("&File"))
        .append(edit, &tr_menu("&Edit"))
        .append(play, &tr_menu("&Play"))
        .append(view, &tr_menu("&View"))
        .build();
    bar.check_item(ID_KEEP_PITCH, options.keep_pitch);
    bar.check_item(ID_SCRUB_SELECTING, options.scrub_while_selecting);
    bar.check_item(ID_CONTROL_SURFACE, options.control_surface);
    bar.check_item(ID_RULER, options.ruler);
    bar
}

/// Menu commands.
fn bind_menu(frame: &Frame, panel: &Panel, state: &Rc<RefCell<State>>) {
    let state = Rc::clone(state);
    let (f, panel) = (*frame, *panel);
    frame.on_menu(move |e| command(&f, &panel, &state, e.get_id(), e.is_checked()));
}

/// View > Perspective: Ctrl+1 to Ctrl+6 show the whole file or the
/// selection's edges in context (issue #100).
fn perspective_menu() -> Menu {
    Perspective::ALL
        .iter()
        .enumerate()
        .fold(Menu::builder(), |m, (i, p)| {
            m.append_item(
                ID_PERSPECTIVE_FIRST + i as Id,
                &format!("{}\tCtrl+{}", tr(p.label()), i + 1),
                &trf(
                    "Zoom to this part, with {seconds} s around the selection's edges",
                    &[("seconds", &lac_app::perspective::CONTEXT_SECONDS)],
                ),
            )
        })
        .build()
}

/// The perspective that View > Perspective command `id` shows.
fn perspective_of(id: Id) -> Option<Perspective> {
    let i = usize::try_from(id.checked_sub(ID_PERSPECTIVE_FIRST)?).ok()?;
    Perspective::ALL.get(i).copied()
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
            .with_message(&tr("Open an audio file"))
            .with_wildcard(&lac_app::file_types::dialog_wildcard())
            .with_style(FileDialogStyle::Open | FileDialogStyle::FileMustExist)
            .build();
        if dialog.show_modal() == ID_OK
            && let Some(path) = dialog.get_path()
        {
            open_file(&f, state, PathBuf::from(path));
        }
    } else if let Some(p) = perspective_of(id) {
        let mut st = state.borrow_mut();
        let st = &mut *st;
        if let (Some(v), Some(ed), Some(rate)) = (&mut st.view, &st.editor, st.rate) {
            v.zoom_exactly_to(&p.range(ed.total(), rate, ed.selection(), ed.cursor()));
        }
    } else if let Some(i) = context_index(id) {
        // Exporting opens a dialog, which must not run while the state is
        // borrowed.
        let exporting =
            state.borrow().menu_actions.get(i) == Some(&context_menu::Action::ExportSelection);
        if exporting {
            export_selection(&f, state);
            return;
        }
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
    } else if id == ID_SAVE_PROJECT || id == ID_SAVE_PROJECT_AS {
        save_project(&f, state, id == ID_SAVE_PROJECT_AS);
    } else if id == ID_EXPORT_SELECTION {
        export_selection(&f, state);
    } else if id == ID_RENAME {
        rename_track(&f, state);
    } else if id == ID_ALL_TRACKS {
        use_for_all_tracks(&f, state);
    } else if id == ID_CLEAR_CACHE {
        let cache = state.borrow().cache.clone();
        let msg = match cache.map(|c| c.clear()) {
            Some(Ok(n)) => trf("waveform cache cleared ({n} entries)", &[("n", &n)]),
            Some(Err(e)) => trf("could not clear the waveform cache: {e}", &[("e", &e)]),
            None => tr("there is no waveform cache on this system").into_owned(),
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
    } else if id == ID_CONTROL_SURFACE {
        // The timer connects or disconnects at its next tick.
        let mut st = state.borrow_mut();
        let on = checked.unwrap_or(!st.options.control_surface);
        st.options.control_surface = on;
        st.options.save();
        return;
    } else if id == ID_RULER {
        let mut st = state.borrow_mut();
        let on = checked.unwrap_or(!st.options.ruler);
        st.options.ruler = on;
        st.options.save();
    } else if id == ID_SETTINGS {
        open_settings(&f, panel, state);
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
            report(&f, &tr(msg));
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
            | ID_MOVE_UP
            | ID_MOVE_DOWN
    )
}

/// Runs Edit menu command `id`. Returns `None` if `id` is not one, else a
/// message for the status bar (empty if there is nothing to say).
fn edit(st: &mut State, id: Id) -> Option<&'static str> {
    // Marked here, translated where shown.
    if !is_edit(id) {
        return None;
    }
    let Some(e) = &mut st.editor else {
        return Some(n_("open a file first"));
    };
    Some(match id {
        ID_UNDO if !e.undo() => n_("nothing to undo"),
        ID_REDO if !e.redo() => n_("nothing to redo"),
        ID_DELETE if e.selection().is_none() => n_("select a part with the mouse first"),
        ID_DELETE if !e.delete_selection() => n_("the selection is deleted already"),
        ID_RESTORE if e.selection().is_none() => n_("select a deleted part first"),
        ID_RESTORE if !e.restore_selection() => n_("nothing in the selection is deleted"),
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
        ID_SPLIT if !e.split_at(e.play_from()) => n_("click where the new track should start"),
        ID_JOIN if !e.join_with_previous(e.track_at(e.play_from())) => {
            n_("the first track has no track before it")
        }
        // The track under the cursor swaps places with its neighbour in
        // the list (issue #85); it stays where it is in the recording.
        ID_MOVE_UP | ID_MOVE_DOWN => {
            let track = e.track_at(e.play_from());
            let place = e.order().iter().position(|&t| t == track).unwrap_or(0);
            let to = if id == ID_MOVE_UP {
                place.checked_sub(1)
            } else {
                Some(place + 1)
            };
            match to {
                Some(to) if e.move_track(place, to) => "",
                _ if id == ID_MOVE_UP => n_("the track is first in the list already"),
                _ => n_("the track is last in the list already"),
            }
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

/// Light, dark or the system's windows (issue #104). Returns whether
/// they are dark. Must run before the first window is created.
fn use_theme(theme: Theme) -> bool {
    let appearance = match theme {
        Theme::System => wxdragon::appearance::Appearance::System,
        Theme::Light => wxdragon::appearance::Appearance::Light,
        Theme::Dark => wxdragon::appearance::Appearance::Dark,
    };
    // Where wxWidgets cannot switch (or forcing a theme is not supported
    // on the platform) the windows follow the system.
    let switched =
        wxdragon::app::set_appearance(appearance) == wxdragon::appearance::AppearanceResult::Ok;
    let system_dark = wxdragon::appearance::get_system_appearance().is_some_and(|a| a.is_dark());
    if switched {
        theme.is_dark(system_dark)
    } else {
        system_dark
    }
}

/// Translates the user interface into `choice` (a code of
/// `lac_app::i18n::LANGUAGES`), or the system's language for `None`:
/// our texts from the built-in catalogs, wxWidgets' own (standard
/// buttons, file dialogs) from its catalog where installed.
fn use_language(choice: Option<&str>) {
    let system = Locale::get_language_canonical_name(Locale::get_system_language());
    lac_app::i18n::set_language(choice.or(system.as_deref()));
    let wx = Translations::new();
    match choice {
        Some(code) => wx.set_language_str(code),
        None => wx.set_language(Language::Default),
    }
    // Missing on systems without wxWidgets' translations; then its own
    // texts stay English, which is no reason to complain.
    let _ = wx.add_std_catalog();
    wx.set_global();
}

/// Edit > Settings: shows the dialog; its changes show at once and are
/// stored on OK, Cancel brings back what was there before.
fn open_settings(frame: &Frame, panel: &Panel, state: &Rc<RefCell<State>>) {
    let before = state.borrow().options;
    let preview: Rc<dyn Fn(Options)> = {
        let (state, f, p) = (Rc::clone(state), *frame, *panel);
        Rc::new(move |o| {
            let Ok(mut st) = state.try_borrow_mut() else {
                return;
            };
            let redetect = st.options.detector != o.detector;
            st.options = o;
            if redetect && st.features.is_some() {
                st.redetect();
                f.set_status_text(&st.suggestion_summary(), FIELD_FILE);
            }
            update_chrome(&f, &st);
            p.refresh(false, None);
        })
    };
    let chosen = settings::show(frame, before, Rc::clone(&preview));
    preview(chosen.unwrap_or(before));
    if chosen.is_some() {
        state.borrow().options.save();
    }
}

/// Title and edit field of the status bar, after anything that may have
/// changed them.
fn update_chrome(frame: &Frame, st: &State) {
    frame.set_title(&st.title());
    frame.set_status_text(&st.edit_status(), FIELD_EDIT);
    frame.set_status_text(&st.time_status(), FIELD_TIME);
    if let Some(g) = &st.grid {
        track_list::refresh(
            g,
            &st.track_rows(),
            &st.options.appearance,
            &st.list_palette,
        );
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
            _ => Some(tr("open a file first").into_owned()),
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
        &trf(
            "{name} has edits that are neither exported nor saved yet. Discard them?",
            &[("name", &name)],
        ),
        &tr("Discard edits?"),
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
    // A project names its recording; read it before forgetting the
    // current file, so that a broken one leaves that open.
    let project = if lac_app::project::is_project(&path) {
        match read_project(&listed) {
            Ok(p) => Some(p),
            Err(e) => {
                report(
                    frame,
                    &trf(
                        "cannot open {file}: {e}",
                        &[("file", &path.display()), ("e", &e)],
                    ),
                );
                return;
            }
        }
    } else {
        None
    };
    {
        let mut st = state.borrow_mut();
        st.recent.add(listed.clone());
        remember_recent(frame, &st.recent);
        match project {
            Some(p) => st.open_project(listed, p),
            None => st.open(path),
        }
    }
    report(frame, &tr("reading…"));
    update_chrome(frame, &state.borrow());
}

/// Asks whether to restore the recovery copy found for the recording just
/// read (see [`State::find_recovery`]); restores it without asking if the
/// user chose to at start.
fn offer_recovery(frame: &Frame, state: &Rc<RefCell<State>>) {
    // Taken out first: the dialog runs the event loop, and the timer must
    // not offer it again meanwhile.
    let (found, silently, name) = {
        let mut st = state.borrow_mut();
        let Some(found) = st.pending_recovery.take() else {
            return;
        };
        let silently = std::mem::take(&mut st.restore_silently);
        (found, silently, st.name())
    };
    let restore = silently || {
        let dialog = MessageDialog::builder(
            frame,
            &trf(
                "Edits of {name} that were neither exported nor saved were kept when \
                 losslessaudiocut last ended unexpectedly. Restore them? (No discards them.)",
                &[("name", &name)],
            ),
            &tr("Restore edits?"),
        )
        .with_style(MessageDialogStyle::YesNo | MessageDialogStyle::IconQuestion)
        .build();
        dialog.show_modal() == ID_YES
    };
    let msg = state.borrow_mut().use_recovery(found, restore);
    report(frame, &msg);
    update_chrome(frame, &state.borrow());
}

/// At start without a file to open: if losslessaudiocut ended without
/// saving or discarding edits (a crash, a power cut), offers to open the
/// recording whose edits were kept last; they are then restored without
/// asking again. Declining throws that copy away; others are offered when
/// their recordings are opened.
fn offer_crashed_edits(frame: &Frame, state: &Rc<RefCell<State>>) {
    let found = {
        let st = state.borrow();
        let Some(r) = &st.recovery else { return };
        r.all().into_iter().next()
    };
    let Some(found) = found else { return };
    let audio = found.project.audio;
    let dialog = MessageDialog::builder(
        frame,
        &trf(
            "losslessaudiocut ended unexpectedly last time. Edits of {file} that were \
             neither exported nor saved were kept. Open it and restore them? (No discards them.)",
            &[("file", &audio.display())],
        ),
        &tr("Restore edits?"),
    )
    .with_style(MessageDialogStyle::YesNo | MessageDialogStyle::IconQuestion)
    .build();
    if dialog.show_modal() == ID_YES {
        state.borrow_mut().restore_silently = true;
        open_file(frame, state, audio);
    } else if let Some(r) = &state.borrow().recovery {
        let _ = r.remove(&audio);
    }
}

/// Reads the project file at `path`.
fn read_project(path: &Path) -> Result<Project, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let dir = path.parent().unwrap_or(Path::new(""));
    Project::parse(&text, dir).map_err(|e| e.to_string())
}

/// File > Save project (`ask` false) and Save project as (`ask` true):
/// writes the edits to the project file, asking for its name the first
/// time or when `ask`.
fn save_project(frame: &Frame, state: &Rc<RefCell<State>>, ask: bool) {
    let (audio, known, project) = {
        let st = state.borrow();
        let (Some(audio), Some(ed)) = (&st.path, &st.editor) else {
            report(frame, &tr("open a file first"));
            return;
        };
        let project = Project {
            audio: std::fs::canonicalize(audio).unwrap_or_else(|_| audio.clone()),
            frames: Some(ed.total()),
            edits: ed.edits().clone(),
            view: st.view.as_ref().map(WaveformView::visible),
        };
        (audio.clone(), st.project_path.clone(), project)
    };
    let target = match known.filter(|_| !ask) {
        Some(p) => p,
        None => {
            let suggested = lac_app::project::default_path(&audio);
            let dialog = FileDialog::builder(frame)
                .with_message(&tr("Save the project"))
                .with_default_dir(&dir_of(&suggested))
                .with_default_file(&file_name_of(&suggested))
                .with_wildcard(&format!(
                    "{}|*.{}",
                    tr("losslessaudiocut projects"),
                    lac_app::project::EXTENSION
                ))
                .with_style(FileDialogStyle::Save | FileDialogStyle::OverwritePrompt)
                .build();
            if dialog.show_modal() != ID_OK {
                return;
            }
            let Some(p) = dialog.get_path().map(PathBuf::from) else {
                return;
            };
            // GTK does not add the extension of the filter.
            if lac_app::project::is_project(&p) {
                p
            } else {
                p.with_extension(lac_app::project::EXTENSION)
            }
        }
    };
    let dir = target.parent().unwrap_or(Path::new(""));
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    match std::fs::write(&target, project.to_text(&dir)) {
        Ok(()) => {
            let mut st = state.borrow_mut();
            if let Some(ed) = &mut st.editor {
                ed.mark_exported(project.edits);
            }
            let listed = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
            st.recent.add(listed.clone());
            remember_recent(frame, &st.recent);
            st.project_path = Some(listed);
            drop(st);
            report(
                frame,
                &trf("saved {file}", &[("file", &file_name_of(&target))]),
            );
        }
        Err(e) => report(
            frame,
            &trf(
                "cannot save {file}: {e}",
                &[("file", &target.display()), ("e", &e)],
            ),
        ),
    }
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
            &trf(
                "{file} no longer exists; removed it from the list",
                &[("file", &path.display())],
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
            report(frame, &tr("an export is still running"));
            return;
        }
        match (&st.path, &st.editor) {
            (Some(p), Some(e)) => (p.clone(), e.clone()),
            _ => {
                report(frame, &tr("open a file first"));
                return;
            }
        }
    };
    let source_dir = dir_of(&source);
    let (tasks, skipped) = if editor.splits().is_empty() {
        // One file: ask for its name.
        let dialog = FileDialog::builder(frame)
            .with_message(&tr("Export the kept parts"))
            .with_default_dir(&source_dir)
            .with_default_file(&file_name_of(&default_output(&source)))
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
        let dialog = DirDialog::builder(
            frame,
            &tr("Export the tracks into this folder"),
            &source_dir,
        )
        .build();
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
                &trf(
                    "{existing} of the {count} track files exist already in {folder}. Replace them?",
                    &[
                        ("existing", &existing),
                        ("count", &tasks.len()),
                        ("folder", &dir.display()),
                    ],
                ),
                &tr("Replace files?"),
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
        report(frame, &tr("everything is deleted; nothing to export"));
        return;
    }
    let edits = editor.edits().clone();
    start_export(frame, state, source, tasks, &skipped, Some(edits));
}

/// File > Export selection, or the waveform menu's "Export selection as…"
/// (issue #101): asks for a file name and writes what is kept of the
/// selection to it. The edits do not count as saved by it.
fn export_selection(frame: &Frame, state: &Rc<RefCell<State>>) {
    let (source, editor) = {
        let st = state.borrow();
        if st.export.is_some() {
            report(frame, &tr("an export is still running"));
            return;
        }
        match (&st.path, &st.editor) {
            (Some(p), Some(e)) => (p.clone(), e.clone()),
            _ => {
                report(frame, &tr("open a file first"));
                return;
            }
        }
    };
    let Some(sel) = editor.selection() else {
        report(frame, &tr("select something to export first"));
        return;
    };
    // Checked before asking for a name, which would be in vain.
    if lac_app::export_job::selection_task(&editor, sel.clone(), "").is_none() {
        report(
            frame,
            &tr("the selection is deleted entirely; nothing to export"),
        );
        return;
    }
    let default = lac_app::export_job::selection_output(&source);
    let dialog = FileDialog::builder(frame)
        .with_message(&tr("Export the selection"))
        .with_default_dir(&dir_of(&source))
        .with_default_file(&file_name_of(&default))
        .with_style(FileDialogStyle::Save | FileDialogStyle::OverwritePrompt)
        .build();
    if dialog.show_modal() != ID_OK {
        return;
    }
    let Some(output) = dialog.get_path().map(PathBuf::from) else {
        return;
    };
    let Some(task) = lac_app::export_job::selection_task(&editor, sel, output) else {
        return;
    };
    start_export(frame, state, source, vec![task], &[], None);
}

/// The file name of `p`, for dialogs and messages.
fn file_name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The folder `p` is in, for dialogs.
fn dir_of(p: &Path) -> String {
    p.parent()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Starts writing `tasks` from `source` in the background and says so;
/// `skipped` are the numbers of tracks deleted entirely, `edits` what a
/// successful export saves (see [`RunningExport::edits`]).
fn start_export(
    frame: &Frame,
    state: &Rc<RefCell<State>>,
    source: PathBuf,
    tasks: Vec<lac_app::export_job::ExportTask>,
    skipped: &[usize],
    edits: Option<Edits>,
) {
    let mut msg = match tasks.as_slice() {
        [one] => trf(
            "exporting to {file}…",
            &[("file", &file_name_of(&one.output))],
        ),
        many => trf("exporting {n} tracks…", &[("n", &many.len())]),
    };
    if !skipped.is_empty() {
        let list: Vec<String> = skipped.iter().map(ToString::to_string).collect();
        msg += &trf(
            " (track {tracks} deleted entirely, no file)",
            &[("tracks", &list.join(", "))],
        );
    }
    report(frame, &msg);
    let job = ExportJob::start_all(&source, tasks);
    state.borrow_mut().export = Some(RunningExport { job, source, edits });
}

/// Edit > Rename track: asks for the name of the track under the cursor.
fn rename_track(frame: &Frame, state: &Rc<RefCell<State>>) {
    let Some((index, number, current)) = state.borrow().editor.as_ref().map(|e| {
        let i = e.track_at(e.play_from());
        let name = e.tracks().get(i).and_then(|t| t.name.clone());
        (i, e.track_number(i), name.unwrap_or_default())
    }) else {
        report(frame, &tr("open a file first"));
        return;
    };
    // No borrow while the dialog runs the event loop.
    let dialog = TextEntryDialog::builder(
        frame,
        &trf(
            "Name of track {n} (also used in its file name):",
            &[("n", &number)],
        ),
        &tr("Rename track"),
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
                let Some(index) = row_track(&st, row) else {
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
            if let (Some(index), Some(ed)) = (row_track(&st, e.get_row()), &st.editor) {
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

/// The track shown in row `row` of the track list (rows follow the list's
/// order, which the user may change, issue #85); `None` for a label row
/// (-1) or no track.
fn row_track(st: &State, row: i32) -> Option<usize> {
    let row = usize::try_from(row).ok()?;
    lac_app::track_list::track_of_row(st.editor.as_ref()?, row)
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

/// Runs what a control surface asked for (issue #95): the menu command of
/// the same name where there is one, else a cursor move like a click.
fn surface_command(f: &Frame, panel: &Panel, state: &Rc<RefCell<State>>, cmd: SurfaceCommand) {
    let playing = state
        .borrow()
        .player
        .as_ref()
        .is_some_and(|p| p.engine().is_playing());
    let id = match cmd {
        // Play/Stop toggles; the surface has a button for each.
        SurfaceCommand::Play if !playing => Some(ID_PLAY),
        SurfaceCommand::Stop if playing => Some(ID_PLAY),
        SurfaceCommand::Play | SurfaceCommand::Stop => return,
        SurfaceCommand::ZoomIn => Some(ID_ZOOM_IN),
        SurfaceCommand::ZoomOut => Some(ID_ZOOM_OUT),
        SurfaceCommand::Split => Some(ID_SPLIT),
        SurfaceCommand::Undo => Some(ID_UNDO),
        _ => None,
    };
    if let Some(id) = id {
        command(f, panel, state, id, None);
        return;
    }
    {
        let mut st = state.borrow_mut();
        let target = match (&st.editor, &st.view, st.rate) {
            (Some(e), Some(v), Some(rate)) => {
                let starts = control_surface::track_starts(e.tracks().into_iter().map(|t| t.span));
                control_surface::cursor_target(
                    cmd,
                    e.cursor(),
                    e.total(),
                    rate,
                    v.samples_per_pixel(),
                    &starts,
                )
            }
            _ => None,
        };
        let Some(to) = target else { return };
        jump_to(&mut st, to);
        update_chrome(f, &st);
        st.show_positions();
    }
    panel.refresh(false, None);
}

/// Selects track `row` of the track list (0-based; anything else, such as
/// -1 for a column label, is ignored), puts the cursor at its start and
/// zooms the waveform to it.
fn show_track(row: i32, frame: &Frame, panel: &Panel, state: &Rc<RefCell<State>>) {
    // Busy elsewhere (e.g. a refresh in progress): the click is dropped.
    let Ok(mut st) = state.try_borrow_mut() else {
        return;
    };
    let Some(index) = row_track(&st, row) else {
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
        let index = row_track(&st, row);
        match (&mut st.editor, track_list::field(col), index) {
            (None, _, _) => Some(tr("open a file first").into_owned()),
            (Some(ed), Some(field), Some(index)) => {
                let text = g.get_cell_value(row, col);
                match lac_app::track_list::edit(ed, &tags, index, field, &text, true) {
                    Ok(_) => Some(trf(
                        "{field} of every track: {text}",
                        &[("field", &tr(field.label())), ("text", &text)],
                    )),
                    Err(e) => Some(e.to_string()),
                }
            }
            _ => Some(tr("select a tag in the track list first").into_owned()),
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
        // Saved or discarded: a normal end keeps no recovery copy.
        state.borrow_mut().drop_recovery();
        let running = state.borrow_mut().export.take();
        if let Some(r) = running {
            report(&f, &tr("finishing the export…"));
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

/// Sets the waveform's tooltip to what is at pixel (`x`, `y`) (issue #93;
/// the texts come from [`lac_app::hover`]). Only changes are passed on,
/// so the tooltip does not flicker while the pointer moves over one mark.
fn update_tooltip(st: &mut State, panel: &Panel, x: f64, y: i32) {
    let tip = with_scene(st, |scene, view| {
        let height = panel.get_client_size().height;
        let ruler = st.ruler_height.get();
        let metrics = metrics(panel);
        let zone = metrics.zone(y, height, ruler);
        let (pos, near) = pointer_at(view, x, &metrics);
        hover::tooltip(scene, pos, zone, near)
    })
    .flatten()
    .unwrap_or_default();
    if tip != st.tooltip {
        panel.set_tooltip(&tip);
        st.tooltip = tip;
    }
}

/// Shows a left-right arrow as the pointer over a track boundary, which
/// can be dragged, and the normal pointer elsewhere.
fn update_pointer_shape(st: &mut State, panel: &Panel, x: f64) {
    let on_boundary = match (&st.view, &st.editor) {
        (Some(v), Some(ed)) => {
            let (pos, near) = pointer_at(v, x, &metrics(panel));
            ed.track_start_near(pos, near).is_some()
        }
        _ => false,
    };
    if on_boundary != st.pointer_on_boundary {
        st.pointer_on_boundary = on_boundary;
        let stock = if on_boundary {
            StockCursor::SizeWE
        } else {
            StockCursor::Arrow
        };
        panel.set_cursor(Cursor::from_stock(stock).as_ref());
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
/// (in samples) a mark still counts as under the pointer
/// ([`Metrics::tooltip_reach`] pixels).
fn pointer_at(view: &WaveformView, x: f64, metrics: &Metrics) -> (SamplePos, SamplePos) {
    let pos = view.x_to_sample(x.clamp(0.0, f64::from(view.width())));
    let near = (metrics.tooltip_reach() * view.samples_per_pixel()).ceil() as SamplePos;
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
                let (_, near) = pointer_at(v, x, &metrics(&p));
                let boundary = ed.track_start_near(pos, near);
                if ev.shift_down() {
                    ed.begin_extend(pos);
                } else if let Some(index) = boundary {
                    // On a track boundary: drag it (issue #84).
                    ed.begin_moving_track_start(index);
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
                let held = st
                    .editor
                    .as_ref()
                    .is_some_and(|ed| ed.dragging() || ed.moving_track_start().is_some());
                if !held {
                    // Just moving: explain what is under the pointer, and
                    // show that a track boundary can be dragged.
                    let y = e_y(&e);
                    update_tooltip(st, &p, x, y);
                    update_pointer_shape(st, &p, x);
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
                let moved = if ed.moving_track_start().is_some() {
                    ed.move_track_start_to(pos);
                    true
                } else {
                    ed.drag_to(pos);
                    ed.selection().is_some()
                };
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
                let pos = v.x_to_sample(x.clamp(0.0, f64::from(v.width())));
                if ed.moving_track_start().is_some() {
                    ed.move_track_start_to(pos);
                    if !ed.end_moving_track_start() {
                        // Not moved: a plain click near the boundary puts
                        // the cursor.
                        ed.begin_drag(pos);
                        ed.end_drag();
                    }
                }
                if ed.dragging() {
                    // A click without movement ends with an empty range,
                    // i.e. no selection.
                    ed.drag_to(pos);
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
                let slop = metrics(&p).click_slop();
                st.right_press = Some(RightPress::new(x, y, Instant::now(), slop));
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
        let (pos, near) = pointer_at(view, x, &metrics(panel));
        context_menu::waveform(scene, pos, near)
    });
    match entries {
        Some(entries) => popup(panel, state, entries),
        None => report(frame, &tr("open a file first")),
    }
}

/// Opens the track list's right-click menu for row `row` (see
/// [`context_menu::track_list`]); anything but a track's row (such as -1
/// for a column label) is ignored.
fn show_track_menu(grid: &Grid, state: &Rc<RefCell<State>>, row: i32) {
    let Some(index) = row_track(&state.borrow(), row) else {
        return;
    };
    // Tracks of the rows the user selected (by dragging, or Shift- or
    // Ctrl-clicking cells or row labels), so the menu can offer to merge
    // them.
    let selected: Vec<usize> = grid
        .get_selected_blocks()
        .iter()
        .flat_map(|b| b.top_row..=b.bottom_row)
        .filter_map(|r| row_track(&state.borrow(), r))
        .collect();
    let track_count = |st: &State| st.editor.as_ref().map(|e| e.splits().len());
    let (entries, before) = match &state.borrow().editor {
        Some(ed) => (
            context_menu::track_list(ed, index, &selected),
            ed.splits().len(),
        ),
        None => return,
    };
    if !entries.is_empty() {
        // The menu is modal: its command has run when it returns.
        popup(grid, state, entries);
        // After a merge or join the selected rows are other tracks; keep
        // them from offering a merge nobody chose.
        if track_count(&state.borrow()) != Some(before) {
            grid.clear_selection();
        }
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
        // Handled before the state is borrowed (see `command`).
        Effect::ExportSelection => None,
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
        let line = marker_width(&p);
        let metrics = metrics(&p);
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
            metrics.ruler_height(dc.get_text_extent("0:00").1)
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
        // Each track tinted, when there are several (issue #83).
        if let Some(e) = editor.filter(|e| !e.splits().is_empty()) {
            for (i, t) in e.tracks().iter().enumerate() {
                let Some(r) = view.pixel_span(&t.span) else {
                    continue;
                };
                let tint = tints::waveform(&st.options.appearance, i, BACKGROUND);
                dc.set_pen(tint, 1, PenStyle::Solid);
                dc.set_brush(tint, BrushStyle::Solid);
                dc.draw_rectangle(px(r.start), 0, px(r.end - r.start), size.height);
            }
        }
        let deleted = editor.map_or_else(Vec::new, |e| spans(&mut e.deleted().iter().cloned()));
        let selected = editor.map_or_else(Vec::new, |e| spans(&mut e.selection().into_iter()));
        let selection = tints::selection(&st.options.appearance, BACKGROUND);
        for (colour, list) in [(DELETED_BACKGROUND, &deleted), (selection, &selected)] {
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
                    size.height - ruler_height - metrics.suggestion_strip(),
                    px(span.end - span.start),
                    metrics.suggestion_strip(),
                );
            }
        }
        // The chained links' starts are there before the analysis is done.
        let split = editor.map_or(&[][..], Editor::splits);
        dc.set_pen(SUGGESTED_SPLIT, 2 * line, PenStyle::Solid);
        for s in st
            .suggested_starts()
            .iter()
            .filter(|s| !split.contains(&s.pos))
        {
            let x = view.sample_to_x(s.pos).round();
            if (0.0..f64::from(view.width())).contains(&x) {
                dc.draw_line(x as i32, 0, x as i32, metrics.suggestion_tick());
            }
        }
        // Track boundaries (dashed) and labels at the top of each track.
        if let Some(e) = editor.filter(|e| !e.splits().is_empty()) {
            let width = f64::from(view.width());
            dc.set_pen(TRACK, line, PenStyle::ShortDash);
            dc.set_text_foreground(TRACK);
            dc.set_background_mode(BackgroundMode::Transparent);
            let (dx, dy) = metrics.track_label_offset();
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
                dc.draw_text(&label, x0.max(0.0) as i32 + dx, dy);
            }
        }
        if let Some(rate) = st.rate.filter(|_| ruler_height > 0) {
            draw_ruler(&dc, view, rate, size, ruler_height, &metrics);
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
                dc.set_pen(colour, line, PenStyle::Solid);
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
                minimap::draw(&dc, size, marker_width(&m), &scene);
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
/// window (`size`): ticks and labels from [`lac_app::ruler::ticks`], sized
/// by `metrics`.
fn draw_ruler(
    dc: &impl DeviceContext,
    view: &WaveformView,
    rate: u32,
    size: Size,
    height: i32,
    metrics: &Metrics,
) {
    let top = size.height - height;
    dc.set_pen(RULER_BACKGROUND, 1, PenStyle::Solid);
    dc.set_brush(RULER_BACKGROUND, BrushStyle::Solid);
    dc.draw_rectangle(0, top, size.width, height);
    dc.set_pen(RULER_TICK, 1, PenStyle::Solid);
    dc.draw_line(0, top, size.width, top);
    dc.set_text_foreground(RULER_TEXT);
    dc.set_background_mode(BackgroundMode::Transparent);
    // Room for the widest usual label and a gap.
    let min_gap = metrics.ruler_label_spacing(dc.get_text_extent("0:00:00.000").0);
    let (dx, dy) = metrics.ruler_label_offset();
    for t in lac_app::ruler::ticks(view, rate, min_gap) {
        let x = t.x.round() as i32;
        let len = if t.label.is_some() {
            metrics.ruler_major()
        } else {
            metrics.ruler_minor()
        };
        // draw_line leaves out the end point, so go one past.
        dc.draw_line(x, top, x, top + len + 1);
        if let Some(label) = &t.label {
            dc.draw_text(label, x + dx, top + dy);
        }
    }
}

/// Width, in device units, of the pen for marker lines (cursor, selection
/// edges, playhead, track boundaries, the minimap's frame) on `window`: one
/// device-independent pixel.
///
/// Where wxWidgets draws in logical points (macOS, GTK with `GDK_SCALE`),
/// that is 1 and the toolkit already makes it 2 physical pixels on a
/// Retina/2x screen. On Windows a DC counts physical pixels, so a hard 1
/// would shrink to half its visual width at 200 % (the manifest declares
/// per-monitor DPI awareness, issue #62); `FromDIP` scales it there and
/// follows the window to another monitor. The waveform's own columns stay
/// 1 wide: they are one per device pixel by design.
fn marker_width(window: &impl WxWidget) -> i32 {
    window.from_dip_int(1).max(1)
}

/// The sizes of the waveform window's marks on `window`'s screen (issue
/// #128). `FromDIP` of a large number keeps fractional scales (125 %,
/// 150 %) exact; it is 1:1 where the toolkit scales by itself (macOS, GTK).
fn metrics(window: &impl WxWidget) -> Metrics {
    const DIPS: i32 = 1000;
    Metrics::from_dip(DIPS, window.from_dip_int(DIPS))
}

/// A pixel coordinate for the device context.
fn px(x: u32) -> i32 {
    i32::try_from(x).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lac_app::detector_settings::DetectorSettings;

    #[test]
    fn track_tints_stay_apart_from_selection_and_deletion() {
        use lac_app::appearance::{Appearance, SELECTION_RANGE, TRACK_TINT_RANGE, distance};
        use tints::rgb;
        // The selection and deleted parts are drawn over the tints, the
        // peaks over everything; at every setting each must still look
        // different from what it is drawn on (issues #126, #132).
        for track_tint in TRACK_TINT_RANGE {
            for selection in SELECTION_RANGE {
                let a = Appearance {
                    track_tint,
                    selection,
                }
                .normalized();
                let sel = rgb(tints::selection(&a, BACKGROUND));
                assert!(distance(sel, rgb(DELETED_BACKGROUND)) >= 60, "{a:?}");
                assert!(distance(sel, rgb(PEAK)) >= 100, "{a:?}");
                for i in 0..4 {
                    let tint = rgb(tints::waveform(&a, i, BACKGROUND));
                    assert!(distance(tint, rgb(DELETED_BACKGROUND)) >= 30, "{a:?} {i}");
                    assert!(distance(tint, rgb(PEAK)) >= 150, "{a:?} {i}");
                }
            }
        }
    }

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

    /// Exporting the selection writes a file but does not save the edits:
    /// closing would still lose them. A full export saves them.
    #[test]
    fn only_a_full_export_saves_the_edits() {
        let src = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lac-container/tests/fixtures/flac_stereo16_44k.flac"
        ));
        let dir = std::env::temp_dir().join(format!("lac-gui-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let mut ed = Editor::new(40_000);
        ed.select(1000, 2000);
        ed.delete_selection();
        ed.select(5000, 9000);
        let mut st = State {
            path: Some(src.clone()),
            editor: Some(ed.clone()),
            ..State::default()
        };
        let run = |st: &mut State, task, edits| {
            st.export = Some(RunningExport {
                job: ExportJob::start_all(&src, vec![task]),
                source: src.clone(),
                edits,
            });
            let start = Instant::now();
            loop {
                if let Some(msg) = st.poll_export() {
                    return msg;
                }
                assert!(start.elapsed() < Duration::from_secs(20), "export hangs");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let sel = ed.selection().expect("selected");
        let task =
            lac_app::export_job::selection_task(&ed, sel, dir.join("sel.flac")).expect("kept");
        run(&mut st, task, None);
        assert!(dir.join("sel.flac").exists());
        assert!(st.unexported(), "a selection export saves nothing");
        let task = lac_app::export_job::single_task(&ed, dir.join("all.flac"));
        run(&mut st, task, Some(ed.edits().clone()));
        assert!(!st.unexported());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A mono 16-bit WAV of `parts`: (seconds, tone or silence).
    fn tone_and_pauses(path: &Path, rate: u32, parts: &[(u32, bool)]) {
        let mut pcm = Vec::new();
        for &(secs, tone) in parts {
            for i in 0..secs * rate {
                let v = if tone {
                    let t = f64::from(i) / f64::from(rate);
                    ((t * 440.0 * std::f64::consts::TAU).sin() * 12_000.0) as i16
                } else {
                    0
                };
                pcm.extend_from_slice(&v.to_le_bytes());
            }
        }
        let len = u32::try_from(pcm.len()).expect("short");
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&len.to_le_bytes());
        wav.extend_from_slice(&pcm);
        std::fs::write(path, wav).expect("write wav");
    }

    /// Changing the detector settings detects again from the stored
    /// features, without decoding the file again (issue #92).
    #[test]
    fn detector_settings_change_the_suggestions_at_once() {
        let dir = std::env::temp_dir().join(format!("lac-gui-detect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("pauses.wav");
        // A 1 s and a 3 s pause between tones.
        tone_and_pauses(
            &path,
            8000,
            &[(3, true), (1, false), (3, true), (3, false), (3, true)],
        );
        let AnalysisStatus::Done { features, .. } =
            AnalysisJob::start(&path, DetectorSettings::default().params()).wait()
        else {
            panic!("analysis failed");
        };
        let mut st = State {
            features: Some(features),
            ..State::default()
        };
        let pauses = |st: &State| st.detection.as_ref().map_or(0, |d| d.regions.len());
        st.redetect();
        assert_eq!(pauses(&st), 2, "both pauses at the defaults");
        st.options.detector.min_pause_ms = 2000;
        st.redetect();
        assert_eq!(pauses(&st), 1, "only the long pause");
        let _ = std::fs::remove_dir_all(&dir);
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
        // Moving the track under the cursor in the list (issue #85).
        if let Some(e) = &mut st.editor {
            e.split_at_all([300, 600]);
            e.set_cursor(700);
        }
        assert_eq!(
            edit(&mut st, ID_MOVE_DOWN),
            Some("the track is last in the list already")
        );
        assert_eq!(edit(&mut st, ID_MOVE_UP), Some(""));
        assert_eq!(edit(&mut st, ID_MOVE_UP), Some(""));
        assert_eq!(st.editor.as_ref().map(Editor::order), Some(vec![2, 0, 1]));
        assert_eq!(
            edit(&mut st, ID_MOVE_UP),
            Some("the track is first in the list already")
        );
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
    fn menu_ids_are_distinct() {
        let mut ids = vec![
            ID_OPEN,
            ID_ZOOM_IN,
            ID_ZOOM_OUT,
            ID_ZOOM_FIT,
            ID_CLEAR_CACHE,
            ID_EXPORT,
            ID_EXPORT_SELECTION,
            ID_SAVE_PROJECT,
            ID_SAVE_PROJECT_AS,
            ID_MOVE_UP,
            ID_MOVE_DOWN,
            ID_UNDO,
            ID_REDO,
            ID_DELETE,
            ID_RESTORE,
            ID_SELECT_ALL,
            ID_SELECT_NONE,
            ID_PLAY,
            ID_TO_START,
            ID_SPLIT,
            ID_JOIN,
            ID_RENAME,
            ID_FASTER,
            ID_SLOWER,
            ID_NORMAL_SPEED,
            ID_KEEP_PITCH,
            ID_SCRUB_SELECTING,
            ID_ALL_TRACKS,
            ID_RULER,
            ID_SUGGESTED_SPLITS,
            ID_SUGGESTED_PAUSES,
            ID_SETTINGS,
            ID_PLAY_TRACK_START,
            ID_PLAY_TRACK_END,
        ];
        ids.extend((0..Perspective::ALL.len()).map(|i| ID_PERSPECTIVE_FIRST + i as Id));
        for (i, id) in ids.iter().enumerate() {
            assert!(!ids[i + 1..].contains(id), "{id} twice");
            assert_eq!(panes::pane_for(*id), None, "{id} toggles a pane");
            assert_eq!(recent::index_of(*id), None, "{id} opens a recent file");
            assert_eq!(context_index(*id), None, "{id} is a right-click entry");
        }
    }

    /// A project's edits are fitted to the recording, and a recording
    /// whose length changed is pointed out.
    #[test]
    fn projects_bring_their_edits() {
        let project = Project {
            audio: PathBuf::from("/rec/talk.wav"),
            frames: Some(1000),
            edits: lac_app::editor::Edits {
                deleted: vec![100..200, 900..1100],
                splits: vec![500],
                ..Default::default()
            },
            view: None,
        };
        let mut st = State::default();
        let e = st.editor_from(project.clone(), 1000);
        assert_eq!(e.deleted(), [100..200, 900..1000]);
        assert_eq!(e.splits(), [500]);
        assert!(!e.has_unexported_changes());
        assert_eq!(st.open_notes(), "", "same length");
        let e = st.editor_from(project, 950);
        assert_eq!(e.deleted(), [100..200, 900..950]);
        assert!(st.open_notes().contains("length changed"));
    }

    #[test]
    fn right_click_menu_ids_are_their_own() {
        assert_eq!(context_index(ID_CONTEXT_FIRST), Some(0));
        let last = ID_CONTEXT_FIRST + CONTEXT_MAX as Id - 1;
        assert_eq!(context_index(last), Some(CONTEXT_MAX - 1));
        assert_eq!(context_index(last + 1), None);
        assert_eq!(context_index(ID_SETTINGS), None);
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
    fn perspective_ids_map_to_the_six_perspectives() {
        for (i, p) in Perspective::ALL.iter().enumerate() {
            let id = ID_PERSPECTIVE_FIRST + i as Id;
            assert_eq!(perspective_of(id), Some(*p));
            assert_eq!(panes::pane_for(id), None, "{id} toggles a pane");
            assert_eq!(recent::index_of(id), None);
        }
        assert_eq!(perspective_of(ID_PERSPECTIVE_FIRST + 6), None);
        assert_eq!(perspective_of(ID_PLAY_TRACK_END), None);
        const { assert!(ID_PERSPECTIVE_FIRST + 6 <= recent::ID_CLEAR) };
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
