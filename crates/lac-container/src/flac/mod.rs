//! Native FLAC files (RFC 9639).
//!
//! ## Layout
//!
//! ```text
//! [ID3v2 tag]                       optional, written by some taggers
//! "fLaC"
//! metadata blocks                   STREAMINFO first, then SEEKTABLE,
//!                                   VORBIS_COMMENT, PICTURE, PADDING, ...
//! frames                            header + subframes + CRC-16
//! ```
//!
//! ## Finding frames without decoding
//!
//! Frame headers carry no length. A frame ends where the next valid frame
//! header starts *and* the two bytes before it are the CRC-16 of the frame.
//! The scanner therefore runs a CRC-16 over each frame and only accepts a
//! sync code as the next frame if the CRC matches. False syncs inside audio
//! data are rejected with probability 1 - 2^-16 by the CRC and further by the
//! header's own CRC-8. Damaged frames are skipped and reported as warnings;
//! the timeline is the sum of the block sizes of the good frames.
//!
//! ## Cutting
//!
//! FLAC is lossless, so cuts are sample-exact and bit-exact:
//!
//! - Frames entirely inside a kept range are copied verbatim; only their
//!   header is rewritten (see below).
//! - The partial frames at the cut points are decoded and re-encoded with
//!   [`encode::encode_frame`]. A re-encoded piece shorter than 16
//!   samples (the minimum block size) is merged with the following frame.
//! - The output is always a *variable block size* stream whose frame headers
//!   carry the sample number, because the new boundary frames have other
//!   sizes than the rest. Only the header bytes change; the CRC-16 is
//!   recomputed over the new header and the unchanged subframe data.
//! - STREAMINFO gets the new total sample count, block and frame size
//!   limits, and the MD5 of the decoded output (computed by decoding every
//!   frame while writing, which also verifies each copied frame).
//! - SEEKTABLE and CUESHEET are dropped because their offsets would be wrong;
//!   VORBIS_COMMENT, PICTURE, APPLICATION and PADDING are copied unchanged.
//!   An ID3v2 tag in front of the stream is dropped with a warning.

mod crc;
pub mod decode;
pub mod encode;
pub mod frame;

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;

use lac_core::{AudioStreamInfo, CutCapabilities, CutPlan, PacketFlags, PacketIndex, PacketInfo};
use md5::{Digest, Md5};

use self::decode::{Channels, decode_frame};
use self::encode::encode_frame;
use self::frame::FrameHeader;
use crate::window::Window;
use crate::{ContainerError, Result};

/// Minimum block size allowed by RFC 9639 (except for the last frame).
const MIN_BLOCK: usize = 16;
/// Frame length used when re-encoded material is split into frames.
const ENCODE_BLOCK: usize = 4096;

const BLOCK_STREAMINFO: u8 = 0;
const BLOCK_PADDING: u8 = 1;
const BLOCK_APPLICATION: u8 = 2;
const BLOCK_VORBIS_COMMENT: u8 = 4;
const BLOCK_PICTURE: u8 = 6;

/// The STREAMINFO metadata block (RFC 9639 8.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    /// Minimum block size in samples (excluding the last frame).
    pub min_block: u16,
    /// Maximum block size in samples.
    pub max_block: u16,
    /// Minimum frame size in bytes, 0 if unknown.
    pub min_frame: u32,
    /// Maximum frame size in bytes, 0 if unknown.
    pub max_frame: u32,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Number of channels (1..=8).
    pub channels: u8,
    /// Bits per sample (4..=32).
    pub bits_per_sample: u32,
    /// Total samples per channel, 0 if unknown.
    pub total_samples: u64,
    /// MD5 of the decoded audio, all zero if unknown.
    pub md5: [u8; 16],
}

impl StreamInfo {
    fn parse(b: &[u8], offset: u64) -> Result<Self> {
        if b.len() != 34 {
            return Err(malformed(offset, "STREAMINFO is not 34 bytes long"));
        }
        let be = |r: Range<usize>| b[r].iter().fold(0u64, |a, &x| (a << 8) | u64::from(x));
        // Bytes 10..18: sample rate (20 bits), channels - 1 (3), bps - 1 (5),
        // total samples (36).
        let packed = be(10..18);
        let si = Self {
            min_block: be(0..2) as u16,
            max_block: be(2..4) as u16,
            min_frame: be(4..7) as u32,
            max_frame: be(7..10) as u32,
            sample_rate: (packed >> 44) as u32,
            channels: ((packed >> 41) & 0x7) as u8 + 1,
            bits_per_sample: ((packed >> 36) & 0x1F) as u32 + 1,
            total_samples: packed & ((1 << 36) - 1),
            md5: b[18..34].try_into().unwrap_or([0; 16]),
        };
        if si.sample_rate == 0 {
            return Err(malformed(offset, "sample rate 0"));
        }
        if si.bits_per_sample < 4 {
            return Err(malformed(offset, "fewer than 4 bits per sample"));
        }
        Ok(si)
    }

    fn to_bytes(&self) -> [u8; 34] {
        let mut b = [0u8; 34];
        b[0..2].copy_from_slice(&self.min_block.to_be_bytes());
        b[2..4].copy_from_slice(&self.max_block.to_be_bytes());
        b[4..7].copy_from_slice(&self.min_frame.to_be_bytes()[1..]);
        b[7..10].copy_from_slice(&self.max_frame.to_be_bytes()[1..]);
        let packed = (u64::from(self.sample_rate) << 44)
            | (u64::from(self.channels - 1) << 41)
            | (u64::from(self.bits_per_sample - 1) << 36)
            | (self.total_samples & ((1 << 36) - 1));
        b[10..18].copy_from_slice(&packed.to_be_bytes());
        b[18..34].copy_from_slice(&self.md5);
        b
    }
}

/// An opened FLAC file.
#[derive(Debug, Clone)]
pub struct FlacFile {
    stream_info: StreamInfo,
    /// Metadata blocks copied to the output: (type, payload).
    kept_blocks: Vec<(u8, Vec<u8>)>,
    /// Parsed header of every frame, parallel to the packet index.
    headers: Vec<FrameHeader>,
    index: PacketIndex,
    warnings: Vec<String>,
}

impl FlacFile {
    /// Parses metadata and locates all frames. Reads the whole file once,
    /// but decodes nothing.
    pub fn open<R: Read + Seek + ?Sized>(r: &mut R) -> Result<Self> {
        let len = r.seek(SeekFrom::End(0))?;
        let mut warnings = Vec::new();
        let mut pos = skip_id3v2(r, len)?;
        if pos > 0 {
            warnings.push("an ID3v2 tag in front of the FLAC stream will not be copied".into());
        }

        let mut magic = [0u8; 4];
        read_exact_at(r, &mut magic, pos)?;
        if &magic != b"fLaC" {
            return Err(malformed(pos, "not a FLAC stream"));
        }
        pos += 4;

        let mut stream_info = None;
        let mut kept_blocks = Vec::new();
        loop {
            let mut h = [0u8; 4];
            read_exact_at(r, &mut h, pos)?;
            let last = h[0] & 0x80 != 0;
            let kind = h[0] & 0x7F;
            let size = u64::from(u32::from_be_bytes([0, h[1], h[2], h[3]]));
            if pos + 4 + size > len {
                return Err(malformed(pos, "metadata block extends beyond the file"));
            }
            let mut payload = vec![0u8; size as usize];
            read_exact_at(r, &mut payload, pos + 4)?;
            match kind {
                BLOCK_STREAMINFO if stream_info.is_none() => {
                    stream_info = Some(StreamInfo::parse(&payload, pos)?);
                }
                BLOCK_PADDING | BLOCK_APPLICATION | BLOCK_VORBIS_COMMENT | BLOCK_PICTURE => {
                    kept_blocks.push((kind, payload));
                }
                127 => return Err(malformed(pos, "forbidden metadata block type 127")),
                _ => {} // SEEKTABLE, CUESHEET, reserved: dropped
            }
            pos += 4 + size;
            if last {
                break;
            }
        }
        let si = stream_info.ok_or_else(|| malformed(8, "no STREAMINFO block"))?;

        let (headers, packets) = scan_frames(r, pos, len, &si, &mut warnings)?;
        let index = PacketIndex::new(
            AudioStreamInfo {
                sample_rate: si.sample_rate,
                channels: u16::from(si.channels),
            },
            packets,
        )
        .map_err(|e| ContainerError::InvalidPlan(format!("internal index error: {e}")))?;
        if si.total_samples != 0 && si.total_samples != index.total_samples() {
            warnings.push(format!(
                "STREAMINFO announces {} samples, the frames contain {}",
                si.total_samples,
                index.total_samples()
            ));
        }
        Ok(Self {
            stream_info: si,
            kept_blocks,
            headers,
            index,
            warnings,
        })
    }

    /// The source's STREAMINFO.
    pub fn stream_info(&self) -> &StreamInfo {
        &self.stream_info
    }

    /// One entry per frame.
    pub fn index(&self) -> &PacketIndex {
        &self.index
    }

    /// Problems that were tolerated while opening the file.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// FLAC cuts are sample-exact (boundary frames are re-encoded losslessly).
    pub fn capabilities(&self) -> CutCapabilities {
        CutCapabilities::PCM
    }

    /// Decodes the sample frames in `range`, one vector per channel.
    ///
    /// Used for tests and for previews; normal playback goes through
    /// `lac-decode`.
    pub fn decode_range<R: Read + Seek + ?Sized>(
        &self,
        r: &mut R,
        range: Range<u64>,
    ) -> Result<Channels> {
        let mut out: Channels = vec![Vec::new(); usize::from(self.stream_info.channels)];
        if range.is_empty() {
            return Ok(out);
        }
        let first = self.index.packet_containing(range.start);
        let last = self.index.packet_containing(range.end - 1);
        let (Some(first), Some(last)) = (first, last) else {
            return Err(ContainerError::InvalidPlan(
                "range outside the stream".into(),
            ));
        };
        for i in first..=last {
            let chans = self.decode_packet(r, i)?;
            let p = &self.index.packets()[i];
            let part = clip(p.sample_range(), &range, p.first_sample);
            for (o, c) in out.iter_mut().zip(chans) {
                o.extend_from_slice(&c[part.clone()]);
            }
        }
        Ok(out)
    }

    /// Writes a new FLAC file with the `actual` ranges of `plans` joined.
    ///
    /// The writer must be seekable because STREAMINFO (with the MD5 of the
    /// output) is only known after all frames have been written.
    pub fn write_cut<R, W>(&self, r: &mut R, plans: &[CutPlan], w: &mut W) -> Result<()>
    where
        R: Read + Seek + ?Sized,
        W: Write + Seek + ?Sized,
    {
        let pieces = self.plan_pieces(r, plans)?;
        let si = &self.stream_info;
        let bps = si.bits_per_sample;

        // Header: "fLaC", placeholder STREAMINFO, kept blocks.
        let start = w.stream_position()?;
        w.write_all(b"fLaC")?;
        let streaminfo_pos = start + 8;
        let n_blocks = 1 + self.kept_blocks.len();
        write_block_header(w, BLOCK_STREAMINFO, n_blocks == 1, 34)?;
        w.write_all(&[0u8; 34])?;
        for (i, (kind, payload)) in self.kept_blocks.iter().enumerate() {
            write_block_header(w, *kind, i + 2 == n_blocks, payload.len())?;
            w.write_all(payload)?;
        }

        let mut stats = OutputStats::new(bps);
        let mut sample: u64 = 0;
        let n_pieces = pieces.len();
        for (i, piece) in pieces.into_iter().enumerate() {
            let is_last = i + 1 == n_pieces;
            match piece {
                Piece::Copy(fi) => {
                    let frame = self.read_frame(r, fi)?;
                    let h = &self.headers[fi];
                    let chans = decode_frame(&frame, h, bps).map_err(|e| {
                        malformed(self.index.packets()[fi].byte_offset, &e.to_string())
                    })?;
                    let mut out = h.to_variable_bytes(sample);
                    out.extend_from_slice(&frame[h.len..frame.len() - 2]);
                    out.extend(crc::crc16(&out).to_be_bytes());
                    stats.add(&chans, out.len(), is_last);
                    w.write_all(&out)?;
                    sample += u64::from(h.block_size);
                }
                Piece::Pcm(chans) => {
                    let frames = split_pcm(chans);
                    let n_frames = frames.len();
                    for (j, c) in frames.into_iter().enumerate() {
                        let out = encode_frame(&c, sample, bps, &self.headers[0]);
                        stats.add(&c, out.len(), is_last && j + 1 == n_frames);
                        w.write_all(&out)?;
                        sample += c[0].len() as u64;
                    }
                }
            }
        }

        let final_info = stats.finish(si, sample);
        let end = w.stream_position()?;
        w.seek(SeekFrom::Start(streaminfo_pos))?;
        w.write_all(&final_info.to_bytes())?;
        w.seek(SeekFrom::Start(end))?;
        Ok(())
    }

    /// Turns cut plans into a list of copied frames and re-encoded pieces.
    fn plan_pieces<R: Read + Seek + ?Sized>(
        &self,
        r: &mut R,
        plans: &[CutPlan],
    ) -> Result<Vec<Piece>> {
        let total = self.index.total_samples();
        let mut pieces: Vec<Piece> = Vec::new();
        for plan in plans {
            let a = &plan.actual;
            if a.start > a.end || a.end > total || plan.packets.end > self.index.packets().len() {
                return Err(ContainerError::InvalidPlan(format!(
                    "frames {}..{} outside 0..{total}",
                    a.start, a.end
                )));
            }
            for fi in plan.packets.clone() {
                let p = &self.index.packets()[fi];
                let fr = p.sample_range();
                if fr.end <= a.start || fr.start >= a.end {
                    continue; // pre-roll or not overlapping
                }
                if a.start <= fr.start && fr.end <= a.end {
                    pieces.push(Piece::Copy(fi));
                } else {
                    let chans = self.decode_packet(r, fi)?;
                    let part = clip(fr, a, p.first_sample);
                    let cut: Channels = chans
                        .into_iter()
                        .map(|c| c[part.clone()].to_vec())
                        .collect();
                    push_pcm(&mut pieces, cut);
                }
            }
        }
        // Too-short re-encoded pieces (only allowed as the very last frame)
        // absorb the piece that follows them.
        let mut i = 0;
        while i + 1 < pieces.len() {
            let short = matches!(&pieces[i], Piece::Pcm(c) if c[0].len() < MIN_BLOCK);
            if !short {
                i += 1;
                continue;
            }
            let next = match pieces.remove(i + 1) {
                Piece::Copy(fi) => self.decode_packet(r, fi)?,
                Piece::Pcm(c) => c,
            };
            if let Piece::Pcm(c) = &mut pieces[i] {
                for (dst, src) in c.iter_mut().zip(next) {
                    dst.extend(src);
                }
            }
        }
        Ok(pieces)
    }

    fn read_frame<R: Read + Seek + ?Sized>(&self, r: &mut R, i: usize) -> Result<Vec<u8>> {
        let p = &self.index.packets()[i];
        let mut buf = vec![0u8; p.byte_len as usize];
        read_exact_at(r, &mut buf, p.byte_offset)?;
        Ok(buf)
    }

    fn decode_packet<R: Read + Seek + ?Sized>(&self, r: &mut R, i: usize) -> Result<Channels> {
        let frame = self.read_frame(r, i)?;
        decode_frame(&frame, &self.headers[i], self.stream_info.bits_per_sample)
            .map_err(|e| malformed(self.index.packets()[i].byte_offset, &e.to_string()))
    }
}

/// One element of the output stream.
enum Piece {
    /// A source frame copied verbatim (header rewritten).
    Copy(usize),
    /// Decoded samples to be re-encoded, one vector per channel.
    Pcm(Channels),
}

/// Appends `c`, merging it into a directly preceding PCM piece.
fn push_pcm(pieces: &mut Vec<Piece>, c: Channels) {
    if let Some(Piece::Pcm(prev)) = pieces.last_mut() {
        for (dst, src) in prev.iter_mut().zip(c) {
            dst.extend(src);
        }
    } else {
        pieces.push(Piece::Pcm(c));
    }
}

/// Splits re-encoded material into frames of [`ENCODE_BLOCK`] samples,
/// avoiding a last frame shorter than [`MIN_BLOCK`] when possible.
fn split_pcm(c: Channels) -> Vec<Channels> {
    let n = c[0].len();
    let mut bounds = Vec::new();
    let mut s = 0;
    while s < n {
        let mut e = (s + ENCODE_BLOCK).min(n);
        let rest = n - e;
        if rest > 0 && rest < MIN_BLOCK {
            // Split the remainder evenly over the last two frames.
            e = s + (n - s) / 2;
        }
        bounds.push(s..e);
        s = e;
    }
    bounds
        .into_iter()
        .map(|b| c.iter().map(|ch| ch[b.clone()].to_vec()).collect())
        .collect()
}

/// Indexes into a frame covering `frame` for the part inside `wanted`.
fn clip(frame: Range<u64>, wanted: &Range<u64>, frame_start: u64) -> Range<usize> {
    let s = frame.start.max(wanted.start) - frame_start;
    let e = frame.end.min(wanted.end) - frame_start;
    s as usize..e as usize
}

/// Accumulates STREAMINFO values while frames are written.
struct OutputStats {
    md5: Md5,
    bytes_per_sample: usize,
    min_block: usize,
    max_block: usize,
    min_frame: usize,
    max_frame: usize,
}

impl OutputStats {
    fn new(bps: u32) -> Self {
        Self {
            md5: Md5::new(),
            bytes_per_sample: bps.div_ceil(8) as usize,
            min_block: usize::MAX,
            max_block: 0,
            min_frame: usize::MAX,
            max_frame: 0,
        }
    }

    fn add(&mut self, chans: &Channels, frame_len: usize, is_last: bool) {
        let n = chans[0].len();
        // The MD5 covers interleaved little-endian samples, each stored in
        // the smallest whole number of bytes (RFC 9639 8.2).
        let mut buf = Vec::with_capacity(n * chans.len() * self.bytes_per_sample);
        for i in 0..n {
            for c in chans {
                buf.extend_from_slice(&c[i].to_le_bytes()[..self.bytes_per_sample]);
            }
        }
        self.md5.update(&buf);
        if !is_last {
            self.min_block = self.min_block.min(n);
        }
        self.max_block = self.max_block.max(n);
        self.min_frame = self.min_frame.min(frame_len);
        self.max_frame = self.max_frame.max(frame_len);
    }

    fn finish(self, source: &StreamInfo, total: u64) -> StreamInfo {
        let max_block = self.max_block.max(MIN_BLOCK);
        let min_block = if self.min_block == usize::MAX {
            max_block
        } else {
            self.min_block.max(MIN_BLOCK)
        };
        StreamInfo {
            min_block: min_block.min(65_535) as u16,
            max_block: max_block.min(65_535) as u16,
            min_frame: if self.max_frame == 0 {
                0
            } else {
                self.min_frame as u32
            },
            max_frame: self.max_frame as u32,
            total_samples: if total < 1 << 36 { total } else { 0 },
            md5: self.md5.finalize().into(),
            ..source.clone()
        }
    }
}

fn write_block_header<W: Write + ?Sized>(
    w: &mut W,
    kind: u8,
    last: bool,
    len: usize,
) -> Result<()> {
    let len = u32::try_from(len)
        .ok()
        .filter(|&l| l < 1 << 24)
        .ok_or_else(|| ContainerError::TooLarge("metadata block above 16 MiB".into()))?;
    let b = len.to_be_bytes();
    w.write_all(&[kind | if last { 0x80 } else { 0 }, b[1], b[2], b[3]])?;
    Ok(())
}

/// Returns the offset after an ID3v2 tag at the start of the file, or 0.
pub(crate) fn skip_id3v2<R: Read + Seek + ?Sized>(r: &mut R, len: u64) -> Result<u64> {
    if len < 10 {
        return Ok(0);
    }
    let mut h = [0u8; 10];
    read_exact_at(r, &mut h, 0)?;
    if &h[0..3] != b"ID3" {
        return Ok(0);
    }
    // Size is a 28-bit "syncsafe" integer (7 bits per byte); a footer
    // (flag 0x10) adds another 10 bytes.
    let size = h[6..10]
        .iter()
        .fold(0u64, |a, &b| (a << 7) | u64::from(b & 0x7F));
    let footer = if h[5] & 0x10 != 0 { 10 } else { 0 };
    Ok(10 + size + footer)
}

/// Finds all frames from `start` to `len`. See the module documentation.
fn scan_frames<R: Read + Seek + ?Sized>(
    r: &mut R,
    start: u64,
    len: u64,
    si: &StreamInfo,
    warnings: &mut Vec<String>,
) -> Result<(Vec<FrameHeader>, Vec<PacketInfo>)> {
    let mut w = Window::new(r, len);
    // Upper bound on a frame's length; generous because STREAMINFO values
    // are sometimes wrong.
    let max_frame = if si.max_frame > 0 {
        u64::from(si.max_frame) * 2 + 1024
    } else {
        16 << 20
    };
    let mut headers = Vec::new();
    let mut packets = Vec::new();
    let mut sample = 0u64;
    let mut pos = start;
    while pos < len {
        w.keep = pos;
        let found = match header_at(&mut w, pos, si)? {
            Some(h) => frame_end(&mut w, pos, &h, si, max_frame)?.map(|end| (h, end)),
            None => None,
        };
        match found {
            Some((h, end)) => {
                packets.push(PacketInfo {
                    byte_offset: pos,
                    byte_len: u32::try_from(end - pos).unwrap_or(u32::MAX),
                    first_sample: sample,
                    n_samples: h.block_size,
                    flags: PacketFlags { independent: true },
                });
                sample += u64::from(h.block_size);
                headers.push(h);
                pos = end;
            }
            None => {
                let next = next_sync(&mut w, pos + 1, si)?;
                // A trailing ID3v1 tag is expected, not damage.
                let is_id3v1 = next == len && len - pos == 128 && w.bytes(pos, 3)? == b"TAG";
                if !is_id3v1 {
                    warnings.push(format!(
                        "skipped {} bytes of undecodable data at byte {pos}",
                        next - pos
                    ));
                }
                pos = next;
            }
        }
    }
    Ok((headers, packets))
}

/// Parses a frame header at `pos` that is consistent with STREAMINFO.
fn header_at<R: Read + Seek + ?Sized>(
    w: &mut Window<R>,
    pos: u64,
    si: &StreamInfo,
) -> Result<Option<FrameHeader>> {
    let Ok(h) = FrameHeader::parse(w.bytes(pos, 16)?) else {
        return Ok(None);
    };
    let consistent = h.sample_rate.is_none_or(|r| r == si.sample_rate)
        && h.bits_per_sample.is_none_or(|b| b == si.bits_per_sample)
        && h.channels.channels() == si.channels
        && (si.max_block == 0 || h.block_size <= u32::from(si.max_block));
    Ok(consistent.then_some(h))
}

/// Finds the end of the frame starting at `pos`: the first position where
/// the CRC-16 over the frame matches and the file ends or a new frame starts.
fn frame_end<R: Read + Seek + ?Sized>(
    w: &mut Window<R>,
    pos: u64,
    h: &FrameHeader,
    si: &StreamInfo,
    max_frame: u64,
) -> Result<Option<u64>> {
    let limit = (pos + max_frame).min(w.len);
    let mut crc = 0u16;
    // Bytes [pos, crc_pos) are already folded into `crc`.
    let mut crc_pos = pos;
    // Smallest possible frame: header, one subframe header byte, CRC-16.
    let mut q = pos + h.len as u64 + 3;
    while q <= limit {
        let at_end = q == w.len;
        let at_sync =
            !at_end && w.byte(q)? == Some(0xFF) && w.byte(q + 1)?.is_some_and(|b| b & 0xFE == 0xF8);
        let at_tag = q + 128 == w.len && w.bytes(q, 3)? == b"TAG";
        if at_end || at_sync || at_tag {
            while crc_pos < q - 2 {
                crc = crc::crc16_update(crc, w.byte(crc_pos)?.unwrap_or(0));
                crc_pos += 1;
            }
            let crc_matches = w.bytes(q - 2, 2)? == crc.to_be_bytes();
            if crc_matches && (!at_sync || header_at(w, q, si)?.is_some()) {
                return Ok(Some(q));
            }
        }
        q += 1;
    }
    Ok(None)
}

/// Position of the next plausible frame header at or after `from`, or the
/// end of the file.
fn next_sync<R: Read + Seek + ?Sized>(
    w: &mut Window<R>,
    from: u64,
    si: &StreamInfo,
) -> Result<u64> {
    let mut p = from;
    while p < w.len {
        w.keep = p;
        if w.byte(p)? == Some(0xFF) && header_at(w, p, si)?.is_some() {
            return Ok(p);
        }
        p += 1;
    }
    Ok(w.len)
}

fn read_exact_at<R: Read + Seek + ?Sized>(r: &mut R, buf: &mut [u8], pos: u64) -> Result<()> {
    r.seek(SeekFrom::Start(pos))?;
    r.read_exact(buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            malformed(pos, "unexpected end of file")
        } else {
            ContainerError::Io(e)
        }
    })
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

    #[test]
    fn split_avoids_tiny_last_frame() {
        let c: Channels = vec![vec![0; ENCODE_BLOCK + 5]];
        let sizes: Vec<usize> = split_pcm(c).iter().map(|f| f[0].len()).collect();
        assert_eq!(sizes.iter().sum::<usize>(), ENCODE_BLOCK + 5);
        assert!(sizes.iter().all(|&s| s >= MIN_BLOCK));
    }

    #[test]
    fn streaminfo_round_trip() {
        let si = StreamInfo {
            min_block: 16,
            max_block: 4608,
            min_frame: 14,
            max_frame: 0xABCDEF,
            sample_rate: 192_000,
            channels: 8,
            bits_per_sample: 24,
            total_samples: (1 << 36) - 1,
            md5: [7; 16],
        };
        assert_eq!(StreamInfo::parse(&si.to_bytes(), 0).expect("valid"), si);
    }
}
