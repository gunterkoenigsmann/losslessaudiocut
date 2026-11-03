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
//! - [`detail`]: decoded samples for a zoomed-in waveform, loaded in the
//!   background with random access.
//!
//! See `docs/architecture.md` in the repository for how this crate fits in.

pub mod detail;
pub mod peak_job;
pub mod ranges;
pub mod session;
pub mod time;
pub mod waveform;

pub use session::{AppError, CutRequest, KeptRange, Source};
pub use time::{TimeError, TimeRange, TimeSpec, format_samples};
