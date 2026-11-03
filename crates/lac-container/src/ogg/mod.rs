//! Ogg files with Vorbis, Opus or Speex audio (RFC 3533, RFC 7845).
//!
//! ## Timeline without granule positions
//!
//! Every Ogg page carries a granule position, which for audio is the
//! number of samples decoded up to the last packet finishing on the page.
//! Many recordings, radio captures in particular, store random or
//! inconsistent values there. The demuxer therefore ignores granule
//! positions completely and computes each packet's duration from the
//! packet itself (see [`codec`]). The consequence is that a source's
//! end trim (a final granule smaller than the decoded length) is not
//! applied; the index may be up to one packet longer than what a player
//! that trusts granules outputs.
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
//! - Opus: packets entirely inside the link's pre-skip have no entry; the
//!   one straddling it gets an entry for its audible part only.
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
//!   bit-exact; Vorbis keeps no decoder state beyond the overlap.
//! - Opus: at least 80 ms (3840 samples) of preceding packets are written
//!   as pre-roll, hidden by the stream's pre-skip (RFC 7845 4.3 recommends
//!   80 ms for the decoder to converge). The output matches the source
//!   except within a short convergence window at every cut point.
//! - Speex: kept packets only, and all kept parts of a link go into one
//!   stream, because ffmpeg and speexdec mishandle chained Speex and
//!   chaining would not help: Speex has no pre-skip to hide pre-roll. The
//!   decoder converges after a join point, at the latest in the next pause.

pub mod codec;
pub mod page;

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;

use lac_core::{AudioStreamInfo, CutCapabilities, CutPlan, PacketFlags, PacketIndex, PacketInfo};

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
            let audible = match link.info.kind {
                CodecKind::Opus => {
                    let skip = u64::from(link.info.pre_skip);
                    let end = p.start_in_link + u64::from(p.samples);
                    end.saturating_sub(skip.max(p.start_in_link))
                }
                CodecKind::Vorbis | CodecKind::Speex => u64::from(p.samples),
            };
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
        Ok(Self {
            links,
            raw,
            entry_raw,
            index,
            warnings,
        })
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

    /// Packet-granular cuts without trimming; Vorbis and Opus pre-roll is
    /// handled inside [`OggFile::write_cut`] and hidden from the listener.
    pub fn capabilities(&self) -> CutCapabilities {
        CutCapabilities {
            preroll_packets: 0,
            can_trim_start: false,
            can_trim_end: false,
            lossless_reencode: false,
            trims_at_file_ends_only: false,
        }
    }

    /// Writes the kept packets of `plans` as a chained Ogg file.
    pub fn write_cut<R, W>(&self, r: &mut R, plans: &[CutPlan], w: &mut W) -> Result<()>
    where
        R: Read + Seek + ?Sized,
        W: Write + Seek + ?Sized,
    {
        let runs = self.runs(plans)?;
        for (i, run) in runs.iter().enumerate() {
            let serial = SERIAL_BASE.wrapping_add(i as u32);
            self.write_run(r, run, serial, w)?;
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
    fn runs(&self, plans: &[CutPlan]) -> Result<Vec<Vec<Range<usize>>>> {
        let n = self.entry_raw.len();
        let mut runs: Vec<Vec<Range<usize>>> = Vec::new();
        for p in plans {
            if p.trim_start != 0 || p.trim_end != 0 {
                return Err(ContainerError::InvalidPlan(
                    "Ogg cuts cannot trim samples".into(),
                ));
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
                let last = runs.last_mut().and_then(|run| {
                    let same_link = self.raw[self.entry_raw[run[0].start]].link == link;
                    same_link.then_some(run)
                });
                match last {
                    Some(run) => match run.last_mut() {
                        Some(part) if part.end == e => part.end = e + 1,
                        _ if joinable => run.push(e..e + 1),
                        _ => runs.push(std::iter::once(e..e + 1).collect()),
                    },
                    None => runs.push(std::iter::once(e..e + 1).collect()),
                }
            }
        }
        Ok(runs)
    }

    /// Writes one logical stream holding the index entries of `run`.
    fn write_run<R, W>(&self, r: &mut R, run: &[Range<usize>], serial: u32, w: &mut W) -> Result<()>
    where
        R: Read + Seek + ?Sized,
        W: Write + Seek + ?Sized,
    {
        // `runs` never creates empty runs or parts.
        let r_start = self.entry_raw[run[0].start];
        let link = &self.links[self.raw[r_start].link];

        // First packet to write, including pre-roll.
        let mut r0 = r_start;
        let mut opus_pre_skip = 0u64;
        match link.info.kind {
            // Index entries never start a link (that packet has no output),
            // so the predecessor exists within the link.
            CodecKind::Vorbis => r0 = r_start.saturating_sub(1).max(link.raw.start),
            CodecKind::Opus => {
                let mut acc = 0u64;
                while r0 > link.raw.start && acc < OPUS_PREROLL {
                    r0 -= 1;
                    acc += u64::from(self.raw[r0].samples);
                }
                // Hide the pre-roll, plus whatever part of the first packet
                // was inside the source's own pre-skip.
                let s = self.raw[r_start].start_in_link;
                opus_pre_skip = s - self.raw[r0].start_in_link
                    + u64::from(link.info.pre_skip).saturating_sub(s);
            }
            CodecKind::Speex => {}
        }
        // Raw packets to write, in order.
        let mut packets: Vec<Range<usize>> =
            std::iter::once(r0..self.entry_raw[run[0].end - 1] + 1).collect();
        for part in &run[1..] {
            packets.push(self.entry_raw[part.start]..self.entry_raw[part.end - 1] + 1);
        }

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
            pg.push(&bytes, granule as i64);
            if k == 1 && link.info.kind == CodecKind::Vorbis {
                // ffmpeg mistimes a Vorbis stream whose first audio page is
                // also its last (it then trims the end); a page break after
                // the second packet avoids that for all but one-packet runs.
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

/// Reads a packet from its pieces.
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
            self.links.push(Link {
                info: c.info,
                headers: c.headers,
                raw: c.raw_start..self.raw.len(),
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
        if h.flags & page::FLAG_EOS != 0 {
            c.ended = true;
        }
        self.warnings.extend(warnings);
        for pieces in done {
            self.packet(w, pieces)?;
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
        p.push(b"comment", 0);
        p.flush();
        for i in 0..n {
            p.push(&vec![i; len], (i64::from(i) + 1) * 320);
        }
        p.finish()
    }

    fn open(bytes: &[u8]) -> OggFile {
        OggFile::open(&mut Cursor::new(bytes)).expect("opens")
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
        bytes.extend(write_page(0, 0, 9, 1, &[7], b"comment"));
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
        f.write_cut(&mut Cursor::new(&bytes), &plans, &mut out)
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
