//! AIFF and AIFF-C files with uncompressed audio.
//!
//! References: Apple "Audio Interchange File Format: AIFF, Version 1.3"
//! (1989) and "AIFF-C, Audio Interchange File Format Extension for
//! Compression" (draft of 1991-08-26), both based on EA IFF 85.
//!
//! ## Layout
//!
//! All numbers are big-endian; chunks are padded to even length.
//!
//! ```text
//! "FORM" <u32 size> "AIFF" | "AIFC"
//!   "FVER" <u32 size> <u32 version>                    (AIFF-C only)
//!   "COMM" <u32 size> <i16 channels> <u32 frames> <i16 sample size>
//!                     <80-bit extended sample rate>
//!                     [<4cc compression> <pstring name>] (AIFF-C only)
//!   ...other chunks (NAME, AUTH, ANNO, "(c) ", ID3, MARK, INST, ...)...
//!   "SSND" <u32 size> <u32 offset> <u32 block size> <offset bytes>
//!                     <sample frames>
//! ```
//!
//! ## Cutting
//!
//! Every sample frame is self-contained, so cuts are sample-exact and
//! bit-exact, exactly as for [`crate::wav`]. Plain AIFF stores signed,
//! big-endian integer PCM; the supported AIFF-C compression types are
//! the uncompressed ones: `NONE` and `twos` (as AIFF), `sowt`
//! (little-endian integers), `fl32`/`fl64` (big-endian IEEE floats), and
//! `alaw`/`ulaw` (G.711). Real compression (IMA ADPCM, MACE, ...) is
//! rejected.
//!
//! A sample size that is no multiple of 8 (e.g. 12 bit) is stored
//! left-justified in whole bytes (AIFF 1.3, "Sample Points"), so the
//! frame size is `channels * ceil(size / 8)`.
//!
//! ## Robustness
//!
//! The `COMM` chunk states the number of sample frames. Recorders that
//! crash leave it, or the `SSND` size, at 0 or too large; the audio
//! actually present in `SSND` (up to the end of the file) is then used,
//! with a warning, rather than failing.
//!
//! ## Tags
//!
//! Text chunks `NAME` (title), `AUTH` (artist), `ANNO` (comment; several
//! are joined by new lines) and `"(c) "` (kept as the extra
//! `COPYRIGHT`), plus an `ID3 ` chunk holding an ID3v2 tag, as written
//! by iTunes and ffmpeg ([`crate::tags::id3`]). The text chunks win where
//! both have a field. An export writes the text chunks and an `ID3 `
//! chunk with everything else (album, track number, ...) and the ID3
//! frames of the source (cover pictures). Markers, instrument and
//! application chunks are not copied: their positions would be wrong.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;

use lac_core::{AudioStreamInfo, CutCapabilities, CutPlan, PacketIndex, SamplePos, Tags};

use crate::tags::id3;
use crate::wav::SampleEncoding;
use crate::{ContainerError, Result};

/// Upper bound for a `COMM` chunk; real ones are 18..60 bytes.
const MAX_COMM_LEN: u32 = 1024;

/// Upper bound for a tag chunk that is read; larger ones (huge cover
/// images) are ignored with a warning.
const MAX_TAG_CHUNK: u32 = 64 << 20;

/// The extra key under which the `"(c) "` chunk is kept.
pub const COPYRIGHT: &str = "COPYRIGHT";

/// An opened AIFF or AIFF-C file.
#[derive(Debug, Clone)]
pub struct AiffFile {
    /// `true` for AIFF-C (`FORM` type `AIFC`).
    aifc: bool,
    encoding: SampleEncoding,
    /// Sample size as stated in `COMM` (bits, may be no multiple of 8).
    sample_size: u16,
    block_align: u16,
    big_endian: bool,
    /// The AIFF-C compression type, or `NONE` for plain AIFF.
    compression: [u8; 4],
    /// The `COMM` payload as found; written back with a new frame count.
    comm_payload: Vec<u8>,
    data_offset: u64,
    data_len: u64,
    tags: Tags,
    /// Frames of the `ID3 ` chunk that [`Tags`] does not hold.
    id3_other: Vec<id3::RawFrame>,
    index: PacketIndex,
    warnings: Vec<String>,
}

impl AiffFile {
    /// Parses the file structure. Reads only the chunk headers, `COMM` and
    /// the tag chunks, never the sample data.
    pub fn open<R: Read + Seek + ?Sized>(r: &mut R) -> Result<Self> {
        let file_len = r.seek(SeekFrom::End(0))?;
        let mut form = [0u8; 12];
        read_exact_at(r, &mut form, 0)?;
        let aifc = match (&form[0..4], &form[8..12]) {
            (b"FORM", b"AIFF") => false,
            (b"FORM", b"AIFC") => true,
            _ => return Err(malformed(0, "not an AIFF file")),
        };
        // The FORM size may be stale like any other size (crashed
        // recorders); the chunk walk is bounded by the file instead.

        let mut warnings = Vec::new();
        let mut comm: Option<(u64, Vec<u8>)> = None;
        let mut ssnd: Option<(u64, u64)> = None;
        let mut text = Tags::default();
        let mut annotations: Vec<String> = Vec::new();
        let mut id3_tag = id3::Id3v2::default();
        let mut pos = 12u64;
        while pos + 8 <= file_len {
            let mut hdr = [0u8; 8];
            read_exact_at(r, &mut hdr, pos)?;
            let id = [hdr[0], hdr[1], hdr[2], hdr[3]];
            let size = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
            let payload = pos + 8;
            let remaining = file_len - payload;
            match &id {
                b"COMM" => {
                    if size > MAX_COMM_LEN || u64::from(size) > remaining {
                        return Err(malformed(pos, "implausible COMM chunk size"));
                    }
                    let mut buf = vec![0u8; size as usize];
                    read_exact_at(r, &mut buf, payload)?;
                    comm = Some((pos, buf));
                }
                b"SSND" => {
                    // offset (4) and block size (4) precede the frames.
                    if remaining < 8 {
                        return Err(malformed(pos, "truncated SSND chunk"));
                    }
                    let mut head = [0u8; 4];
                    read_exact_at(r, &mut head, payload)?;
                    let skip = 8 + u64::from(u32::from_be_bytes(head));
                    let len = if size == 0 || size == u32::MAX {
                        warnings.push(format!(
                            "SSND chunk size is a placeholder ({size:#x}); using the rest of the file"
                        ));
                        remaining
                    } else if u64::from(size) > remaining {
                        warnings.push(format!(
                            "SSND chunk claims {size} bytes but only {remaining} are present; file is truncated"
                        ));
                        remaining
                    } else {
                        u64::from(size)
                    };
                    if skip > len {
                        return Err(malformed(pos, "SSND offset beyond the chunk"));
                    }
                    ssnd = Some((payload + skip, len - skip));
                    if len == remaining {
                        break;
                    }
                }
                b"NAME" | b"AUTH" | b"ANNO" | b"(c) " | b"ID3 " | b"id3 "
                    if u64::from(size) <= remaining =>
                {
                    if size > MAX_TAG_CHUNK {
                        warnings.push(format!("ignoring a {size}-byte tag chunk"));
                    } else {
                        let mut buf = vec![0u8; size as usize];
                        read_exact_at(r, &mut buf, payload)?;
                        // AIFF text chunks are ASCII (AIFF 1.3); taggers
                        // write UTF-8 or Latin-1, so decode leniently.
                        let s = text_of(&buf);
                        match &id {
                            b"NAME" => text.title = crate::tags::non_empty(&s),
                            b"AUTH" => text.artist = crate::tags::non_empty(&s),
                            b"ANNO" => annotations.extend(crate::tags::non_empty(&s)),
                            b"(c) " => {
                                if let Some(c) = crate::tags::non_empty(&s) {
                                    text.extra.push((COPYRIGHT.into(), c));
                                }
                            }
                            _ => {
                                id3_tag = id3::parse(&buf);
                                warnings.append(&mut id3_tag.warnings);
                            }
                        }
                    }
                }
                _ => {}
            }
            pos = payload + u64::from(size) + u64::from(size & 1);
        }
        if !annotations.is_empty() {
            text.comment = Some(annotations.join("\n"));
        }

        let (comm_pos, comm_payload) = comm.ok_or_else(|| malformed(12, "no COMM chunk"))?;
        let c = parse_comm(&comm_payload, comm_pos, aifc, &mut warnings)?;
        let (data_offset, mut data_len) = match ssnd {
            Some(s) => s,
            // AIFF 1.3: SSND may be missing when there are no frames.
            None if c.frames == 0 => (file_len, 0),
            None => return Err(malformed(12, "no SSND chunk")),
        };
        let ba = u64::from(c.block_align);
        let present = data_len / ba;
        let frames = if c.frames > present {
            warnings.push(format!(
                "COMM claims {} sample frames but only {present} are present",
                c.frames
            ));
            present
        } else if c.frames == 0 && present > 0 {
            warnings.push(format!(
                "COMM claims no sample frames but {present} are present; using them"
            ));
            present
        } else {
            c.frames
        };
        data_len = frames * ba;
        let index = crate::wav::build_index(c.info, frames, c.block_align, data_offset)?;
        let tags = merge_tags(text, id3_tag.tags);
        Ok(Self {
            aifc,
            encoding: c.encoding,
            sample_size: c.sample_size,
            block_align: c.block_align,
            big_endian: c.big_endian,
            compression: c.compression,
            comm_payload,
            data_offset,
            data_len,
            tags,
            id3_other: id3_tag.other,
            index,
            warnings,
        })
    }

    /// The packet index (blocks of [`crate::wav::BLOCK_FRAMES`] frames).
    pub fn index(&self) -> &PacketIndex {
        &self.index
    }

    /// The source's tags (see the module documentation).
    pub fn tags(&self) -> &Tags {
        &self.tags
    }

    /// Sample rate and channel count.
    pub fn info(&self) -> AudioStreamInfo {
        self.index.info()
    }

    /// Whether the file is AIFF-C rather than plain AIFF.
    pub fn is_aifc(&self) -> bool {
        self.aifc
    }

    /// Sample encoding.
    pub fn encoding(&self) -> SampleEncoding {
        self.encoding
    }

    /// Sample size in bits as stated in the file (may be no multiple of 8).
    pub fn sample_size(&self) -> u16 {
        self.sample_size
    }

    /// Bytes per sample frame (all channels).
    pub fn block_align(&self) -> u16 {
        self.block_align
    }

    /// Whether multi-byte samples are big-endian (all but AIFF-C `sowt`).
    pub fn big_endian(&self) -> bool {
        self.big_endian
    }

    /// The AIFF-C compression type (`NONE` for plain AIFF).
    pub fn compression(&self) -> [u8; 4] {
        self.compression
    }

    /// Byte offset of the first sample frame in the file.
    pub fn data_offset(&self) -> u64 {
        self.data_offset
    }

    /// Length of the usable sample data in bytes (whole frames only).
    pub fn data_len(&self) -> u64 {
        self.data_len
    }

    /// Problems that were tolerated while opening the file.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// AIFF can be cut at any sample frame.
    pub fn capabilities(&self) -> CutCapabilities {
        CutCapabilities::PCM
    }

    /// A single section of PCM blocks, see [`crate::decode`].
    pub fn decode_sections<R: Read + Seek + ?Sized>(
        &self,
        _r: &mut R,
    ) -> Result<Vec<crate::DecodeSection>> {
        Ok(vec![crate::DecodeSection {
            codec: crate::Codec::Pcm {
                encoding: self.encoding,
                bits_per_sample: self.block_align / self.info().channels * 8,
                block_align: self.block_align,
                big_endian: self.big_endian,
                signed_8bit: true,
            },
            sample_rate: self.info().sample_rate,
            channels: self.info().channels,
            extradata: Vec::new(),
            packet_ends: crate::decode::running_ends(
                self.index.packets().iter().map(|p| u64::from(p.n_samples)),
            ),
            keep: 0..self.index.total_samples(),
        }])
    }

    /// The bytes of PCM block `packet`.
    pub fn read_decode_packet<R: Read + Seek + ?Sized>(
        &self,
        r: &mut R,
        section: usize,
        packet: usize,
    ) -> Result<Vec<u8>> {
        if section != 0 {
            return Err(crate::decode::no_section(section));
        }
        crate::decode::read_index_packet(&self.index, r, packet)
    }

    /// Reads the raw bytes of the sample frames in `range`, the reference
    /// for equivalence tests (see [`crate::wav::WavFile::read_frames`]).
    pub fn read_frames<R: Read + Seek + ?Sized>(
        &self,
        r: &mut R,
        range: Range<SamplePos>,
    ) -> Result<Vec<u8>> {
        let bytes = self.byte_range(&range)?;
        let len = usize::try_from(bytes.end - bytes.start)
            .map_err(|_| ContainerError::TooLarge("range does not fit in memory".into()))?;
        let mut buf = vec![0u8; len];
        read_exact_at(r, &mut buf, bytes.start)?;
        Ok(buf)
    }

    /// Writes a new file of the same kind (AIFF or AIFF-C) containing the
    /// `actual` ranges of `plans`, one after the other.
    ///
    /// `COMM` is copied with the new frame count; sample bytes are copied
    /// verbatim. `tags`, or else the source's tags, are written as text
    /// chunks and an `ID3 ` chunk (see the module documentation).
    pub fn write_cut<R: Read + Seek + ?Sized, W: Write + ?Sized>(
        &self,
        r: &mut R,
        plans: &[CutPlan],
        tags: Option<&Tags>,
        w: &mut W,
    ) -> Result<()> {
        let tags = tags.unwrap_or(&self.tags);
        let mut byte_ranges = Vec::with_capacity(plans.len());
        for p in plans {
            byte_ranges.push(self.byte_range(&p.actual)?);
        }
        let data_len: u64 = byte_ranges.iter().map(|b| b.end - b.start).sum();
        let frames = u32::try_from(data_len / u64::from(self.block_align))
            .map_err(|_| ContainerError::TooLarge("more than 2^32 sample frames".into()))?;

        let mut head = Vec::new();
        if self.aifc {
            // AIFF-C draft: FVER holds the timestamp of the draft version.
            put_chunk(&mut head, b"FVER", &0xA280_5140u32.to_be_bytes());
        }
        let mut comm = self.comm_payload.clone();
        comm[2..6].copy_from_slice(&frames.to_be_bytes());
        put_chunk(&mut head, b"COMM", &comm);
        head.extend(tag_chunks(tags, &self.id3_other)?);

        // SSND: header, offset and block size 0, then the frames.
        let ssnd_size = 8 + data_len;
        let form_size = 4 + head.len() as u64 + 8 + ssnd_size + (ssnd_size & 1);
        let form_size = u32::try_from(form_size).map_err(|_| {
            ContainerError::TooLarge(format!("{data_len} bytes of audio exceed the AIFF limit"))
        })?;
        w.write_all(b"FORM")?;
        w.write_all(&form_size.to_be_bytes())?;
        w.write_all(if self.aifc { b"AIFC" } else { b"AIFF" })?;
        w.write_all(&head)?;
        w.write_all(b"SSND")?;
        // Fits, since the FORM size did.
        w.write_all(&(ssnd_size as u32).to_be_bytes())?;
        w.write_all(&[0; 8])?;
        for b in byte_ranges {
            r.seek(SeekFrom::Start(b.start))?;
            let n = io::copy(&mut (&mut *r).take(b.end - b.start), w)?;
            if n != b.end - b.start {
                return Err(malformed(b.start + n, "file shrank while copying"));
            }
        }
        if ssnd_size & 1 == 1 {
            w.write_all(&[0])?;
        }
        Ok(())
    }

    /// Converts a range of sample frames into a byte range of the file.
    fn byte_range(&self, range: &Range<SamplePos>) -> Result<Range<u64>> {
        let total = self.index.total_samples();
        if range.start > range.end || range.end > total {
            return Err(ContainerError::InvalidPlan(format!(
                "frames {}..{} outside 0..{total}",
                range.start, range.end
            )));
        }
        let ba = u64::from(self.block_align);
        Ok(self.data_offset + range.start * ba..self.data_offset + range.end * ba)
    }
}

/// The interpreted contents of a `COMM` chunk.
struct Comm {
    info: AudioStreamInfo,
    frames: u64,
    sample_size: u16,
    block_align: u16,
    encoding: SampleEncoding,
    big_endian: bool,
    compression: [u8; 4],
}

/// Parses a `COMM` payload (AIFF 1.3 "Common Chunk", AIFF-C "Extended
/// Common Chunk"); `pos` is the chunk's file offset for error messages.
fn parse_comm(p: &[u8], pos: u64, aifc: bool, warnings: &mut Vec<String>) -> Result<Comm> {
    if p.len() < 18 || (aifc && p.len() < 22) {
        return Err(malformed(pos, "COMM chunk too short"));
    }
    let channels = u16::from_be_bytes([p[0], p[1]]);
    let frames = u64::from(u32::from_be_bytes([p[2], p[3], p[4], p[5]]));
    let sample_size = u16::from_be_bytes([p[6], p[7]]);
    let mut rate_bytes = [0u8; 10];
    rate_bytes.copy_from_slice(&p[8..18]);
    let sample_rate = extended_to_rate(rate_bytes, warnings)
        .ok_or_else(|| malformed(pos, "invalid sample rate"))?;
    let compression = if aifc {
        [p[18], p[19], p[20], p[21]]
    } else {
        *b"NONE"
    };
    // (encoding, big-endian, bytes per sample or None for the sample size)
    let (encoding, big_endian, fixed_bytes) = match &compression {
        b"NONE" | b"twos" => (SampleEncoding::Pcm, true, None),
        b"sowt" => (SampleEncoding::Pcm, false, None),
        b"fl32" | b"FL32" => (SampleEncoding::Float, true, Some(4)),
        b"fl64" | b"FL64" => (SampleEncoding::Float, true, Some(8)),
        b"alaw" | b"ALAW" => (SampleEncoding::ALaw, true, Some(1)),
        b"ulaw" | b"ULAW" => (SampleEncoding::MuLaw, true, Some(1)),
        other => {
            return Err(ContainerError::Unsupported(format!(
                "compressed AIFF-C type {:?}",
                String::from_utf8_lossy(other)
            )));
        }
    };
    if channels == 0 || sample_size == 0 {
        return Err(malformed(pos, "zero channels or sample size"));
    }
    let bytes = fixed_bytes.unwrap_or(sample_size.div_ceil(8));
    if !(1..=4).contains(&bytes) && fixed_bytes.is_none() {
        return Err(ContainerError::Unsupported(format!(
            "{sample_size}-bit AIFF samples"
        )));
    }
    let block_align = channels
        .checked_mul(bytes)
        .ok_or_else(|| malformed(pos, "too many channels"))?;
    Ok(Comm {
        info: AudioStreamInfo {
            sample_rate,
            channels,
        },
        frames,
        sample_size,
        block_align,
        encoding,
        big_endian,
        compression,
    })
}

/// Converts an 80-bit IEEE 754 extended float (sign, 15-bit exponent with
/// bias 16383, 64-bit mantissa with explicit integer bit), as AIFF stores
/// the sample rate, to a whole rate in Hz. A fractional rate is rounded
/// with a warning. `None` for zero, negative, infinite or absurd rates.
fn extended_to_rate(b: [u8; 10], warnings: &mut Vec<String>) -> Option<u32> {
    let sign_exp = u16::from_be_bytes([b[0], b[1]]);
    let mut m = [0u8; 8];
    m.copy_from_slice(&b[2..10]);
    let mantissa = u64::from_be_bytes(m);
    if sign_exp & 0x8000 != 0 || mantissa == 0 {
        return None;
    }
    // value = mantissa * 2^(exp - 16383 - 63)
    let shift = i32::from(sign_exp & 0x7FFF) - 16383 - 63;
    if shift >= 0 {
        // At least 2^63 Hz.
        return None;
    }
    let shift = shift.unsigned_abs();
    if shift >= 64 {
        // Below 1 Hz.
        return None;
    }
    let whole = mantissa >> shift;
    let frac = mantissa & ((1u64 << shift) - 1);
    let rounded = whole + u64::from(frac >= 1u64 << (shift - 1));
    if frac != 0 {
        warnings.push(format!("fractional sample rate rounded to {rounded} Hz"));
    }
    u32::try_from(rounded).ok().filter(|&r| r > 0)
}

/// The text of an AIFF text chunk: UTF-8 if valid, else Latin-1.
fn text_of(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_owned(),
        Err(_) => id3::latin1(b),
    }
}

/// The text chunks' fields, with those they lack taken from ID3; extras
/// of both.
fn merge_tags(text: Tags, id3: Tags) -> Tags {
    let mut extra = text.extra;
    extra.extend(id3.extra);
    Tags {
        title: text.title.or(id3.title),
        artist: text.artist.or(id3.artist),
        album: id3.album,
        album_artist: id3.album_artist,
        track_number: id3.track_number,
        track_total: id3.track_total,
        date: id3.date,
        genre: id3.genre,
        comment: text.comment.or(id3.comment),
        extra,
    }
}

/// The text chunks and the `ID3 ` chunk for `tags`.
fn tag_chunks(tags: &Tags, id3_other: &[id3::RawFrame]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    if let Some(t) = &tags.title {
        put_chunk(&mut out, b"NAME", t.as_bytes());
    }
    if let Some(a) = &tags.artist {
        put_chunk(&mut out, b"AUTH", a.as_bytes());
    }
    if let Some(c) = &tags.comment {
        put_chunk(&mut out, b"ANNO", c.as_bytes());
    }
    for (_, c) in tags.extra.iter().filter(|(k, _)| k == COPYRIGHT) {
        put_chunk(&mut out, b"(c) ", c.as_bytes());
    }
    // Everything goes into ID3 as well (readers of ID3 only, e.g. iTunes),
    // except the copyright, whose chunk is its only place.
    let id3_tags = Tags {
        extra: tags
            .extra
            .iter()
            .filter(|(k, _)| k != COPYRIGHT)
            .cloned()
            .collect(),
        ..tags.clone()
    };
    let tag = id3::write(&id3_tags, id3_other).map_err(ContainerError::TooLarge)?;
    if !tag.is_empty() {
        put_chunk(&mut out, b"ID3 ", &tag);
    }
    Ok(out)
}

/// Appends a chunk with its header and pad byte.
fn put_chunk(out: &mut Vec<u8>, id: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(id);
    out.extend((payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    if payload.len() & 1 == 1 {
        out.push(0);
    }
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

    /// 80-bit extended encodings of common rates, as written by ffmpeg.
    fn rate_bytes(rate: u32) -> [u8; 10] {
        let lz = rate.leading_zeros();
        let exp = 16383 + 31 - lz as u16;
        let mantissa = u64::from(rate) << (32 + lz);
        let mut b = [0u8; 10];
        b[..2].copy_from_slice(&exp.to_be_bytes());
        b[2..].copy_from_slice(&mantissa.to_be_bytes());
        b
    }

    #[test]
    fn extended_sample_rates() {
        let mut w = Vec::new();
        for rate in [8000, 11025, 22050, 44_100, 48_000, 96_000, 192_000, 1] {
            assert_eq!(extended_to_rate(rate_bytes(rate), &mut w), Some(rate));
        }
        assert!(w.is_empty());
        // 44100 Hz exactly as in the AIFF spec's example bytes.
        let b = [0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0];
        assert_eq!(extended_to_rate(b, &mut w), Some(44_100));
        // 44100.5 rounds, with a warning.
        let mut b = rate_bytes(44_100);
        b[4] |= 0x80; // the bit worth 0.5, right below the 16 integer bits
        assert_eq!(extended_to_rate(b, &mut w), Some(44_101));
        assert_eq!(w.len(), 1);
        // Zero, negative and huge rates are invalid.
        assert_eq!(extended_to_rate([0; 10], &mut w), None);
        let mut neg = rate_bytes(8000);
        neg[0] |= 0x80;
        assert_eq!(extended_to_rate(neg, &mut w), None);
        assert_eq!(
            extended_to_rate([0x7F, 0xFF, 0x80, 0, 0, 0, 0, 0, 0, 0], &mut w),
            None
        );
    }

    #[test]
    fn compressed_types_are_rejected() {
        let mut comm = vec![0, 1, 0, 0, 0, 10, 0, 16];
        comm.extend(rate_bytes(8000));
        comm.extend(b"ima4");
        comm.extend([0, 0]);
        let mut w = Vec::new();
        assert!(matches!(
            parse_comm(&comm, 0, true, &mut w),
            Err(ContainerError::Unsupported(_))
        ));
        comm[18..22].copy_from_slice(b"sowt");
        let c = parse_comm(&comm, 0, true, &mut w).expect("sowt");
        assert!(!c.big_endian);
        assert_eq!(c.block_align, 2);
    }

    #[test]
    fn odd_sample_sizes_round_up_to_bytes() {
        let mut comm = vec![0, 2, 0, 0, 0, 10, 0, 12];
        comm.extend(rate_bytes(8000));
        let c = parse_comm(&comm, 0, false, &mut Vec::new()).expect("12 bit");
        assert_eq!((c.sample_size, c.block_align), (12, 4));
        comm[7] = 40;
        assert!(parse_comm(&comm, 0, false, &mut Vec::new()).is_err());
    }
}
