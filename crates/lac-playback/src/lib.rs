//! Playback for losslessaudiocut.
//!
//! Plays the **result of the edits**, not the source: a [`Timeline`] of
//! the kept source ranges, one after the other, so one hears the cuts
//! before exporting (`docs/architecture.md`, section "Playback").
//!
//! - [`Timeline`] maps between playback positions and source positions.
//! - [`Engine`] runs the decoder thread and the ring buffer; its
//!   [`Output`] end is what the audio callback reads. Everything is tested
//!   without a sound card by calling [`Output::fill`] directly.
//! - [`Player`] is an engine plus the system's default output device
//!   (`cpal`).
//! - [`convert`] adapts the sample rate and channel count to the device;
//!   with equal formats the audio passes through bit for bit.
//! - [`scrub`] decides when scrubbing restarts playback (grain restart).
//! - [`Speed`] is the playback speed; [`stretch`] keeps the pitch at
//!   speeds other than normal.
//!
//! The decoder is not part of this crate: the application provides it as a
//! [`PcmSource`] (`lac_app` uses random-access decoding), which keeps this
//! crate free of FFmpeg and testable with generated audio.
//!
//! The other scrub modes of the architecture doc (varispeed and
//! pitch-preserving scrubbing driven by the mouse) come later; like the
//! speed, they are render stages between the decoder and the ring buffer.

pub mod convert;
mod device;
mod engine;
pub mod scrub;
mod speed;
pub mod stretch;
mod timeline;

use std::ops::Range;

use lac_core::SamplePos;

pub use engine::{Engine, FADE_SECONDS, Output, OutputFormat, PcmSource};
pub use speed::{STEPS, Speed};
pub use timeline::Timeline;

/// Why playback could not start.
#[derive(Debug, thiserror::Error)]
pub enum PlaybackError {
    /// The system has no audio output.
    #[error("no audio output device found")]
    NoDevice,
    /// The device refused, e.g. it is in use or was unplugged.
    #[error("audio output: {0}")]
    Device(String),
}

/// An [`Engine`] playing through the default output device.
#[derive(Debug)]
pub struct Player {
    engine: Engine,
    /// Dropping it stops the sound; it must outlive the engine's use.
    _device: device::Device,
}

impl Player {
    /// Opens the default output device and prepares to play `timeline` of
    /// `source`, paused at its start.
    pub fn open<S: PcmSource>(source: S, timeline: Timeline) -> Result<Self, PlaybackError> {
        let (dev, config) = device::default_output()?;
        let (engine, output) = Engine::start(source, timeline, device::format_of(&config));
        let device = device::open(&dev, &config, output)?;
        Ok(Player {
            engine,
            _device: device,
        })
    }

    /// The engine, for transport control and the position.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// The engine, for seeking and changing the timeline.
    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }
}

/// A [`PcmSource`] from a closure, for simple sources and tests.
pub struct FnSource<F> {
    channels: usize,
    rate: u32,
    read: F,
}

impl<F> FnSource<F>
where
    F: FnMut(Range<SamplePos>) -> Result<Vec<f32>, String> + Send + 'static,
{
    /// A source of `channels` channels at `rate` reading through `read`.
    pub fn new(channels: usize, rate: u32, read: F) -> Self {
        FnSource {
            channels,
            rate,
            read,
        }
    }
}

impl<F> PcmSource for FnSource<F>
where
    F: FnMut(Range<SamplePos>) -> Result<Vec<f32>, String> + Send + 'static,
{
    fn channels(&self) -> usize {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn read(&mut self, range: Range<SamplePos>) -> Result<Vec<f32>, String> {
        (self.read)(range)
    }
}
