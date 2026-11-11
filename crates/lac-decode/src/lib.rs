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
//! Video tracks are decoded picture by picture for the preview, see
//! [`VideoDecoder`]; pictures are converted to RGB by our own code
//! ([`picture`]).
//!
//! Requires FFmpeg 5.1 or newer (the `AVChannelLayout` API).

mod ffi;
pub mod picture;
mod video;

use lac_container::{Container, ContainerError, ReadSeek};
use std::ops::Range;

use lac_core::SamplePos;

pub use ffi::FfmpegError;
pub use picture::{Frame, Picture};
pub use video::VideoDecoder;

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
    /// FFmpeg could not set up a decoder for the video track.
    #[error("cannot set up the {codec} video decoder: {source}")]
    VideoSetup {
        /// The codec, for the message.
        codec: String,
        /// What FFmpeg reported.
        #[source]
        source: FfmpegError,
    },
    /// A decoded picture could not be read.
    #[error("cannot read a decoded picture: {0}")]
    Video(#[source] FfmpegError),
    /// The video track has no picture to show (no frames, or none
    /// decoded).
    #[error("the video track has no picture there")]
    NoPicture,
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
        for i in 0..section.packets() {
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

/// Decodes only the timeline sample frames in `range` (interleaved `f32`,
/// exactly `range.end - range.start` frames), for drawing a zoomed-in
/// waveform or playing from a position without decoding from the start.
///
/// A fresh decoder starts a codec-specific pre-roll before `range` and is
/// fed up to the packet that completes `range`. Its output is placed by
/// [`lac_container::DecodeSection::packet_ends`]: the last sample after
/// packet *j* lies just before `packet_ends[j]`, for a fresh decoder as for
/// one that decoded the whole section. PCM, FLAC, ALAC, MP3, Vorbis and AAC
/// decode bit for bit like [`decode_all`] after their pre-roll; the IIR
/// state of Opus and Speex converges only approximately (inaudible,
/// invisible), and AAC's noise substitution draws other random noise. Parts
/// that cannot be decoded are silence, as in [`decode_all`].
pub fn decode_range(
    source: &dyn Container,
    r: &mut dyn ReadSeek,
    range: Range<SamplePos>,
) -> Result<Vec<f32>, DecodeError> {
    let channels = usize::from(source.index().info().channels);
    let total = source.index().total_samples();
    let range = range.start.min(total)..range.end.clamp(range.start.min(total), total);
    let mut out = Vec::with_capacity((range.end - range.start) as usize * channels);
    // Timeline position of the current section's first kept sample.
    let mut timeline = 0u64;
    for (s, section) in source.decode_sections(r)?.iter().enumerate() {
        let len = section.keep.end - section.keep.start;
        let ta = range.start.max(timeline);
        let tb = range.end.min(timeline + len);
        timeline += len;
        if ta >= tb {
            continue;
        }
        // The wanted part in section output coordinates.
        let a = section.keep.start + (ta - (timeline - len));
        let b = a + (tb - ta);
        let got = decode_section_part(source, r, s, section, a..b, channels)?;
        out.extend(got);
    }
    // Sections cover the timeline; anything missing is silence.
    out.resize((range.end - range.start) as usize * channels, 0.0);
    Ok(out)
}

/// How much to decode before the wanted part so that a fresh decoder has
/// built up its state: (packets, sample frames), the larger one counts.
fn preroll(codec: lac_container::Codec, sample_rate: u32) -> (usize, u64) {
    use lac_container::Codec;
    let ms = |m: u64| u64::from(sample_rate) * m / 1000;
    match codec {
        Codec::Pcm { .. } | Codec::Flac | Codec::Alac => (0, 0),
        // The bit reservoir reaches up to 511 bytes back (ISO/IEC 11172-3
        // 2.4.1.7: main_data_begin), five frames at 32 kbit/s; one more
        // frame for the IMDCT overlap and the 512-tap synthesis filter.
        Codec::Mp3 => (8, 0),
        // A Vorbis packet's output needs the previous packet's window half
        // (Vorbis I 1.3.2); the first packet of a decoder only primes it.
        Codec::Vorbis => (2, 0),
        // An AAC frame's output overlaps with its predecessor's (MDCT,
        // ISO/IEC 14496-3 4.6.11); one more frame for HE-AAC's SBR filter
        // bank, as lac_container::mp4 writes it.
        Codec::Aac => (2, 0),
        // The first block's IMDCT output overlaps with the previous
        // frame's last block (A/52 7.9.4.2); nothing reaches further back.
        Codec::Ac3 => (1, 0),
        // RFC 7845 4.6 recommends at least 80 ms of pre-roll. On the
        // speech fixtures the output is bit-exact again about 140 ms later;
        // the tones of `video_h264_opus.mp4` (CELT) still differ by 4.5
        // LSB after 250 ms, by far less than one after 400 ms.
        Codec::Opus => (1, ms(400)),
        // Speex's float state decays geometrically and need not become
        // bit-exact at all (issue #17); after 400 ms it is far below one
        // 16-bit LSB.
        Codec::Speex => (1, ms(400)),
    }
}

/// Decodes `want` (section output coordinates) of section `s` with a fresh
/// decoder, see [`decode_range`]. Returns exactly `want.len()` frames.
fn decode_section_part(
    source: &dyn Container,
    r: &mut dyn ReadSeek,
    s: usize,
    section: &lac_container::DecodeSection,
    want: Range<u64>,
    channels: usize,
) -> Result<Vec<f32>, DecodeError> {
    let ends = &section.packet_ends;
    let n_frames = (want.end - want.start) as usize;
    let mut result = vec![0f32; n_frames * channels];
    if ends.is_empty() {
        return Ok(result);
    }
    // Packet i outputs the frames ends[i - 1]..ends[i].
    let start_of = |i: usize| if i == 0 { 0 } else { ends[i - 1] };
    // The packet holding `want.start`, and the one completing `want`.
    let first = ends
        .partition_point(|&e| e <= want.start)
        .min(ends.len() - 1);
    let last = ends.partition_point(|&e| e < want.end).min(ends.len() - 1);
    let (pre_packets, pre_frames) = preroll(section.codec, section.sample_rate);
    let mut k = first;
    while k > 0
        && (first - k < pre_packets || want.start - start_of(k).min(want.start) < pre_frames)
    {
        k -= 1;
    }
    let mut dec = ffi::Decoder::new(section).map_err(|source| DecodeError::Setup {
        codec: codec_name(section.codec),
        source,
    })?;
    let mut decoded = Vec::new();
    for i in k..=last {
        let packet = source.read_decode_packet(r, s, i)?;
        // A packet that does not decode leaves a gap that the end alignment
        // below turns into silence, as decode_all's warning does.
        let _ = dec.decode(&packet);
        while let Some(samples) = dec.next_samples(channels) {
            decoded.extend_from_slice(&samples);
        }
    }
    // The decoded frames end at ends[last] (see DecodeSection::packet_ends).
    let n = (decoded.len() / channels) as u64;
    let out_start = ends[last].saturating_sub(n);
    // Copy the overlap of out_start..ends[last] with `want`.
    let a = want.start.max(out_start);
    let b = want.end.min(ends[last]);
    if a < b {
        let src =
            &decoded[(a - out_start) as usize * channels..(b - out_start) as usize * channels];
        let dst = (a - want.start) as usize * channels;
        result[dst..dst + src.len()].copy_from_slice(src);
    }
    Ok(result)
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
        Codec::Aac => "AAC",
        Codec::Alac => "ALAC",
        Codec::Ac3 => "AC-3",
    }
}
