//! # lac-analysis
//!
//! Suggestions where to cut: stretches without voice or music that can be
//! deleted, and where new tracks start. See `docs/architecture.md`,
//! section "Silence and track detection".
//!
//! Two steps, so that the expensive one runs once and the cheap one can be
//! repeated with other settings:
//!
//! 1. [`FeatureExtractor`] turns decoded PCM (fed in timeline order, like
//!    `lac_peaks::PeakBuilder`) into [`Features`]: per analysis frame of
//!    about 20 ms the level, the *spectral flatness* (how evenly the
//!    energy is spread over the spectrum: noise is flat, voice and music
//!    are made of harmonics), a coarse log-spaced band spectrum and the
//!    chroma (energy per pitch class).
//! 2. [`detect`] classifies every frame as content, quiet or noise and
//!    derives [`Region`]s to delete and [`SplitSuggestion`]s for track
//!    starts, from silence gaps and from sudden changes of timbre or key.
//!
//! The crate does no decoding and no I/O, so every rule is tested on
//! synthetic signals. Positions are sample frames ([`SamplePos`]) of the
//! source timeline; they are frame-granular (one hop, about 20 ms), which
//! is far finer than any track boundary needs and is snapped to packets by
//! the cut planner anyway.

mod detect;
mod features;

pub use detect::{
    DetectParams, Detection, FrameClass, Region, RegionKind, SplitReason, SplitSuggestion,
    Thresholds, classify, detect,
};
pub use features::{
    BANDS, CHROMA_HIGH_HZ, CHROMA_LOW_HZ, FLATNESS_HIGH_HZ, FLATNESS_LOW_HZ, FeatureExtractor,
    Features, FeaturesFormatError, Frame, hop_for_rate,
};

pub use lac_core::SamplePos;
