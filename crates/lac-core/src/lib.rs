//! # lac-core
//!
//! The format-independent data model of *losslessaudiocut*.
//!
//! This crate has no I/O and no knowledge of any container or codec. It
//! defines
//!
//! - the integer time base ([`SamplePos`]),
//! - the description of a source file as a sequence of packets
//!   ([`PacketInfo`], [`PacketIndex`]),
//! - the non-destructive edit model ([`EditList`], [`Track`], [`Segment`],
//!   [`Tags`]),
//! - and cut planning, which maps a wanted sample range onto whole packets
//!   ([`CutCapabilities`], [`CutPlan`], [`plan_cut`]).
//!
//! See `docs/architecture.md` in the repository for the overall design.

mod edit;
mod packet;
mod plan;
mod tags;

pub use edit::{EditError, EditList, Segment, SourceId, Track};
pub use packet::{AudioStreamInfo, IndexError, PacketFlags, PacketIndex, PacketInfo};
pub use plan::{CutCapabilities, CutPlan, PlanError, plan_cut, plan_cuts};
pub use tags::{TagField, TagValueError, Tags, parse_track_number};

/// A position or a duration, counted in sample frames at the stream's own
/// sample rate.
///
/// One *sample frame* holds one sample of every channel, so a 4-channel file
/// advances by one `SamplePos` per four samples. All positions inside the
/// program use this integer type; floating-point seconds only exist in user
/// interfaces, so that rounding can never move a cut.
pub type SamplePos = u64;
