//! # lac-app
//!
//! The use cases of *losslessaudiocut*, shared by the command line (`lac-cli`)
//! and the GUI (`lac-gui`).
//!
//! Everything here is independent of any user interface toolkit, so it can
//! be unit-tested without a display. Front ends only translate user input
//! into calls of this crate and present the results.
//!
//! - [`time`]: parsing and formatting of user-facing times; the one place
//!   where seconds become sample frames.
//! - [`ranges`]: helpers on sample ranges.
//! - [`session`]: opening a source file, building its waveform summary
//!   and exporting a cut of it.
//! - [`waveform`]: the waveform display's view model (zoom, scroll,
//!   pixel columns).
//! - [`peak_job`]: building the waveform summary in a background thread.
//! - [`editor`]: selection, deletions and undo of one recording.
//! - [`export_job`]: writing a cut in a background thread.
//! - [`tag_edits`]: what the user changed in a track's tags.
//! - [`track_list`]: the track list's view model (tags per track).
//! - [`playback`]: playing the kept parts through the audio device.
//! - [`peak_cache`]: the on-disk cache of waveform summaries.
//! - [`analysis_job`]: suggesting deletable pauses and track starts
//!   (`lac-analysis`) in a background thread.
//! - [`layout`]: the main window's dockable panes and how a stored
//!   arrangement of them is restored.
//! - [`positions`]: the position bar (selection start, end, length and
//!   play clock) and typing times into it.
//! - [`ruler`]: ticks and labels of the time ruler under the waveform.
//! - [`minimap`]: the whole recording in one strip, with the visible
//!   part framed and draggable.
//! - [`detail`]: decoded samples for a zoomed-in waveform, loaded in the
//!   background with random access.
//!
//! See `docs/architecture.md` in the repository for how this crate fits in.

pub mod analysis_job;
pub mod detail;
pub mod editor;
pub mod export_job;
pub mod layout;
pub mod minimap;
pub mod peak_cache;
pub mod peak_job;
pub mod playback;
pub mod positions;
pub mod ranges;
pub mod ruler;
pub mod session;
pub mod tag_edits;
pub mod time;
pub mod track_list;
pub mod waveform;

/// Returned by progress callbacks such as [`Source::analyze`] to go on or stop.
pub use lac_decode::Flow;
pub use session::{AppError, CutRequest, KeptRange, Source};
pub use tag_edits::TagEdits;
pub use time::{TimeError, TimeRange, TimeSpec, format_samples};
