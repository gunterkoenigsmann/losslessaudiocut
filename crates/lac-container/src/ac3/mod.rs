//! AC-3 (Dolby Digital) elementary streams, `.ac3` (ATSC A/52, "Digital
//! Audio Compression Standard"), optionally with an ID3v2 tag in front.
//!
//! DVD rips and broadcast recordings keep AC-3 this way: a plain sequence
//! of sync frames without any container.
//!
//! ## Sync frames
//!
//! Each frame starts with the sync word `0x0B77`, then `crc1` (16 bits),
//! `fscod` (2 bits: 48, 44.1 or 32 kHz) and `frmsizecod` (6 bits: the
//! frame size, A/52 Table 5.18), followed by the bit stream information
//! (`bsid`, `bsmod`, `acmod` with its mix levels, `lfeon`, A/52 5.4.2). A
//! frame always holds six blocks of 256 samples, 1536 per channel. Frames
//! are found by their sync word and a plausible header, confirmed by the
//! next frame's sync word; junk in between is skipped with a warning.
//!
//! E-AC-3 (`bsid` 11..16, A/52 Annex E) is not supported yet.
//!
//! ## Cutting
//!
//! An AC-3 frame is decoded on its own except for one thing: the first
//! block's inverse MDCT output is overlapped with the second half of the
//! previous block (A/52 7.9.4.2), which lies in the previous frame. A
//! frame copied after a different predecessor, or as the first frame of a
//! file, therefore decodes differently for its first 256 samples (about
//! 5 ms) and exactly from then on. Raw AC-3 has no way to hide a pre-roll
//! frame, so cuts are frame-granular without pre-roll: that transient
//! occurs at the file's start (unless it is the source's start) and after
//! each join, see "Join points inside a file" in `docs/architecture.md`.
//!
//! Decoders fill mantissas that got no bits with dither noise
//! (`dithflag`, A/52 7.3.4) from a random generator; libavcodec runs that
//! generator from the start of the stream unless it is told to seed it per
//! frame from the frame's content (`cons_noisegen`), which `lac-decode`
//! does, so a cut decodes like the source.
//!
//! ## Tags
//!
//! AC-3 has no tags of its own. An ID3v2 tag in front, as ffmpeg and
//! taggers read it, is read; an export copies it verbatim, or writes a new
//! ID3v2.4 tag with the given tags and the source's other frames (see
//! [`crate::tags::id3`]).

use std::io::{self, Read, Seek, SeekFrom, Write};

use lac_core::{
    AudioStreamInfo, CutCapabilities, CutPlan, PacketFlags, PacketIndex, PacketInfo, Tags,
};

use crate::tags::id3;
use crate::window::Window;
use crate::{ContainerError, Result};

/// Samples per channel in one frame (six blocks of 256).
pub const FRAME_SAMPLES: u32 = 1536;

/// Frame sizes in 16-bit words per `frmsizecod` (A/52 Table 5.18), for
/// 48, 44.1 and 32 kHz. Odd codes at 44.1 kHz add one padding word.
const FRAME_WORDS: [[u16; 38]; 3] = [
    [
        64, 64, 80, 80, 96, 96, 112, 112, 128, 128, 160, 160, 192, 192, 224, 224, 256, 256, 320,
        320, 384, 384, 448, 448, 512, 512, 640, 640, 768, 768, 896, 896, 1024, 1024, 1152, 1152,
        1280, 1280,
    ],
    [
        69, 70, 87, 88, 104, 105, 121, 122, 139, 140, 174, 175, 208, 209, 243, 244, 278, 279, 348,
        349, 417, 418, 487, 488, 557, 558, 696, 697, 835, 836, 975, 976, 1114, 1115, 1253, 1254,
        1393, 1394,
    ],
    [
        96, 96, 120, 120, 144, 144, 168, 168, 192, 192, 240, 240, 288, 288, 336, 336, 384, 384,
        480, 480, 576, 576, 672, 672, 768, 768, 960, 960, 1152, 1152, 1344, 1344, 1536, 1536, 1728,
        1728, 1920, 1920,
    ],
];

/// The parameters of a frame header that must stay the same throughout
/// the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamParams {
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Audio coding mode (A/52 Table 5.8): channel arrangement.
    pub acmod: u8,
    /// Whether the low-frequency effects channel is present.
    pub lfe: bool,
}

impl StreamParams {
    /// Number of channels, the LFE channel included.
    pub fn channels(&self) -> u16 {
        // A/52 Table 5.8: full-bandwidth channels per acmod.
        const NFCHANS: [u16; 8] = [2, 1, 2, 3, 3, 4, 4, 5];
        NFCHANS[usize::from(self.acmod)] + u16::from(self.lfe)
    }

    /// The channel arrangement as Dolby writes it: front/rear channels,
    /// ".1" for LFE, e.g. "3/2.1" for 5.1 and "1+1" for dual mono.
    pub fn layout(&self) -> String {
        // A/52 Table 5.8.
        const NAMES: [&str; 8] = ["1+1", "1/0", "2/0", "3/0", "2/1", "3/1", "2/2", "3/2"];
        let lfe = if self.lfe { ".1" } else { "" };
        format!("{}{lfe}", NAMES[usize::from(self.acmod)])
    }
}

/// Why a header is not an AC-3 frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// Too few bytes to parse it.
    Truncated,
    /// No sync word, or reserved values.
    Invalid,
    /// A valid E-AC-3 (or other newer) frame.
    Eac3,
}

/// Parses a frame header at the start of `b`: (stream parameters, frame
/// length in bytes).
pub fn parse_header(b: &[u8]) -> std::result::Result<(StreamParams, usize), HeaderError> {
    if b.len() < 2 {
        return Err(HeaderError::Truncated);
    }
    if b[0] != 0x0B || b[1] != 0x77 {
        return Err(HeaderError::Invalid);
    }
    if b.len() < 7 {
        return Err(HeaderError::Truncated);
    }
    // A/52 5.3.1 syncinfo: syncword, crc1, fscod (2), frmsizecod (6).
    let fscod = usize::from(b[4] >> 6);
    let frmsizecod = usize::from(b[4] & 0x3F);
    // A/52 5.3.2 bsi: bsid (5), bsmod (3), acmod (3), ...
    let bsid = b[5] >> 3;
    if bsid > 10 {
        return Err(if bsid <= 16 {
            HeaderError::Eac3
        } else {
            HeaderError::Invalid
        });
    }
    if fscod == 3 || frmsizecod >= 38 {
        return Err(HeaderError::Invalid);
    }
    let acmod = b[6] >> 5;
    // lfeon follows acmod and the 2-bit fields that acmod switches on:
    // cmixlev if a centre channel exists (acmod odd and not mono),
    // surmixlev if surround channels exist (acmod & 4), dsurmod for 2/0.
    let mut bit = 3; // bits of byte 6 used so far
    if acmod & 1 != 0 && acmod != 1 {
        bit += 2;
    }
    if acmod & 4 != 0 {
        bit += 2;
    }
    if acmod == 2 {
        bit += 2;
    }
    // At most 3 + 2 + 2 = 7 bits before lfeon, so it is in byte 6 or 7.
    let byte = 6 + bit / 8;
    let lfe = *b.get(byte).ok_or(HeaderError::Truncated)? & (0x80 >> (bit % 8)) != 0;
    let words = FRAME_WORDS[fscod][frmsizecod];
    let sample_rate = [48_000, 44_100, 32_000][fscod];
    Ok((
        StreamParams {
            sample_rate,
            acmod,
            lfe,
        },
        usize::from(words) * 2,
    ))
}

/// An opened AC-3 file.
#[derive(Debug, Clone)]
pub struct Ac3File {
    params: StreamParams,
    /// The ID3v2 tag in front, verbatim (empty without one).
    id3_raw: Vec<u8>,
    tags: Tags,
    /// ID3 frames [`Tags`] does not hold.
    id3_other: Vec<id3::RawFrame>,
    index: PacketIndex,
    warnings: Vec<String>,
}

impl Ac3File {
    /// Scans the file for frames and builds the packet index. Reads every
    /// frame header once but decodes nothing.
    pub fn open<R: Read + Seek + ?Sized>(r: &mut R) -> Result<Self> {
        let len = r.seek(SeekFrom::End(0))?;
        let mut warnings = Vec::new();
        // An ID3v2 tag in front.
        let mut head = [0u8; 10];
        r.seek(SeekFrom::Start(0))?;
        let n = read_up_to(r, &mut head)?;
        let mut id3_raw = Vec::new();
        let mut tag = id3::Id3v2::default();
        if let Some(tl) = id3::tag_len(&head[..n]) {
            let tl = (tl as u64).min(len);
            id3_raw = vec![0u8; tl as usize];
            r.seek(SeekFrom::Start(0))?;
            r.read_exact(&mut id3_raw)?;
            tag = id3::parse(&id3_raw);
            warnings.append(&mut tag.warnings);
        }
        let start = id3_raw.len() as u64;

        let mut w = Window::new(r, len);
        let mut packets = Vec::new();
        let mut params: Option<StreamParams> = None;
        let mut pos = start;
        let mut junk_from: Option<u64> = None;
        while pos < len {
            w.keep = pos;
            let hb = w.bytes(pos, 8)?;
            let parsed = parse_header(hb);
            let frame = match parsed {
                Ok((p, size)) => {
                    let end = pos + size as u64;
                    if end > len {
                        warnings.push(format!(
                            "the last frame at byte {pos} is truncated and was dropped"
                        ));
                        break;
                    }
                    // Confirmed by the next frame's sync word, the end of
                    // the file, or (before junk) its own checksum.
                    let confirmed = end == len
                        || w.bytes(end, 2)? == [0x0B, 0x77]
                        || crc_ok(w.bytes(pos, size as u64)?);
                    confirmed.then_some((p, size))
                }
                Err(HeaderError::Truncated) => {
                    warnings.push(format!("ignoring {} trailing bytes", len - pos));
                    break;
                }
                Err(HeaderError::Eac3) if params.is_none() => {
                    return Err(ContainerError::Unsupported(
                        "E-AC-3 (Dolby Digital Plus)".into(),
                    ));
                }
                Err(_) => None,
            };
            let Some((p, size)) = frame else {
                junk_from.get_or_insert(pos);
                pos += 1;
                continue;
            };
            if let Some(j) = junk_from.take() {
                warnings.push(format!("skipped {} bytes of junk at byte {j}", pos - j));
            }
            match params {
                None => params = Some(p),
                Some(q) if q != p => {
                    warnings.push(format!(
                        "the stream changes from {} to {} channels or another sample rate at byte {pos}; the rest is ignored",
                        q.channels(),
                        p.channels()
                    ));
                    break;
                }
                _ => {}
            }
            packets.push(PacketInfo {
                byte_offset: pos,
                // At most 3840 bytes.
                byte_len: size as u32,
                first_sample: packets.len() as u64 * u64::from(FRAME_SAMPLES),
                n_samples: FRAME_SAMPLES,
                hidden_before: 0,
                hidden_after: 0,
                flags: PacketFlags { independent: false },
            });
            pos += size as u64;
        }
        if let Some(j) = junk_from {
            warnings.push(format!("skipped {} bytes of junk at byte {j}", len - j));
        }
        let params =
            params.ok_or_else(|| ContainerError::Unsupported("no AC-3 frame found".into()))?;
        let info = AudioStreamInfo {
            sample_rate: params.sample_rate,
            channels: params.channels(),
        };
        let index = PacketIndex::new(info, packets)
            .map_err(|e| ContainerError::InvalidPlan(format!("internal index error: {e}")))?;
        Ok(Self {
            params,
            id3_raw,
            tags: tag.tags,
            id3_other: tag.other,
            index,
            warnings,
        })
    }

    /// The stream's parameters.
    pub fn params(&self) -> StreamParams {
        self.params
    }

    /// One entry per frame.
    pub fn index(&self) -> &PacketIndex {
        &self.index
    }

    /// The tags of the ID3v2 tag in front, if any.
    pub fn tags(&self) -> &Tags {
        &self.tags
    }

    /// Problems that were tolerated while opening the file.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Frame-granular, without pre-roll (see the module documentation).
    pub fn capabilities(&self) -> CutCapabilities {
        CutCapabilities {
            preroll_packets: 0,
            can_trim_start: false,
            can_trim_end: false,
            lossless_reencode: false,
            trims_at_file_ends_only: false,
        }
    }

    /// One section of all frames.
    pub fn decode_sections<R: Read + Seek + ?Sized>(
        &self,
        _r: &mut R,
    ) -> Result<Vec<crate::DecodeSection>> {
        Ok(vec![crate::DecodeSection {
            codec: crate::Codec::Ac3,
            sample_rate: self.params.sample_rate,
            channels: self.params.channels(),
            extradata: Vec::new(),
            packet_ends: crate::decode::running_ends(
                self.index.packets().iter().map(|p| u64::from(p.n_samples)),
            ),
            keep: 0..self.index.total_samples(),
        }])
    }

    /// Frame `packet`.
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

    /// Writes the frames of `plans`, joined, after an ID3v2 tag: the
    /// source's verbatim without `tags`, else a new one (none if `tags` is
    /// empty and the source has no other ID3 frames).
    pub fn write_cut<R: Read + Seek + ?Sized, W: Write + ?Sized>(
        &self,
        r: &mut R,
        plans: &[CutPlan],
        tags: Option<&Tags>,
        w: &mut W,
    ) -> Result<()> {
        let n = self.index.packets().len();
        for p in plans {
            if p.packets.end > n || p.trim_start != 0 || p.trim_end != 0 {
                return Err(ContainerError::InvalidPlan(format!(
                    "AC-3 cannot cut inside frames, or packets {:?} lie outside 0..{n}",
                    p.packets
                )));
            }
        }
        match tags {
            None => w.write_all(&self.id3_raw)?,
            Some(t) => {
                let tag = id3::write(t, &self.id3_other).map_err(ContainerError::TooLarge)?;
                w.write_all(&tag)?;
            }
        }
        for p in plans {
            let frames = &self.index.packets()[p.packets.clone()];
            let (Some(a), Some(b)) = (frames.first(), frames.last()) else {
                continue;
            };
            // Consecutive frames are contiguous in the file.
            let from = a.byte_offset;
            let to = b.byte_offset + u64::from(b.byte_len);
            if frames
                .windows(2)
                .all(|f| f[0].byte_offset + u64::from(f[0].byte_len) == f[1].byte_offset)
            {
                r.seek(SeekFrom::Start(from))?;
                let copied = io::copy(&mut (&mut *r).take(to - from), w)?;
                if copied != to - from {
                    return Err(malformed(from + copied, "file shrank while copying"));
                }
            } else {
                // Junk between frames is left out.
                for f in frames {
                    let mut buf = vec![0u8; f.byte_len as usize];
                    r.seek(SeekFrom::Start(f.byte_offset))?;
                    r.read_exact(&mut buf)?;
                    w.write_all(&buf)?;
                }
            }
        }
        Ok(())
    }
}

/// Whether `frame` passes its CRC: A/52 7.10.1 places `crc2` so that
/// the CRC-16 (x^16 + x^15 + x^2 + 1, initial value 0) over the frame
/// after the sync word comes out zero.
pub fn crc_ok(frame: &[u8]) -> bool {
    frame.get(2..).is_some_and(|b| crc16(b) == 0)
}

/// CRC-16 with polynomial 0x8005, not reflected, initial value 0.
fn crc16(b: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in b {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Reads until `buf` is full or the input ends.
fn read_up_to<R: Read + ?Sized>(r: &mut R, buf: &mut [u8]) -> Result<usize> {
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

    /// A frame header: 48 kHz, frmsizecod 0 (128 bytes), bsid 8, the
    /// given acmod and lfeon placed after acmod's mix-level fields.
    fn header(acmod: u8, lfe: bool) -> Vec<u8> {
        let mut bits: u32 = u32::from(acmod);
        let mut n = 3;
        for present in [acmod & 1 != 0 && acmod != 1, acmod & 4 != 0, acmod == 2] {
            if present {
                bits <<= 2;
                n += 2;
            }
        }
        bits = (bits << 1) | u32::from(lfe);
        n += 1;
        let bits = bits << (16 - n);
        let mut h = vec![0x0B, 0x77, 0, 0, 0x00, 8 << 3];
        h.extend((bits as u16).to_be_bytes());
        h
    }

    #[test]
    fn channel_layouts() {
        for acmod in 0..8u8 {
            for lfe in [false, true] {
                let (p, size) = parse_header(&header(acmod, lfe)).expect("valid");
                assert_eq!((p.acmod, p.lfe, size), (acmod, lfe, 128), "{acmod}");
            }
        }
        let (p, _) = parse_header(&header(7, true)).expect("valid");
        assert_eq!(p.channels(), 6); // 5.1
        let (p, _) = parse_header(&header(2, false)).expect("valid");
        assert_eq!(p.channels(), 2);
    }

    #[test]
    fn invalid_headers() {
        let h = header(2, false);
        assert_eq!(parse_header(&h[..1]), Err(HeaderError::Truncated));
        assert_eq!(parse_header(&h[..5]), Err(HeaderError::Truncated));
        assert_eq!(
            parse_header(&[0x0B, 0x78, 0, 0, 0, 0, 0]),
            Err(HeaderError::Invalid)
        );
        let mut eac3 = h.clone();
        eac3[5] = 16 << 3;
        assert_eq!(parse_header(&eac3), Err(HeaderError::Eac3));
        let mut reserved = h.clone();
        reserved[4] = 0xC0; // fscod 3
        assert_eq!(parse_header(&reserved), Err(HeaderError::Invalid));
        reserved[4] = 38; // frmsizecod 38
        assert_eq!(parse_header(&reserved), Err(HeaderError::Invalid));
    }

    #[test]
    fn frames_junk_and_truncation() {
        let mut frame = header(2, false);
        frame.resize(126, 0x55);
        // crc2: the CRC of the rest, appended big-endian, zeroes the total.
        let crc = crc16(&frame[2..]);
        frame.extend(crc.to_be_bytes());
        assert!(crc_ok(&frame));
        // The frame before the junk is confirmed by its CRC alone.
        let mut file = frame.repeat(3);
        file.extend([1, 2, 3]); // junk
        file.extend(frame.repeat(2));
        file.extend(&frame[..50]); // truncated
        let f = Ac3File::open(&mut std::io::Cursor::new(&file)).expect("opens");
        assert_eq!(f.index().packets().len(), 5);
        assert_eq!(f.index().total_samples(), 5 * 1536);
        assert_eq!(f.warnings().len(), 2, "{:?}", f.warnings());
        assert_eq!(f.index().packets()[3].byte_offset, 3 * 128 + 3);
        // Without a valid CRC, a frame before junk counts as junk.
        let mut bad = frame.clone();
        bad[100] ^= 1;
        let mut file = bad.repeat(2);
        file.extend([1, 2, 3]);
        file.extend(&frame);
        let f = Ac3File::open(&mut std::io::Cursor::new(&file)).expect("opens");
        assert_eq!(f.index().packets().len(), 2);
    }
}
