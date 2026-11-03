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
//!
//! See `docs/architecture.md` in the repository for how this crate fits in.

pub mod ranges;
pub mod session;
pub mod time;

pub use session::{AppError, CutRequest, KeptRange, Source};
pub use time::{TimeError, TimeRange, TimeSpec, format_samples};
