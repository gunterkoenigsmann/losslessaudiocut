//! RIFF/WAVE files with uncompressed audio.
//!
//! References: Microsoft "Multimedia Programming Interface and Data
//! Specifications 1.0" (RIFF, 1991) and "Multiple Channel Audio Data and WAVE
//! Files" (`WAVE_FORMAT_EXTENSIBLE`).
//!
//! ## Layout
//!
//! ```text
//! "RIFF" <u32 le size> "WAVE"
//!   "fmt " <u32 le size> <format payload>   [pad byte if size is odd]
//!   ...other chunks (LIST, bext, id3, ...)...
//!   "data" <u32 le size> <sample frames>    [pad byte if size is odd]
//! ```
//!
//! ## Cutting
//!
//! Every sample frame is `block_align` bytes and independent of its
//! neighbours, so cuts are sample-exact and bit-exact. The packet index
//! groups frames into blocks of [`BLOCK_FRAMES`] purely to keep the index
//! small; the blocks have no meaning in the file.
//!
//! Supported encodings are integer PCM, IEEE float, A-law and µ-law, both
//! with the classic format tags and as `WAVE_FORMAT_EXTENSIBLE`; all of them
//! store one self-contained frame per `block_align` bytes. Compressed WAV
//! variants (ADPCM, MP3-in-WAV, ...) are rejected.
//!
//! ## Robustness
//!
//! Recorders that crash or stream to disk often leave a `data` size of 0 or
//! `0xFFFF_FFFF`, or a size larger than the file. In all these cases the data
//! is taken to extend to the end of the file and a warning is recorded
//! instead of failing. A trailing partial frame is ignored with a warning.
//!
//! ## Not yet supported
//!
//! RF64/BW64 (files above 4 GiB) and copying `LIST/INFO` tags; both are
//! planned for later pull requests.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;

use lac_core::{
    AudioStreamInfo, CutCapabilities, CutPlan, PacketFlags, PacketIndex, PacketInfo, SamplePos,
};

use crate::{ContainerError, Result};

/// Number of sample frames grouped into one index entry.
pub const BLOCK_FRAMES: u32 = 4096;

/// Upper bound for the `fmt ` chunk payload; real ones are 16..40 bytes.
const MAX_FMT_LEN: u32 = 1024;

const FORMAT_PCM: u16 = 0x0001;
const FORMAT_IEEE_FLOAT: u16 = 0x0003;
const FORMAT_ALAW: u16 = 0x0006;
const FORMAT_MULAW: u16 = 0x0007;
const FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// The last 14 bytes of every `KSDATAFORMAT_SUBTYPE_*` GUID used by
/// `WAVE_FORMAT_EXTENSIBLE`; the first two bytes are the classic format tag.
const SUBFORMAT_GUID_TAIL: [u8; 14] = [
    0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// How samples are encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleEncoding {
    /// Signed integer PCM (unsigned for 8 bit, per the WAV convention).
    Pcm,
    /// IEEE 754 floating point.
    Float,
    /// ITU-T G.711 A-law.
    ALaw,
    /// ITU-T G.711 µ-law.
    MuLaw,
}

/// An opened WAV file: its format, where its sample data is, and its index.
#[derive(Debug, Clone)]
pub struct WavFile {
    encoding: SampleEncoding,
    bits_per_sample: u16,
    block_align: u16,
    /// The `fmt ` payload exactly as found, written back unchanged so that
    /// channel masks and extensible fields survive the cut.
    fmt_payload: Vec<u8>,
    data_offset: u64,
    data_len: u64,
    index: PacketIndex,
    warnings: Vec<String>,
}

impl WavFile {
    /// Parses the file structure. Reads only the chunk headers and the
    /// `fmt ` chunk, never the sample data.
    pub fn open<R: Read + Seek + ?Sized>(r: &mut R) -> Result<Self> {
        let file_len = r.seek(SeekFrom::End(0))?;
        r.seek(SeekFrom::Start(0))?;

        let mut riff = [0u8; 12];
        read_exact_at(r, &mut riff, 0)?;
        if &riff[0..4] != b"RIFF" || &riff[8..12] != b"WAVE" {
            return Err(malformed(0, "not a RIFF/WAVE file"));
        }

        let mut warnings = Vec::new();
        let mut fmt: Option<(u64, Vec<u8>)> = None;
        let mut data: Option<(u64, u64)> = None;
        let mut pos: u64 = 12;

        // Walk the chunk list. A chunk header needs 8 bytes; anything
        // shorter at the end is trailing garbage and ignored.
        while pos + 8 <= file_len {
            let mut hdr = [0u8; 8];
            read_exact_at(r, &mut hdr, pos)?;
            let id = [hdr[0], hdr[1], hdr[2], hdr[3]];
            let size = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
            let payload = pos + 8;
            let remaining = file_len - payload;

            match &id {
                b"fmt " => {
                    if size > MAX_FMT_LEN || u64::from(size) > remaining {
                        return Err(malformed(pos, "implausible fmt chunk size"));
                    }
                    let mut buf = vec![0u8; size as usize];
                    read_exact_at(r, &mut buf, payload)?;
                    fmt = Some((pos, buf));
                }
                b"data" => {
                    // Sizes of 0 and 0xFFFFFFFF are placeholders written by
                    // streaming recorders that never patched the header.
                    let len = if size == 0 || size == u32::MAX {
                        warnings.push(format!(
                            "data chunk size is a placeholder ({size:#x}); using the rest of the file"
                        ));
                        remaining
                    } else if u64::from(size) > remaining {
                        warnings.push(format!(
                            "data chunk claims {size} bytes but only {remaining} are present; file is truncated"
                        ));
                        remaining
                    } else {
                        u64::from(size)
                    };
                    data = Some((payload, len));
                    if len == remaining {
                        break;
                    }
                }
                _ => {}
            }
            if data.is_some() && fmt.is_some() && &id == b"data" {
                // Chunks after the data (LIST, id3, ...) are not needed yet.
                break;
            }
            // Chunks are padded to even length (RIFF spec, "chunk").
            pos = payload + u64::from(size) + u64::from(size & 1);
        }

        let (fmt_pos, fmt_payload) = fmt.ok_or_else(|| malformed(12, "no fmt chunk"))?;
        let (data_offset, mut data_len) = data.ok_or_else(|| malformed(12, "no data chunk"))?;
        let f = parse_fmt(&fmt_payload, fmt_pos)?;

        let partial = data_len % u64::from(f.block_align);
        if partial != 0 {
            warnings.push(format!(
                "ignoring {partial} bytes of a trailing partial frame"
            ));
            data_len -= partial;
        }

        let frames = data_len / u64::from(f.block_align);
        let index = build_index(f.info, frames, f.block_align, data_offset)?;
        Ok(Self {
            encoding: f.encoding,
            bits_per_sample: f.bits_per_sample,
            block_align: f.block_align,
            fmt_payload,
            data_offset,
            data_len,
            index,
            warnings,
        })
    }

    /// The packet index (blocks of [`BLOCK_FRAMES`] frames).
    pub fn index(&self) -> &PacketIndex {
        &self.index
    }

    /// Sample rate and channel count.
    pub fn info(&self) -> AudioStreamInfo {
        self.index.info()
    }

    /// Sample encoding.
    pub fn encoding(&self) -> SampleEncoding {
        self.encoding
    }

    /// Bits per sample as stored in the file (container size, e.g. 24).
    pub fn bits_per_sample(&self) -> u16 {
        self.bits_per_sample
    }

    /// Bytes per sample frame (all channels).
    pub fn block_align(&self) -> u16 {
        self.block_align
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

    /// WAV can be cut at any sample frame.
    pub fn capabilities(&self) -> CutCapabilities {
        CutCapabilities::PCM
    }

    /// Reads the raw bytes of the sample frames in `range`.
    ///
    /// For PCM this *is* the decoded audio, which makes it the reference for
    /// equivalence tests.
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

    /// Writes a new WAV file containing the `actual` ranges of `plans`, one
    /// after the other.
    ///
    /// The `fmt ` chunk is copied unchanged; sample bytes are copied
    /// verbatim. Other chunks of the source are not copied yet.
    pub fn write_cut<R: Read + Seek + ?Sized, W: Write + ?Sized>(
        &self,
        r: &mut R,
        plans: &[CutPlan],
        w: &mut W,
    ) -> Result<()> {
        let mut byte_ranges = Vec::with_capacity(plans.len());
        for p in plans {
            byte_ranges.push(self.byte_range(&p.actual)?);
        }
        let data_len: u64 = byte_ranges.iter().map(|b| b.end - b.start).sum();
        let fmt_len = self.fmt_payload.len() as u64;
        // RIFF size counts everything after itself: "WAVE" + both chunks,
        // each with header and pad byte.
        let riff_size = 4 + (8 + fmt_len + (fmt_len & 1)) + (8 + data_len + (data_len & 1));
        let riff_size = u32::try_from(riff_size).map_err(|_| {
            ContainerError::TooLarge(format!(
                "{data_len} bytes of audio exceed the 4 GiB WAV limit (RF64 is not supported yet)"
            ))
        })?;
        let data_size = u32::try_from(data_len).unwrap_or(u32::MAX);

        w.write_all(b"RIFF")?;
        w.write_all(&riff_size.to_le_bytes())?;
        w.write_all(b"WAVE")?;
        w.write_all(b"fmt ")?;
        w.write_all(&(fmt_len as u32).to_le_bytes())?;
        w.write_all(&self.fmt_payload)?;
        if fmt_len & 1 == 1 {
            w.write_all(&[0])?;
        }
        w.write_all(b"data")?;
        w.write_all(&data_size.to_le_bytes())?;
        for b in byte_ranges {
            r.seek(SeekFrom::Start(b.start))?;
            let n = io::copy(&mut (&mut *r).take(b.end - b.start), w)?;
            if n != b.end - b.start {
                return Err(malformed(b.start + n, "file shrank while copying"));
            }
        }
        if data_len & 1 == 1 {
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

/// The interpreted contents of a `fmt ` chunk.
struct Fmt {
    info: AudioStreamInfo,
    encoding: SampleEncoding,
    bits_per_sample: u16,
    block_align: u16,
}

/// Parses `WAVEFORMATEX` / `WAVEFORMATEXTENSIBLE`; `pos` is the chunk's file
/// offset for error messages.
fn parse_fmt(p: &[u8], pos: u64) -> Result<Fmt> {
    if p.len() < 16 {
        return Err(malformed(pos, "fmt chunk shorter than 16 bytes"));
    }
    let u16_at = |i: usize| u16::from_le_bytes([p[i], p[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([p[i], p[i + 1], p[i + 2], p[i + 3]]);
    let mut tag = u16_at(0);
    let channels = u16_at(2);
    let sample_rate = u32_at(4);
    let block_align = u16_at(12);
    let bits_per_sample = u16_at(14);

    if tag == FORMAT_EXTENSIBLE {
        // cbSize (2) + wValidBitsPerSample (2) + dwChannelMask (4) +
        // SubFormat GUID (16) follow the 16-byte base structure.
        if p.len() < 40 {
            return Err(malformed(pos, "WAVE_FORMAT_EXTENSIBLE fmt chunk too short"));
        }
        if p[26..40] != SUBFORMAT_GUID_TAIL {
            return Err(ContainerError::Unsupported(
                "unknown WAVE_FORMAT_EXTENSIBLE sub-format".into(),
            ));
        }
        tag = u16_at(24);
    }
    let encoding = match tag {
        FORMAT_PCM => SampleEncoding::Pcm,
        FORMAT_IEEE_FLOAT => SampleEncoding::Float,
        FORMAT_ALAW => SampleEncoding::ALaw,
        FORMAT_MULAW => SampleEncoding::MuLaw,
        other => {
            return Err(ContainerError::Unsupported(format!(
                "compressed WAV format tag {other:#06x}"
            )));
        }
    };
    if channels == 0 || sample_rate == 0 || bits_per_sample == 0 {
        return Err(malformed(pos, "zero channels, sample rate or bit depth"));
    }
    // Each channel needs at least ceil(bits / 8) bytes per frame.
    let min_align = u32::from(channels) * u32::from(bits_per_sample.div_ceil(8));
    if u32::from(block_align) < min_align {
        return Err(malformed(
            pos,
            &format!("block_align {block_align} is smaller than {min_align}"),
        ));
    }
    Ok(Fmt {
        info: AudioStreamInfo {
            sample_rate,
            channels,
        },
        encoding,
        bits_per_sample,
        block_align,
    })
}

/// Groups `frames` sample frames into index entries of [`BLOCK_FRAMES`].
fn build_index(
    info: AudioStreamInfo,
    frames: u64,
    block_align: u16,
    data_offset: u64,
) -> Result<PacketIndex> {
    let block = u64::from(BLOCK_FRAMES);
    let ba = u64::from(block_align);
    let packets = (0..frames.div_ceil(block))
        .map(|i| {
            let first = i * block;
            let n = (frames - first).min(block);
            PacketInfo {
                byte_offset: data_offset + first * ba,
                // At most 4096 * 65535 bytes, which fits in a u32.
                byte_len: (n * ba) as u32,
                first_sample: first,
                n_samples: n as u32,
                hidden_before: 0,
                hidden_after: 0,
                flags: PacketFlags { independent: true },
            }
        })
        .collect();
    PacketIndex::new(info, packets)
        .map_err(|e| ContainerError::InvalidPlan(format!("internal index error: {e}")))
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
