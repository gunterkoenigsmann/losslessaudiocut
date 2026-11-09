//! Raw AAC in ADTS (Audio Data Transport Stream), `.aac` (ISO/IEC
//! 14496-3 1.A.2.2, "Audio_Data_Transport_Stream frame"), optionally with
//! an ID3v2 tag in front.
//!
//! Radio and streaming captures, and some recorders, keep AAC this way: a
//! plain sequence of frames, each with a small header, without any
//! container.
//!
//! ## Frames
//!
//! Each frame starts with a 7-byte header (9 bytes with the CRC that
//! follows it when `protection_absent` is 0), ISO/IEC 14496-3 Table 1.A.5
//! and 1.A.6:
//!
//! ```text
//! syncword 12 (0xFFF) | ID 1 | layer 2 (0) | protection_absent 1
//! profile 2 (object type - 1) | sampling_frequency_index 4 | private 1
//! channel_configuration 3 | original 1 | home 1
//! copyright_id_bit 1 | copyright_id_start 1 | aac_frame_length 13
//! adts_buffer_fullness 11 | number_of_raw_data_blocks_in_frame 2
//! ```
//!
//! `aac_frame_length` counts the header too. A frame normally holds one
//! raw data block of 1024 samples per channel; frames with several blocks
//! (`number_of_raw_data_blocks_in_frame` > 0, which encoders practically
//! never write) are refused, and so is `channel_configuration` 0 (the
//! layout is in a program config element inside the audio). Frames are
//! found by their sync word and a plausible header, confirmed by the next
//! frame's header or the end of the file; junk in between is skipped with
//! a warning. A frame right before junk cannot be confirmed and counts as
//! junk too.
//!
//! The decoder gets each raw data block without its header, plus an
//! AudioSpecificConfig built from the first header (ISO/IEC 14496-3
//! 1.6.2.1: object type, sampling frequency index, channel configuration).
//! HE-AAC (SBR) in ADTS is signalled only implicitly and decodes to twice
//! the samples; it is untested (issue #74).
//!
//! ## Cutting
//!
//! An AAC frame's output is the overlap-add of its inverse MDCT with the
//! second half of the previous frame's (ISO/IEC 14496-3 4.6.11), so a
//! frame decoded after a different predecessor differs for all of its
//! 1024 samples, and is exact from the next frame on (checked with ffmpeg:
//! exactly one frame). ADTS has no way to hide samples, so cuts are
//! frame-granular and one pre-roll frame is copied in front of every kept
//! range ([`CutCapabilities::preroll_packets`]): the wanted frames decode
//! exactly, and the audible pre-roll frame is the one-frame transient at
//! the file's start (unless it is the source's start) and after each join.
//! See "Join points inside a file" in `docs/architecture.md`.
//!
//! Perceptual noise substitution decodes to different noise after any
//! cut, as in MP4 ([`crate::mp4::aac`], issue #72).
//!
//! ## Tags
//!
//! ADTS has no tags of its own. An ID3v2 tag in front, as taggers write
//! it, is read; an export copies it verbatim, or writes a new ID3v2.4 tag
//! with the given tags and the source's other frames (see
//! [`crate::tags::id3`]).

use std::io::{self, Read, Seek, SeekFrom, Write};

use lac_core::{
    AudioStreamInfo, CutCapabilities, CutPlan, PacketFlags, PacketIndex, PacketInfo, Tags,
};

use crate::mp4::aac;
use crate::tags::id3;
use crate::window::Window;
use crate::{ContainerError, Result};

/// Samples per channel in one raw data block.
pub const FRAME_SAMPLES: u32 = 1024;

/// Sample rates per `sampling_frequency_index` (ISO/IEC 14496-3 Table
/// 1.18); 13 and 14 are reserved, 15 (explicit rate) is not allowed in
/// ADTS.
const SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// The header fields that must stay the same throughout the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamParams {
    /// MPEG-4 audio object type (`profile` + 1): 1 Main, 2 LC, 3 SSR,
    /// 4 LTP.
    pub object_type: u8,
    /// `sampling_frequency_index`, see [`StreamParams::sample_rate`].
    pub sf_index: u8,
    /// `channel_configuration` (ISO/IEC 14496-3 Table 1.19): 1..6 is the
    /// channel count except 7 = 7.1 (8 channels).
    pub channel_config: u8,
}

impl StreamParams {
    /// Sample rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        SAMPLE_RATES[usize::from(self.sf_index)]
    }

    /// The profile's name, e.g. "AAC LC" (ISO/IEC 14496-3 Table 1.A.11).
    pub fn profile_name(&self) -> &'static str {
        match self.object_type {
            1 => "AAC Main",
            2 => "AAC LC",
            3 => "AAC SSR",
            _ => "AAC LTP",
        }
    }

    /// Number of output channels.
    pub fn channels(&self) -> u16 {
        match self.channel_config {
            7 => 8,
            c => u16::from(c),
        }
    }

    /// The AudioSpecificConfig a decoder needs (ISO/IEC 14496-3 1.6.2.1):
    /// audioObjectType (5 bits), samplingFrequencyIndex (4),
    /// channelConfiguration (4), then GASpecificConfig's
    /// frameLengthFlag, dependsOnCoreCoder and extensionFlag, all 0
    /// (1024-sample frames, no core coder, no error resilience).
    pub fn audio_specific_config(&self) -> [u8; 2] {
        let v = (u16::from(self.object_type) << 11)
            | (u16::from(self.sf_index) << 7)
            | (u16::from(self.channel_config) << 3);
        v.to_be_bytes()
    }
}

/// A parsed frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// The stream parameters it announces.
    pub params: StreamParams,
    /// Header length: 7, or 9 with the CRC.
    pub header_len: u8,
    /// `aac_frame_length`: the whole frame, header included.
    pub frame_len: u16,
    /// `number_of_raw_data_blocks_in_frame` + 1.
    pub blocks: u8,
}

/// Why bytes are not an ADTS frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// Too few bytes to parse it.
    Truncated,
    /// No sync word, a nonzero layer, reserved values, or a frame length
    /// shorter than the header.
    Invalid,
}

/// Parses a frame header at the start of `b`.
pub fn parse_header(b: &[u8]) -> std::result::Result<Header, HeaderError> {
    if b.len() < 2 {
        return Err(HeaderError::Truncated);
    }
    // Sync word 0xFFF and layer 00; ID (bit 3) and protection_absent
    // (bit 0) may be anything.
    if b[0] != 0xFF || b[1] & 0xF6 != 0xF0 {
        return Err(HeaderError::Invalid);
    }
    if b.len() < 7 {
        return Err(HeaderError::Truncated);
    }
    let protection_absent = b[1] & 1 != 0;
    let object_type = (b[2] >> 6) + 1;
    let sf_index = (b[2] >> 2) & 0x0F;
    let channel_config = ((b[2] & 1) << 2) | (b[3] >> 6);
    let frame_len = (u16::from(b[3] & 0x03) << 11) | (u16::from(b[4]) << 3) | u16::from(b[5] >> 5);
    let blocks = (b[6] & 0x03) + 1;
    let header_len = if protection_absent { 7 } else { 9 };
    if usize::from(sf_index) >= SAMPLE_RATES.len() || frame_len <= u16::from(header_len) {
        return Err(HeaderError::Invalid);
    }
    Ok(Header {
        params: StreamParams {
            object_type,
            sf_index,
            channel_config,
        },
        header_len,
        frame_len,
        blocks,
    })
}

/// Whether `b` starts with an ADTS sync word and layer 0, the test the
/// format detection uses.
pub fn looks_like_adts(b: &[u8]) -> bool {
    b.len() >= 2 && b[0] == 0xFF && b[1] & 0xF6 == 0xF0
}

/// An opened ADTS file.
#[derive(Debug, Clone)]
pub struct AdtsFile {
    params: StreamParams,
    /// Header length of each frame (7 or 9), parallel to the index.
    header_lens: Vec<u8>,
    /// The ID3v2 tag in front, verbatim (empty without one).
    id3_raw: Vec<u8>,
    tags: Tags,
    /// ID3 frames [`Tags`] does not hold.
    id3_other: Vec<id3::RawFrame>,
    index: PacketIndex,
    warnings: Vec<String>,
}

impl AdtsFile {
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
        let mut header_lens = Vec::new();
        let mut params: Option<StreamParams> = None;
        let mut pos = start;
        let mut junk_from: Option<u64> = None;
        while pos < len {
            w.keep = pos;
            let frame = match parse_header(w.bytes(pos, 7)?) {
                Ok(h) => {
                    let end = pos + u64::from(h.frame_len);
                    if end > len {
                        warnings.push(format!(
                            "the last frame at byte {pos} is truncated and was dropped"
                        ));
                        break;
                    }
                    // Confirmed by the end of the file or a next header
                    // of the same stream (the fixed header, Table 1.A.6,
                    // stays the same, so its sync word alone is weak).
                    let next = parse_header(w.bytes(end, 7)?).ok();
                    let confirmed = end == len || next.is_some_and(|n| n.params == h.params);
                    confirmed.then_some(h)
                }
                Err(HeaderError::Truncated) => {
                    warnings.push(format!("ignoring {} trailing bytes", len - pos));
                    break;
                }
                Err(HeaderError::Invalid) => None,
            };
            let Some(h) = frame else {
                junk_from.get_or_insert(pos);
                pos += 1;
                continue;
            };
            if let Some(j) = junk_from.take() {
                warnings.push(format!("skipped {} bytes of junk at byte {j}", pos - j));
            }
            match params {
                None => {
                    if h.params.channel_config == 0 {
                        return Err(ContainerError::Unsupported(
                            "ADTS with the channel layout in the audio data (channel configuration 0)"
                                .into(),
                        ));
                    }
                    params = Some(h.params);
                }
                Some(q) if q != h.params => {
                    warnings.push(format!(
                        "the stream changes from {} to {} channels or another sample rate or profile at byte {pos}; the rest is ignored",
                        q.channels(),
                        h.params.channels()
                    ));
                    break;
                }
                _ => {}
            }
            if h.blocks != 1 {
                return Err(ContainerError::Unsupported(format!(
                    "ADTS frames with {} raw data blocks (at byte {pos})",
                    h.blocks
                )));
            }
            packets.push(PacketInfo {
                byte_offset: pos,
                byte_len: u32::from(h.frame_len),
                first_sample: packets.len() as u64 * u64::from(FRAME_SAMPLES),
                n_samples: FRAME_SAMPLES,
                hidden_before: 0,
                hidden_after: 0,
                flags: PacketFlags { independent: false },
            });
            header_lens.push(h.header_len);
            pos += u64::from(h.frame_len);
        }
        if let Some(j) = junk_from {
            warnings.push(format!("skipped {} bytes of junk at byte {j}", len - j));
        }
        let params =
            params.ok_or_else(|| ContainerError::Unsupported("no ADTS frame found".into()))?;
        let info = AudioStreamInfo {
            sample_rate: params.sample_rate(),
            channels: params.channels(),
        };
        let index = PacketIndex::new(info, packets)
            .map_err(|e| ContainerError::InvalidPlan(format!("internal index error: {e}")))?;
        Ok(Self {
            params,
            header_lens,
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

    /// Frame-granular, with one audible pre-roll frame (see the module
    /// documentation).
    pub fn capabilities(&self) -> CutCapabilities {
        CutCapabilities {
            preroll_packets: 1,
            can_trim_start: false,
            can_trim_end: false,
            lossless_reencode: false,
            trims_at_file_ends_only: false,
        }
    }

    /// Whether any frame codes a band with perceptual noise substitution
    /// (see [`crate::mp4::aac`]). Reads frames until one does; only AAC
    /// LC streams are checked, the other profiles are rare in ADTS.
    pub fn uses_noise_substitution<R: Read + Seek + ?Sized>(&self, r: &mut R) -> Result<bool> {
        if self.params.object_type != 2 {
            return Ok(false);
        }
        // As in `Mp4File::uses_noise_substitution`: the first channel's
        // section data lies well within 128 bytes of the raw data block.
        let mut head = [0u8; 128];
        for (p, &h) in self.index.packets().iter().zip(&self.header_lens) {
            let n = (p.byte_len as usize - usize::from(h)).min(head.len());
            r.seek(SeekFrom::Start(p.byte_offset + u64::from(h)))?;
            r.read_exact(&mut head[..n])?;
            if aac::first_channel(&head[..n]).is_some_and(|c| c.uses_noise_substitution()) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// One section of all frames, with the AudioSpecificConfig of the
    /// first header as extradata.
    pub fn decode_sections<R: Read + Seek + ?Sized>(
        &self,
        _r: &mut R,
    ) -> Result<Vec<crate::DecodeSection>> {
        Ok(vec![crate::DecodeSection {
            codec: crate::Codec::Aac,
            sample_rate: self.params.sample_rate(),
            channels: self.params.channels(),
            extradata: self.params.audio_specific_config().to_vec(),
            packet_ends: crate::decode::running_ends(
                self.index.packets().iter().map(|p| u64::from(p.n_samples)),
            ),
            keep: 0..self.index.total_samples(),
        }])
    }

    /// The raw data block of frame `packet`, without its header.
    pub fn read_decode_packet<R: Read + Seek + ?Sized>(
        &self,
        r: &mut R,
        section: usize,
        packet: usize,
    ) -> Result<Vec<u8>> {
        if section != 0 {
            return Err(crate::decode::no_section(section));
        }
        let mut frame = crate::decode::read_index_packet(&self.index, r, packet)?;
        let h = self.header_lens.get(packet).copied().unwrap_or(7);
        frame.drain(..usize::from(h).min(frame.len()));
        Ok(frame)
    }

    /// Writes the frames of `plans` (pre-roll frames included), joined,
    /// after an ID3v2 tag: the source's verbatim without `tags`, else a
    /// new one (none if `tags` is empty and the source has no other ID3
    /// frames). Frames are copied verbatim, headers included: an ADTS
    /// header describes only its own frame.
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
                    "ADTS cannot cut inside frames, or packets {:?} lie outside 0..{n}",
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
            let from = a.byte_offset;
            let to = b.byte_offset + u64::from(b.byte_len);
            if frames
                .windows(2)
                .all(|f| f[0].byte_offset + u64::from(f[0].byte_len) == f[1].byte_offset)
            {
                // Consecutive frames are contiguous: one copy.
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
    use std::io::Cursor;

    /// A frame of `len` bytes: AAC LC, 44.1 kHz (index 4), stereo, no CRC,
    /// one raw data block, the payload filled with `fill`.
    fn frame(len: u16, fill: u8) -> Vec<u8> {
        let mut f = vec![
            0xFF,
            0xF1,
            (1 << 6) | (4 << 2), // profile 1 (LC), sf index 4, private 0, chan bit 2 = 0
            (2 << 6) | ((len >> 11) as u8 & 0x03),
            (len >> 3) as u8,
            ((len & 7) as u8) << 5 | 0x1F, // buffer fullness 0x7FF (VBR)
            0xFC,                          // fullness, 0 extra blocks
        ];
        f.resize(usize::from(len), fill);
        f
    }

    fn open(bytes: &[u8]) -> Result<AdtsFile> {
        AdtsFile::open(&mut Cursor::new(bytes))
    }

    #[test]
    fn headers_are_parsed() {
        let h = parse_header(&frame(300, 0)).expect("valid");
        assert_eq!(
            h,
            Header {
                params: StreamParams {
                    object_type: 2,
                    sf_index: 4,
                    channel_config: 2
                },
                header_len: 7,
                frame_len: 300,
                blocks: 1
            }
        );
        assert_eq!(h.params.sample_rate(), 44_100);
        assert_eq!(h.params.channels(), 2);
        // ISO/IEC 14496-3 1.6.2.1: 00010 0100 0010 0 0 0 = 0x12 0x10, the
        // AudioSpecificConfig ffmpeg writes for 44.1 kHz stereo AAC LC.
        assert_eq!(h.params.audio_specific_config(), [0x12, 0x10]);
        let mut crc = frame(300, 0);
        crc[1] = 0xF0; // protection_absent 0: a CRC follows
        assert_eq!(parse_header(&crc).expect("valid").header_len, 9);
        // The channel configuration's top bit is the last bit of byte 2.
        let mut six = frame(300, 0);
        six[2] |= 1;
        assert_eq!(parse_header(&six).expect("valid").params.channel_config, 6);
    }

    #[test]
    fn invalid_headers() {
        let f = frame(300, 0);
        assert_eq!(parse_header(&f[..1]), Err(HeaderError::Truncated));
        assert_eq!(parse_header(&f[..6]), Err(HeaderError::Truncated));
        let mut mp3 = f.clone();
        mp3[1] = 0xFB; // MPEG-1 layer III
        assert_eq!(parse_header(&mp3), Err(HeaderError::Invalid));
        assert!(!looks_like_adts(&mp3));
        let mut reserved = f.clone();
        reserved[2] = (1 << 6) | (13 << 2); // sf index 13
        assert_eq!(parse_header(&reserved), Err(HeaderError::Invalid));
        assert_eq!(parse_header(&frame(7, 0)), Err(HeaderError::Invalid));
    }

    #[test]
    fn frames_junk_and_truncation() {
        let f = frame(200, 0x55);
        let mut file = f.repeat(3);
        file.extend([1, 2, 3]); // junk: the third frame cannot be confirmed
        file.extend(f.repeat(2));
        file.extend(&f[..50]); // truncated
        let a = open(&file).expect("opens");
        assert_eq!(a.index().packets().len(), 4);
        assert_eq!(a.index().total_samples(), 4 * 1024);
        assert_eq!(a.warnings().len(), 2, "{:?}", a.warnings());
        assert_eq!(a.index().packets()[2].byte_offset, 3 * 200 + 3);
        // The decoder gets the block without its header.
        let block = a
            .read_decode_packet(&mut Cursor::new(&file), 0, 2)
            .expect("read");
        assert_eq!(block, vec![0x55; 193]);
    }

    #[test]
    fn unsupported_layouts_are_refused() {
        let mut pce = frame(100, 0);
        pce[3] &= 0x3F; // channel configuration 0
        assert!(matches!(
            open(&pce.repeat(2)),
            Err(ContainerError::Unsupported(_))
        ));
        let mut blocks = frame(100, 0);
        blocks[6] |= 1; // two raw data blocks
        assert!(matches!(
            open(&blocks.repeat(2)),
            Err(ContainerError::Unsupported(_))
        ));
        assert!(matches!(open(&[]), Err(ContainerError::Unsupported(_))));
    }

    #[test]
    fn a_change_of_stream_parameters_ends_the_stream() {
        let mut mono = frame(100, 0);
        mono[3] = (1 << 6) | (mono[3] & 0x3F);
        let mut file = frame(100, 0).repeat(2);
        file.extend(mono.repeat(2));
        let a = open(&file).expect("opens");
        // The second stereo frame is not confirmed by a mono successor.
        assert_eq!(a.index().packets().len(), 1);
        assert!(!a.warnings().is_empty());
    }

    #[test]
    fn a_cut_copies_frames_and_the_tag() {
        let frames: Vec<Vec<u8>> = (0..5).map(|i| frame(64, i)).collect();
        let file = frames.concat();
        let a = open(&file).expect("opens");
        let plan = lac_core::plan_cut(a.index(), a.capabilities(), 2048..3072).expect("plan");
        assert_eq!(plan.packets, 1..3, "one pre-roll frame");
        let mut out = Vec::new();
        a.write_cut(&mut Cursor::new(&file), &[plan], None, &mut out)
            .expect("writes");
        assert_eq!(out, [frames[1].clone(), frames[2].clone()].concat());
    }
}
