//! MP4 / M4A files (ISO/IEC 14496-12, the ISO base media file format, and
//! 14496-14, the MP4 file format) with AAC, ALAC, Opus, AMR or uncompressed (PCM) audio.
//!
//! Phones, iTunes and most podcast tools record and distribute AAC this
//! way (`.m4a`, `.m4b`, `.mp4`); Apple Lossless (ALAC) uses the same
//! container, and Android phones record videos with Opus sound in it.
//! 3GPP files (`.3gp`, 3GPP TS 26.244) are MP4 files too; phones record
//! voice notes, and older phones videos, with AMR sound in them. QuickTime `.mov` files share the box structure and open too;
//! cameras and screen recorders store uncompressed sound in them
//! (`sowt`, `twos`, `lpcm`, `in24`, `fl32`, ...), as ISO files may
//! (`ipcm`/`fpcm`, ISO/IEC 23003-5).
//!
//! ## Structure
//!
//! A sequence of boxes: `ftyp` (brands), `moov` (all metadata) and `mdat`
//! (the media data), in either order. Inside `moov` each `trak` describes
//! one track; its sample table (`mdia/minf/stbl`) lists every sample
//! (= packet): sizes (`stsz`), durations (`stts`), and where they lie in
//! the file (`stco`/`co64` chunk offsets plus `stsc`, which maps chunks to
//! samples). The first audio track and the first video track are cut
//! (the video is read into a [`lac_core::VideoIndex`], see [`video`]);
//! other tracks (chapters as text tracks, a second audio track, ...) are
//! not written, with a warning.
//!
//! Fragmented files, as recorders and streaming tools write them so that
//! an interrupted recording stays readable, add samples after `moov` in
//! movie fragments (ISO/IEC 14496-12 8.8): each `moof` holds a `traf` per
//! track whose `trun` boxes list further samples (sizes, durations, a data
//! offset), with defaults from the track's `trex` (in `moov/mvex`) and the
//! fragment's `tfhd`. They are appended to the samples of the `moov`
//! sample table, which is often empty. Cuts are written unfragmented.
//!
//! ## Timeline
//!
//! Each sample's decoder output is known without decoding: an AAC frame
//! always yields the same number of samples (1024 for AAC-LC, the most
//! common `stts` duration of the track), an ALAC frame its own `stts`
//! duration. An Opus sample decodes to its `stts` duration too, except
//! the first and the last, whose length comes from their TOC byte (RFC
//! 6716 3.1): the muxer states the encoder's padding by shortening the
//! last one's `stts` duration, as for AAC. ffmpeg's fragmented files without an edit list lengthen the
//! first AAC sample's duration to fill the time before it; that gap is
//! left out of the audio timeline and only shifts the video's alignment.
//! The decoder output that belongs to the timeline is given by the edit
//! list (`edts/elst`, ISO/IEC 14496-12 8.6.6): its first non-empty entry's
//! `media_time` hides the encoder delay (AAC priming, 1024 or 2112
//! samples) and its duration the encoder padding at the end. Without an
//! edit list the whole decoder output belongs to the timeline, up to the
//! sum of the `stts` durations (encoders shorten the last sample's
//! duration to state the padding).
//!
//! ffmpeg honours the edit list's start; version 6.1 decodes the last
//! frame whole, version 9 honours the end too, as Apple's players do. Our decoding follows the edit list,
//! so the timeline is the encoder's input.
//!
//! ## Writing
//!
//! A cut is a fresh file: the source's `ftyp`, a `moov` with one or two
//! tracks (the source's sample descriptions `stsd` verbatim, new sample
//! tables, edit lists; see `mux.rs`), and one `mdat` with the kept samples.
//! `moov` comes first, so players can start before the whole file is
//! read.
//!
//! Video (with audio, `lac_core::plan_av_cuts` plans both): kept frames
//! are copied verbatim in decode order, from a keyframe on. The edit list
//! hides what the first part shows before the cut's start and what the
//! last part shows after its end, so both file ends are frame-exact; each
//! further part starts at a clean keyframe and follows the previous one
//! without a gap (the previous part's last decoded frame lasts as long as
//! that needs). When the kept range starts before the first picture
//! (B-frame delay without an edit list), a leading empty edit delays the
//! video as in the source. The audio's join points follow the video's,
//! within half an AAC frame.
//!
//! Decoders differ in honouring a video edit list: ffmpeg 6.1 with
//! libdav1d shows AV1 frames the edit list hides and drops as many at the
//! end; libaom and the other codecs' decoders honour it.
//!
//! For AAC:
//!
//! - **Start:** [`AAC_PREROLL_FRAMES`] samples before the first kept one
//!   are written as pre-roll (the MDCT overlap of the next frame needs
//!   them), and hidden, together with the head of the first kept sample,
//!   by the edit list's `media_time`, so the start is sample-exact.
//! - **End:** the tail of the last sample is hidden by the edit list's
//!   duration (and, as ffmpeg writes it, by a shorter last `stts`
//!   duration), so the end is sample-exact.
//! - **Joins:** an edit list could join trimmed parts, but players handle
//!   multi-entry edit lists poorly, so kept parts are joined in one
//!   stream without pre-roll, and inner cut points are widened to sample
//!   boundaries ([`CutCapabilities::trims_at_file_ends_only`]). The first
//!   frame after a join overlaps with a foreign predecessor: a transient
//!   of one frame.
//!
//! Opus is cut like AAC, with at least [`OPUS_PREROLL`] samples of
//! pre-roll; the sample entry's `dOps` pre-skip is set to the edit list's
//! `media_time`, so players that use either agree. After a join the
//! decoder converges within a few hundred milliseconds (as for Opus in
//! Matroska).
//!
//! AMR (narrowband and wideband) is cut like Opus, with
//! [`AMR_PREROLL_MS`] of pre-roll. AMR-WB generates its top band from
//! random noise, so after any cut that band holds other noise of the same
//! level ([`AMR_WB_NOTE`]).
//!
//! ALAC is lossless and its frames are independent, so every frame that a
//! cut point cuts (at the file's ends or at a join, or by the source's own
//! edit list) is decoded and written back as an uncompressed ALAC frame
//! of exactly the kept samples (see [`alac`]). The other frames are copied
//! verbatim, the edit list keeps everything, and the cut is sample-exact
//! and bit-exact everywhere.
//!
//! ## Tags
//!
//! iTunes-style metadata in `moov/udta/meta/ilst`, see [`ilst`]. Chapter
//! lists and other `udta` content are not copied.

pub mod aac;
pub mod alac;
pub mod boxes;
pub mod ilst;
mod mux;
pub mod video;

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;

use lac_core::{
    AudioStreamInfo, CutCapabilities, CutPlan, MediaTime, PacketFlags, PacketIndex, PacketInfo,
    PlanError, Rounding, Tags, VideoCutPlan, plan_av_cuts, plan_cuts, rescale,
};

use crate::{ContainerError, Result};
use boxes::{BoxRef, Reader};

/// Largest `moov` box read into memory. A 10-hour AAC recording has about
/// 1.7 million samples, i.e. a sample table of about 7 MB.
const MAX_MOOV: u64 = 256 << 20;

/// AAC samples (frames) written as pre-roll in front of a cut's first
/// sample: two. AAC-LC needs one (the MDCT overlap with the previous frame);
/// the second leaves room for decoders whose state reaches further
/// (HE-AAC's SBR filter bank).
pub const AAC_PREROLL_FRAMES: usize = 2;

/// Opus pre-roll in 48 kHz samples (RFC 7845 4.3: 80 ms), written in
/// front of a cut's first sample: the decoder converges within it.
pub const OPUS_PREROLL: u64 = 3840;

/// The sample rate of every Opus stream's decoder output (RFC 7845 4).
const OPUS_RATE: u32 = 48_000;

/// AMR pre-roll in milliseconds. AMR's predictors (LSF, pitch, gains)
/// carry state from frame to frame; on the fixtures AMR-NB decodes bit for
/// bit again after 200 ms of pre-roll.
pub const AMR_PREROLL_MS: u64 = 400;

/// What the export report says about every cut of an AMR-WB stream.
pub const AMR_WB_NOTE: &str = "this AMR-WB stream's highest band (6.4 to \
     7 kHz) is noise that decoders generate; after a cut, players generate \
     different noise of the same loudness there, everything else is \
     unchanged";

/// The codec of the audio track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mp4Codec {
    /// MPEG-4 AAC; `asc` is the AudioSpecificConfig (ISO/IEC 14496-3
    /// 1.6.2.1) from the `esds` box.
    Aac {
        /// The AudioSpecificConfig.
        asc: Vec<u8>,
    },
    /// Apple Lossless; `cookie` is the whole `alac` box of the sample
    /// entry (size, type, version and the 24-byte ALACSpecificConfig), the
    /// extradata FFmpeg's decoder expects.
    Alac {
        /// The `alac` box.
        cookie: Vec<u8>,
    },
    /// Opus, as Android phones record it (the Opus mapping for ISOBMFF,
    /// "Encapsulation of Opus in ISO Base Media File Format"); `head` is
    /// the `OpusHead` (RFC 7845 5.1) equivalent of the sample entry's
    /// `dOps` box, the extradata FFmpeg's decoder expects.
    Opus {
        /// The `OpusHead`.
        head: Vec<u8>,
    },
    /// AMR (3GPP TS 26.244 6.5: `samr` and `sawb` sample entries), as
    /// phones record voice notes and older phones videos in 3GP files.
    Amr {
        /// AMR-WB (16 kHz, 320 samples per frame) rather than AMR-NB
        /// (8 kHz, 160 samples per frame).
        wideband: bool,
    },
    /// Uncompressed audio, as cameras record it in QuickTime files
    /// (`sowt`, `twos`, `raw `, `in24`, `in32`, `fl32`, `fl64`, `lpcm`,
    /// `alaw`, `ulaw`; QuickTime File Format, "Sound sample descriptions")
    /// and ISO/IEC 23003-5 in MP4 (`ipcm`, `fpcm`).
    Pcm(PcmFormat),
}

/// How uncompressed samples are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    /// Integer, float, A-law or µ-law.
    pub encoding: crate::wav::SampleEncoding,
    /// Bits per sample (8, 16, 24, 32 or 64).
    pub bits: u16,
    /// Most significant byte first.
    pub big_endian: bool,
    /// 8-bit integers are signed (`twos`) rather than offset by 128
    /// (`raw `).
    pub signed_8bit: bool,
    /// Bytes per sample frame (all channels).
    pub frame_bytes: u32,
}

/// Sample frames per raw packet of uncompressed audio: QuickTime stores
/// each frame as a sample, which is far too fine for the packet index, so
/// the frames of a chunk are grouped (as WAV's are, see
/// [`crate::wav::BLOCK_FRAMES`]).
const PCM_BLOCK_FRAMES: u32 = 4096;

impl Mp4Codec {
    /// The codec's name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Aac { .. } => "AAC",
            Self::Alac { .. } => "ALAC",
            Self::Opus { .. } => "Opus",
            Self::Amr { wideband: false } => "AMR-NB",
            Self::Amr { wideband: true } => "AMR-WB",
            Self::Pcm(f) => match f.encoding {
                crate::wav::SampleEncoding::ALaw => "A-law",
                crate::wav::SampleEncoding::MuLaw => "µ-law",
                _ => "PCM",
            },
        }
    }
}

/// A sample (packet) as found in the source.
#[derive(Debug, Clone, Copy)]
struct RawPacket {
    offset: u64,
    len: u32,
    /// Decoder output in samples.
    out: u32,
    /// Sum of `out` of the preceding packets.
    start: u64,
}

/// An opened MP4 file.
#[derive(Debug, Clone)]
pub struct Mp4File {
    /// The source's `ftyp` box (header included), copied to exports.
    ftyp: Vec<u8>,
    codec: Mp4Codec,
    channels: u16,
    /// The media timescale, which is the sample rate.
    rate: u32,
    /// The `stsd` box (header included), copied to exports.
    stsd: Vec<u8>,
    /// Opus: where in `stsd` the `dOps` box's PreSkip lies.
    preskip_at: Option<usize>,
    /// ISO 639-2 language code as packed in `mdhd`.
    language: u16,
    raw: Vec<RawPacket>,
    /// Decoder output samples that belong to the timeline.
    audible: Range<u64>,
    /// How much later than the movie's start (in samples) the index's
    /// sample 1024 plays when the first AAC sample lasts longer than it
    /// decodes and no edit list says otherwise: ffmpeg's fragmented files
    /// fill the time before the first packet so. Only the video's
    /// alignment needs it; the index leaves the gap out.
    lead: u64,
    /// Raw packet of each index entry.
    entry_raw: Vec<usize>,
    index: PacketIndex,
    other_tracks: usize,
    video: Option<video::Mp4Video>,
    tags: Tags,
    /// `ilst` items [`Tags`] does not hold, as whole boxes.
    ilst_other: Vec<Vec<u8>>,
    warnings: Vec<String>,
}

impl Mp4File {
    /// Reads the box structure and the sample table of the first audio
    /// track. Reads `moov` into memory but never the media data.
    pub fn open<R: Read + Seek + ?Sized>(r: &mut R) -> Result<Self> {
        let len = r.seek(SeekFrom::End(0))?;
        let mut warnings = Vec::new();
        let mut ftyp = None;
        let mut moov = None;
        // Movie fragments: (file offset of the moof box, its payload).
        let mut fragments: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut fragment_bytes = 0u64;
        let mut pos = 0u64;
        while pos + 8 <= len {
            let mut head = [0u8; 16];
            let n = read_up_to(r, pos, &mut head)?;
            let Some((kind, hlen, size)) = boxes::parse_header(&head[..n], len - pos) else {
                if moov.is_some() {
                    warnings.push(format!("ignoring damaged data at byte {pos}"));
                    break;
                }
                return Err(malformed(pos, "invalid box header"));
            };
            let end = pos.saturating_add(size);
            if end > len {
                if &kind == b"mdat" {
                    warnings.push("the file is truncated (mdat ends early)".into());
                } else if &kind != b"moov" {
                    warnings.push(format!("the box at byte {pos} is truncated"));
                }
            }
            match &kind {
                b"ftyp" if ftyp.is_none() && size <= 4096 && end <= len => {
                    let mut b = vec![0u8; size as usize];
                    read_exact_at(r, pos, &mut b)?;
                    ftyp = Some(b);
                }
                b"moov" if moov.is_none() => {
                    if size > MAX_MOOV {
                        return Err(malformed(pos, "implausibly large moov box"));
                    }
                    if end > len {
                        return Err(malformed(pos, "the moov box is truncated"));
                    }
                    let mut b = vec![0u8; (size - hlen as u64) as usize];
                    read_exact_at(r, pos + hlen as u64, &mut b)?;
                    moov = Some(b);
                }
                b"moof" if end <= len => {
                    // Fragments hold the sample tables of the rest of the
                    // file, so they share moov's bound.
                    fragment_bytes += size;
                    if fragment_bytes > MAX_MOOV {
                        return Err(malformed(pos, "implausibly large movie fragments"));
                    }
                    let mut b = vec![0u8; (size - hlen as u64) as usize];
                    read_exact_at(r, pos + hlen as u64, &mut b)?;
                    fragments.push((pos, b));
                }
                _ => {}
            }
            // A box running past the end (its size may be up to 2^64 - 1)
            // ends the scan.
            pos = end.min(len);
        }
        let moov = moov.ok_or_else(|| {
            ContainerError::Unsupported(
                "MP4 file without a moov box (an unfinished recording?)".into(),
            )
        })?;
        let ftyp = ftyp.unwrap_or_else(default_ftyp);
        let movie_timescale = boxes::find(&moov, b"mvhd")
            .and_then(|b| parse_mvhd_timescale(b.body))
            .ok_or_else(|| malformed(0, "missing or invalid mvhd box"))?;

        let mut track = None;
        let mut video_track = None;
        let mut other_tracks = 0;
        let frag = Fragments {
            moofs: &fragments,
            defaults: parse_trex(&moov),
        };
        for trak in boxes::Boxes::new(&moov).filter(|b| &b.kind == b"trak") {
            let handler = boxes::path(trak.body, &[b"mdia", b"hdlr"])
                .and_then(|h| h.body.get(8..12))
                .unwrap_or_default();
            if handler == b"vide" && video_track.is_none() {
                // A video track that cannot be read never keeps the audio
                // from opening; it is then left out like other tracks.
                match video::parse_video_track(r, trak, movie_timescale, &frag, len, &mut warnings)
                {
                    Ok(v) => video_track = Some(v),
                    Err(e) => {
                        warnings.push(format!("ignoring the video track: {e}"));
                        other_tracks += 1;
                    }
                }
                continue;
            }
            if handler != b"soun" || track.is_some() {
                other_tracks += 1;
                continue;
            }
            track = Some(parse_track(
                r,
                trak,
                movie_timescale,
                &frag,
                len,
                &mut warnings,
            )?);
        }
        let t = track.ok_or_else(|| {
            ContainerError::Unsupported("no audio track found in the MP4 file".into())
        })?;
        if other_tracks > 0 {
            warnings.push(format!(
                "ignoring {other_tracks} other track(s); only the first audio and video track are cut"
            ));
        }
        let meta = boxes::path(&moov, &[b"udta", b"meta"]);
        let ilst::Ilst { tags, other } = meta.map(|m| ilst::parse_meta(m.body)).unwrap_or_default();

        let mut packets = Vec::new();
        let mut entry_raw = Vec::new();
        let mut pos = 0u64;
        for (i, p) in t.raw.iter().enumerate() {
            let end = p.start + u64::from(p.out);
            let a = p.start.max(t.audible.start);
            let b = end.min(t.audible.end);
            if b <= a {
                continue;
            }
            packets.push(PacketInfo {
                byte_offset: p.offset,
                byte_len: p.len,
                first_sample: pos,
                // At most one packet's output, which fits u32.
                n_samples: (b - a) as u32,
                hidden_before: (a - p.start) as u32,
                hidden_after: (end - b) as u32,
                flags: PacketFlags {
                    independent: matches!(t.codec, Mp4Codec::Alac { .. } | Mp4Codec::Pcm(_)),
                },
            });
            entry_raw.push(i);
            pos += b - a;
        }
        let info = AudioStreamInfo {
            sample_rate: t.rate,
            channels: t.channels,
        };
        let index = PacketIndex::new(info, packets)
            .map_err(|e| ContainerError::InvalidPlan(format!("internal index error: {e}")))?;
        Ok(Self {
            ftyp,
            codec: t.codec,
            channels: t.channels,
            rate: t.rate,
            stsd: t.stsd,
            preskip_at: t.preskip_at,
            language: t.language,
            raw: t.raw,
            audible: t.audible,
            lead: t.lead,
            entry_raw,
            index,
            other_tracks,
            video: video_track,
            tags,
            ilst_other: other,
            warnings,
        })
    }

    /// The first video track, if the file has a readable one.
    pub fn video(&self) -> Option<&video::Mp4Video> {
        self.video.as_ref()
    }

    /// The audio codec.
    pub fn codec(&self) -> &Mp4Codec {
        &self.codec
    }

    /// Number of tracks besides the first audio and the first video track,
    /// which are ignored.
    pub fn other_tracks(&self) -> usize {
        self.other_tracks
    }

    /// One entry per sample that produces timeline output.
    pub fn index(&self) -> &PacketIndex {
        &self.index
    }

    /// The source's tags (see [`ilst`]).
    pub fn tags(&self) -> &Tags {
        &self.tags
    }

    /// Problems that were tolerated while opening the file.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Reads the payload of index entry `entry`.
    pub fn read_packet<R: Read + Seek + ?Sized>(&self, r: &mut R, entry: usize) -> Result<Vec<u8>> {
        let raw = self
            .entry_raw
            .get(entry)
            .ok_or_else(|| ContainerError::InvalidPlan(format!("no packet {entry}")))?;
        read_raw(r, &self.raw[*raw])
    }

    /// Whether the AAC stream codes any band with perceptual noise
    /// substitution (see [`aac`]), judged from the first channel of each
    /// frame. Reads every frame, stopping at the first such band; `false`
    /// for ALAC and for AAC object types other than LC and HE-AAC (whose
    /// core is LC).
    pub fn uses_noise_substitution<R: Read + Seek + ?Sized>(&self, r: &mut R) -> Result<bool> {
        let Mp4Codec::Aac { asc } = &self.codec else {
            return Ok(false);
        };
        // ISO/IEC 14496-3 1.6.2.1: audioObjectType, 5 bits (2 LC, 5 SBR,
        // 29 PS); escape value 31 is not used by these.
        if !matches!(asc.first().map(|b| b >> 3), Some(2 | 5 | 29)) {
            return Ok(false);
        }
        // The element header, ics_info and section data of the first
        // channel take well under 128 bytes (49 bands of at most 9 bits
        // each, plus 49 M/S bits).
        let mut head = [0u8; 128];
        for raw in &self.raw {
            let n = (raw.len as usize).min(head.len());
            read_exact_at(r, raw.offset, &mut head[..n])?;
            if aac::first_channel(&head[..n]).is_some_and(|c| c.uses_noise_substitution()) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The video media time (ticks of the video timescale) that plays
    /// with the index's audio sample 0, for a file with a video track.
    pub fn video_timeline_offset(&self) -> Option<MediaTime> {
        self.video.as_ref().map(|v| self.video_offset(v))
    }

    /// The video media time that plays with the index's audio sample 0.
    fn video_offset(&self, v: &video::Mp4Video) -> MediaTime {
        let lead = rescale(
            MediaTime::try_from(self.lead).unwrap_or(0),
            self.rate,
            v.index().info().timescale,
            Rounding::Nearest,
        );
        v.video_offset() + lead
    }

    /// Plans the joined output of the kept `ranges`: with a video track,
    /// both tracks together ([`lac_core::plan_av_cuts`]: joins move to
    /// keyframes, parts may merge), else the audio alone.
    pub fn plan(&self, ranges: &[Range<u64>]) -> std::result::Result<Vec<CutPlan>, PlanError> {
        match &self.video {
            // The video media time shown at audio sample 0: the source's
            // video edit list (the audio's is already in the index).
            Some(v) => plan_av_cuts(
                &self.index,
                self.capabilities(),
                v.index(),
                self.video_offset(v),
                ranges,
            ),
            None => plan_cuts(&self.index, self.capabilities(), ranges),
        }
    }

    /// AAC and Opus: sample-exact at the start and end of the output file
    /// (edit list); joins inside the file are packet-granular. Pre-roll is
    /// added and hidden inside [`Mp4File::write_cut`]. ALAC: sample-exact
    /// everywhere, since a frame cut by a cut point is rewritten as an
    /// uncompressed frame of the kept samples (see [`alac`]).
    pub fn capabilities(&self) -> CutCapabilities {
        match self.codec {
            Mp4Codec::Aac { .. } | Mp4Codec::Opus { .. } | Mp4Codec::Amr { .. } => {
                CutCapabilities {
                    preroll_packets: 0,
                    can_trim_start: true,
                    can_trim_end: true,
                    lossless_reencode: false,
                    trims_at_file_ends_only: true,
                }
            }
            Mp4Codec::Alac { .. } => CutCapabilities {
                lossless_reencode: true,
                ..CutCapabilities::default()
            },
            // Any frame boundary is a cut point: the frames a cut keeps of
            // a packet are written as a packet of their own.
            Mp4Codec::Pcm(_) => CutCapabilities::PCM,
        }
    }

    /// One section: every sample of the track, including those the edit
    /// list hides; `keep` is the timeline part.
    pub fn decode_sections<R: Read + Seek + ?Sized>(
        &self,
        _r: &mut R,
    ) -> Result<Vec<crate::DecodeSection>> {
        if self.raw.is_empty() {
            return Ok(Vec::new());
        }
        let (codec, extradata) = match &self.codec {
            Mp4Codec::Aac { asc } => (crate::Codec::Aac, asc.clone()),
            Mp4Codec::Alac { cookie } => (crate::Codec::Alac, cookie.clone()),
            Mp4Codec::Opus { head } => (crate::Codec::Opus, head.clone()),
            Mp4Codec::Amr { wideband: false } => (crate::Codec::AmrNb, Vec::new()),
            Mp4Codec::Amr { wideband: true } => (crate::Codec::AmrWb, Vec::new()),
            Mp4Codec::Pcm(f) => (
                crate::Codec::Pcm {
                    encoding: f.encoding,
                    bits_per_sample: f.bits,
                    // Fits: checked when parsing.
                    block_align: f.frame_bytes as u16,
                    big_endian: f.big_endian,
                    signed_8bit: f.signed_8bit,
                },
                Vec::new(),
            ),
        };
        Ok(vec![crate::DecodeSection {
            codec,
            sample_rate: self.rate,
            channels: self.channels,
            extradata,
            packet_ends: self
                .raw
                .iter()
                .map(|p| p.start + u64::from(p.out))
                .collect(),
            keep: self.audible.clone(),
        }])
    }

    /// Sample `packet` of section `section` (there is only one).
    pub fn read_decode_packet<R: Read + Seek + ?Sized>(
        &self,
        r: &mut R,
        section: usize,
        packet: usize,
    ) -> Result<Vec<u8>> {
        if section != 0 {
            return Err(crate::decode::no_section(section));
        }
        let p = self
            .raw
            .get(packet)
            .ok_or_else(|| ContainerError::InvalidPlan(format!("no packet {packet}")))?;
        read_raw(r, p)
    }

    /// Writes the kept samples of `plans`, joined, as a new MP4 file. For
    /// AAC and Opus only the start of the first and the end of the last non-empty
    /// plan may be trimmed (see [`Mp4File::capabilities`]). `tags`, or
    /// else the source's tags, are written as the `ilst`.
    pub fn write_cut<R, W>(
        &self,
        r: &mut R,
        plans: &[CutPlan],
        tags: Option<&Tags>,
        w: &mut W,
    ) -> Result<()>
    where
        R: Read + Seek + ?Sized,
        W: Write + Seek + ?Sized,
    {
        let n = self.entry_raw.len();
        let kept: Vec<&CutPlan> = plans.iter().filter(|p| !p.packets.is_empty()).collect();
        for (i, p) in kept.iter().enumerate() {
            if p.packets.end > n {
                return Err(ContainerError::InvalidPlan(format!(
                    "packets {:?} outside the index (length {n})",
                    p.packets
                )));
            }
            let trims_inside =
                (p.trim_start != 0 && i != 0) || (p.trim_end != 0 && i + 1 != kept.len());
            if trims_inside && !matches!(self.codec, Mp4Codec::Alac { .. } | Mp4Codec::Pcm(_)) {
                return Err(ContainerError::InvalidPlan(format!(
                    "MP4 with {} can only trim at the start and end of the file",
                    self.codec.name()
                )));
            }
        }
        let tags = tags.unwrap_or(&self.tags);
        let video_out = self.video_track(plans)?;
        let video = video_out
            .as_ref()
            .map(|(t, frames)| (t.clone(), frames.as_slice()));
        if let Mp4Codec::Alac { cookie } = &self.codec {
            let samples = self.alac_samples(r, cookie, &kept)?;
            return self.write_file(r, &samples, (0, 0), video, tags, w);
        }
        if let Mp4Codec::Pcm(f) = &self.codec {
            let samples = self.pcm_samples(r, f.frame_bytes, &kept)?;
            return self.write_file(r, &samples, (0, 0), video, tags, w);
        }
        let (Some(first), Some(last)) = (kept.first(), kept.last()) else {
            // Nothing kept: a valid file with an empty track.
            return self.write_file(r, &[], (0, 0), video, tags, w);
        };

        // Raw packets to write: pre-roll, then the kept entries in order.
        let r_first = self.entry_raw[first.packets.start];
        let r0 = self.preroll_start(r_first);
        let mut order: Vec<OutSample> = (r0..r_first).map(OutSample::Source).collect();
        for p in &kept {
            order.extend(
                p.packets
                    .clone()
                    .map(|e| OutSample::Source(self.entry_raw[e])),
            );
        }

        // media_time: the pre-roll plus whatever of the first packet lies
        // before the wanted start (hidden in the source or trimmed).
        let fp = &self.raw[r_first];
        let start = fp.start.max(self.audible.start) + first.trim_start;
        let media_time = start - self.raw[r0].start;
        // The part of the last packet after the wanted end.
        let lp = &self.raw[self.entry_raw[last.packets.end - 1]];
        let lp_end = lp.start + u64::from(lp.out);
        let discard = lp_end - (lp_end.min(self.audible.end) - last.trim_end);
        self.write_file(r, &order, (media_time, discard), video, tags, w)
    }

    /// The first raw packet to write for a cut whose first kept sample is
    /// raw packet `first`: [`AAC_PREROLL_FRAMES`] earlier for AAC, as many
    /// as hold [`OPUS_PREROLL`] samples for Opus and [`AMR_PREROLL_MS`] for
    /// AMR (fewer at the stream's start, where the decoder starts clean
    /// anyway).
    fn preroll_start(&self, first: usize) -> usize {
        let preroll = match self.codec {
            Mp4Codec::Opus { .. } => OPUS_PREROLL,
            Mp4Codec::Amr { .. } => u64::from(self.rate) * AMR_PREROLL_MS / 1000,
            _ => 0,
        };
        match self.codec {
            Mp4Codec::Opus { .. } | Mp4Codec::Amr { .. } => {
                let mut start = first;
                let mut held = 0u64;
                while start > 0 && held < preroll {
                    start -= 1;
                    held += u64::from(self.raw[start].out);
                }
                start
            }
            _ => first.saturating_sub(AAC_PREROLL_FRAMES),
        }
    }

    /// The video track of a cut and the source frames (decode indexes) of
    /// its samples, from the plans' video parts; `None` when the plans
    /// have none (audio-only planning, or a file without video).
    ///
    /// Frames are copied verbatim with their durations, composition
    /// offsets and (checked) keyframe flags. Each part's decode and
    /// presentation times are shifted so its first shown frame follows the
    /// last shown frame of the part before; the last decoded frame of a
    /// part gets whatever duration that needs. The edit list hides what the
    /// first part shows before its actual start and the last part after
    /// its actual end.
    fn video_track(&self, plans: &[CutPlan]) -> Result<Option<(mux::TrakOut, Vec<usize>)>> {
        let Some(v) = &self.video else {
            return Ok(None);
        };
        let parts: Vec<&VideoCutPlan> = plans
            .iter()
            .filter_map(|p| p.video.as_ref())
            .filter(|p| !p.frames.is_empty())
            .collect();
        if parts.is_empty() {
            return Ok(None);
        }
        let index = v.index();
        let n = index.frames().len();
        let mut frames = Vec::new();
        let mut durations: Vec<u32> = Vec::new();
        let mut ctts = Vec::new();
        let mut sync = Vec::new();
        // Output time = source time + shift of the part.
        let mut first_shift = 0;
        // End of the decode times and of the shown span so far (output).
        let mut dts_end: MediaTime = 0;
        let mut shown_end: MediaTime = 0;
        for (i, p) in parts.iter().enumerate() {
            if p.frames.end > n || !index.frames()[p.frames.start].sync {
                return Err(ContainerError::InvalidPlan(format!(
                    "video frames {:?} do not start at a keyframe inside the track ({n} frames)",
                    p.frames
                )));
            }
            let first_dts = index.decode_time(p.frames.start);
            let shift = if i == 0 {
                first_shift = -first_dts;
                first_shift
            } else {
                // Shown right after the previous part, unless the decode
                // times would run backwards (a deeper frame reordering than
                // before): then the previous picture stays a little longer.
                let shift = (shown_end - p.actual.start).max(dts_end - first_dts);
                let gap = first_dts + shift - dts_end;
                if let Some(d) = durations.last_mut() {
                    *d = u32::try_from(MediaTime::from(*d) + gap).map_err(|_| {
                        ContainerError::InvalidPlan("video join gap too large".into())
                    })?;
                }
                shift
            };
            for k in p.frames.clone() {
                let f = &index.frames()[k];
                frames.push(k);
                durations.push(f.duration);
                ctts.push(f.composition_offset);
                sync.push(f.sync);
            }
            let last = p.frames.end - 1;
            dts_end =
                index.decode_time(last) + MediaTime::from(index.frames()[last].duration) + shift;
            shown_end = p.actual.end + shift;
        }
        if durations.contains(&0) {
            return Err(ContainerError::InvalidPlan(
                "video frame without duration after a join".into(),
            ));
        }
        let media_time = parts[0].actual.start + first_shift;
        let shown = (shown_end - media_time).max(0);
        let rate = self.rate;
        let timescale = index.info().timescale;
        let edit_duration = rescale(shown, timescale, rate, Rounding::Nearest).max(0) as u64;
        // Where the first picture belongs on the output's (audio) timeline:
        // the audio of the parts before it plus, inside its part, how far
        // the picture comes after the part's audio start. It comes later
        // when the kept range starts before the source's first picture
        // (B-frame delay without an edit list, or a late video track); an
        // empty edit then delays the video as in the source. A picture
        // that would come earlier is the frame-exact start itself.
        let first = plans
            .iter()
            .position(|p| p.video.as_ref().is_some_and(|v| !v.frames.is_empty()))
            .unwrap_or(0);
        let before: u64 = plans[..first]
            .iter()
            .map(|p| p.actual.end - p.actual.start)
            .sum();
        let picture = rescale(
            parts[0].actual.start - self.video_offset(v),
            timescale,
            rate,
            Rounding::Nearest,
        );
        let into_part = u64::try_from(picture)
            .unwrap_or(0)
            .saturating_sub(plans[first].actual.start);
        let empty_edit = before + into_part;
        let all_sync = sync.iter().all(|&s| s);
        let trak = mux::TrakOut {
            id: 2,
            handler: mux::Handler::Video {
                matrix: v.tkhd_matrix(),
                size: v.tkhd_size(),
            },
            timescale,
            language: v.language(),
            stsd: v.stsd().to_vec(),
            sizes: frames.iter().map(|&k| index.frames()[k].byte_len).collect(),
            durations,
            ctts,
            sync: (!all_sync).then_some(sync),
            roll: None,
            empty_edit,
            media_time,
            edit_duration,
            pcm_frame_bytes: None,
        };
        Ok(Some((trak, frames)))
    }

    /// The samples of an uncompressed cut: kept packets verbatim, and of
    /// each packet that a cut point cuts, the kept frames' bytes.
    fn pcm_samples<R: Read + Seek + ?Sized>(
        &self,
        r: &mut R,
        frame_bytes: u32,
        kept: &[&CutPlan],
    ) -> Result<Vec<OutSample>> {
        let mut out = Vec::new();
        for p in kept {
            for e in p.packets.clone() {
                let raw_i = self.entry_raw[e];
                let raw = &self.raw[raw_i];
                let info = &self.index.packets()[e];
                let skip = u64::from(info.hidden_before)
                    + if e == p.packets.start {
                        p.trim_start
                    } else {
                        0
                    };
                let drop = u64::from(info.hidden_after)
                    + if e + 1 == p.packets.end {
                        p.trim_end
                    } else {
                        0
                    };
                if skip == 0 && drop == 0 {
                    out.push(OutSample::Source(raw_i));
                    continue;
                }
                let end = u64::from(raw.out)
                    .checked_sub(drop)
                    .filter(|&end| end > skip)
                    .ok_or_else(|| {
                        ContainerError::InvalidPlan(format!("trims beyond packet {e}"))
                    })?;
                let fb = u64::from(frame_bytes);
                let mut data = vec![0; ((end - skip) * fb) as usize];
                read_exact_at(r, raw.offset + skip * fb, &mut data)?;
                out.push(OutSample::Rewritten {
                    data,
                    // At most one packet's frames.
                    out: (end - skip) as u32,
                });
            }
        }
        Ok(out)
    }

    /// The samples of an ALAC cut: kept frames verbatim, and each frame
    /// that a cut point (or the source's edit list) cuts decoded and
    /// rewritten as an uncompressed frame of exactly its kept samples.
    fn alac_samples<R: Read + Seek + ?Sized>(
        &self,
        r: &mut R,
        cookie: &[u8],
        kept: &[&CutPlan],
    ) -> Result<Vec<OutSample>> {
        let cfg = alac::AlacConfig::parse(cookie)
            .map_err(|e| malformed(0, &format!("sample description: {e}")))?;
        let mut out = Vec::new();
        for p in kept {
            for e in p.packets.clone() {
                let info = &self.index.packets()[e];
                let raw_i = self.entry_raw[e];
                let mut skip = u64::from(info.hidden_before);
                let mut drop = u64::from(info.hidden_after);
                if e == p.packets.start {
                    skip += p.trim_start;
                }
                if e + 1 == p.packets.end {
                    drop += p.trim_end;
                }
                if skip == 0 && drop == 0 {
                    out.push(OutSample::Source(raw_i));
                    continue;
                }
                let raw = &self.raw[raw_i];
                let frame = alac::decode(&cfg, &read_raw(r, raw)?)
                    .map_err(|err| malformed(raw.offset, &err.to_string()))?;
                let len = frame.len() as u64;
                if len != u64::from(raw.out) {
                    return Err(malformed(
                        raw.offset,
                        "ALAC frame length differs from its sample duration",
                    ));
                }
                let end = len
                    .checked_sub(drop)
                    .filter(|&end| end > skip)
                    .ok_or_else(|| {
                        ContainerError::InvalidPlan(format!("trims beyond packet {e}"))
                    })?;
                let data = frame.encode_escaped(&cfg, skip as usize..end as usize);
                if u32::try_from(data.len()).is_err() {
                    return Err(ContainerError::InvalidPlan(format!(
                        "uncompressed frame of packet {e} too large"
                    )));
                }
                out.push(OutSample::Rewritten {
                    data,
                    // At most one frame's length.
                    out: (end - skip) as u32,
                });
            }
        }
        Ok(out)
    }

    /// Writes `ftyp`, `moov` and `mdat` for `samples`, of whose decoder
    /// output `trims` hides the first and the last samples (the edit
    /// list's media time and the discarded tail), plus `video`: a video
    /// track and the source frames (decode indexes) of its samples.
    fn write_file<R, W>(
        &self,
        r: &mut R,
        samples: &[OutSample],
        (media_time, discard): (u64, u64),
        video: Option<(mux::TrakOut, &[usize])>,
        tags: &Tags,
        w: &mut W,
    ) -> Result<()>
    where
        R: Read + Seek + ?Sized,
        W: Write + Seek + ?Sized,
    {
        let sizes: Vec<u32> = samples
            .iter()
            .map(|s| match s {
                OutSample::Source(i) => self.raw[*i].len,
                // Fits: checked in `alac_samples`.
                OutSample::Rewritten { data, .. } => data.len() as u32,
            })
            .collect();
        // Sample durations: the decoder output. For AAC and Opus the last
        // one is shortened by the discarded tail, as ffmpeg writes it. ALAC
        // cuts discard nothing: cut frames are rewritten to their kept
        // length.
        let mut durations: Vec<u32> = samples
            .iter()
            .map(|s| match s {
                OutSample::Source(i) => self.raw[*i].out,
                OutSample::Rewritten { out, .. } => *out,
            })
            .collect();
        let decoded: u64 = durations.iter().map(|&d| u64::from(d)).sum();
        let lossy = !matches!(self.codec, Mp4Codec::Alac { .. } | Mp4Codec::Pcm(_));
        if let (Some(d), true) = (durations.last_mut(), lossy) {
            // discard < out: the last packet keeps at least one sample.
            *d -= discard as u32;
        }
        let media_duration: u64 = durations.iter().map(|&d| u64::from(d)).sum();
        let duration = decoded - media_time - discard;
        let udta = ilst::udta(&ilst::write(tags, &self.ilst_other));
        let stsd = self.stsd_for(sizes.iter().copied().max().unwrap_or(0), media_time);

        let audio = mux::TrakOut {
            id: 1,
            handler: mux::Handler::Sound,
            timescale: self.rate,
            language: self.language,
            stsd,
            durations,
            sizes,
            ctts: Vec::new(),
            sync: None,
            roll: self.roll_distance(),
            empty_edit: 0,
            media_time: i64::try_from(media_time).unwrap_or(i64::MAX),
            edit_duration: duration,
            pcm_frame_bytes: match &self.codec {
                Mp4Codec::Pcm(f) => Some(f.frame_bytes),
                _ => None,
            },
        };
        debug_assert_eq!(
            audio.durations.iter().map(|&d| u64::from(d)).sum::<u64>(),
            media_duration
        );
        let mut tracks = vec![audio];
        let mut video_frames: &[usize] = &[];
        if let Some((trak, frames)) = video {
            tracks.push(trak);
            video_frames = frames;
        }
        let chunks = mux::layout(&tracks);
        let data_len: u64 = tracks
            .iter()
            .flat_map(|t| t.sizes.iter())
            .map(|&s| u64::from(s))
            .sum();
        let n_samples: u64 = tracks.iter().map(|t| t.sizes.len() as u64).sum();
        // co64 and a 64-bit mdat header once the file may pass 4 GiB.
        let large = data_len + (1 << 24) + n_samples * 16 > u64::from(u32::MAX);
        let mdat_header = if large { 16 } else { 8 };
        // Chunk offsets per track, in each track's chunk order, for media
        // data starting at `data_start`.
        let offsets = |data_start: u64| {
            let mut out = vec![Vec::new(); tracks.len()];
            let mut off = data_start;
            for c in &chunks {
                out[c.track].push(off);
                off += tracks[c.track].sizes[c.samples.clone()]
                    .iter()
                    .map(|&s| u64::from(s))
                    .sum::<u64>();
            }
            out
        };
        let probe = mux::moov(self.rate, &tracks, &chunks, &offsets(0), large, &udta);
        let data_start = (self.ftyp.len() + probe.len() + mdat_header) as u64;
        let moov = mux::moov(
            self.rate,
            &tracks,
            &chunks,
            &offsets(data_start),
            large,
            &udta,
        );
        debug_assert_eq!(moov.len(), probe.len());

        let mut head = self.ftyp.clone();
        head.extend(moov);
        if large {
            head.extend(1u32.to_be_bytes());
            head.extend(b"mdat");
            head.extend((data_len + 16).to_be_bytes());
        } else {
            head.extend(((data_len + 8) as u32).to_be_bytes());
            head.extend(b"mdat");
        }
        w.write_all(&head)?;
        let frames = self.video.as_ref().map(|v| v.index().frames());
        for c in &chunks {
            for i in c.samples.clone() {
                if c.track == 0 {
                    match &samples[i] {
                        OutSample::Source(i) => w.write_all(&read_raw(r, &self.raw[*i])?)?,
                        OutSample::Rewritten { data, .. } => w.write_all(data)?,
                    }
                } else if let Some(f) = frames.and_then(|f| f.get(video_frames[i])) {
                    let mut buf = vec![0u8; f.byte_len as usize];
                    read_exact_at(r, f.byte_offset, &mut buf)?;
                    w.write_all(&buf)?;
                }
            }
        }
        Ok(())
    }

    /// The `roll` sample group's distance for the codec (ISO/IEC
    /// 14496-12 10.1): how many samples before a sample a player must
    /// decode from to play it correctly; `None` for ALAC, whose frames are
    /// independent. AAC needs its predecessor; Opus 80 ms (the Opus
    /// mapping for ISOBMFF, 4.4), counted in samples of the common length.
    fn roll_distance(&self) -> Option<i16> {
        match self.codec {
            Mp4Codec::Aac { .. } => Some(-1),
            // ffmpeg writes none for AMR, and players start AMR anywhere.
            Mp4Codec::Alac { .. } | Mp4Codec::Amr { .. } | Mp4Codec::Pcm(_) => None,
            Mp4Codec::Opus { .. } => {
                let outs: Vec<u32> = self.raw.iter().map(|p| p.out).collect();
                let frame = u64::from(most_common(&outs).unwrap_or(960).max(1));
                let n = OPUS_PREROLL.div_ceil(frame);
                Some(-i16::try_from(n).unwrap_or(i16::MAX))
            }
        }
    }

    /// The source's `stsd` for an output whose largest sample has
    /// `largest` bytes and whose edit list starts at `media_time`.
    ///
    /// An ALAC configuration states the largest frame of the stream (0 for
    /// unknown); an uncompressed frame may exceed it, so it is raised then,
    /// as decoders may size their buffers by it.
    ///
    /// Opus states its pre-skip in `dOps` as well as in the edit list;
    /// players use one or the other (ffmpeg the edit list, and the
    /// pre-skip when told to ignore edit lists), so it is set to the edit
    /// list's start, which then hides the pre-roll and the cut-away head.
    fn stsd_for(&self, largest: u32, media_time: u64) -> Vec<u8> {
        let mut stsd = self.stsd.clone();
        if let (Some(at), Ok(skip)) = (self.preskip_at, u16::try_from(media_time))
            && let Some(field) = stsd.get_mut(at..at + 2)
        {
            field.copy_from_slice(&skip.to_be_bytes());
        }
        let Mp4Codec::Alac { cookie } = &self.codec else {
            return stsd;
        };
        let Ok(cfg) = alac::AlacConfig::parse(cookie) else {
            return stsd;
        };
        if cfg.max_frame_bytes == 0 || cfg.max_frame_bytes >= largest {
            return stsd;
        }
        // The cookie is the sample entry's `alac` box, verbatim.
        if let Some(at) = stsd.windows(cookie.len()).position(|w| w == &cookie[..]) {
            let field = at + alac::MAX_FRAME_BYTES_AT;
            stsd[field..field + 4].copy_from_slice(&largest.to_be_bytes());
        }
        stsd
    }
}

/// A sample of an output file.
enum OutSample {
    /// A raw packet of the source, copied verbatim.
    Source(usize),
    /// A rewritten ALAC frame and its decoder output in samples.
    Rewritten { data: Vec<u8>, out: u32 },
}

/// The audio track as parsed from its `trak` box.
struct Track {
    codec: Mp4Codec,
    channels: u16,
    rate: u32,
    stsd: Vec<u8>,
    /// See [`Mp4File`]'s field of the same name.
    preskip_at: Option<usize>,
    language: u16,
    raw: Vec<RawPacket>,
    audible: Range<u64>,
    /// See [`Mp4File`]'s field of the same name.
    lead: u64,
}

/// `ftyp` for files without one: M4A, compatible with MP4 and ISO.
fn default_ftyp() -> Vec<u8> {
    let mut out = Vec::new();
    boxes::put(&mut out, b"ftyp", b"M4A \0\0\x02\0M4A mp42isom");
    out
}

/// The timescale from an `mvhd` body (ISO/IEC 14496-12 8.2.2).
fn parse_mvhd_timescale(b: &[u8]) -> Option<u32> {
    let mut r = Reader::new(b);
    let version = r.u8()?;
    r.bytes(3)?;
    r.bytes(if version == 1 { 16 } else { 8 })?;
    r.u32().filter(|&t| t > 0)
}

/// Parses an audio `trak` box, with its samples in the movie fragments
/// `frag`. Reads the first bytes of an Opus track's first and last
/// sample from `r`.
fn parse_track<R: Read + Seek + ?Sized>(
    r: &mut R,
    trak: BoxRef,
    movie_timescale: u32,
    frag: &Fragments,
    file_len: u64,
    warnings: &mut Vec<String>,
) -> Result<Track> {
    let at = |what: &str| malformed(0, &format!("audio track: {what}"));
    let mdia = boxes::find(trak.body, b"mdia").ok_or_else(|| at("no mdia box"))?;
    let mdhd = boxes::find(mdia.body, b"mdhd").ok_or_else(|| at("no mdhd box"))?;
    let (timescale, language) = parse_mdhd(mdhd.body).ok_or_else(|| at("invalid mdhd"))?;
    let stbl = boxes::path(mdia.body, &[b"minf", b"stbl"]).ok_or_else(|| at("no sample table"))?;
    let stsd = boxes::find(stbl.body, b"stsd").ok_or_else(|| at("no stsd box"))?;
    let (codec, channels, preskip_at) = parse_stsd(stsd.body)?;
    let mut stsd_box = Vec::new();
    boxes::put(&mut stsd_box, b"stsd", stsd.body);
    // The body follows the box header.
    let preskip_at = preskip_at.map(|at| at + stsd_box.len() - stsd.body.len());
    let opus = matches!(codec, Mp4Codec::Opus { .. });
    // Sample durations would have to be rescaled to the decoder's rate
    // otherwise; the mappings recommend it and recorders write it.
    let (needed, amr_frame) = match codec {
        Mp4Codec::Opus { .. } => (Some(OPUS_RATE), None),
        Mp4Codec::Amr { wideband: false } => (Some(8_000), Some(160)),
        Mp4Codec::Amr { wideband: true } => (Some(16_000), Some(320)),
        _ => (None, None),
    };
    if let Some(rate) = needed.filter(|&r| r != timescale) {
        return Err(ContainerError::Unsupported(format!(
            "{} in MP4 with a timescale of {timescale} (only {rate} is supported)",
            codec.name()
        )));
    }

    // The decoder output of each sample; `stts_total` is the sum of the
    // durations, which may state padding at the end; `first_duration`
    // the first sample's, which may state a gap before it.
    let (raw, stts_total, first_duration) = if let Mp4Codec::Pcm(fmt) = &codec {
        if !frag.moofs.is_empty() {
            return Err(ContainerError::Unsupported(
                "uncompressed audio in movie fragments".into(),
            ));
        }
        let raw = read_pcm(stbl.body, fmt.frame_bytes, file_len, warnings)?;
        let total = raw.last().map_or(0, |p| p.start + u64::from(p.out));
        (raw, total, None)
    } else {
        let Samples {
            offsets,
            sizes,
            durations,
            ..
        } = read_samples(trak, stbl.body, frag, file_len, "audio", warnings)?;
        let count = offsets.len();

        let fixed_out = match codec {
            // An AAC frame always decodes to the same length: the most common
            // duration. The encoder may shorten the last sample's to state its
            // padding, and a muxer may lengthen the first one's to fill a gap
            // before it (ffmpeg's fragmented files without an edit list).
            Mp4Codec::Aac { .. } => most_common(&durations[..count]),
            _ => None,
        };
        let mut raw = Vec::with_capacity(count);
        let mut start = 0u64;
        let mut stts_total = 0u64;
        for i in 0..count {
            let len = sizes[i];
            let end = offsets[i].saturating_add(u64::from(len));
            if end > file_len {
                warnings.push(format!(
                    "the file is truncated; {} of {count} samples are present",
                    raw.len()
                ));
                break;
            }
            let mut out = fixed_out.unwrap_or(durations[i]);
            if let Some(frame) = amr_frame {
                // A sample holds one or more whole frames (3GPP TS 26.244
                // 6.7: `frames_per_sample`); a shortened duration (the last
                // sample's, stating padding) still decodes whole frames.
                out = out.div_ceil(frame) * frame;
            }
            if opus && (i == 0 || i + 1 == count) {
                // An Opus packet's duration is in its TOC (RFC 6716 3.1). The
                // muxer shortens the last sample's `stts` duration by the
                // encoder's padding, and may lengthen the first one's to fill
                // a gap before it, like for AAC.
                let mut toc = [0u8; 2];
                let n = read_up_to(r, offsets[i], &mut toc[..(len as usize).min(2)])?;
                match crate::ogg::codec::opus_packet_samples(&toc[..n]) {
                    Ok(samples) => out = samples,
                    Err(e) => warnings.push(format!("Opus sample {i}: {e}")),
                }
            }
            if out == 0 {
                warnings.push(format!("skipped sample {i} without duration"));
                continue;
            }
            raw.push(RawPacket {
                offset: offsets[i],
                len,
                out,
                start,
            });
            start += u64::from(out);
            stts_total += u64::from(durations[i]);
        }
        let first_duration = durations.first().copied();
        (raw, stts_total, first_duration)
    };
    let total = raw.last().map_or(0, |p| p.start + u64::from(p.out));

    // The edit list (ISO/IEC 14496-12 8.6.6): the first non-empty edit.
    let edits = boxes::path(trak.body, &[b"edts", b"elst"])
        .and_then(|b| parse_elst(b.body))
        .unwrap_or_default();
    let mut real = edits.iter().filter(|e| e.1 >= 0);
    let audible = match real.next() {
        None => {
            if !edits.is_empty() {
                warnings.push("the edit list holds no media; ignoring it".into());
            }
            0..stts_total.min(total)
        }
        Some(&(dur, media_time)) => {
            if edits.first().is_some_and(|e| e.1 < 0) {
                warnings.push("ignoring an empty edit (leading silence) in the edit list".into());
            }
            if real.next().is_some() {
                warnings.push("only the first edit of the edit list is used".into());
            }
            let a = (media_time as u64).min(total);
            // Duration is in the movie timescale; 0 means "to the end".
            let len = (u128::from(dur) * u128::from(timescale) + u128::from(movie_timescale) / 2)
                / u128::from(movie_timescale);
            let b = if dur == 0 {
                total
            } else {
                a.saturating_add(u64::try_from(len).unwrap_or(u64::MAX))
                    .min(total)
            };
            a..b
        }
    };
    let lead = match (edits.is_empty(), raw.first(), first_duration) {
        (true, Some(p), Some(d)) => u64::from(d.saturating_sub(p.out)),
        _ => 0,
    };
    Ok(Track {
        codec,
        channels,
        rate: timescale,
        stsd: stsd_box,
        preskip_at,
        language,
        raw,
        audible,
        lead,
    })
}

/// The raw packets of an uncompressed track from its sample table `stbl`:
/// QuickTime stores one sample per sample frame (`stts` durations of 1),
/// `frame_bytes` each, so the frames of each chunk are grouped into
/// packets of up to [`PCM_BLOCK_FRAMES`] without expanding the table.
fn read_pcm(
    stbl: &[u8],
    frame_bytes: u32,
    file_len: u64,
    warnings: &mut Vec<String>,
) -> Result<Vec<RawPacket>> {
    let at = |m: &str| malformed(0, &format!("audio track: {m}"));
    let get = |k: &[u8; 4]| boxes::find(stbl, k).map(|b| b.body);
    // stts (8.6.1.2): the number of frames.
    let mut r = Reader::new(get(b"stts").ok_or_else(|| at("no stts box"))?);
    let invalid_stts = || at("invalid stts box");
    r.u32().ok_or_else(invalid_stts)?;
    let mut frames = 0u64;
    for _ in 0..r.u32().ok_or_else(invalid_stts)? {
        let (count, delta) = (
            r.u32().ok_or_else(invalid_stts)?,
            r.u32().ok_or_else(invalid_stts)?,
        );
        if delta != 1 && count > 0 {
            return Err(ContainerError::Unsupported(
                "uncompressed audio with several frames per sample".into(),
            ));
        }
        frames += u64::from(count);
    }
    let chunk_offsets = match (get(b"stco"), get(b"co64")) {
        (Some(b), _) => parse_offsets(b, false),
        (None, Some(b)) => parse_offsets(b, true),
        _ => None,
    }
    .ok_or_else(|| at("missing or invalid chunk offsets"))?;
    let stsc = parse_stsc(get(b"stsc").ok_or_else(|| at("no stsc box"))?)
        .ok_or_else(|| at("invalid stsc box"))?;
    if stsc.iter().any(|e| e.2 != 1) {
        return Err(ContainerError::Unsupported(
            "MP4 track with several sample descriptions".into(),
        ));
    }
    let fb = u64::from(frame_bytes);
    let mut out = Vec::new();
    let mut start = 0u64;
    for (c, &chunk_off) in chunk_offsets.iter().enumerate() {
        let chunk_no = c as u32 + 1;
        // The last stsc entry whose first chunk is at most this one.
        let per = stsc
            .iter()
            .take_while(|e| e.0 <= chunk_no)
            .last()
            .map_or(0, |e| e.1);
        let mut left = u64::from(per).min(frames - start);
        let mut off = chunk_off;
        while left > 0 {
            let n = left.min(u64::from(PCM_BLOCK_FRAMES));
            // Whatever of the block the file still holds.
            let present = file_len.saturating_sub(off) / fb;
            let n = n.min(present);
            if n == 0 {
                warnings.push(format!(
                    "the file is truncated; {start} of {frames} sample frames are present"
                ));
                return Ok(out);
            }
            out.push(RawPacket {
                offset: off,
                // At most PCM_BLOCK_FRAMES frames of at most u16 bytes.
                len: (n * fb) as u32,
                out: n as u32,
                start,
            });
            start += n;
            off += n * fb;
            left -= n;
        }
        if start == frames {
            break;
        }
    }
    if start < frames {
        warnings.push(format!(
            "the chunk table covers only {start} of {frames} sample frames"
        ));
    }
    Ok(out)
}

/// The sample table of a track, in decode order, with the samples of
/// movie fragments appended.
#[derive(Debug, Default)]
struct Samples {
    /// File offset of each sample.
    offsets: Vec<u64>,
    /// Size of each sample in bytes.
    sizes: Vec<u32>,
    /// Duration of each sample (`stts`), in the media timescale.
    durations: Vec<u32>,
    /// Composition time offset of each sample (`ctts`), 0 without one.
    ctts: Vec<i32>,
    /// Whether each sample is a sync sample (`stss`; all are without one).
    sync: Vec<bool>,
}

/// Reads the sample table `stbl` (the body of `trak`'s
/// `mdia/minf/stbl`) plus the track's samples in movie fragments. `what`
/// names the track in errors and warnings ("audio", "video").
fn read_samples(
    trak: BoxRef,
    stbl: &[u8],
    frag: &Fragments,
    file_len: u64,
    what: &str,
    warnings: &mut Vec<String>,
) -> Result<Samples> {
    let at = |m: &str| malformed(0, &format!("{what} track: {m}"));
    let get = |k: &[u8; 4]| boxes::find(stbl, k).map(|b| b.body);
    // Every sample takes at least one byte of the file, which bounds the
    // tables a damaged count could otherwise blow up to gigabytes.
    let max_samples = usize::try_from(file_len).unwrap_or(usize::MAX);
    let durations = parse_stts(get(b"stts").ok_or_else(|| at("no stts box"))?, max_samples)
        .ok_or_else(|| at("invalid stts box"))?;
    let sizes = match (get(b"stsz"), get(b"stz2")) {
        (Some(b), _) => parse_stsz(b, max_samples),
        (None, Some(b)) => parse_stz2(b),
        _ => None,
    }
    .ok_or_else(|| at("missing or invalid sample sizes"))?;
    let chunk_offsets = match (get(b"stco"), get(b"co64")) {
        (Some(b), _) => parse_offsets(b, false),
        (None, Some(b)) => parse_offsets(b, true),
        _ => None,
    }
    .ok_or_else(|| at("missing or invalid chunk offsets"))?;
    let stsc = parse_stsc(get(b"stsc").ok_or_else(|| at("no stsc box"))?)
        .ok_or_else(|| at("invalid stsc box"))?;
    let several_descriptions =
        || ContainerError::Unsupported("MP4 track with several sample descriptions".into());
    if stsc.iter().any(|e| e.2 != 1) {
        return Err(several_descriptions());
    }

    // Each sample's file offset: chunk offset plus the sizes before it.
    let mut count = sizes.len();
    if durations.len() != count {
        warnings.push(format!(
            "{what} track: the sample table lists {} durations but {count} sizes; using {}",
            durations.len(),
            durations.len().min(count)
        ));
        count = count.min(durations.len());
    }
    let mut offsets = Vec::with_capacity(count);
    'chunks: for (c, &chunk_off) in chunk_offsets.iter().enumerate() {
        let chunk_no = c as u32 + 1;
        // The last stsc entry whose first chunk is at most this one.
        let per = stsc
            .iter()
            .take_while(|e| e.0 <= chunk_no)
            .last()
            .map_or(0, |e| e.1);
        let mut off = chunk_off;
        for _ in 0..per {
            if offsets.len() == count {
                break 'chunks;
            }
            offsets.push(off);
            off = off.saturating_add(u64::from(sizes[offsets.len() - 1]));
        }
    }
    if offsets.len() < count {
        warnings.push(format!(
            "{what} track: the chunk table covers only {} of {count} samples",
            offsets.len()
        ));
        count = offsets.len();
    }
    let (mut sizes, mut durations) = (sizes, durations);
    sizes.truncate(count);
    durations.truncate(count);
    offsets.truncate(count);
    // ctts (8.6.1.3) and stss (8.6.2) of the sample table part; both are
    // optional.
    let mut ctts = match get(b"ctts") {
        Some(b) => parse_ctts(b, count).ok_or_else(|| at("invalid ctts box"))?,
        None => Vec::new(),
    };
    ctts.resize(count, 0);
    let mut sync = match get(b"stss") {
        Some(b) => parse_stss(b, count).ok_or_else(|| at("invalid stss box"))?,
        None => vec![true; count],
    };
    sync.resize(count, false);

    // Samples in movie fragments follow those of the sample table.
    if !frag.moofs.is_empty() {
        let track_id = boxes::find(trak.body, b"tkhd")
            .and_then(|b| parse_tkhd_track_id(b.body))
            .ok_or_else(|| at("invalid tkhd box"))?;
        let mut samples = Vec::new();
        for (i, (moof_pos, moof)) in frag.moofs.iter().enumerate() {
            let budget = max_samples.saturating_sub(count + samples.len());
            match parse_moof(moof, *moof_pos, track_id, &frag.defaults, budget) {
                Ok(s) => samples.extend(s),
                Err(FragmentError::SeveralDescriptions) => return Err(several_descriptions()),
                Err(FragmentError::Invalid) => {
                    warnings.push(format!(
                        "ignoring damaged movie fragments from byte {moof_pos} on ({} of {})",
                        frag.moofs.len() - i,
                        frag.moofs.len()
                    ));
                    break;
                }
            }
        }
        for s in samples {
            offsets.push(s.offset);
            sizes.push(s.size);
            durations.push(s.duration);
            ctts.push(s.cto);
            sync.push(s.sync);
        }
    }
    Ok(Samples {
        offsets,
        sizes,
        durations,
        ctts,
        sync,
    })
}

/// The value that occurs most often in `values` (the largest of equally
/// frequent ones), `None` when empty.
fn most_common(values: &[u32]) -> Option<u32> {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted
        .chunk_by(|a, b| a == b)
        .max_by_key(|run| (run.len(), run[0]))
        .map(|run| run[0])
}

/// The timescale (never 0) and packed ISO 639-2 language of an `mdhd`
/// body (ISO/IEC 14496-12 8.4.2); the language defaults to "und".
fn parse_mdhd(b: &[u8]) -> Option<(u32, u16)> {
    let mut r = Reader::new(b);
    let v = r.u8()?;
    r.bytes(3);
    r.bytes(if v == 1 { 16 } else { 8 });
    let ts = r.u32().filter(|&t| t > 0)?;
    r.bytes(if v == 1 { 8 } else { 4 });
    Some((ts, r.u16().unwrap_or(0x55C4)))
}

/// The movie fragments of a file and the sample defaults they start from.
struct Fragments<'a> {
    /// Each `moof` box: its file offset and payload.
    moofs: &'a [(u64, Vec<u8>)],
    /// `trex` defaults by track ID.
    defaults: Vec<(u32, SampleDefaults)>,
}

/// Defaults for the samples of a track fragment (`trex`, ISO/IEC 14496-12
/// 8.8.3, overridden by `tfhd`, 8.8.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SampleDefaults {
    description: u32,
    duration: u32,
    size: u32,
    /// Sample flags (8.8.3.1): dependency bits and
    /// `sample_is_non_sync_sample`.
    flags: u32,
}

impl Default for SampleDefaults {
    /// What a track without `trex` gets: the first sample description and
    /// no duration or size (each `trun` must then give them).
    fn default() -> Self {
        Self {
            description: 1,
            duration: 0,
            size: 0,
            flags: 0,
        }
    }
}

/// A sample found in a movie fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FragmentSample {
    offset: u64,
    size: u32,
    duration: u32,
    /// Composition time offset (presentation minus decode time).
    cto: i32,
    /// A sync sample: decoding can start here.
    sync: bool,
}

/// `sample_is_non_sync_sample` in sample flags (ISO/IEC 14496-12 8.8.3.1).
const NON_SYNC_SAMPLE: u32 = 0x1_0000;

/// Why a fragment's samples could not be listed.
#[derive(Debug, PartialEq, Eq)]
enum FragmentError {
    /// A box is too short, or holds more samples than the file can.
    Invalid,
    /// The samples use another sample description than the first.
    SeveralDescriptions,
}

/// The `trex` boxes of `moov/mvex`: sample defaults by track ID.
fn parse_trex(moov: &[u8]) -> Vec<(u32, SampleDefaults)> {
    let Some(mvex) = boxes::find(moov, b"mvex") else {
        return Vec::new();
    };
    boxes::Boxes::new(mvex.body)
        .filter(|b| &b.kind == b"trex")
        .filter_map(|b| {
            let mut r = Reader::new(b.body);
            r.u32()?; // version and flags
            let id = r.u32()?;
            let description = r.u32()?;
            let duration = r.u32()?;
            let size = r.u32()?;
            let flags = r.u32().unwrap_or(0);
            Some((
                id,
                SampleDefaults {
                    description,
                    duration,
                    size,
                    flags,
                },
            ))
        })
        .collect()
}

/// The track ID from a `tkhd` body (ISO/IEC 14496-12 8.3.2).
fn parse_tkhd_track_id(b: &[u8]) -> Option<u32> {
    let mut r = Reader::new(b);
    let version = r.u8()?;
    r.bytes(3)?;
    // Creation and modification time.
    r.bytes(if version == 1 { 16 } else { 8 })?;
    r.u32()
}

/// The samples of track `track_id` in the `moof` payload `moof`, whose box
/// starts at file offset `moof_pos`; at most `max` of them. Each `traf`
/// (ISO/IEC 14496-12 8.8.6) takes its defaults from `tfhd`, falling back
/// to the track's `trex`, and lists samples in `trun` boxes (8.8.8).
fn parse_moof(
    moof: &[u8],
    moof_pos: u64,
    track_id: u32,
    trex: &[(u32, SampleDefaults)],
    max: usize,
) -> std::result::Result<Vec<FragmentSample>, FragmentError> {
    use FragmentError::Invalid;
    let mut out = Vec::new();
    // 8.8.7.1: without an explicit base, the first traf's data starts at
    // the moof box and each further traf's where the previous one's ends.
    let mut previous_end = moof_pos;
    for traf in boxes::Boxes::new(moof).filter(|b| &b.kind == b"traf") {
        let tfhd = boxes::find(traf.body, b"tfhd").ok_or(Invalid)?;
        let mut r = Reader::new(tfhd.body);
        let flags = r.u32().ok_or(Invalid)? & 0xFF_FFFF;
        let id = r.u32().ok_or(Invalid)?;
        let mut d = trex
            .iter()
            .find(|t| t.0 == id)
            .map_or_else(SampleDefaults::default, |t| t.1);
        let base = if flags & 0x01 != 0 {
            r.u64().ok_or(Invalid)?
        } else if flags & 0x2_0000 != 0 {
            // default-base-is-moof
            moof_pos
        } else {
            previous_end
        };
        if flags & 0x02 != 0 {
            d.description = r.u32().ok_or(Invalid)?;
        }
        if flags & 0x08 != 0 {
            d.duration = r.u32().ok_or(Invalid)?;
        }
        if flags & 0x10 != 0 {
            d.size = r.u32().ok_or(Invalid)?;
        }
        if flags & 0x20 != 0 {
            d.flags = r.u32().ok_or(Invalid)?;
        }
        // 0x1_0000 (duration-is-empty, which comes without trun boxes)
        // needs no handling.

        let mine = id == track_id;
        if mine && d.description != 1 {
            return Err(FragmentError::SeveralDescriptions);
        }
        let mut data = base;
        for trun in boxes::Boxes::new(traf.body).filter(|b| &b.kind == b"trun") {
            let mut r = Reader::new(trun.body);
            let flags = r.u32().ok_or(Invalid)? & 0xFF_FFFF;
            let n = r.u32().ok_or(Invalid)? as usize;
            if flags & 0x01 != 0 {
                // Signed, relative to the base; without it a run's data
                // follows the previous run's.
                let rel = i64::from(r.u32().ok_or(Invalid)? as i32);
                data = base.checked_add_signed(rel).ok_or(Invalid)?;
            }
            let first_flags = if flags & 0x04 != 0 {
                Some(r.u32().ok_or(Invalid)?)
            } else {
                None
            };
            if mine && out.len().saturating_add(n) > max {
                return Err(Invalid);
            }
            for k in 0..n {
                let duration = if flags & 0x100 != 0 {
                    r.u32().ok_or(Invalid)?
                } else {
                    d.duration
                };
                let size = if flags & 0x200 != 0 {
                    r.u32().ok_or(Invalid)?
                } else {
                    d.size
                };
                let mut sample_flags = d.flags;
                if flags & 0x400 != 0 {
                    sample_flags = r.u32().ok_or(Invalid)?;
                } else if k == 0
                    && let Some(f) = first_flags
                {
                    sample_flags = f;
                }
                // Unsigned in trun version 0 and signed in version 1;
                // encoders only write values that fit both readings.
                let cto = if flags & 0x800 != 0 {
                    r.u32().ok_or(Invalid)? as i32
                } else {
                    0
                };
                if mine {
                    out.push(FragmentSample {
                        offset: data,
                        size,
                        duration,
                        cto,
                        sync: sample_flags & NON_SYNC_SAMPLE == 0,
                    });
                }
                data = data.checked_add(u64::from(size)).ok_or(Invalid)?;
                if !mine && flags & 0xF00 == 0 {
                    // Another track's run without per-sample fields: its
                    // end is known without walking (maybe 2^32) samples.
                    data = data
                        .checked_add(u64::from(size) * (n as u64 - 1))
                        .ok_or(Invalid)?;
                    break;
                }
            }
        }
        previous_end = data;
    }
    Ok(out)
}

/// Parses an `stsd` body: the codec and channel count of its first entry,
/// and for Opus where in `b` the `dOps` PreSkip field lies.
fn parse_stsd(b: &[u8]) -> Result<(Mp4Codec, u16, Option<usize>)> {
    let bad = |what: &str| malformed(0, &format!("sample description: {what}"));
    let entry = boxes::Boxes::new(b.get(8..).unwrap_or_default())
        .next()
        .ok_or_else(|| bad("no entry"))?;
    // ISO/IEC 14496-12 12.2.3 AudioSampleEntry, with QuickTime's versions
    // 1 and 2 (16 and 36 more bytes before the child boxes).
    let mut r = Reader::new(entry.body);
    r.bytes(8).ok_or_else(|| bad("short entry"))?;
    let version = r.u16().ok_or_else(|| bad("short entry"))?;
    r.bytes(6);
    let entry_channels = r.u16().unwrap_or(0);
    let children_at = match version {
        0 => 28,
        1 => 44,
        2 => 64,
        v => return Err(bad(&format!("sound sample description version {v}"))),
    };
    let children = entry.body.get(children_at..).unwrap_or_default();
    match &entry.kind {
        b"mp4a" => {
            let esds = boxes::find(children, b"esds")
                .or_else(|| boxes::path(children, &[b"wave", b"esds"]))
                .ok_or_else(|| bad("mp4a without an esds box"))?;
            let asc = parse_esds(esds.body.get(4..).unwrap_or_default())?;
            let channels = match asc_channels(&asc) {
                Some(c) if c > 0 => c,
                _ => entry_channels,
            };
            if channels == 0 {
                return Err(bad("no channel count"));
            }
            Ok((Mp4Codec::Aac { asc }, channels, None))
        }
        b"alac" => {
            let cookie = boxes::find(children, b"alac")
                .filter(|c| c.body.len() >= 28)
                .ok_or_else(|| bad("alac entry without its configuration"))?;
            let mut out = Vec::new();
            boxes::put(&mut out, b"alac", cookie.body);
            // ALACSpecificConfig: numChannels at offset 9 after version
            // and flags (Apple's ALAC "Magic Cookie" description).
            let channels = u16::from(cookie.body[4 + 9]);
            if channels == 0 {
                return Err(bad("ALAC without channels"));
            }
            Ok((Mp4Codec::Alac { cookie: out }, channels, None))
        }
        b"Opus" => {
            let dops = boxes::find(children, b"dOps").ok_or_else(|| bad("Opus without dOps"))?;
            let head = opus_head(dops.body).ok_or_else(|| bad("invalid dOps box"))?;
            // OutputChannelCount, checked by `opus_head`.
            let channels = u16::from(head[9]);
            // PreSkip follows Version and OutputChannelCount. Both slices
            // point into `b`.
            let at = dops.body.as_ptr() as usize - b.as_ptr() as usize + 2;
            Ok((Mp4Codec::Opus { head }, channels, Some(at)))
        }
        // AMR is always mono; 3GPP TS 26.244 6.5 asks for a channel count
        // of 2 in the entry and for readers to ignore it.
        b"samr" => Ok((Mp4Codec::Amr { wideband: false }, 1, None)),
        b"sawb" => Ok((Mp4Codec::Amr { wideband: true }, 1, None)),
        kind @ (b"sowt" | b"twos" | b"raw " | b"in24" | b"in32" | b"fl32" | b"fl64" | b"lpcm"
        | b"alaw" | b"ulaw" | b"ipcm" | b"fpcm") => {
            let (format, channels) = pcm_format(kind, version, entry.body, children)
                .ok_or_else(|| bad("unsupported uncompressed audio format"))?;
            Ok((Mp4Codec::Pcm(format), channels, None))
        }
        other => Err(ContainerError::Unsupported(format!(
            "MP4 audio codec {:?} (only AAC, ALAC, Opus and AMR are supported)",
            String::from_utf8_lossy(other)
        ))),
    }
}

/// The format and channel count of an uncompressed sound sample entry
/// `kind` of `version` (0, 1 or 2) with the entry's `body` and the child
/// boxes after its fields (QuickTime File Format, "Sound sample
/// descriptions" and "Sound sample description extensions"; ISO/IEC
/// 23003-5 5 for `ipcm` and `fpcm`). `None` for a combination that is no
/// plain PCM, A-law or µ-law.
fn pcm_format(
    kind: &[u8; 4],
    version: u16,
    body: &[u8],
    children: &[u8],
) -> Option<(PcmFormat, u16)> {
    use crate::wav::SampleEncoding::{ALaw, Float, MuLaw, Pcm};
    let u16_at = |at: usize| {
        body.get(at..at + 2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
    };
    let u32_at = |at: usize| {
        body.get(at..at + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    let mut channels = u16_at(16)?;
    // Version 0 states the sample size in bits; version 1 adds the bytes
    // per sample (36: bytes per frame, 40: bytes per sample).
    let mut bits = match version {
        1 => u16::try_from(u32_at(40)?.checked_mul(8)?).ok()?,
        _ => u16_at(18)?,
    };
    // `in24`, `in32`, `fl32` and `fl64` are big-endian unless an `enda`
    // box (in `wave`) says 1.
    let little = boxes::find(children, b"enda")
        .or_else(|| boxes::path(children, &[b"wave", b"enda"]))
        .and_then(|b| b.body.get(..2))
        .is_some_and(|b| b == [0, 1]);
    let (encoding, big_endian, signed_8bit) = match kind {
        b"sowt" => (Pcm, false, true),
        b"twos" => (Pcm, true, true),
        b"raw " => {
            bits = 8;
            (Pcm, false, false)
        }
        b"in24" => {
            bits = 24;
            (Pcm, !little, true)
        }
        b"in32" => {
            bits = 32;
            (Pcm, !little, true)
        }
        b"fl32" => {
            bits = 32;
            (Float, !little, true)
        }
        b"fl64" => {
            bits = 64;
            (Float, !little, true)
        }
        b"alaw" | b"ulaw" => {
            bits = 8;
            (if kind == b"alaw" { ALaw } else { MuLaw }, false, true)
        }
        b"lpcm" => {
            // Version 2 (Apple Core Audio's description): 40 channels,
            // 48 bits per channel, 52 flags (1 float, 2 big-endian, 4
            // signed integer).
            if version != 2 {
                return None;
            }
            channels = u16::try_from(u32_at(40)?).ok()?;
            bits = u16::try_from(u32_at(48)?).ok()?;
            let flags = u32_at(52)?;
            let float = flags & 1 != 0;
            if !float && flags & 4 == 0 && bits != 8 {
                return None; // unsigned integers wider than 8 bits
            }
            (
                if float { Float } else { Pcm },
                flags & 2 != 0,
                flags & 4 != 0,
            )
        }
        _ => {
            // ISO/IEC 23003-5 5.1: pcmC holds format_flags (bit 0: little
            // endian) and PCM_sample_size after version and flags.
            let pcmc = boxes::find(children, b"pcmC")?;
            let little = pcmc.body.get(4)? & 1 == 1;
            bits = u16::from(*pcmc.body.get(5)?);
            (if kind == b"fpcm" { Float } else { Pcm }, !little, true)
        }
    };
    let valid = match encoding {
        Float => matches!(bits, 32 | 64),
        ALaw | MuLaw => bits == 8,
        Pcm => matches!(bits, 8 | 16 | 24 | 32),
    };
    let frame_bytes = u32::from(channels) * u32::from(bits / 8);
    // The decoder's block_align is 16 bits wide.
    if !valid || channels == 0 || u16::try_from(frame_bytes).is_err() {
        return None;
    }
    Some((
        PcmFormat {
            encoding,
            bits,
            big_endian: big_endian && bits > 8,
            signed_8bit,
            frame_bytes,
        },
        channels,
    ))
}

/// The `OpusHead` (RFC 7845 5.1) that says what a `dOps` body says (the
/// Opus mapping for ISOBMFF, 4.3.2): the same fields, but little-endian,
/// with a magic signature and version 1. `None` when malformed.
fn opus_head(dops: &[u8]) -> Option<Vec<u8>> {
    let mut r = Reader::new(dops);
    if r.u8()? != 0 {
        return None; // Version
    }
    let channels = r.u8()?;
    let pre_skip = r.u16()?;
    let input_rate = r.u32()?;
    let gain = r.u16()?;
    let family = r.u8()?;
    if channels == 0 {
        return None;
    }
    let mut head = b"OpusHead".to_vec();
    head.push(1);
    head.push(channels);
    head.extend(pre_skip.to_le_bytes());
    head.extend(input_rate.to_le_bytes());
    head.extend(gain.to_le_bytes());
    head.push(family);
    if family != 0 {
        // StreamCount, CoupledCount and the channel mapping.
        head.extend(r.bytes(2 + usize::from(channels))?);
    }
    Some(head)
}

/// Reads an MPEG-4 descriptor's tag and length (ISO/IEC 14496-1 8.3.3:
/// up to four bytes of 7 bits, the top bit marking continuation).
fn descriptor(r: &mut Reader) -> Option<(u8, usize)> {
    let tag = r.u8()?;
    let mut len = 0usize;
    for _ in 0..4 {
        let b = r.u8()?;
        len = (len << 7) | usize::from(b & 0x7F);
        if b & 0x80 == 0 {
            break;
        }
    }
    Some((tag, len))
}

/// The DecoderConfigDescriptor's `objectTypeIndication` of an `esds`
/// payload (after version/flags), and a reader positioned after it:
/// ES_Descriptor (tag 3) > DecoderConfigDescriptor (tag 4), ISO/IEC
/// 14496-1 7.2.6.5 and 7.2.6.6.
fn esds_decoder_config(b: &[u8]) -> Option<(u8, Reader<'_>)> {
    let mut r = Reader::new(b);
    let (tag, _) = descriptor(&mut r)?;
    if tag != 3 {
        return None;
    }
    r.u16()?; // ES_ID
    let flags = r.u8()?;
    if flags & 0x80 != 0 {
        r.u16()?; // dependsOn_ES_ID
    }
    if flags & 0x40 != 0 {
        let n = r.u8()?;
        r.bytes(usize::from(n))?; // URL
    }
    if flags & 0x20 != 0 {
        r.u16()?; // OCR_ES_ID
    }
    let (tag, _) = descriptor(&mut r)?;
    if tag != 4 {
        return None;
    }
    Some((r.u8()?, r))
}

/// The AudioSpecificConfig from an `esds` payload (after version/flags):
/// the DecoderSpecificInfo (tag 5) of its DecoderConfigDescriptor.
fn parse_esds(b: &[u8]) -> Result<Vec<u8>> {
    let bad = || malformed(0, "invalid esds box");
    let (object_type, r) = esds_decoder_config(b).ok_or_else(bad)?;
    // 0x40: MPEG-4 audio; 0x66-0x68: MPEG-2 AAC profiles.
    if !matches!(object_type, 0x40 | 0x66..=0x68) {
        return Err(ContainerError::Unsupported(format!(
            "MP4 audio object type {object_type:#04x} (only AAC is supported in mp4a)"
        )));
    }
    Ok(decoder_specific_info(r).ok_or_else(bad)?.to_vec())
}

/// The DecoderSpecificInfo (tag 5, ISO/IEC 14496-1 7.2.6.7) that follows
/// a DecoderConfigDescriptor's object type, read by
/// [`esds_decoder_config`]; `None` when missing or empty.
fn decoder_specific_info(mut r: Reader<'_>) -> Option<&[u8]> {
    r.bytes(12)?; // stream type, buffer size, bitrates
    let (tag, len) = descriptor(&mut r)?;
    if tag != 5 || len == 0 {
        return None;
    }
    r.bytes(len)
}

/// The channel configuration of an AudioSpecificConfig (ISO/IEC 14496-3
/// 1.6.2.1), mapped to a channel count; `None` for configuration 0
/// (described by a program config element) or a malformed config.
fn asc_channels(asc: &[u8]) -> Option<u16> {
    let mut bits = crate::bits::BitReader::new(asc);
    let mut aot = bits.read_u32(5).ok()?;
    if aot == 31 {
        aot = 32 + bits.read_u32(6).ok()?;
    }
    let _ = aot;
    if bits.read_u32(4).ok()? == 15 {
        bits.read_u32(24).ok()?; // explicit frequency
    }
    // Table 1.19: configurations 1-7 have 1, 2, 3, 4, 5, 6 and 8 channels.
    match bits.read_u32(4).ok()? {
        0 => None,
        c @ 1..=6 => Some(c as u16),
        7 => Some(8),
        _ => None,
    }
}

/// `stts` (ISO/IEC 14496-12 8.6.1.2): one duration per sample, at most
/// `max` of them (more is damage).
fn parse_stts(b: &[u8], max: usize) -> Option<Vec<u32>> {
    let mut r = Reader::new(b);
    r.u32()?;
    let n = r.u32()?;
    let mut out = Vec::new();
    for _ in 0..n {
        let count = r.u32()?;
        let delta = r.u32()?;
        if out.len().saturating_add(count as usize) > max {
            return None;
        }
        out.extend(std::iter::repeat_n(delta, count as usize));
    }
    Some(out)
}

/// `stsz` (8.7.3.2): one size per sample, at most `max` of them.
fn parse_stsz(b: &[u8], max: usize) -> Option<Vec<u32>> {
    let mut r = Reader::new(b);
    r.u32()?;
    let fixed = r.u32()?;
    let n = r.u32()? as usize;
    if fixed != 0 {
        // A fixed size needs no table, so bound the count explicitly.
        if n > max {
            return None;
        }
        return Some(vec![fixed; n]);
    }
    let table = r.bytes(n.checked_mul(4)?)?;
    Some(
        table
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&c| u32::from_be_bytes(c))
            .collect(),
    )
}

/// `stz2` (8.7.3.3): compact sample sizes of 4, 8 or 16 bits.
fn parse_stz2(b: &[u8]) -> Option<Vec<u32>> {
    let mut r = Reader::new(b);
    r.u32()?;
    r.bytes(3)?;
    let field = r.u8()?;
    let n = r.u32()? as usize;
    let rest = r.rest();
    match field {
        4 => {
            let t = rest.get(..n.div_ceil(2))?;
            Some(
                (0..n)
                    .map(|i| {
                        u32::from(if i % 2 == 0 {
                            t[i / 2] >> 4
                        } else {
                            t[i / 2] & 15
                        })
                    })
                    .collect(),
            )
        }
        8 => Some(rest.get(..n)?.iter().map(|&s| u32::from(s)).collect()),
        16 => Some(
            rest.get(..n.checked_mul(2)?)?
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&c| u32::from(u16::from_be_bytes(c)))
                .collect(),
        ),
        _ => None,
    }
}

/// `stco` / `co64` (8.7.5): chunk offsets.
fn parse_offsets(b: &[u8], wide: bool) -> Option<Vec<u64>> {
    let mut r = Reader::new(b);
    r.u32()?;
    let n = r.u32()? as usize;
    let w = if wide { 8 } else { 4 };
    let t = r.bytes(n.checked_mul(w)?)?;
    Some(
        t.chunks_exact(w)
            .map(|c| {
                if wide {
                    u64::from_be_bytes(c.try_into().unwrap_or_default())
                } else {
                    u64::from(u32::from_be_bytes(c.try_into().unwrap_or_default()))
                }
            })
            .collect(),
    )
}

/// `stsc` (8.7.4): (first chunk, samples per chunk, description index).
fn parse_stsc(b: &[u8]) -> Option<Vec<(u32, u32, u32)>> {
    let mut r = Reader::new(b);
    r.u32()?;
    let n = r.u32()? as usize;
    let mut out = Vec::with_capacity(n.min(b.len() / 12));
    for _ in 0..n {
        out.push((r.u32()?, r.u32()?, r.u32()?));
    }
    Some(out)
}

/// `ctts` (8.6.1.3): one composition offset per sample, at most `max`
/// of them; version 0 offsets are unsigned and version 1 signed, but
/// encoders only write values that fit both readings.
fn parse_ctts(b: &[u8], max: usize) -> Option<Vec<i32>> {
    let mut r = Reader::new(b);
    r.u32()?;
    let n = r.u32()?;
    let mut out = Vec::new();
    for _ in 0..n {
        let count = r.u32()? as usize;
        let offset = r.u32()? as i32;
        // More offsets than samples is damage; stop at the table's end.
        let count = count.min(max - out.len().min(max));
        out.extend(std::iter::repeat_n(offset, count));
    }
    Some(out)
}

/// `stss` (8.6.2): the sync samples among `count` samples (numbers start
/// at 1); numbers beyond `count` are ignored.
fn parse_stss(b: &[u8], count: usize) -> Option<Vec<bool>> {
    let mut r = Reader::new(b);
    r.u32()?;
    let n = r.u32()? as usize;
    let table = r.bytes(n.checked_mul(4)?)?;
    let mut out = vec![false; count];
    for c in table.as_chunks::<4>().0 {
        let i = u32::from_be_bytes(*c) as usize;
        if let Some(s) = i.checked_sub(1).and_then(|i| out.get_mut(i)) {
            *s = true;
        }
    }
    Some(out)
}

/// `elst` (8.6.6): (segment duration, media time) per edit; a media time
/// of -1 is an empty edit.
fn parse_elst(b: &[u8]) -> Option<Vec<(u64, i64)>> {
    let mut r = Reader::new(b);
    let v = r.u8()?;
    r.bytes(3)?;
    let n = r.u32()? as usize;
    let mut out = Vec::with_capacity(n.min(b.len() / 12));
    for _ in 0..n {
        let e = if v == 1 {
            (r.u64()?, r.u64()? as i64)
        } else {
            (u64::from(r.u32()?), i64::from(r.u32()? as i32))
        };
        r.u32()?; // media rate
        out.push(e);
    }
    Some(out)
}

/// Reads a raw packet's payload.
fn read_raw<R: Read + Seek + ?Sized>(r: &mut R, p: &RawPacket) -> Result<Vec<u8>> {
    let mut out = vec![0u8; p.len as usize];
    read_exact_at(r, p.offset, &mut out)?;
    Ok(out)
}

fn read_exact_at<R: Read + Seek + ?Sized>(r: &mut R, pos: u64, buf: &mut [u8]) -> Result<()> {
    r.seek(SeekFrom::Start(pos))?;
    r.read_exact(buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            malformed(pos, "unexpected end of file")
        } else {
            ContainerError::Io(e)
        }
    })
}

/// Reads up to `buf.len()` bytes at `pos`; returns how many.
fn read_up_to<R: Read + Seek + ?Sized>(r: &mut R, pos: u64, buf: &mut [u8]) -> Result<usize> {
    r.seek(SeekFrom::Start(pos))?;
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(n)
}

fn malformed(offset: u64, reason: &str) -> ContainerError {
    ContainerError::Malformed {
        offset,
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fuzzer find: a top-level box with a 64-bit size of 2^64 - 1
    /// overflowed the scan position.
    #[test]
    fn huge_box_size_is_an_error_not_a_panic() {
        let b: &[u8] = &[
            0, 0, 0, 1, 5, 0, b'o', b'k', 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ];
        assert!(Mp4File::open(&mut std::io::Cursor::new(b)).is_err());
    }

    #[test]
    fn esds_and_channels() {
        // ES_Descriptor with ES_ID 1, DecoderConfigDescriptor (AAC),
        // DecoderSpecificInfo 0x12 0x10 (AAC-LC, 44.1 kHz, stereo); the
        // lengths use the 4-byte form, as ffmpeg writes them.
        let b = [
            0x03, 0x80, 0x80, 0x80, 0x25, 0x00, 0x01, 0x00, 0x04, 0x80, 0x80, 0x80, 0x17, 0x40,
            0x15, 0x00, 0x00, 0x00, 0x00, 0x01, 0xf4, 0x17, 0x00, 0x01, 0xf4, 0x17, 0x05, 0x80,
            0x80, 0x80, 0x02, 0x12, 0x10, 0x06, 0x80, 0x80, 0x80, 0x01, 0x02,
        ];
        let asc = parse_esds(&b).expect("valid");
        assert_eq!(asc, [0x12, 0x10]);
        assert_eq!(asc_channels(&asc), Some(2));
        assert_eq!(asc_channels(&[0x12, 0x08]), Some(1));
        assert_eq!(asc_channels(&[0x12, 0x00]), None);
        // MP3 in MP4 (object type 0x6B) is refused.
        let mut mp3 = b;
        mp3[13] = 0x6B;
        assert!(matches!(
            parse_esds(&mp3),
            Err(ContainerError::Unsupported(_))
        ));
        // Truncated anywhere before the end of the AudioSpecificConfig
        // (byte 33; the SLConfigDescriptor after it is not needed): an
        // error, never a panic.
        for n in 0..33 {
            assert!(parse_esds(&b[..n]).is_err(), "{n}");
        }
    }

    #[test]
    fn sample_tables() {
        let mut stts = vec![0; 4];
        stts.extend(2u32.to_be_bytes());
        stts.extend([0, 0, 0, 3, 0, 0, 4, 0, 0, 0, 0, 1, 0, 0, 1, 0]);
        assert_eq!(parse_stts(&stts, 100), Some(vec![1024, 1024, 1024, 256]));
        assert_eq!(parse_stts(&stts, 3), None);
        // A huge count in a tiny box is refused.
        let mut huge = vec![0; 4];
        huge.extend(1u32.to_be_bytes());
        huge.extend(u32::MAX.to_be_bytes());
        huge.extend(1024u32.to_be_bytes());
        assert_eq!(parse_stts(&huge, 1 << 20), None);

        let stz2 = [0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 3, 0x12, 0x30];
        assert_eq!(parse_stz2(&stz2), Some(vec![1, 2, 3]));
        let mut co64 = vec![0; 4];
        co64.extend(1u32.to_be_bytes());
        co64.extend((5u64 << 32).to_be_bytes());
        assert_eq!(parse_offsets(&co64, true), Some(vec![5 << 32]));
        assert_eq!(parse_offsets(&co64[..10], true), None);

        // elst version 0 with an empty edit, then media from 1024.
        let mut elst = vec![0; 4];
        elst.extend(2u32.to_be_bytes());
        elst.extend([0, 0, 0, 10, 0xFF, 0xFF, 0xFF, 0xFF, 0, 1, 0, 0]);
        elst.extend([0, 0, 5, 0xDC, 0, 0, 4, 0, 0, 1, 0, 0]);
        assert_eq!(parse_elst(&elst), Some(vec![(10, -1), (1500, 1024)]));
    }

    /// A `tfhd` body: flags, track ID and the optional fields in order.
    fn tfhd(flags: u32, id: u32, fields: &[u32]) -> Vec<u8> {
        let mut b = flags.to_be_bytes().to_vec();
        b.extend(id.to_be_bytes());
        fields.iter().for_each(|f| b.extend(f.to_be_bytes()));
        let mut out = Vec::new();
        boxes::put(&mut out, b"tfhd", &b);
        out
    }

    /// A `trun` box: flags, sample count and the remaining fields.
    fn trun(flags: u32, count: u32, fields: &[u32]) -> Vec<u8> {
        let mut b = flags.to_be_bytes().to_vec();
        b.extend(count.to_be_bytes());
        fields.iter().for_each(|f| b.extend(f.to_be_bytes()));
        let mut out = Vec::new();
        boxes::put(&mut out, b"trun", &b);
        out
    }

    fn traf(children: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        boxes::put(&mut out, b"traf", &children.concat());
        out
    }

    fn sample(offset: u64, size: u32, duration: u32) -> FragmentSample {
        FragmentSample {
            offset,
            size,
            duration,
            cto: 0,
            sync: true,
        }
    }

    /// Sample flags come from trex, then tfhd, then a trun's first sample
    /// flags (for its first sample only) or per-sample flags; composition
    /// offsets from the trun.
    #[test]
    fn fragment_sync_flags_and_composition_offsets() {
        let trex = [(
            1,
            SampleDefaults {
                description: 1,
                duration: 512,
                size: 10,
                flags: 0,
            },
        )];
        let moof = [
            // tfhd default flags: non-sync; the first sample is sync via
            // first_sample_flags; composition offsets per sample.
            traf(&[
                tfhd(0x20, 1, &[NON_SYNC_SAMPLE]),
                trun(0x804, 3, &[0x0200_0000, 1024, 0, (-512i32) as u32]),
            ]),
            // Per-sample flags override everything.
            traf(&[
                tfhd(0x20, 1, &[NON_SYNC_SAMPLE]),
                trun(0x400, 2, &[0, NON_SYNC_SAMPLE]),
            ]),
        ]
        .concat();
        let got = parse_moof(&moof, 0, 1, &trex, 100).expect("valid");
        let flags: Vec<(bool, i32)> = got.iter().map(|s| (s.sync, s.cto)).collect();
        assert_eq!(
            flags,
            [
                (true, 1024),
                (false, 0),
                (false, -512),
                (true, 0),
                (false, 0)
            ]
        );
    }

    /// ISO/IEC 14496-12 8.8.7.1: without a base offset, the first traf's
    /// data starts at the moof box, a further traf's where the previous
    /// one's ends, and a trun without data offset follows the previous run.
    #[test]
    fn fragment_data_offsets_chain() {
        let trex = [(
            2,
            SampleDefaults {
                description: 1,
                duration: 1024,
                size: 0,
                flags: 0,
            },
        )];
        let moof = [
            // Track 1 (video, say): 3 samples of 100 bytes from moof + 50.
            traf(&[tfhd(0x10, 1, &[100]), trun(0x01, 3, &[50])]),
            // Track 2: two runs, the second continuing the first; the
            // durations come from trex, the sizes from the runs.
            traf(&[
                tfhd(0, 2, &[]),
                trun(0x200, 2, &[10, 20]),
                trun(0x300, 1, &[512, 7]),
            ]),
        ]
        .concat();
        let got = parse_moof(&moof, 1000, 2, &trex, 100).expect("valid");
        assert_eq!(
            got,
            [
                sample(1350, 10, 1024),
                sample(1360, 20, 1024),
                sample(1380, 7, 512)
            ]
        );
        // An explicit base, and default-base-is-moof with a negative data
        // offset (data before the moof box).
        let moof = traf(&[tfhd(0x01 | 0x10, 2, &[0, 5000, 4]), trun(0, 2, &[])]);
        let got = parse_moof(&moof, 1000, 2, &trex, 100).expect("valid");
        assert_eq!(got, [sample(5000, 4, 1024), sample(5004, 4, 1024)]);
        let moof = traf(&[tfhd(0x2_0000, 2, &[]), trun(0x201, 1, &[-200i32 as u32, 9])]);
        let got = parse_moof(&moof, 1000, 2, &trex, 100).expect("valid");
        assert_eq!(got, [sample(800, 9, 1024)]);
    }

    /// Damage is an error, never a panic or a huge allocation.
    #[test]
    fn damaged_fragments_are_refused() {
        let trex = [(1, SampleDefaults::default())];
        let moof = traf(&[tfhd(0x10, 1, &[100]), trun(0x300, 3, &[1, 2, 3, 4])]);
        for n in 0..moof.len() {
            // A cut-off box ends the box iteration early, which may just
            // lose samples; it must never panic.
            let _ = parse_moof(&moof[..n], 0, 1, &trex, 100);
        }
        assert_eq!(
            parse_moof(&moof, 0, 1, &trex, 100),
            Err(FragmentError::Invalid)
        );
        // 2^32 - 1 samples without per-sample fields: beyond the budget
        // for the audio track, skipped in one step for another track.
        let huge = traf(&[tfhd(0x10, 1, &[100]), trun(0, u32::MAX, &[])]);
        assert_eq!(
            parse_moof(&huge, 0, 1, &trex, 1000),
            Err(FragmentError::Invalid)
        );
        assert_eq!(parse_moof(&huge, 0, 2, &trex, 1000), Ok(Vec::new()));
        // Another sample description than the first.
        let other = traf(&[tfhd(0x02, 1, &[2]), trun(0, 0, &[])]);
        assert_eq!(
            parse_moof(&other, 0, 1, &trex, 10),
            Err(FragmentError::SeveralDescriptions)
        );
        assert_eq!(parse_moof(&other, 0, 3, &trex, 10), Ok(Vec::new()));
    }
}
