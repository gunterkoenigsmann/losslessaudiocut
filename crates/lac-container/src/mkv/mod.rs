//! Matroska and WebM files with Opus audio (RFC 9559, RFC 8794, and the
//! Matroska codec mapping for `A_OPUS`).
//!
//! WebM is Matroska restricted to a few codecs; both share this module and
//! differ only in the `DocType` of the EBML header, which is kept on
//! export. Browsers (MediaRecorder), OBS and many conferencing tools
//! record Opus this way.
//!
//! ## Structure
//!
//! An EBML header, then one `Segment` holding `Info`, `Tracks` and a
//! sequence of `Cluster`s, each with a timestamp and `SimpleBlock`s or
//! `BlockGroup`s. A block holds one or (with lacing) several frames of one
//! track; for Opus a frame is one Opus packet. Live recordings write the
//! Segment, and sometimes the Clusters, with an unknown size; such a
//! Cluster ends where the next top-level element begins.
//!
//! ## Timeline
//!
//! As for Ogg, block timestamps are not used: each packet's duration comes
//! from its TOC byte (RFC 6716 3.1), and the timeline is the running sum.
//! The decoder's first output samples are the codec delay, taken from the
//! track's `CodecDelay` (nanoseconds) or, if that is missing, from the
//! `OpusHead` pre-skip in `CodecPrivate`. A `DiscardPadding` (nanoseconds)
//! on the last block trims the end, as the encoder's padding. Discard
//! padding on other blocks is not honoured, with a warning.
//!
//! Only the first audio track is used. It must be Opus; other tracks
//! (video, subtitles, further audio) are ignored with a warning and do not
//! appear in exports. Chapters, cues and attachments of the source are
//! not copied.
//!
//! ## Tags
//!
//! The source's tags are its file-wide `Tags` (see [`tags`]), with the
//! `Info` element's `Title` as the title if they have none. An export
//! writes the source's tags, or new ones, as a `Tags` element after
//! `Tracks`, and the title also as the `Info` title.
//!
//! ## Writing
//!
//! A cut is one Opus track in a fresh file: EBML header, a Segment with a
//! SeekHead, Info (with the new duration), Tracks, Clusters of verbatim
//! packets in `SimpleBlock`s, and Cues for seeking.
//!
//! - **Start:** at least 80 ms of preceding packets are written as
//!   pre-roll (RFC 7845 4.3) and hidden, together with the head of the
//!   first kept packet, by the new codec delay (`CodecDelay` and the
//!   `OpusHead` pre-skip), so the start is sample-exact.
//! - **End:** the tail of the last packet is hidden by a `DiscardPadding`,
//!   so the end is sample-exact.
//! - **Joins:** Matroska has no way to drop samples at the start of a
//!   block in the middle of a track (ffmpeg ignores negative
//!   `DiscardPadding`), so kept parts are joined in one stream without
//!   pre-roll, and the inner cut points are widened to packet boundaries
//!   ([`CutCapabilities::trims_at_file_ends_only`]). The decoder converges
//!   within a short window after each join, as for Ogg Speex.

pub mod block;
pub mod ebml;
pub mod tags;

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;

use lac_core::{
    AudioStreamInfo, CutCapabilities, CutPlan, PacketFlags, PacketIndex, PacketInfo, Tags,
};

use crate::ogg::codec::{CodecInfo, CodecKind, opus_packet_samples};
use crate::window::Window;
use crate::{ContainerError, Result};
use ebml::{Header, HeaderError};

/// Element IDs (RFC 9559 5.1, RFC 8794 11.2).
mod id {
    pub const EBML: u32 = 0x1A45_DFA3;
    pub const EBML_VERSION: u32 = 0x4286;
    pub const EBML_READ_VERSION: u32 = 0x42F7;
    pub const EBML_MAX_ID_LENGTH: u32 = 0x42F2;
    pub const EBML_MAX_SIZE_LENGTH: u32 = 0x42F3;
    pub const DOC_TYPE: u32 = 0x4282;
    pub const DOC_TYPE_VERSION: u32 = 0x4287;
    pub const DOC_TYPE_READ_VERSION: u32 = 0x4285;
    pub const SEGMENT: u32 = 0x1853_8067;
    pub const SEEK_HEAD: u32 = 0x114D_9B74;
    pub const SEEK: u32 = 0x4DBB;
    pub const SEEK_ID: u32 = 0x53AB;
    pub const SEEK_POSITION: u32 = 0x53AC;
    pub const INFO: u32 = 0x1549_A966;
    pub const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
    pub const DURATION: u32 = 0x4489;
    pub const MUXING_APP: u32 = 0x4D80;
    pub const TITLE: u32 = 0x7BA9;
    pub const WRITING_APP: u32 = 0x5741;
    pub const TRACKS: u32 = 0x1654_AE6B;
    pub const TRACK_ENTRY: u32 = 0xAE;
    pub const TRACK_NUMBER: u32 = 0xD7;
    pub const TRACK_UID: u32 = 0x73C5;
    pub const TRACK_TYPE: u32 = 0x83;
    pub const FLAG_LACING: u32 = 0x9C;
    pub const CODEC_ID: u32 = 0x86;
    pub const CODEC_PRIVATE: u32 = 0x63A2;
    pub const CODEC_DELAY: u32 = 0x56AA;
    pub const SEEK_PRE_ROLL: u32 = 0x56BB;
    pub const CONTENT_ENCODINGS: u32 = 0x6D80;
    pub const AUDIO: u32 = 0xE1;
    pub const SAMPLING_FREQUENCY: u32 = 0xB5;
    pub const CHANNELS: u32 = 0x9F;
    pub const CLUSTER: u32 = 0x1F43_B675;
    pub const TIMESTAMP: u32 = 0xE7;
    pub const SIMPLE_BLOCK: u32 = 0xA3;
    pub const BLOCK_GROUP: u32 = 0xA0;
    pub const BLOCK: u32 = 0xA1;
    pub const DISCARD_PADDING: u32 = 0x75A2;
    pub const CUES: u32 = 0x1C53_BB6B;
    pub const CUE_POINT: u32 = 0xBB;
    pub const CUE_TIME: u32 = 0xB3;
    pub const CUE_TRACK_POSITIONS: u32 = 0xB7;
    pub const CUE_TRACK: u32 = 0xF7;
    pub const CUE_CLUSTER_POSITION: u32 = 0xF1;
    pub const TAGS: u32 = 0x1254_C367;
    pub const CHAPTERS: u32 = 0x1043_A770;
    pub const ATTACHMENTS: u32 = 0x1941_A469;

    /// Children of a Segment; one of them ends a Cluster of unknown size.
    pub const TOP_LEVEL: [u32; 8] = [
        SEEK_HEAD,
        INFO,
        TRACKS,
        CLUSTER,
        CUES,
        TAGS,
        CHAPTERS,
        ATTACHMENTS,
    ];
}

/// Opus pre-roll in 48 kHz samples (RFC 7845 4.3: 80 ms).
const OPUS_PREROLL: u64 = 3840;

/// Matroska's `TrackType` for audio.
const TRACK_TYPE_AUDIO: u64 = 2;

/// Largest element body read into memory while scanning (a block, the
/// track list); anything larger is treated as damage.
const MAX_ELEMENT: u64 = 16 << 20;

/// Output clusters start at least this often (milliseconds), so a cluster
/// stays well within the 16-bit relative block timestamps.
const CLUSTER_MS: u64 = 5_000;

/// Nanoseconds to 48 kHz samples, rounded to the nearest (as ffmpeg does).
fn ns_to_samples(ns: u64) -> u64 {
    ((u128::from(ns) * 48_000 + 500_000_000) / 1_000_000_000) as u64
}

/// 48 kHz samples to nanoseconds, rounded, so that [`ns_to_samples`]
/// returns the same count.
fn samples_to_ns(n: u64) -> u64 {
    ((u128::from(n) * 1_000_000_000 + 24_000) / 48_000) as u64
}

/// The Opus track of the source.
#[derive(Debug, Clone)]
struct Track {
    number: u64,
    uid: u64,
    /// The `OpusHead` packet (RFC 7845 5.1).
    codec_private: Vec<u8>,
    channels: u16,
    /// Samples the decoder outputs before the timeline starts.
    pre_skip: u64,
}

/// An Opus packet as found in the source.
#[derive(Debug, Clone, Copy)]
struct RawPacket {
    offset: u64,
    len: u32,
    samples: u32,
    /// Sum of `samples` of the preceding packets.
    start: u64,
}

/// An opened Matroska or WebM file.
#[derive(Debug, Clone)]
pub struct MkvFile {
    doc_type: String,
    track: Track,
    raw: Vec<RawPacket>,
    /// Decoder output samples that belong to the timeline, in the
    /// coordinates of [`RawPacket::start`].
    audible: Range<u64>,
    /// Raw packet of each index entry.
    entry_raw: Vec<usize>,
    index: PacketIndex,
    other_tracks: usize,
    /// The source's file-wide tags.
    tags: Tags,
    warnings: Vec<String>,
}

impl MkvFile {
    /// Scans the file and builds the packet index. Reads every block
    /// header once but decodes nothing.
    pub fn open<R: Read + Seek + ?Sized>(r: &mut R) -> Result<Self> {
        let len = r.seek(SeekFrom::End(0))?;
        let mut w = Window::new(r, len);
        let mut s = Scanner::default();
        s.scan(&mut w)?;
        let Scanner {
            doc_type,
            track,
            other_tracks,
            raw,
            discard,
            mut tags,
            title,
            mut warnings,
            ..
        } = s;
        if tags.title.is_none() {
            tags.title = title.as_deref().and_then(crate::tags::non_empty);
        }
        let track = track.ok_or_else(|| {
            ContainerError::Unsupported("no Opus audio track found in the Matroska file".into())
        })?;
        if other_tracks > 0 {
            warnings.push(format!(
                "ignoring {other_tracks} other track(s); only the first audio track is cut"
            ));
        }
        let total = raw.last().map_or(0, |p| p.start + u64::from(p.samples));
        let last_samples = raw.last().map_or(0, |p| u64::from(p.samples));
        let end_trim = discard.min(last_samples);
        let audible = track.pre_skip.min(total)..total - end_trim;

        let mut packets = Vec::new();
        let mut entry_raw = Vec::new();
        let mut pos = 0u64;
        for (i, p) in raw.iter().enumerate() {
            let end = p.start + u64::from(p.samples);
            let a = p.start.max(audible.start);
            let b = end.min(audible.end);
            if b <= a {
                continue;
            }
            packets.push(PacketInfo {
                byte_offset: p.offset,
                byte_len: p.len,
                first_sample: pos,
                // At most one packet's samples, which fit u32.
                n_samples: (b - a) as u32,
                hidden_before: (a - p.start) as u32,
                hidden_after: (end - b) as u32,
                flags: PacketFlags { independent: false },
            });
            entry_raw.push(i);
            pos += b - a;
        }
        let info = AudioStreamInfo {
            sample_rate: 48_000,
            channels: track.channels,
        };
        let index = PacketIndex::new(info, packets)
            .map_err(|e| ContainerError::InvalidPlan(format!("internal index error: {e}")))?;
        Ok(Self {
            doc_type,
            track,
            raw,
            audible,
            entry_raw,
            index,
            other_tracks,
            tags,
            warnings,
        })
    }

    /// The source's tags (see the module documentation).
    pub fn tags(&self) -> &Tags {
        &self.tags
    }

    /// The EBML `DocType`: `"webm"` or `"matroska"`.
    pub fn doc_type(&self) -> &str {
        &self.doc_type
    }

    /// Number of tracks besides the Opus track, which are ignored.
    pub fn other_tracks(&self) -> usize {
        self.other_tracks
    }

    /// One entry per Opus packet that produces output.
    pub fn index(&self) -> &PacketIndex {
        &self.index
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

    /// Sample-exact at the start and end of the output file (codec delay
    /// and discard padding); joins inside the file are packet-granular.
    /// Pre-roll is added and hidden inside [`MkvFile::write_cut`].
    pub fn capabilities(&self) -> CutCapabilities {
        CutCapabilities {
            preroll_packets: 0,
            can_trim_start: true,
            can_trim_end: true,
            lossless_reencode: false,
            trims_at_file_ends_only: true,
        }
    }

    /// One section: all Opus packets, including those inside the codec
    /// delay; `keep` is the audible part. The extradata is the `OpusHead`.
    pub fn decode_sections<R: Read + Seek + ?Sized>(
        &self,
        _r: &mut R,
    ) -> Result<Vec<crate::DecodeSection>> {
        if self.raw.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![crate::DecodeSection {
            codec: crate::Codec::Opus,
            sample_rate: 48_000,
            channels: self.track.channels,
            extradata: self.track.codec_private.clone(),
            packet_ends: self
                .raw
                .iter()
                .map(|p| p.start + u64::from(p.samples))
                .collect(),
            keep: self.audible.clone(),
        }])
    }

    /// Opus packet `packet` of section `section` (there is only one).
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

    /// Writes the kept packets of `plans`, joined, as a new file of the
    /// same `DocType`. Only the start of the first and the end of the last
    /// non-empty plan may be trimmed (see [`MkvFile::capabilities`]).
    /// `tags`, or else the source's tags, are written as the file's tags.
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
            if (p.trim_start != 0 && i != 0) || (p.trim_end != 0 && i + 1 != kept.len()) {
                return Err(ContainerError::InvalidPlan(
                    "Matroska can only trim at the start and end of the file".into(),
                ));
            }
        }
        let (Some(first), Some(last)) = (kept.first(), kept.last()) else {
            return Ok(());
        };

        // Raw packets to write: pre-roll, then the kept entries in order.
        let r_first = self.entry_raw[first.packets.start];
        let mut r0 = r_first;
        let mut acc = 0u64;
        while r0 > 0 && acc < OPUS_PREROLL {
            r0 -= 1;
            acc += u64::from(self.raw[r0].samples);
        }
        let mut order: Vec<usize> = (r0..r_first).collect();
        for p in &kept {
            order.extend(p.packets.clone().map(|e| self.entry_raw[e]));
        }

        // Codec delay: the pre-roll plus whatever of the first packet lies
        // before the wanted start (inside the source's own codec delay or
        // trimmed by the plan).
        let fp = &self.raw[r_first];
        let start = fp.start.max(self.audible.start) + first.trim_start;
        let pre_skip = start - self.raw[r0].start;
        let pre_skip_u16 = u16::try_from(pre_skip)
            .map_err(|_| ContainerError::TooLarge("Opus pre-skip exceeds 65535 samples".into()))?;
        // Discard padding: the part of the last packet after the wanted end.
        let lp = &self.raw[self.entry_raw[last.packets.end - 1]];
        let lp_end = lp.start + u64::from(lp.samples);
        let discard = lp_end - (lp_end.min(self.audible.end) - last.trim_end);
        let decoded: u64 = order.iter().map(|&i| u64::from(self.raw[i].samples)).sum();
        let duration = decoded - pre_skip - discard;

        let mut head = self.track.codec_private.clone();
        // RFC 7845 5.1: pre-skip is a u16 at offset 10 (checked by `open`).
        head[10..12].copy_from_slice(&pre_skip_u16.to_le_bytes());

        let out = OutputTrack {
            track: &self.track,
            head: &head,
            pre_skip,
            duration,
            tags: tags.unwrap_or(&self.tags),
        };
        let mut mx = Muxer::start(w, &self.doc_type, &out)?;
        let mut pos = 0u64;
        for (k, &i) in order.iter().enumerate() {
            let p = &self.raw[i];
            let bytes = read_raw(r, p)?;
            let discard = if k + 1 == order.len() { discard } else { 0 };
            mx.packet(pos, &bytes, discard)?;
            pos += u64::from(p.samples);
        }
        mx.finish()
    }
}

/// Reads a raw packet's payload.
fn read_raw<R: Read + Seek + ?Sized>(r: &mut R, p: &RawPacket) -> Result<Vec<u8>> {
    r.seek(SeekFrom::Start(p.offset))?;
    let mut out = vec![0u8; p.len as usize];
    r.read_exact(&mut out).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            malformed(p.offset, "packet extends beyond the file")
        } else {
            ContainerError::Io(e)
        }
    })?;
    Ok(out)
}

fn malformed(offset: u64, reason: &str) -> ContainerError {
    ContainerError::Malformed {
        offset,
        reason: reason.to_owned(),
    }
}

// ------------------------------------------------------------ scanning

/// State while reading the file.
#[derive(Debug, Default)]
struct Scanner {
    doc_type: String,
    track: Option<Track>,
    other_tracks: usize,
    raw: Vec<RawPacket>,
    /// `DiscardPadding` of the most recent block of our track, in samples.
    discard: u64,
    /// A non-final block had discard padding (warned about once).
    inner_discard: bool,
    /// File-wide tags.
    tags: Tags,
    /// The `Info` element's title.
    title: Option<String>,
    warnings: Vec<String>,
}

/// Result of reading an element header from the file.
enum Next {
    Element(Header),
    /// The file ends inside the header.
    Truncated,
    /// The bytes are no element header.
    Invalid,
}

fn header_at<R: Read + Seek + ?Sized>(w: &mut Window<R>, pos: u64) -> io::Result<Next> {
    // ID (4) plus size (8).
    Ok(match ebml::parse_header(w.bytes(pos, 12)?) {
        Ok(h) => Next::Element(h),
        Err(HeaderError::Truncated) => Next::Truncated,
        Err(HeaderError::Invalid) => Next::Invalid,
    })
}

/// Adds the fields of `more` that `t` lacks, and all its extras.
fn merge_into(t: &mut Tags, more: Tags) {
    let fill = |a: &mut Option<String>, b: Option<String>| {
        if a.is_none() {
            *a = b;
        }
    };
    fill(&mut t.title, more.title);
    fill(&mut t.artist, more.artist);
    fill(&mut t.album, more.album);
    fill(&mut t.album_artist, more.album_artist);
    fill(&mut t.date, more.date);
    fill(&mut t.genre, more.genre);
    fill(&mut t.comment, more.comment);
    t.track_number = t.track_number.or(more.track_number);
    t.track_total = t.track_total.or(more.track_total);
    t.extra.extend(more.extra);
}

/// Reads a whole element body into memory.
fn body_at<R: Read + Seek + ?Sized>(w: &mut Window<R>, pos: u64, size: u64) -> Result<Vec<u8>> {
    if size > MAX_ELEMENT {
        return Err(malformed(pos, "implausibly large element"));
    }
    let b = w.bytes(pos, size)?;
    if (b.len() as u64) < size {
        return Err(malformed(pos, "element extends beyond the end of the file"));
    }
    Ok(b.to_vec())
}

/// Iterates over the child elements of a master element's body held in
/// memory: (id, body). Stops at the first malformed child.
fn children(body: &[u8]) -> impl Iterator<Item = (u32, &[u8])> {
    let mut p = 0usize;
    std::iter::from_fn(move || {
        let h = ebml::parse_header(body.get(p..)?).ok()?;
        let start = p + h.len;
        let end = start.checked_add(usize::try_from(h.size?).ok()?)?;
        let b = body.get(start..end)?;
        p = end;
        Some((h.id, b))
    })
}

impl Scanner {
    fn scan<R: Read + Seek + ?Sized>(&mut self, w: &mut Window<R>) -> Result<()> {
        // RFC 8794 11.2: the EBML header.
        let Next::Element(h) = header_at(w, 0)? else {
            return Err(ContainerError::Unsupported("not an EBML file".into()));
        };
        let size = h
            .size
            .filter(|_| h.id == id::EBML)
            .ok_or_else(|| ContainerError::Unsupported("not an EBML file".into()))?;
        let body = body_at(w, h.len as u64, size)?;
        for (i, b) in children(&body) {
            if i == id::DOC_TYPE {
                // Strings may be padded with zero bytes (RFC 8794 7.4).
                let s = b.split(|&c| c == 0).next().unwrap_or_default();
                self.doc_type = String::from_utf8_lossy(s).into_owned();
            }
        }
        if self.doc_type != "webm" && self.doc_type != "matroska" {
            return Err(ContainerError::Unsupported(format!(
                "EBML document type {:?} is not Matroska or WebM",
                self.doc_type
            )));
        }

        // The Segment, possibly after Void elements.
        let mut pos = h.len as u64 + size;
        let (seg_start, seg_end) = loop {
            match header_at(w, pos)? {
                Next::Element(h) if h.id == id::SEGMENT => {
                    let start = pos + h.len as u64;
                    let end = h.size.map_or(w.len, |s| start.saturating_add(s));
                    if end > w.len {
                        self.warnings
                            .push("the file is truncated (the segment ends early)".into());
                    }
                    break (start, end.min(w.len));
                }
                Next::Element(Header {
                    size: Some(s), len, ..
                }) => pos += len as u64 + s,
                _ => return Err(malformed(pos, "no Matroska segment")),
            }
        };

        pos = seg_start;
        while pos < seg_end {
            w.keep = pos;
            let h = match header_at(w, pos)? {
                Next::Element(h) => h,
                Next::Truncated => {
                    self.warnings
                        .push(format!("the last element at byte {pos} is truncated"));
                    break;
                }
                Next::Invalid => {
                    pos = self.resync(w, pos, seg_end)?;
                    continue;
                }
            };
            let body = pos + h.len as u64;
            if h.id == id::CLUSTER {
                pos = self.cluster(w, body, h.size, seg_end)?;
                continue;
            }
            let Some(size) = h.size else {
                return Err(malformed(pos, "element of unknown size outside a cluster"));
            };
            let end = body.saturating_add(size);
            match h.id {
                id::TRACKS => {
                    if self.track.is_some() {
                        self.warnings.push("a second track list was ignored".into());
                    } else {
                        let b = body_at(w, body, size)?;
                        self.tracks(&b, body)?;
                    }
                }
                id::TAGS if size <= MAX_ELEMENT && end <= seg_end => {
                    let b = body_at(w, body, size)?;
                    let t = tags::parse(&b);
                    // Several Tags elements: the first value of a field wins.
                    merge_into(&mut self.tags, t);
                }
                id::INFO if size <= MAX_ELEMENT && end <= seg_end => {
                    let b = body_at(w, body, size)?;
                    if let Some((_, t)) = children(&b).find(|&(i, _)| i == id::TITLE) {
                        self.title = Some(tags::string(t));
                    }
                }
                // A second segment (concatenated files) is not supported.
                id::SEGMENT => {
                    self.warnings
                        .push(format!("ignoring a second segment at byte {pos}"));
                    break;
                }
                _ => {}
            }
            if end > seg_end {
                if h.id != id::TRACKS {
                    self.warnings
                        .push(format!("the element at byte {pos} is truncated"));
                }
                break;
            }
            pos = end;
        }
        if let Some(t) = &self.track
            && t.channels == 0
        {
            return Err(ContainerError::Unsupported(
                "Opus track without channels".into(),
            ));
        }
        Ok(())
    }

    /// Skips damaged bytes up to the next Cluster ID. Returns its
    /// position, or `end`.
    fn resync<R: Read + Seek + ?Sized>(
        &mut self,
        w: &mut Window<R>,
        from: u64,
        end: u64,
    ) -> Result<u64> {
        const STEP: u64 = 1 << 16;
        let magic = id::CLUSTER.to_be_bytes();
        let mut pos = from + 1;
        let next = loop {
            if pos >= end {
                break end;
            }
            w.keep = pos;
            let chunk = w.bytes(pos, STEP + 3)?;
            if let Some(i) = chunk.windows(4).position(|x| x == magic) {
                break pos + i as u64;
            }
            if chunk.len() < 4 {
                break end;
            }
            pos += chunk.len() as u64 - 3;
        };
        self.warnings.push(format!(
            "skipped {} bytes of damaged data at byte {from}",
            next - from
        ));
        Ok(next)
    }

    /// Reads the track list and picks the first audio track.
    fn tracks(&mut self, body: &[u8], offset: u64) -> Result<()> {
        for (i, entry) in children(body) {
            if i != id::TRACK_ENTRY {
                continue;
            }
            let (mut number, mut uid, mut kind) = (0u64, 0u64, 0u64);
            let mut codec = String::new();
            let mut private = Vec::new();
            let mut delay = None;
            let mut encoded = false;
            for (i, b) in children(entry) {
                match i {
                    id::TRACK_NUMBER => number = ebml::read_uint(b).unwrap_or(0),
                    id::TRACK_UID => uid = ebml::read_uint(b).unwrap_or(0),
                    id::TRACK_TYPE => kind = ebml::read_uint(b).unwrap_or(0),
                    id::CODEC_ID => codec = String::from_utf8_lossy(b).into_owned(),
                    id::CODEC_PRIVATE => private = b.to_vec(),
                    id::CODEC_DELAY => delay = ebml::read_uint(b),
                    id::CONTENT_ENCODINGS => encoded = true,
                    _ => {}
                }
            }
            if kind != TRACK_TYPE_AUDIO || self.track.is_some() {
                self.other_tracks += 1;
                continue;
            }
            let codec = codec.trim_end_matches('\0');
            if codec != "A_OPUS" {
                return Err(ContainerError::Unsupported(format!(
                    "Matroska audio codec {codec} (only Opus is supported)"
                )));
            }
            if encoded {
                // Compression (e.g. header stripping) or encryption would
                // make the stored bytes differ from the Opus packets.
                return Err(ContainerError::Unsupported(
                    "compressed or encrypted Matroska track".into(),
                ));
            }
            let info = CodecInfo::from_id_header(CodecKind::Opus, &private)
                .map_err(|e| malformed(offset, &format!("Opus CodecPrivate: {e}")))?;
            let head_skip = u64::from(info.pre_skip);
            let pre_skip = match delay.map(ns_to_samples) {
                Some(d) if d != head_skip => {
                    self.warnings.push(format!(
                        "CodecDelay ({d} samples) differs from the OpusHead pre-skip ({head_skip}); using CodecDelay"
                    ));
                    d
                }
                _ => head_skip,
            };
            if number == 0 {
                return Err(malformed(offset, "audio track without a track number"));
            }
            self.track = Some(Track {
                number,
                uid,
                codec_private: private,
                channels: info.channels,
                pre_skip,
            });
        }
        Ok(())
    }

    /// Reads a Cluster whose body starts at `body`. Returns the position
    /// after it.
    fn cluster<R: Read + Seek + ?Sized>(
        &mut self,
        w: &mut Window<R>,
        body: u64,
        size: Option<u64>,
        seg_end: u64,
    ) -> Result<u64> {
        let Some(track) = self.track.as_ref().map(|t| t.number) else {
            return Err(ContainerError::Unsupported(
                "Matroska cluster before the track list".into(),
            ));
        };
        let end = size.map_or(seg_end, |s| body.saturating_add(s).min(seg_end));
        let mut pos = body;
        while pos < end {
            w.keep = pos;
            let h = match header_at(w, pos)? {
                Next::Element(h) => h,
                Next::Truncated => {
                    self.warnings
                        .push(format!("the last element at byte {pos} is truncated"));
                    return Ok(seg_end);
                }
                Next::Invalid => return self.resync(w, pos, seg_end),
            };
            // An unknown-size cluster ends where the next top-level
            // element starts (RFC 9559 6.3).
            if size.is_none() && id::TOP_LEVEL.contains(&h.id) {
                return Ok(pos);
            }
            let Some(s) = h.size else {
                return self.resync(w, pos, seg_end);
            };
            let child = pos + h.len as u64;
            let child_end = child.saturating_add(s);
            if child_end > w.len {
                self.warnings
                    .push(format!("the last block at byte {pos} is truncated"));
                return Ok(seg_end);
            }
            match h.id {
                id::SIMPLE_BLOCK => {
                    let b = body_at(w, child, s)?;
                    self.block(&b, child, track, 0);
                }
                id::BLOCK_GROUP => {
                    let g = body_at(w, child, s)?;
                    let mut block = None;
                    let mut discard = 0i64;
                    // Children of a BlockGroup: offsets relative to `g`.
                    let mut p = 0usize;
                    while let Ok(gh) = ebml::parse_header(&g[p..]) {
                        let Some(gs) = gh.size.and_then(|s| usize::try_from(s).ok()) else {
                            break;
                        };
                        let start = p + gh.len;
                        let Some(b) = g.get(start..start + gs) else {
                            break;
                        };
                        match gh.id {
                            id::BLOCK => block = Some((start, b)),
                            id::DISCARD_PADDING => discard = ebml::read_int(b).unwrap_or(0),
                            _ => {}
                        }
                        p = start + gs;
                    }
                    if let Some((start, b)) = block {
                        if discard < 0 {
                            self.warnings
                                .push(format!("negative DiscardPadding at byte {child} ignored"));
                        }
                        let d = ns_to_samples(discard.max(0) as u64);
                        self.block(b, child + start as u64, track, d);
                    }
                }
                _ => {}
            }
            pos = child_end;
        }
        Ok(end)
    }

    /// Adds the frames of a block (body at file offset `at`) if it belongs
    /// to `track`.
    fn block(&mut self, b: &[u8], at: u64, track: u64, discard: u64) {
        let blk = match block::parse_block(b) {
            Ok(blk) => blk,
            Err(e) => {
                self.warnings
                    .push(format!("skipped a damaged block at byte {at}: {e}"));
                return;
            }
        };
        if blk.track != track {
            return;
        }
        if self.discard > 0 && !self.inner_discard {
            self.inner_discard = true;
            self.warnings
                .push("DiscardPadding inside the track (not on its last block) is ignored".into());
        }
        let mut added = false;
        for (off, len) in blk.frames {
            let offset = at + off as u64;
            match opus_packet_samples(&b[off..off + len]) {
                Ok(samples) => {
                    let start = self
                        .raw
                        .last()
                        .map_or(0, |p| p.start + u64::from(p.samples));
                    self.raw.push(RawPacket {
                        offset,
                        // Bounded by MAX_ELEMENT.
                        len: len as u32,
                        samples,
                        start,
                    });
                    added = true;
                }
                Err(e) => self
                    .warnings
                    .push(format!("skipped undecodable packet at byte {offset}: {e}")),
            }
        }
        // The padding is at the end of the block, i.e. of its last frame.
        if added {
            self.discard = discard;
        }
    }
}

// ------------------------------------------------------------ writing

/// Writes a Matroska file with one Opus track, element by element.
struct Muxer<'a, W: Write + Seek + ?Sized> {
    w: &'a mut W,
    /// File offset of the Segment's size field.
    segment_size_at: u64,
    /// File offset of the Segment's body; positions are relative to it.
    segment_start: u64,
    /// File offset of the SeekHead's (8-byte) Cues position value.
    cues_seek_at: u64,
    /// The cluster being built: (timestamp in ms, body).
    cluster: Option<(u64, Vec<u8>)>,
    /// (timestamp in ms, segment position) of every written cluster.
    cues: Vec<(u64, u64)>,
}

/// What the Info and Tracks elements of an output file say.
struct OutputTrack<'t> {
    track: &'t Track,
    /// `OpusHead` with the new pre-skip.
    head: &'t [u8],
    /// The new pre-skip in samples.
    pre_skip: u64,
    /// Output length in samples (after pre-skip and discard padding).
    duration: u64,
    /// The file's tags.
    tags: &'t Tags,
}

impl<'a, W: Write + Seek + ?Sized> Muxer<'a, W> {
    /// Writes the EBML header, the Segment header, SeekHead, Info and
    /// Tracks.
    fn start(w: &'a mut W, doc_type: &str, t: &OutputTrack) -> Result<Self> {
        let mut out = Vec::new();
        let mut h = Vec::new();
        ebml::put_uint(&mut h, id::EBML_VERSION, 1);
        ebml::put_uint(&mut h, id::EBML_READ_VERSION, 1);
        ebml::put_uint(&mut h, id::EBML_MAX_ID_LENGTH, 4);
        ebml::put_uint(&mut h, id::EBML_MAX_SIZE_LENGTH, 8);
        ebml::put_element(&mut h, id::DOC_TYPE, doc_type.as_bytes());
        // DiscardPadding and CodecDelay need DocType version 4.
        ebml::put_uint(&mut h, id::DOC_TYPE_VERSION, 4);
        ebml::put_uint(&mut h, id::DOC_TYPE_READ_VERSION, 2);
        ebml::put_element(&mut out, id::EBML, &h);

        let base = w.stream_position()?;
        ebml::put_id(&mut out, id::SEGMENT);
        let segment_size_at = base + out.len() as u64;
        // Patched by `finish`.
        ebml::put_size_n(&mut out, 0, 8);
        let segment_start = base + out.len() as u64;

        // Info. Timestamps are in milliseconds (the default scale).
        let mut info = Vec::new();
        ebml::put_uint(&mut info, id::TIMESTAMP_SCALE, 1_000_000);
        ebml::put_element(&mut info, id::MUXING_APP, b"losslessaudiocut");
        ebml::put_element(&mut info, id::WRITING_APP, b"losslessaudiocut");
        ebml::put_float(&mut info, id::DURATION, t.duration as f64 / 48.0);
        if let Some(title) = &t.tags.title {
            ebml::put_element(&mut info, id::TITLE, title.as_bytes());
        }
        let mut info_el = Vec::new();
        ebml::put_element(&mut info_el, id::INFO, &info);

        // Tracks: the Opus track, renumbered as track 1.
        let mut audio = Vec::new();
        ebml::put_uint(&mut audio, id::CHANNELS, u64::from(t.track.channels));
        // The Matroska Opus mapping always states 48 kHz.
        ebml::put_float(&mut audio, id::SAMPLING_FREQUENCY, 48_000.0);
        let mut entry = Vec::new();
        ebml::put_uint(&mut entry, id::TRACK_NUMBER, 1);
        ebml::put_uint(&mut entry, id::TRACK_UID, t.track.uid.max(1));
        ebml::put_uint(&mut entry, id::TRACK_TYPE, TRACK_TYPE_AUDIO);
        ebml::put_uint(&mut entry, id::FLAG_LACING, 0);
        ebml::put_element(&mut entry, id::CODEC_ID, b"A_OPUS");
        ebml::put_element(&mut entry, id::CODEC_PRIVATE, t.head);
        ebml::put_uint(&mut entry, id::CODEC_DELAY, samples_to_ns(t.pre_skip));
        ebml::put_uint(&mut entry, id::SEEK_PRE_ROLL, samples_to_ns(OPUS_PREROLL));
        ebml::put_element(&mut entry, id::AUDIO, &audio);
        let mut tracks = Vec::new();
        ebml::put_element(&mut tracks, id::TRACK_ENTRY, &entry);
        let mut tracks_el = Vec::new();
        ebml::put_element(&mut tracks_el, id::TRACKS, &tracks);

        let tags_el = tags::element(t.tags);

        // SeekHead. Its size does not depend on the (8-byte) positions, so
        // Info, Tracks and Tags, which follow it, are placed by a first
        // pass. Cues come last, so their position is the SeekHead's last
        // value.
        let mut targets = vec![id::INFO, id::TRACKS];
        if !tags_el.is_empty() {
            targets.push(id::TAGS);
        }
        targets.push(id::CUES);
        let seek_head = |positions: &[u64]| {
            let mut seeks = Vec::new();
            for (&target, &at) in targets.iter().zip(positions) {
                let mut tid = Vec::new();
                ebml::put_id(&mut tid, target);
                let mut s = Vec::new();
                ebml::put_element(&mut s, id::SEEK_ID, &tid);
                ebml::put_uint8(&mut s, id::SEEK_POSITION, at);
                ebml::put_element(&mut seeks, id::SEEK, &s);
            }
            let mut el = Vec::new();
            ebml::put_element(&mut el, id::SEEK_HEAD, &seeks);
            el
        };
        let head_len = seek_head(&[0; 4]).len() as u64;
        let info_pos = head_len;
        let tracks_pos = info_pos + info_el.len() as u64;
        let tags_pos = tracks_pos + tracks_el.len() as u64;
        let head = if tags_el.is_empty() {
            seek_head(&[info_pos, tracks_pos, 0])
        } else {
            seek_head(&[info_pos, tracks_pos, tags_pos, 0])
        };
        // The Cues position is the last value in the SeekHead.
        let cues_seek_at = segment_start + head.len() as u64 - 8;
        out.extend_from_slice(&head);
        out.extend_from_slice(&info_el);
        out.extend_from_slice(&tracks_el);
        out.extend_from_slice(&tags_el);
        w.write_all(&out)?;
        Ok(Self {
            w,
            segment_size_at,
            segment_start,
            cues_seek_at,
            cluster: None,
            cues: Vec::new(),
        })
    }

    /// Adds a packet whose output starts at sample `pos` of the decoded
    /// stream; `discard` samples at its end are hidden.
    fn packet(&mut self, pos: u64, data: &[u8], discard: u64) -> Result<()> {
        // Block timestamps in ms; the first block is at 0 and the codec
        // delay is subtracted by the reader (RFC 9559 5.1.4.1.28.17).
        let ms = (pos + 24) / 48;
        if let Some((start, _)) = &self.cluster
            && ms - start >= CLUSTER_MS
        {
            self.flush_cluster()?;
        }
        let (start, body) = self.cluster.get_or_insert_with(|| {
            let mut b = Vec::new();
            ebml::put_uint(&mut b, id::TIMESTAMP, ms);
            (ms, b)
        });
        // Below CLUSTER_MS, so it fits the 16-bit relative timestamp.
        let rel = (ms - *start) as i16;
        let mut block = vec![0x81]; // track 1 as a one-byte VINT
        block.extend_from_slice(&rel.to_be_bytes());
        if discard == 0 {
            block.push(0x80); // keyframe, no lacing
            block.extend_from_slice(data);
            ebml::put_element(body, id::SIMPLE_BLOCK, &block);
        } else {
            block.push(0x00);
            block.extend_from_slice(data);
            let mut group = Vec::new();
            ebml::put_element(&mut group, id::BLOCK, &block);
            let ns = i64::try_from(samples_to_ns(discard)).unwrap_or(i64::MAX);
            ebml::put_int(&mut group, id::DISCARD_PADDING, ns);
            ebml::put_element(body, id::BLOCK_GROUP, &group);
        }
        Ok(())
    }

    fn flush_cluster(&mut self) -> Result<()> {
        if let Some((ms, body)) = self.cluster.take() {
            let pos = self.w.stream_position()? - self.segment_start;
            self.cues.push((ms, pos));
            let mut el = Vec::with_capacity(body.len() + 12);
            ebml::put_element(&mut el, id::CLUSTER, &body);
            self.w.write_all(&el)?;
        }
        Ok(())
    }

    /// Writes the last cluster and the Cues, and patches the sizes and
    /// positions that were left open.
    fn finish(mut self) -> Result<()> {
        self.flush_cluster()?;
        let cues_pos = self.w.stream_position()? - self.segment_start;
        let mut points = Vec::new();
        for &(ms, pos) in &self.cues {
            let mut tp = Vec::new();
            ebml::put_uint(&mut tp, id::CUE_TRACK, 1);
            ebml::put_uint(&mut tp, id::CUE_CLUSTER_POSITION, pos);
            let mut cp = Vec::new();
            ebml::put_uint(&mut cp, id::CUE_TIME, ms);
            ebml::put_element(&mut cp, id::CUE_TRACK_POSITIONS, &tp);
            ebml::put_element(&mut points, id::CUE_POINT, &cp);
        }
        let mut cues = Vec::new();
        ebml::put_element(&mut cues, id::CUES, &points);
        self.w.write_all(&cues)?;
        let end = self.w.stream_position()?;

        let mut size = Vec::new();
        ebml::put_size_n(&mut size, end - self.segment_start, 8);
        self.w.seek(SeekFrom::Start(self.segment_size_at))?;
        self.w.write_all(&size)?;
        self.w.seek(SeekFrom::Start(self.cues_seek_at))?;
        self.w.write_all(&cues_pos.to_be_bytes())?;
        self.w.seek(SeekFrom::Start(end))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// `OpusHead` for stereo with a pre-skip of 312 (RFC 7845 5.1).
    fn opus_head() -> Vec<u8> {
        let mut h = b"OpusHead".to_vec();
        h.extend([1, 2]);
        h.extend(312u16.to_le_bytes());
        h.extend(48_000u32.to_le_bytes());
        h.extend([0, 0, 0]);
        h
    }

    /// A 20 ms CELT packet (TOC config 31, one frame) tagged with `tag`.
    fn packet(tag: u8) -> Vec<u8> {
        vec![31 << 3, tag, tag]
    }

    fn track_entry(number: u64, kind: u64, codec: &str, extra: &[u8]) -> Vec<u8> {
        let mut e = Vec::new();
        ebml::put_uint(&mut e, id::TRACK_NUMBER, number);
        ebml::put_uint(&mut e, id::TRACK_TYPE, kind);
        ebml::put_element(&mut e, id::CODEC_ID, codec.as_bytes());
        ebml::put_element(&mut e, id::CODEC_PRIVATE, &opus_head());
        e.extend_from_slice(extra);
        let mut out = Vec::new();
        ebml::put_element(&mut out, id::TRACK_ENTRY, &e);
        out
    }

    fn simple_block(track: u8, flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut b = vec![0x80 | track, 0, 0, flags];
        b.extend_from_slice(payload);
        let mut out = Vec::new();
        ebml::put_element(&mut out, id::SIMPLE_BLOCK, &b);
        out
    }

    fn discard_block(track: u8, payload: &[u8], ns: i64) -> Vec<u8> {
        let mut b = vec![0x80 | track, 0, 0, 0];
        b.extend_from_slice(payload);
        let mut g = Vec::new();
        ebml::put_element(&mut g, id::BLOCK, &b);
        ebml::put_int(&mut g, id::DISCARD_PADDING, ns);
        let mut out = Vec::new();
        ebml::put_element(&mut out, id::BLOCK_GROUP, &g);
        out
    }

    /// A file with the given track entries and cluster bodies; clusters of
    /// unknown size if `live`, followed by an empty Cues element.
    fn file(tracks: &[Vec<u8>], clusters: &[Vec<u8>], live: bool) -> Vec<u8> {
        let mut seg = Vec::new();
        ebml::put_element(&mut seg, id::TRACKS, &tracks.concat());
        for c in clusters {
            let mut body = Vec::new();
            ebml::put_uint(&mut body, id::TIMESTAMP, 0);
            body.extend_from_slice(c);
            if live {
                ebml::put_id(&mut seg, id::CLUSTER);
                seg.push(0xFF); // one-byte unknown size
                seg.extend_from_slice(&body);
            } else {
                ebml::put_element(&mut seg, id::CLUSTER, &body);
            }
        }
        ebml::put_element(&mut seg, id::CUES, &[]);
        let mut h = Vec::new();
        ebml::put_element(&mut h, id::DOC_TYPE, b"webm");
        let mut out = Vec::new();
        ebml::put_element(&mut out, id::EBML, &h);
        ebml::put_element(&mut out, id::SEGMENT, &seg);
        out
    }

    fn opus_track() -> Vec<u8> {
        track_entry(1, TRACK_TYPE_AUDIO, "A_OPUS", &[])
    }

    fn open(bytes: &[u8]) -> Result<MkvFile> {
        MkvFile::open(&mut Cursor::new(bytes))
    }

    fn payloads(f: &MkvFile, bytes: &[u8]) -> Vec<Vec<u8>> {
        (0..f.index().packets().len())
            .map(|e| f.read_packet(&mut Cursor::new(bytes), e).expect("packet"))
            .collect()
    }

    #[test]
    fn laced_frames_become_packets() {
        let (a, b, c) = (packet(1), packet(2), packet(3));
        // Xiph lacing: count - 1, then the sizes of all but the last.
        let laced = [vec![2, 3, 3], a.clone(), b.clone(), c.clone()].concat();
        let bytes = file(&[opus_track()], &[simple_block(1, 0x82, &laced)], false);
        let f = open(&bytes).expect("opens");
        assert!(f.warnings().is_empty(), "{:?}", f.warnings());
        assert_eq!(payloads(&f, &bytes), vec![a, b, c]);
        assert_eq!(f.index().total_samples(), 3 * 960 - 312);
    }

    #[test]
    fn unknown_size_clusters_end_at_the_next_cluster_or_cues() {
        let clusters = [
            [
                simple_block(1, 0x80, &packet(1)),
                simple_block(1, 0x80, &packet(2)),
            ]
            .concat(),
            simple_block(1, 0x80, &packet(3)),
        ];
        let live = file(&[opus_track()], &clusters, true);
        let fixed = file(&[opus_track()], &clusters, false);
        let (fl, ff) = (open(&live).expect("live"), open(&fixed).expect("fixed"));
        assert!(fl.warnings().is_empty(), "{:?}", fl.warnings());
        assert_eq!(payloads(&fl, &live), payloads(&ff, &fixed));
        assert_eq!(fl.index().packets().len(), 3);
    }

    #[test]
    fn final_discard_padding_trims_the_end() {
        // 10 ms = 480 samples.
        let c = [
            simple_block(1, 0x80, &packet(1)),
            discard_block(1, &packet(2), 10_000_000),
        ]
        .concat();
        let f = open(&file(&[opus_track()], &[c], false)).expect("opens");
        assert!(f.warnings().is_empty(), "{:?}", f.warnings());
        assert_eq!(f.index().total_samples(), 2 * 960 - 312 - 480);
        assert_eq!(f.index().packets()[1].hidden_after, 480);
    }

    #[test]
    fn inner_discard_padding_is_ignored_with_warning() {
        let c = [
            discard_block(1, &packet(1), 10_000_000),
            simple_block(1, 0x80, &packet(2)),
        ]
        .concat();
        let f = open(&file(&[opus_track()], &[c], false)).expect("opens");
        assert_eq!(f.index().total_samples(), 2 * 960 - 312);
        assert_eq!(f.warnings().len(), 1, "{:?}", f.warnings());
    }

    #[test]
    fn other_tracks_are_ignored() {
        let video = track_entry(1, 1, "V_MJPEG", &[]);
        let audio = track_entry(2, TRACK_TYPE_AUDIO, "A_OPUS", &[]);
        let c = [
            simple_block(1, 0x80, &[9; 50]),
            simple_block(2, 0x80, &packet(1)),
            simple_block(1, 0x80, &[9; 50]),
        ]
        .concat();
        let bytes = file(&[video, audio], &[c], false);
        let f = open(&bytes).expect("opens");
        assert_eq!(f.other_tracks(), 1);
        assert_eq!(f.warnings().len(), 1, "{:?}", f.warnings());
        assert_eq!(payloads(&f, &bytes), vec![packet(1)]);
    }

    #[test]
    fn unsupported_tracks_are_errors() {
        let c = simple_block(1, 0x80, &packet(1));
        let vorbis = track_entry(1, TRACK_TYPE_AUDIO, "A_VORBIS", &[]);
        assert!(matches!(
            open(&file(&[vorbis], std::slice::from_ref(&c), false)),
            Err(ContainerError::Unsupported(_))
        ));
        // Header stripping or encryption changes the stored bytes.
        let mut enc = Vec::new();
        ebml::put_element(&mut enc, id::CONTENT_ENCODINGS, &[]);
        let encoded = track_entry(1, TRACK_TYPE_AUDIO, "A_OPUS", &enc);
        assert!(matches!(
            open(&file(&[encoded], std::slice::from_ref(&c), false)),
            Err(ContainerError::Unsupported(_))
        ));
        // No audio track at all.
        assert!(open(&file(&[], &[], false)).is_err());
    }

    #[test]
    fn cluster_before_tracks_is_an_error() {
        let mut seg = Vec::new();
        let mut body = Vec::new();
        ebml::put_uint(&mut body, id::TIMESTAMP, 0);
        body.extend(simple_block(1, 0x80, &packet(1)));
        ebml::put_element(&mut seg, id::CLUSTER, &body);
        ebml::put_element(&mut seg, id::TRACKS, &opus_track());
        let mut h = Vec::new();
        ebml::put_element(&mut h, id::DOC_TYPE, b"matroska");
        let mut bytes = Vec::new();
        ebml::put_element(&mut bytes, id::EBML, &h);
        ebml::put_element(&mut bytes, id::SEGMENT, &seg);
        assert!(open(&bytes).is_err());
    }

    #[test]
    fn other_doc_types_are_rejected() {
        let mut h = Vec::new();
        ebml::put_element(&mut h, id::DOC_TYPE, b"something");
        let mut bytes = Vec::new();
        ebml::put_element(&mut bytes, id::EBML, &h);
        assert!(matches!(open(&bytes), Err(ContainerError::Unsupported(_))));
    }

    #[test]
    fn time_conversions_round_trip() {
        for n in [0u64, 1, 312, 480, 3840, 65_535, 48_000 * 3600] {
            assert_eq!(ns_to_samples(samples_to_ns(n)), n);
        }
        assert_eq!(ns_to_samples(6_500_000), 312);
        assert_eq!(ns_to_samples(13_500_000), 648);
    }
}
