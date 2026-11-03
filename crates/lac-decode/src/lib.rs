//! # lac-decode
//!
//! Turns a source back into PCM for playback, the waveform cache and
//! analysis.
//!
//! The packets come from *our* demuxers (`lac-container`); FFmpeg is used
//! only for its decoders (libavcodec), never for demuxing. So the decoded
//! timeline is exactly the one of the packet index, and a waveform position
//! is the position a cut is made at.
//!
//! Output is interleaved `f32` in `-1.0..1.0` (integer PCM scaled by
//! `2^-(bits-1)`), the native format of the lossy decoders.
//!
//! Requires FFmpeg 5.1 or newer (the `AVChannelLayout` API).

mod ffi;

use lac_container::{Container, ContainerError, ReadSeek};
use lac_core::SamplePos;

pub use ffi::FfmpegError;

/// Errors of this crate.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// Reading packets or codec headers from the source failed.
    #[error(transparent)]
    Container(#[from] ContainerError),
    /// FFmpeg could not set up a decoder for the stream.
    #[error("cannot set up the {codec} decoder: {source}")]
    Setup {
        /// The codec, for the message.
        codec: &'static str,
        /// What FFmpeg reported.
        #[source]
        source: FfmpegError,
    },
    /// The sink returned an error; decoding stopped.
    #[error("decoding was stopped")]
    Stopped,
}

/// What happened while decoding a whole source.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecodeReport {
    /// Sample frames delivered to the sink; always the length of the
    /// packet index.
    pub frames: SamplePos,
    /// Problems that were worked around (damaged packets, length
    /// mismatches). Positions after such a problem may be off.
    pub warnings: Vec<String>,
}

/// Whether the sink wants more samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Keep decoding.
    Continue,
    /// Stop; [`decode_all`] returns [`DecodeError::Stopped`].
    Stop,
}

/// Decodes the whole timeline of `source` (read through `r`) and hands it
/// to `sink` in order, as chunks of interleaved `f32` sample frames.
///
/// Exactly `source.index().total_samples()` frames are delivered: if the
/// decoder yields fewer (a damaged file), the rest is filled with silence
/// and a warning is reported.
pub fn decode_all(
    source: &dyn Container,
    r: &mut dyn ReadSeek,
    mut sink: impl FnMut(&[f32]) -> Flow,
) -> Result<DecodeReport, DecodeError> {
    let mut report = DecodeReport::default();
    let channels = usize::from(source.index().info().channels);
    for (s, section) in source.decode_sections(r)?.iter().enumerate() {
        let mut dec = ffi::Decoder::new(section).map_err(|source| DecodeError::Setup {
            codec: codec_name(section.codec),
            source,
        })?;
        let mut out = SectionOutput {
            pos: dec.delay(),
            keep: section.keep.clone(),
            channels,
        };
        for i in 0..section.packets {
            let packet = source.read_decode_packet(r, s, i)?;
            if let Err(e) = dec.decode(&packet) {
                report
                    .warnings
                    .push(format!("section {s}, packet {i} could not be decoded: {e}"));
            }
            while let Some(samples) = dec.next_samples(channels) {
                out.deliver(&samples, &mut report, &mut sink)?;
            }
        }
        dec.flush();
        while let Some(samples) = dec.next_samples(channels) {
            out.deliver(&samples, &mut report, &mut sink)?;
        }
        if out.pos < section.keep.end {
            let missing = section.keep.end - out.pos;
            report.warnings.push(format!(
                "section {s} decoded to {missing} sample frames fewer than its packets announce; filled with silence"
            ));
            let silence = vec![0f32; 4096 * channels];
            let mut left = missing;
            while left > 0 {
                let n = left.min(4096);
                out.deliver(&silence[..n as usize * channels], &mut report, &mut sink)?;
                left -= n;
            }
        }
    }
    Ok(report)
}

/// Decodes the whole timeline into one buffer of interleaved samples.
/// Convenient for short files and tests; a multi-hour recording needs
/// gigabytes this way, so use [`decode_all`] with a streaming sink there.
pub fn decode_to_vec(
    source: &dyn Container,
    r: &mut dyn ReadSeek,
) -> Result<(Vec<f32>, DecodeReport), DecodeError> {
    let mut out = Vec::new();
    let report = decode_all(source, r, |chunk| {
        out.extend_from_slice(chunk);
        Flow::Continue
    })?;
    Ok((out, report))
}

/// Cuts one section's decoder output down to its `keep` range.
struct SectionOutput {
    /// Sample frames of decoder output seen so far.
    pos: u64,
    keep: std::ops::Range<u64>,
    channels: usize,
}

impl SectionOutput {
    /// Passes the part of `samples` (the next decoder output) that lies in
    /// `keep` on to `sink`.
    fn deliver(
        &mut self,
        samples: &[f32],
        report: &mut DecodeReport,
        sink: &mut impl FnMut(&[f32]) -> Flow,
    ) -> Result<(), DecodeError> {
        let start = self.pos;
        let n = (samples.len() / self.channels) as u64;
        self.pos += n;
        // The part of [start, start + n) inside `keep`, if any.
        let a = start.max(self.keep.start).min(self.pos);
        let b = self.pos.min(self.keep.end).max(a);
        if a == b {
            return Ok(());
        }
        let chunk =
            &samples[(a - start) as usize * self.channels..(b - start) as usize * self.channels];
        report.frames += b - a;
        match sink(chunk) {
            Flow::Continue => Ok(()),
            Flow::Stop => Err(DecodeError::Stopped),
        }
    }
}

fn codec_name(c: lac_container::Codec) -> &'static str {
    use lac_container::Codec;
    match c {
        Codec::Pcm { .. } => "PCM",
        Codec::Flac => "FLAC",
        Codec::Vorbis => "Vorbis",
        Codec::Opus => "Opus",
        Codec::Speex => "Speex",
        Codec::Mp3 => "MP3",
    }
}
