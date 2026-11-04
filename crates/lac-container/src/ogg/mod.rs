//! Ogg files with Vorbis, Opus or Speex audio (RFC 3533, RFC 7845).
//!
//! ## Timeline without granule positions
//!
//! Every Ogg page carries a granule position, which for audio is the
//! number of samples decoded up to the last packet finishing on the page.
//! Many recordings, radio captures in particular, store random or
//! inconsistent values there. The demuxer therefore ignores granule
//! positions completely and computes each packet's duration from the
//! packet itself (see [`codec`]).
//!
//! The one exception is the *end trim* of Vorbis and Opus (Vorbis I
//! A.2, RFC 7845 4.4): the final page of a stream may carry a granule
//! position smaller than the decoded length, telling the decoder to drop
//! the excess samples (the encoder's padding of the last block). It is
//! honoured only when the granule of the page before agrees exactly with
//! the packet-derived timeline, so random granules can never shorten a
//! stream. In a chained file this matters at every link boundary, not just
//! at the end: without it the padding of each link would be played in the
//! middle of the file, which libvorbisfile, opusfile and ffmpeg do not do.
//! Speex end trims, and the end trims of streams whose granules do not
//! start at zero, are not applied; the index may then be up to one page
//! longer than what a player that trusts granules outputs.
//!
//! ## Packets, links and the index
//!
//! A file is a sequence of *links* (chained logical streams, e.g. one per
//! song in a radio capture). Each link has its own header packets followed
//! by audio packets. The index holds one entry per audio packet that
//! produces output, numbered across all links:
//!
//! - Vorbis: the first audio packet of a link only primes the MDCT overlap
//!   and produces nothing, so it has no index entry.
//! - Opus: packets entirely inside the link's pre-skip or end trim have no
//!   entry; one straddling either gets an entry for its audible part only.
//! - Speex: every audio packet has an entry.
//!
//! Other logical streams (Skeleton, video, unknown codecs) are ignored with
//! a warning.
//!
//! ## Writing
//!
//! A cut is written as a chained Ogg file: every run of kept packets
//! becomes a logical stream of its own with a copy of its link's headers,
//! newly computed granule positions and a new serial number. Audio packets
//! are copied verbatim; only the page framing is new.
//!
//! Reference decoders (libvorbisfile, opusfile) reset at every chain
//! boundary. ffmpeg does not reset its Vorbis decoder there and outputs one
//! extra block overlap (up to about 23 ms) per join point; Opus is fine.
//!
//! - Vorbis: the packet before the first kept one is written in front of
//!   it. A new stream's first packet produces no output, so the output is
//!   bit-exact; Vorbis keeps no decoder state beyond the overlap. A stream
//!   that ends with the end-trimmed last packet of a source link gets the
//!   same end trim in its final granule position.
//! - Opus: at least 80 ms (3840 samples) of preceding packets are written
//!   as pre-roll, hidden by the stream's pre-skip (RFC 7845 4.3 recommends
//!   80 ms for the decoder to converge). Cuts are sample-exact: the
//!   pre-skip also drops the head of the first kept packet, and the final
//!   granule position of every stream drops the tail of its last packet
//!   (RFC 7845 4.4), so every chained stream can trim both its ends. The
//!   output matches the source except within a short convergence window
//!   at every cut point.
//!
//!   ffmpeg (6.1) attaches the end trim to the last packet as side data
//!   that also resets the pending pre-skip. A stream whose kept part
//!   starts and ends inside its last packet therefore decodes too long
//!   there, by the part of the pre-skip that lies in that packet. The file
//!   is correct; opusfile decodes it exactly. ffmpeg also ignores the end
//!   trim when a stream's first audio page is its last, so the last packet
//!   gets a page of its own.
//! - Speex: kept packets only, and all kept parts of a link go into one
//!   stream, because ffmpeg and speexdec mishandle chained Speex and
//!   chaining would not help: Speex has no pre-skip to hide pre-roll. The
//!   decoder converges after a join point, at the latest in the next pause.
//!
//! ## Tags
//!
//! The comment header (the second header packet) holds Vorbis comments:
//! after `\x03vorbis` and followed by a framing bit for Vorbis (Vorbis I
//! 5.2.1), after `OpusTags` for Opus (RFC 7845 5.2, where binary data may
//! follow), and on its own for Speex. The source's tags are those of the
//! first link. An export with new tags writes them into the comment header
//! of every output stream, keeping the vendor string and anything after
//! the comments; without new tags each stream keeps its link's header.

pub mod codec;
pub mod page;

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;

use lac_core::{
    AudioStreamInfo, CutCapabilities, CutPlan, PacketFlags, PacketIndex, PacketInfo, Tags,
};

use crate::tags::vorbis::VorbisComment;
use crate::window::Window;
use crate::{ContainerError, Result};
use codec::{CodecInfo, CodecKind};
use page::{PageError, Paginator, parse_page};

/// Opus pre-roll in 48 kHz samples (RFC 7845 4.3: 80 ms).
const OPUS_PREROLL: u64 = 3840;

/// Serial number of the first output stream; later ones count up.
const SERIAL_BASE: u32 = 0x6C61_6300; // "lac\0"

/// Location of a packet in the source: (file offset, length) of each
/// contiguous piece. A packet spans several pieces when it continues on
/// the next page.
type Pieces = Vec<(u64, u32)>;

/// One logical stream of the source with a recognised codec.
#[derive(Debug, Clone)]
struct Link {
    info: CodecInfo,
    /// Header packets, in order.
    headers: Vec<Pieces>,
    /// Range of this link's audio packets in [`OggFile::raw`].
    raw: Range<usize>,
    /// Samples of the link that are output, in the coordinates of
    /// [`RawPacket::start_in_link`]: Opus pre-skip and end trim removed.
    audible: Range<u64>,
}

/// An audio packet as found in the source.
#[derive(Debug, Clone)]
struct RawPacket {
    pieces: Pieces,
    /// Samples it adds to the decoded output (before Opus pre-skip).
    samples: u32,
    /// Sum of `samples` of the preceding packets of the same link.
    start_in_link: u64,
    /// Index into [`OggFile::links`].
    link: usize,
}

/// An opened Ogg file.
#[derive(Debug, Clone)]
pub struct OggFile {
    links: Vec<Link>,
    raw: Vec<RawPacket>,
    /// Raw packet of each index entry.
    entry_raw: Vec<usize>,
    index: PacketIndex,
    /// Tags of the first link.
    tags: Tags,
    warnings: Vec<String>,
}

impl OggFile {
    /// Scans all pages and builds the packet index. Reads the whole file
    /// once but decodes nothing.
    pub fn open<R: Read + Seek + ?Sized>(r: &mut R) -> Result<Self> {
        let len = r.seek(SeekFrom::End(0))?;
        let mut s = Scanner::default();
        let mut w = Window::new(r, len);
        s.scan(&mut w)?;
        s.finish_link();
        let Scanner {
            links,
            raw,
            mut warnings,
            ..
        } = s;
        let Some(first) = links.first() else {
            return Err(ContainerError::Unsupported(
                "no Vorbis, Opus or Speex stream found".into(),
            ));
        };
        let info = AudioStreamInfo {
            sample_rate: first.info.sample_rate,
            channels: first.info.channels,
        };
        for l in &links[1..] {
            if l.info.kind != first.info.kind
                || l.info.sample_rate != info.sample_rate
                || l.info.channels != info.channels
            {
                return Err(ContainerError::Unsupported(format!(
                    "chained streams differ in codec, rate or channels ({} {} Hz {} ch, then {} {} Hz {} ch)",
                    first.info.kind.name(),
                    info.sample_rate,
                    info.channels,
                    l.info.kind.name(),
                    l.info.sample_rate,
                    l.info.channels
                )));
            }
        }

        // Index entries: see the module documentation.
        let mut packets = Vec::new();
        let mut entry_raw = Vec::new();
        let mut pos = 0u64;
        for (i, p) in raw.iter().enumerate() {
            let link = &links[p.link];
            let end = p.start_in_link + u64::from(p.samples);
            let audible = end
                .min(link.audible.end)
                .saturating_sub(p.start_in_link.max(link.audible.start));
            if audible == 0 {
                continue;
            }
            let byte_len: u64 = p.pieces.iter().map(|&(_, l)| u64::from(l)).sum();
            packets.push(PacketInfo {
                byte_offset: p.pieces.first().map_or(0, |&(o, _)| o),
                byte_len: u32::try_from(byte_len).unwrap_or(u32::MAX),
                first_sample: pos,
                // At most one packet's samples, which fit u32.
                n_samples: audible as u32,
                hidden_before: 0,
                hidden_after: 0,
                flags: PacketFlags { independent: false },
            });
            entry_raw.push(i);
            pos += audible;
        }
        if links.iter().any(|l| l.raw.is_empty()) {
            warnings.push("a stream without audio packets was ignored".into());
        }
        let index = PacketIndex::new(info, packets)
            .map_err(|e| ContainerError::InvalidPlan(format!("internal index error: {e}")))?;
        let tags = match links[0].headers.get(1) {
            Some(h) => {
                let bytes = read_pieces(r, h)?;
                match CommentHeader::parse(links[0].info.kind, &bytes) {
                    Some(c) => c.comment.tags(),
                    None => {
                        warnings.push("the comment header is not readable".into());
                        Tags::default()
                    }
                }
            }
            None => Tags::default(),
        };
        Ok(Self {
            links,
            raw,
            entry_raw,
            index,
            tags,
            warnings,
        })
    }

    /// The tags of the first link (see the module documentation).
    pub fn tags(&self) -> &Tags {
        &self.tags
    }

    /// The codec of the audio stream.
    pub fn codec(&self) -> CodecKind {
        // `open` guarantees at least one link.
        self.links[0].info.kind
    }

    /// Number of chained logical streams carrying audio.
    pub fn links(&self) -> usize {
        self.links.len()
    }

    /// One entry per audio packet that produces output.
    pub fn index(&self) -> &PacketIndex {
        &self.index
    }

    /// Problems that were tolerated while opening the file.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Reads the payload of index entry `entry` (for decoders, and for
    /// tests that check packets are copied verbatim).
    pub fn read_packet<R: Read + Seek + ?Sized>(&self, r: &mut R, entry: usize) -> Result<Vec<u8>> {
        let raw = self
            .entry_raw
            .get(entry)
            .ok_or_else(|| ContainerError::InvalidPlan(format!("no packet {entry}")))?;
        read_pieces(r, &self.raw[*raw].pieces)
    }

    /// Opus cuts are sample-exact at every cut point (pre-skip and end
    /// trim of each chained stream); Vorbis and Speex cuts are
    /// packet-granular. Vorbis and Opus pre-roll is handled inside
    /// [`OggFile::write_cut`] and hidden from the listener, so no pre-roll
    /// packets are planned.
    pub fn capabilities(&self) -> CutCapabilities {
        let opus = self.codec() == CodecKind::Opus;
        CutCapabilities {
            preroll_packets: 0,
            can_trim_start: opus,
            can_trim_end: opus,
            lossless_reencode: false,
            trims_at_file_ends_only: false,
        }
    }

    /// One section per chained stream (link) that holds audio. Its
    /// packets are all audio packets of the link, also those that are not
    /// audible (Vorbis's first packet, Opus pre-skip), so the decoder state
    /// builds up as in any player; `keep` is the link's audible part. The
    /// extradata is the identification header for Opus and Speex, and all
    /// three headers in Xiph lacing for Vorbis.
    pub fn decode_sections<R: Read + Seek + ?Sized>(
        &self,
        r: &mut R,
    ) -> Result<Vec<crate::DecodeSection>> {
        self.decode_links()
            .map(|l| {
                let headers = l
                    .headers
                    .iter()
                    .map(|h| read_pieces(r, h))
                    .collect::<Result<Vec<_>>>()?;
                let (codec, extradata) = match l.info.kind {
                    CodecKind::Vorbis => (crate::Codec::Vorbis, crate::decode::xiph_lace(&headers)),
                    CodecKind::Opus => (crate::Codec::Opus, first_header(headers)),
                    CodecKind::Speex => (crate::Codec::Speex, first_header(headers)),
                };
                Ok(crate::DecodeSection {
                    codec,
                    sample_rate: l.info.sample_rate,
                    channels: l.info.channels,
                    extradata,
                    packet_ends: self.raw[l.raw.clone()]
                        .iter()
                        .map(|p| p.start_in_link + u64::from(p.samples))
                        .collect(),
                    keep: l.audible.clone(),
                })
            })
            .collect()
    }

    /// Links that hold audio packets, in order: the sections of
    /// [`OggFile::decode_sections`].
    fn decode_links(&self) -> impl Iterator<Item = &Link> {
        self.links.iter().filter(|l| !l.raw.is_empty())
    }

    /// Audio packet `packet` of section `section`.
    pub fn read_decode_packet<R: Read + Seek + ?Sized>(
        &self,
        r: &mut R,
        section: usize,
        packet: usize,
    ) -> Result<Vec<u8>> {
        let link = self
            .decode_links()
            .nth(section)
            .ok_or_else(|| crate::decode::no_section(section))?;
        let raw = link
            .raw
            .clone()
            .nth(packet)
            .ok_or_else(|| ContainerError::InvalidPlan(format!("no packet {packet}")))?;
        read_pieces(r, &self.raw[raw].pieces)
    }

    /// Writes the kept packets of `plans` as a chained Ogg file. `tags`,
    /// if given, go into the comment header of every stream.
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
        let runs = self.runs(plans)?;
        for (i, run) in runs.iter().enumerate() {
            let serial = SERIAL_BASE.wrapping_add(i as u32);
            self.write_run(r, run, serial, tags, w)?;
        }
        Ok(())
    }

    /// Groups the kept index entries into output streams.
    ///
    /// Each returned run holds entry ranges of one link. For Vorbis and
    /// Opus a run is a single contiguous range, so every join point starts
    /// a new chained stream with clean decoder state. Speex has no pre-skip
    /// that could hide pre-roll, and common decoders mishandle chained
    /// Speex, so all kept parts of one link are joined in one stream.
    ///
    /// A plan's trims go to the run holding its first and last entry. A run
    /// with a trimmed end, or a plan with a trimmed start, never merges with
    /// its neighbour, because a stream can only trim at its two ends.
    fn runs(&self, plans: &[CutPlan]) -> Result<Vec<Run>> {
        let n = self.entry_raw.len();
        let mut runs: Vec<Run> = Vec::new();
        for p in plans {
            if (p.trim_start != 0 || p.trim_end != 0) && self.codec() != CodecKind::Opus {
                return Err(ContainerError::InvalidPlan(format!(
                    "{} cuts cannot trim samples",
                    self.codec().name()
                )));
            }
            if p.packets.start > p.packets.end || p.packets.end > n {
                return Err(ContainerError::InvalidPlan(format!(
                    "packets {:?} outside the index (length {n})",
                    p.packets
                )));
            }
            for e in p.packets.clone() {
                let link = self.raw[self.entry_raw[e]].link;
                let joinable = self.links[link].info.kind == CodecKind::Speex;
                let trim_start = if e == p.packets.start {
                    p.trim_start
                } else {
                    0
                };
                let last = runs.last_mut().filter(|run| {
                    let same_link = self.raw[self.entry_raw[run.parts[0].start]].link == link;
                    same_link && run.trim_end == 0 && trim_start == 0
                });
                let new = || Run {
                    parts: std::iter::once(e..e + 1).collect(),
                    trim_start,
                    trim_end: 0,
                };
                match last {
                    Some(run) => match run.parts.last_mut() {
                        Some(part) if part.end == e => part.end = e + 1,
                        _ if joinable => run.parts.push(e..e + 1),
                        _ => runs.push(new()),
                    },
                    None => runs.push(new()),
                }
            }
            if !p.packets.is_empty()
                && let Some(run) = runs.last_mut()
            {
                run.trim_end = p.trim_end;
            }
        }
        Ok(runs)
    }

    /// Writes one logical stream holding the index entries of `run`.
    fn write_run<R, W>(
        &self,
        r: &mut R,
        run: &Run,
        serial: u32,
        tags: Option<&Tags>,
        w: &mut W,
    ) -> Result<()>
    where
        R: Read + Seek + ?Sized,
        W: Write + Seek + ?Sized,
    {
        // `runs` never creates empty runs or parts.
        let parts = &run.parts;
        let r_start = self.entry_raw[parts[0].start];
        let r_last = self.entry_raw[parts[parts.len() - 1].end - 1];
        let link = &self.links[self.raw[r_start].link];

        // First packet to write, including pre-roll.
        let mut r0 = r_start;
        let mut opus_pre_skip = 0u64;
        // Granule position of the last page if it trims the end of the
        // last packet (RFC 7845 4.4, Vorbis I A.2).
        let mut end_granule = None;
        match link.info.kind {
            // Index entries never start a link (that packet has no output),
            // so the predecessor exists within the link.
            CodecKind::Vorbis => {
                r0 = r_start.saturating_sub(1).max(link.raw.start);
                // The source's own end trim, if the last packet straddles
                // it. The first written packet produces no output, so the
                // output starts at its end.
                let last = &self.raw[r_last];
                let last_end = last.start_in_link + u64::from(last.samples);
                if last_end > link.audible.end {
                    let first = &self.raw[r0];
                    let origin = first.start_in_link + u64::from(first.samples);
                    end_granule = Some(link.audible.end - origin);
                }
            }
            CodecKind::Opus => {
                let mut acc = 0u64;
                while r0 > link.raw.start && acc < OPUS_PREROLL {
                    r0 -= 1;
                    acc += u64::from(self.raw[r0].samples);
                }
                // Wanted part in link coordinates: from the audible start of
                // the first packet (later than its start if the packet
                // straddles the source's pre-skip) plus the plan's trim, to
                // the audible end of the last packet minus the plan's trim.
                // The planner keeps both trims below the packets' audible
                // lengths, so `start < end` and both lie inside packets.
                let first = &self.raw[r_start];
                let last = &self.raw[r_last];
                let start = first.start_in_link.max(link.audible.start) + run.trim_start;
                let end = (last.start_in_link + u64::from(last.samples)).min(link.audible.end)
                    - run.trim_end;
                // Hide the pre-roll and the head of the first packet.
                let origin = self.raw[r0].start_in_link;
                opus_pre_skip = start - origin;
                end_granule = Some(end - origin);
            }
            CodecKind::Speex => {}
        }
        // Raw packets to write, in order.
        let mut packets: Vec<Range<usize>> =
            std::iter::once(r0..self.entry_raw[parts[0].end - 1] + 1).collect();
        for part in &parts[1..] {
            packets.push(self.entry_raw[part.start]..self.entry_raw[part.end - 1] + 1);
        }
        let n_packets: usize = packets.iter().map(ExactSizeIterator::len).sum();

        let mut pg = Paginator::new(serial);
        for (i, h) in link.headers.iter().enumerate() {
            let mut bytes = read_pieces(r, h)?;
            if link.info.kind == CodecKind::Opus && i == 0 {
                // RFC 7845 5.1: pre-skip is a u16 at offset 10.
                let v = u16::try_from(opus_pre_skip).map_err(|_| {
                    ContainerError::TooLarge("Opus pre-skip exceeds 65535 samples".into())
                })?;
                bytes[10..12].copy_from_slice(&v.to_le_bytes());
            }
            if i == 1
                && let Some(tags) = tags
            {
                bytes = CommentHeader::parse(link.info.kind, &bytes)
                    .unwrap_or_else(|| CommentHeader::empty(link.info.kind))
                    .with_tags(tags)
                    .to_bytes(link.info.kind);
            }
            pg.push(&bytes, 0);
            // Every header packet ends its page; Vorbis, Opus and Speex
            // all require the first audio packet to start a new page.
            pg.flush();
        }

        let mut granule = 0u64;
        let raw = packets.into_iter().flat_map(|range| &self.raw[range]);
        for (k, p) in raw.enumerate() {
            let bytes = read_pieces(r, &p.pieces)?;
            // The first Vorbis packet of a stream produces no output.
            if !(link.info.kind == CodecKind::Vorbis && k == 0) {
                granule += u64::from(p.samples);
            }
            // The last packet always ends on the stream's last page, so its
            // granule position is that page's.
            if k + 1 == n_packets
                && let Some(end) = end_granule
            {
                granule = end;
                // ffmpeg ignores the Opus end trim of a stream whose first
                // audio page is also its last; a page of its own for the
                // last packet avoids that for all but one-packet streams.
                // (Vorbis streams break after their second packet below,
                // so the flush is a no-op for two-packet Vorbis streams.)
                if k > 0 {
                    pg.flush();
                }
            }
            pg.push(&bytes, granule as i64);
            // Decoders mistime a Vorbis stream whose first audio page is
            // also its last: ffmpeg then trims the end, libvorbisfile drops
            // the whole stream from a chain. A page break after the second
            // packet avoids that; a two-packet stream (the priming packet
            // plus one) breaks after the first instead.
            let break_after = if n_packets <= 2 { 0 } else { 1 };
            if k == break_after && k + 1 < n_packets && link.info.kind == CodecKind::Vorbis {
                pg.flush();
            }
            if k % 256 == 255 {
                w.write_all(&pg.take_complete())?;
            }
        }
        w.write_all(&pg.finish())?;
        Ok(())
    }
}

/// One output stream: entry ranges of one link (see [`OggFile::runs`]) and
/// the samples to trim at its two ends (Opus only).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Run {
    /// Non-empty, increasing ranges of index entries.
    parts: Vec<Range<usize>>,
    /// Samples to drop from the audible start of the first entry.
    trim_start: u64,
    /// Samples to drop from the audible end of the last entry.
    trim_end: u64,
}

/// Reads a packet from its pieces.
/// A parsed comment header (see the module documentation).
#[derive(Debug, Clone, PartialEq, Eq)]
struct CommentHeader {
    comment: VorbisComment,
    /// Bytes after the comments: Vorbis' framing bit, Opus' extension
    /// data, Speex padding.
    rest: Vec<u8>,
}

impl CommentHeader {
    /// The bytes in front of the comments.
    fn magic(kind: CodecKind) -> &'static [u8] {
        match kind {
            CodecKind::Vorbis => b"\x03vorbis",
            CodecKind::Opus => b"OpusTags",
            CodecKind::Speex => b"",
        }
    }

    /// Parses a comment header packet; `None` if it is not one.
    fn parse(kind: CodecKind, p: &[u8]) -> Option<Self> {
        let body = p.strip_prefix(Self::magic(kind))?;
        let (comment, used) = VorbisComment::parse(body).ok()?;
        Some(CommentHeader {
            comment,
            rest: body[used..].to_vec(),
        })
    }

    /// A header without comments, for a source whose header is damaged.
    fn empty(kind: CodecKind) -> Self {
        CommentHeader {
            comment: VorbisComment {
                vendor: "losslessaudiocut".into(),
                fields: Vec::new(),
            },
            // Vorbis I 5.2.1: the framing bit must be set.
            rest: if kind == CodecKind::Vorbis {
                vec![1]
            } else {
                Vec::new()
            },
        }
    }

    /// The same header with the fields of `tags`.
    fn with_tags(self, tags: &Tags) -> Self {
        CommentHeader {
            comment: self.comment.with_tags(tags),
            rest: self.rest,
        }
    }

    /// The packet.
    fn to_bytes(&self, kind: CodecKind) -> Vec<u8> {
        let mut out = Self::magic(kind).to_vec();
        out.extend(self.comment.to_bytes());
        out.extend_from_slice(&self.rest);
        out
    }
}

/// The identification header, the first of a link's headers.
fn first_header(headers: Vec<Vec<u8>>) -> Vec<u8> {
    headers.into_iter().next().unwrap_or_default()
}

fn read_pieces<R: Read + Seek + ?Sized>(r: &mut R, pieces: &[(u64, u32)]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(pieces.iter().map(|&(_, l)| l as usize).sum());
    for &(off, len) in pieces {
        r.seek(SeekFrom::Start(off))?;
        let at = out.len();
        out.resize(at + len as usize, 0);
        r.read_exact(&mut out[at..]).map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                malformed(off, "packet extends beyond the file")
            } else {
                ContainerError::Io(e)
            }
        })?;
    }
    Ok(out)
}

/// Reads up to `max` bytes of a packet during the scan.
fn gather<R: Read + Seek + ?Sized>(
    w: &mut Window<R>,
    pieces: &[(u64, u32)],
    max: usize,
) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    for &(off, len) in pieces {
        if out.len() >= max {
            break;
        }
        let want = (max - out.len()).min(len as usize);
        out.extend_from_slice(w.bytes(off, want as u64)?);
    }
    Ok(out)
}

fn malformed(offset: u64, reason: &str) -> ContainerError {
    ContainerError::Malformed {
        offset,
        reason: reason.to_owned(),
    }
}

/// State of the link being read.
#[derive(Debug)]
struct Current {
    serial: u32,
    info: CodecInfo,
    headers: Vec<Pieces>,
    /// Expected sequence number of the next page.
    next_seq: u32,
    /// Pieces of a packet continued on the next page.
    pending: Pieces,
    /// Whether `pending` holds the start of a packet (as opposed to
    /// nothing, after a gap).
    have_start: bool,
    /// Vorbis: block size of the previous packet.
    prev: Option<u32>,
    raw_start: usize,
    samples: u64,
    /// The link ended with an EOS page.
    ended: bool,
    /// The granule position of the last page that completed an audio
    /// packet equals the packet-derived sample count up to it (true before
    /// the first such page: header pages have granule 0).
    granules_agree: bool,
    /// Vorbis and Opus: samples to drop at the end, from the final page's
    /// granule position (RFC 7845 4.4, Vorbis I A.2); see the module
    /// documentation.
    end_trim: u64,
}

impl Current {
    /// Updates the granule bookkeeping after a page whose completed audio
    /// packets took the sample count from `before` to `self.samples`.
    fn audio_page(&mut self, granule: i64, before: u64, eos: bool) {
        if self.samples == before || granule == page::NO_GRANULE {
            return;
        }
        let g = u64::try_from(granule).ok();
        if eos
            && self.info.kind != CodecKind::Speex
            && self.granules_agree
            && let Some(g) = g
            // RFC 7845 4.4: the trim may only remove samples of this last
            // page, and the stream may not end inside its pre-skip (which
            // is 0 for Vorbis). Vorbis I A.2 allows the same for Vorbis.
            && g >= before.max(u64::from(self.info.pre_skip))
            && g < self.samples
        {
            self.end_trim = self.samples - g;
        }
        self.granules_agree = g == Some(self.samples);
    }
}

/// Page-by-page reader that assembles packets.
#[derive(Debug, Default)]
struct Scanner {
    links: Vec<Link>,
    raw: Vec<RawPacket>,
    warnings: Vec<String>,
    cur: Option<Current>,
    /// Serial numbers of streams that are ignored.
    ignored: Vec<u32>,
}

impl Scanner {
    fn scan<R: Read + Seek + ?Sized>(&mut self, w: &mut Window<R>) -> Result<()> {
        let mut pos = 0u64;
        while pos < w.len {
            w.keep = pos;
            // A page is at most 27 + 255 + 255 * 255 bytes.
            let parsed = parse_page(w.bytes(pos, 27 + 255 + 255 * 255)?);
            match parsed {
                Ok(h) => {
                    let body = pos + h.header_len() as u64;
                    self.page(w, &h, body)?;
                    pos = body + h.body_len() as u64;
                }
                Err(e) => {
                    let next = next_capture(w, pos + 1)?;
                    if e == PageError::Truncated && next == w.len {
                        self.warnings
                            .push(format!("the last page at byte {pos} is truncated"));
                    } else {
                        self.warnings.push(format!(
                            "skipped {} bytes of damaged data at byte {pos}",
                            next - pos
                        ));
                    }
                    if let Some(c) = &mut self.cur {
                        // A packet in progress cannot be completed.
                        c.pending.clear();
                        c.have_start = false;
                    }
                    pos = next;
                }
            }
        }
        Ok(())
    }

    /// Ends the current link and records it if it is usable.
    fn finish_link(&mut self) {
        if let Some(c) = self.cur.take() {
            if c.headers.len() < c.info.header_packets {
                self.warnings
                    .push(format!("stream {:08x} ends inside its headers", c.serial));
                // Its packets were never added to `raw` (only audio packets
                // are, after all headers).
                return;
            }
            let id = self.links.len();
            for p in &mut self.raw[c.raw_start..] {
                p.link = id;
            }
            let total = c.samples;
            let audible = u64::from(c.info.pre_skip).min(total)..total - c.end_trim;
            self.links.push(Link {
                info: c.info,
                headers: c.headers,
                raw: c.raw_start..self.raw.len(),
                audible,
            });
            if self.links[id].raw.is_empty() {
                // Keep it out of the index; the warning is added by `open`.
                self.links.pop();
                self.warnings
                    .push(format!("stream {:08x} has no audio packets", c.serial));
            }
        }
    }

    fn page<R: Read + Seek + ?Sized>(
        &mut self,
        w: &mut Window<R>,
        h: &page::PageHeader,
        body: u64,
    ) -> Result<()> {
        if h.flags & page::FLAG_BOS != 0 {
            return self.bos_page(w, h, body);
        }
        let page_pos = body - h.header_len() as u64;
        let Some(c) = &mut self.cur else {
            self.unknown_page(h.serial);
            return Ok(());
        };
        if c.serial != h.serial {
            self.unknown_page(h.serial);
            return Ok(());
        }
        if c.ended {
            return Ok(());
        }
        let mut warnings = Vec::new();
        if h.sequence != c.next_seq && !c.pending.is_empty() {
            warnings.push(format!(
                "page sequence gap before byte {page_pos}: a packet was lost"
            ));
            c.pending.clear();
            c.have_start = false;
        }
        c.next_seq = h.sequence.wrapping_add(1);
        if h.flags & page::FLAG_CONTINUED == 0 {
            if !c.pending.is_empty() {
                warnings.push(format!("unfinished packet before byte {page_pos} dropped"));
                c.pending.clear();
            }
            c.have_start = true;
        }
        // Packets completed on this page.
        let mut done = Vec::new();
        let mut off = body;
        for &l in &h.lacing {
            if l > 0 {
                match c.pending.last_mut() {
                    // Segments of one packet on one page are contiguous.
                    Some((o, n)) if *o + u64::from(*n) == off => *n += u32::from(l),
                    _ => c.pending.push((off, u32::from(l))),
                }
            }
            off += u64::from(l);
            if l < 255 {
                let pieces = std::mem::take(&mut c.pending);
                if c.have_start {
                    done.push(pieces);
                }
                // Otherwise this was the tail of a packet whose start was
                // lost; the next packet starts cleanly.
                c.have_start = true;
            }
        }
        let eos = h.flags & page::FLAG_EOS != 0;
        if eos {
            c.ended = true;
        }
        let before = c.samples;
        self.warnings.extend(warnings);
        for pieces in done {
            self.packet(w, pieces)?;
        }
        if let Some(c) = &mut self.cur {
            c.audio_page(h.granule, before, eos);
        }
        Ok(())
    }

    /// A page starting a logical stream.
    fn bos_page<R: Read + Seek + ?Sized>(
        &mut self,
        w: &mut Window<R>,
        h: &page::PageHeader,
        body: u64,
    ) -> Result<()> {
        // RFC 3533: the BOS page holds exactly the identification packet.
        let first_len: u64 = h.lacing.iter().map(|&l| u64::from(l)).sum();
        let id = w.bytes(body, first_len)?.to_vec();
        let kind = CodecKind::detect(&id);
        let Some(kind) = kind else {
            if !self.ignored.contains(&h.serial) {
                self.ignored.push(h.serial);
                self.warnings.push(format!(
                    "ignoring logical stream {:08x} with an unsupported codec",
                    h.serial
                ));
            }
            return Ok(());
        };
        // A further stream starting before the current one carries audio
        // is multiplexed with it (grouping, RFC 3533 4): ignore it. After
        // audio, a new BOS starts a new link of a chain.
        if let Some(c) = &self.cur
            && self.raw.len() == c.raw_start
            && !c.ended
        {
            self.ignored.push(h.serial);
            self.warnings.push(format!(
                "ignoring additional {} stream {:08x}; only the first audio stream is used",
                kind.name(),
                h.serial
            ));
            return Ok(());
        }
        self.finish_link();
        let info = CodecInfo::from_id_header(kind, &id).map_err(|e| {
            ContainerError::Unsupported(format!("{} stream at byte {body}: {e}", kind.name()))
        })?;
        self.ignored.retain(|&s| s != h.serial);
        self.cur = Some(Current {
            serial: h.serial,
            info,
            headers: vec![vec![(body, first_len as u32)]],
            next_seq: h.sequence.wrapping_add(1),
            pending: Vec::new(),
            have_start: true,
            prev: None,
            raw_start: self.raw.len(),
            samples: 0,
            ended: h.flags & page::FLAG_EOS != 0,
            granules_agree: true,
            end_trim: 0,
        });
        Ok(())
    }

    fn unknown_page(&mut self, serial: u32) {
        if !self.ignored.contains(&serial) {
            self.ignored.push(serial);
            self.warnings.push(format!(
                "ignoring pages of stream {serial:08x}, which has no BOS page"
            ));
        }
    }

    /// A complete packet of the current link.
    fn packet<R: Read + Seek + ?Sized>(&mut self, w: &mut Window<R>, pieces: Pieces) -> Result<()> {
        let Some(c) = &mut self.cur else {
            return Ok(());
        };
        let offset = pieces.first().map_or(0, |&(o, _)| o);
        if c.headers.len() < c.info.header_packets {
            let bytes = gather(w, &pieces, usize::MAX)?;
            c.info
                .add_header(c.headers.len(), &bytes)
                .map_err(|e| malformed(offset, &e.to_string()))?;
            c.headers.push(pieces);
            return Ok(());
        }
        // Durations only need the first two bytes (Opus TOC and frame count,
        // Vorbis mode number).
        let head = gather(w, &pieces, 2)?;
        let len: u64 = pieces.iter().map(|&(_, l)| u64::from(l)).sum();
        if len == 0 && c.info.kind == CodecKind::Vorbis {
            // Legal, decodes to nothing and does not change decoder state.
            return Ok(());
        }
        match c.info.packet_samples(&head, &mut c.prev) {
            Ok(samples) => {
                self.raw.push(RawPacket {
                    pieces,
                    samples,
                    start_in_link: c.samples,
                    link: usize::MAX, // set by finish_link
                });
                c.samples += u64::from(samples);
            }
            Err(e) => self
                .warnings
                .push(format!("skipped undecodable packet at byte {offset}: {e}")),
        }
        Ok(())
    }
}

/// The next position at or after `from` holding "OggS", or the file end.
fn next_capture<R: Read + Seek + ?Sized>(w: &mut Window<R>, from: u64) -> io::Result<u64> {
    const STEP: u64 = 1 << 16;
    let mut pos = from;
    while pos < w.len {
        w.keep = pos;
        let chunk = w.bytes(pos, STEP + 3)?;
        if let Some(i) = chunk.windows(4).position(|x| x == b"OggS") {
            return Ok(pos + i as u64);
        }
        if chunk.len() < 4 {
            break;
        }
        pos += chunk.len() as u64 - 3;
    }
    Ok(w.len)
}

#[cfg(test)]
mod tests {
    use super::page::{FLAG_BOS, FLAG_CONTINUED, FLAG_EOS, write_page};
    use super::*;
    use lac_core::plan_cuts;
    use std::io::Cursor;

    /// A minimal Speex header: 16 kHz mono, 320 samples per packet.
    fn speex_header() -> Vec<u8> {
        let mut h = b"Speex   1.2.1".to_vec();
        h.resize(80, 0);
        // speex_header.h offsets: rate 36, channels 48, frame size 56,
        // frames per packet 64, extra headers 68.
        for (at, v) in [
            (28, 1u32),
            (32, 80),
            (36, 16_000),
            (48, 1),
            (56, 320),
            (64, 1),
        ] {
            h[at..at + 4].copy_from_slice(&v.to_le_bytes());
        }
        h
    }

    /// Pages of a Speex stream with `n` audio packets of `len` bytes, each
    /// filled with its packet number.
    fn speex_stream(serial: u32, n: u8, len: usize) -> Vec<u8> {
        let mut p = Paginator::new(serial);
        p.push(&speex_header(), 0);
        p.flush();
        p.push(&speex_comment("Title"), 0);
        p.flush();
        for i in 0..n {
            p.push(&vec![i; len], (i64::from(i) + 1) * 320);
        }
        p.finish()
    }

    /// A Speex comment header with a title.
    fn speex_comment(title: &str) -> Vec<u8> {
        VorbisComment {
            vendor: "test".into(),
            fields: vec![("TITLE".into(), title.into())],
        }
        .to_bytes()
    }

    fn open(bytes: &[u8]) -> OggFile {
        OggFile::open(&mut Cursor::new(bytes)).expect("opens")
    }

    #[test]
    fn speex_tags_are_read_and_rewritten() {
        let bytes = speex_stream(1, 4, 10);
        let f = open(&bytes);
        assert_eq!(f.tags().title.as_deref(), Some("Title"));
        let plans = plan_cuts(
            f.index(),
            f.capabilities(),
            std::slice::from_ref(&(0..f.index().total_samples())),
        )
        .expect("plans");
        let tags = Tags {
            title: Some("New".into()),
            track_number: Some(2),
            ..Tags::default()
        };
        let mut out = Cursor::new(Vec::new());
        f.write_cut(&mut Cursor::new(&bytes), &plans, Some(&tags), &mut out)
            .expect("writes");
        let g = open(out.get_ref());
        assert_eq!(*g.tags(), tags);
        assert_eq!(packets(&g, out.get_ref()), packets(&f, &bytes));
        // The vendor string is kept.
        let comment =
            read_pieces(&mut Cursor::new(out.get_ref()), &g.links[0].headers[1]).expect("header");
        assert_eq!(
            CommentHeader::parse(CodecKind::Speex, &comment)
                .expect("parses")
                .comment
                .vendor,
            "test"
        );
    }

    fn packets(f: &OggFile, bytes: &[u8]) -> Vec<Vec<u8>> {
        (0..f.index().packets().len())
            .map(|e| f.read_packet(&mut Cursor::new(bytes), e).expect("packet"))
            .collect()
    }

    #[test]
    fn packets_spanning_pages_are_reassembled() {
        // 300-byte packets need two lacing values; 20 of them overflow the
        // 4096-byte page target, so some continue on the next page.
        let bytes = speex_stream(1, 20, 300);
        let f = open(&bytes);
        assert!(f.warnings().is_empty(), "{:?}", f.warnings());
        let got = packets(&f, &bytes);
        assert_eq!(got.len(), 20);
        assert!(
            got.iter()
                .enumerate()
                .all(|(i, p)| *p == vec![i as u8; 300])
        );
        assert_eq!(f.index().total_samples(), 20 * 320);
    }

    #[test]
    fn multiplexed_second_stream_is_ignored() {
        // Grouping (RFC 3533 4): both BOS pages come first.
        let a = speex_stream(1, 3, 10);
        let b = speex_stream(2, 3, 10);
        let first_page = |s: &[u8]| {
            let h = parse_page(s).expect("page");
            h.header_len() + h.body_len()
        };
        let (la, lb) = (first_page(&a), first_page(&b));
        let bytes = [&a[..la], &b[..lb], &a[la..], &b[lb..]].concat();
        let f = open(&bytes);
        assert_eq!(f.links(), 1);
        assert_eq!(f.index().packets().len(), 3);
        assert_eq!(f.warnings().len(), 1, "{:?}", f.warnings());
    }

    #[test]
    fn chained_streams_form_links() {
        let bytes = [speex_stream(1, 3, 10), speex_stream(2, 4, 10)].concat();
        let f = open(&bytes);
        assert_eq!(f.links(), 2);
        assert_eq!(f.index().total_samples(), 7 * 320);
    }

    #[test]
    fn sequence_gap_drops_the_broken_packet() {
        let h = speex_header();
        let mut bytes = write_page(FLAG_BOS, 0, 9, 0, &[80], &h);
        let comment = speex_comment("x");
        bytes.extend(write_page(0, 0, 9, 1, &[comment.len() as u8], &comment));
        // Packet 0 complete, packet 1 starts (255 = continues).
        bytes.extend(write_page(
            0,
            320,
            9,
            2,
            &[5, 255],
            &[[0u8; 5].as_slice(), &[1; 255]].concat(),
        ));
        // Page 3 is lost; page 4 continues some packet and then has packet 2.
        bytes.extend(write_page(
            FLAG_CONTINUED | FLAG_EOS,
            960,
            9,
            4,
            &[3, 5],
            &[[1u8; 3].as_slice(), &[2; 5]].concat(),
        ));
        let f = open(&bytes);
        assert_eq!(packets(&f, &bytes), vec![vec![0; 5], vec![2; 5]]);
        assert!(
            f.warnings()[0].contains("sequence gap"),
            "{:?}",
            f.warnings()
        );
    }

    #[test]
    fn pages_of_unknown_streams_are_ignored() {
        let mut bytes = speex_stream(1, 2, 10);
        bytes.extend(write_page(0, 5, 77, 3, &[4], b"junk"));
        let f = open(&bytes);
        assert_eq!(f.index().packets().len(), 2);
        assert_eq!(f.warnings().len(), 1, "{:?}", f.warnings());
    }

    /// An Opus stream of four 20 ms packets (TOC config 31, one frame),
    /// one per page, with pre-skip 312 and the given page granules.
    fn opus_stream(granules: [i64; 4]) -> Vec<u8> {
        let mut head = b"OpusHead".to_vec();
        // RFC 7845 5.1: version 1, 2 channels, pre-skip, 48 kHz input,
        // gain 0, mapping family 0.
        head.extend([1, 2]);
        head.extend(312u16.to_le_bytes());
        head.extend(48_000u32.to_le_bytes());
        head.extend([0, 0, 0]);
        let mut p = Paginator::new(5);
        p.push(&head, 0);
        p.flush();
        p.push(b"OpusTags\0\0\0\0\0\0\0\0", 0);
        p.flush();
        for g in granules {
            p.push(&[31 << 3, 0xAB], g);
            p.flush();
        }
        p.finish()
    }

    #[test]
    fn opus_end_trim_is_honoured_when_granules_agree() {
        let f = open(&opus_stream([960, 1920, 2880, 3740]));
        // 3840 samples minus pre-skip and the 100 samples end trim.
        assert_eq!(f.index().total_samples(), 3840 - 312 - 100);
        assert_eq!(f.index().packets().last().map(|p| p.n_samples), Some(860));
    }

    #[test]
    fn opus_end_trim_after_bad_granules_is_ignored() {
        // Constant, offset and random granules: none agree with the
        // packet-derived timeline, so the final granule is not trusted.
        for granules in [
            [0, 0, 0, 0],
            [5_000, 5_960, 6_920, 6_000],
            [77, 123_456, 9, 3_740],
        ] {
            let f = open(&opus_stream(granules));
            assert_eq!(f.index().total_samples(), 3840 - 312, "{granules:?}");
        }
        // A final granule beyond the last page's samples is no trim either.
        let f = open(&opus_stream([960, 1920, 2880, 2000]));
        assert_eq!(f.index().total_samples(), 3840 - 312);
    }

    #[test]
    fn speex_parts_join_in_one_stream() {
        let bytes = speex_stream(1, 10, 10);
        let f = open(&bytes);
        let plans: Vec<_> = [0..2, 5..7]
            .into_iter()
            .map(|r| CutPlan {
                actual: r.start as u64 * 320..r.end as u64 * 320,
                packets: r,
                trim_start: 0,
                trim_end: 0,
                padding_before: 0,
                padding_after: 0,
            })
            .collect();
        let mut out = Cursor::new(Vec::new());
        f.write_cut(&mut Cursor::new(&bytes), &plans, None, &mut out)
            .expect("write");
        let out = out.into_inner();
        let o = open(&out);
        assert_eq!(o.links(), 1);
        assert_eq!(
            packets(&o, &out),
            [0u8, 1, 5, 6].map(|i| vec![i; 10]).to_vec()
        );
    }
}
