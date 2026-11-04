//! MP3 files: MPEG-1/2/2.5 Audio Layer III elementary streams
//! (ISO/IEC 11172-3, ISO/IEC 13818-3), optionally with an ID3v2 tag in
//! front, a Xing/Info + LAME tag in the first frame and ID3v1/APE tags at
//! the end.
//!
//! ## The bit reservoir
//!
//! A frame's compressed audio ("main data") does not have to sit in the
//! frame itself: `main_data_begin` says how many bytes *before* the frame's
//! own data slot it starts, in the slots of earlier frames (at most 511
//! bytes for MPEG-1, 255 for MPEG-2). Copying frames verbatim from the
//! middle of a file therefore loses the start of their main data.
//!
//! This module never re-encodes. Instead the writer **repacks the
//! reservoir**: it collects each kept frame's main data bytes (exactly as
//! encoded) and lays them out anew over the output frames' slots, setting
//! every frame's `main_data_begin` accordingly. Where the data does not
//! fit, a frame's bitrate index is raised, which only enlarges its slot
//! (the extra space is zero padding the decoder never reads). The decoder
//! therefore sees exactly the same main data for every frame.
//!
//! ## Pre-roll and gapless trimming
//!
//! Decoded output of a frame also depends on its predecessor: the hybrid
//! filterbank overlaps each granule's IMDCT output with the previous
//! granule, and the polyphase synthesis filter keeps 512 samples of
//! history. Both reach back exactly one granule, so a frame decodes
//! correctly once the previous granule's spectrum was decoded correctly.
//! The writer puts one frame (MPEG-1, two granules) or two frames
//! (MPEG-2/2.5, one granule each) of pre-roll in front of the first kept
//! frame and hides it with the LAME tag's encoder delay. The LAME tag's
//! padding trims the end. Both make start and end sample-exact on gapless
//! decoders (ffmpeg, mpg123, iTunes); others play the pre-roll.
//!
//! A LAME tag holds one delay and one padding, so trimming is possible only
//! at the start and end of a file. At a join inside the file the frames are
//! simply consecutive; the frame after the join decodes with a foreign
//! predecessor granule, so up to one frame of output differs there (see
//! "Join points inside a file" in `docs/architecture.md`).
//!
//! The delay of a source with a LAME tag is honoured: the timeline starts
//! after `delay + 529` decoded samples and ends `padding - 529` samples
//! before the end of the last frame, as gapless players play it.
//!
//! ## Robustness
//!
//! Frames are found by their sync word and confirmed by a following frame.
//! Junk between frames is skipped with a warning; frames whose reservoir
//! bytes lie before the start of the file or before skipped junk are kept,
//! with the missing bytes replaced by zeros, and reported.
//!
//! ## Tags
//!
//! The source's tags come from the ID3v2 tag, with fields it lacks taken
//! from an ID3v1 tag. An export without new tags copies both tags
//! verbatim; one with new tags writes a fresh ID3v2.4 tag holding them
//! plus the source's other frames (pictures and the like, see
//! [`crate::tags::id3`]) and no ID3v1 tag, which could only hold a
//! truncated copy. APE tags are never copied.

pub mod frame;
pub mod xing;

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;

use lac_core::{
    AudioStreamInfo, CutCapabilities, CutPlan, PacketFlags, PacketIndex, PacketInfo, Tags,
};

use crate::window::Window;
use crate::{ContainerError, Result};
use frame::{FrameHeader, HeaderError, MAX_BITRATE_INDEX, SideInfo, Version};
use xing::{DECODER_DELAY, LameTag};

/// Virtual-offset distance between reservoir segments separated by junk,
/// larger than any `main_data_begin`, so lookups never cross a gap.
const GAP: u64 = 1 << 20;

/// One audio frame of the source.
#[derive(Debug, Clone)]
struct Frame {
    /// File offset of the header.
    offset: u64,
    header: FrameHeader,
    /// The side information bytes.
    side: Vec<u8>,
    info: SideInfo,
    /// Virtual offset of this frame's slot in the source reservoir (slots
    /// of consecutive frames are adjacent).
    voff: u64,
    /// Junk was skipped right before this frame.
    after_gap: bool,
}

impl Frame {
    fn slot_file_offset(&self) -> u64 {
        self.offset + self.header.slot_offset() as u64
    }
}

/// An opened MP3 file.
#[derive(Debug, Clone)]
pub struct Mp3File {
    frames: Vec<Frame>,
    /// Header of the first audio frame, the template for new frames.
    first: FrameHeader,
    /// Byte range of a leading ID3v2 tag (empty if none).
    id3v2: Range<u64>,
    /// Offset of a trailing ID3v1 tag.
    id3v1: Option<u64>,
    /// The tags parsed from both, and the ID3v2 frames to keep.
    id3: crate::tags::id3::Id3v2,
    lame: Option<LameTag>,
    /// Frame of each index entry.
    entry_frame: Vec<usize>,
    /// Raw offset within its frame at which each entry starts (non-zero
    /// only for an entry partly inside the encoder delay).
    entry_offset: Vec<u32>,
    vbr: bool,
    /// Decoder output (from the first frame on) that forms the timeline:
    /// encoder delay plus decoder delay skipped, end padding dropped.
    decode_keep: Range<u64>,
    index: PacketIndex,
    warnings: Vec<String>,
}

impl Mp3File {
    /// Scans the file and builds the packet index; decodes nothing.
    pub fn open<R: Read + Seek + ?Sized>(r: &mut R) -> Result<Self> {
        let len = r.seek(SeekFrom::End(0))?;
        let mut warnings = Vec::new();
        let start = crate::flac::skip_id3v2(r, len)?.min(len);
        let (end, id3v1) = trailing_tags(r, start, len, &mut warnings)?;
        let id3 = read_tags(r, start, id3v1, &mut warnings)?;

        let mut w = Window::new(r, len);
        let scan = scan(&mut w, start, end, &mut warnings)?;
        let Scan { mut frames, xing } = scan;
        let Some(first) = frames.first().map(|f| f.header) else {
            return Err(ContainerError::Unsupported("no MP3 frames found".into()));
        };
        // Frames must fit the reservoir model; report frames whose data is
        // (partly) missing.
        let mut missing = 0;
        let mut seg_start = 0;
        for f in &mut frames {
            if f.after_gap || f.voff == 0 {
                seg_start = f.voff;
            }
            let data_start = f.voff as i64 - f.info.main_data_begin as i64;
            if data_start < seg_start as i64 {
                missing += 1;
            }
            let max = f.info.main_data_begin + f.header.slot_len();
            if f.info.main_data_len > max {
                warnings.push(format!(
                    "frame at byte {} claims more main data than it can hold",
                    f.offset
                ));
                f.info.main_data_len = max;
            }
        }
        if missing > 0 {
            warnings.push(format!(
                "{missing} frame(s) refer to bit reservoir data before the start of the stream or a damaged part; it is replaced by silence"
            ));
        }

        let lame = xing.as_ref().and_then(|x| x.lame.clone());
        let spf = u64::from(first.samples());
        let total = frames.len() as u64 * spf;
        let (skip, stop) = match &lame {
            Some(l) => {
                let skip = (u64::from(l.delay) + u64::from(DECODER_DELAY)).min(total);
                let frames_match =
                    xing.as_ref().and_then(|x| x.frames) == Some(frames.len() as u32);
                if !frames_match {
                    warnings.push(
                        "the Xing frame count does not match the file; the end padding is ignored"
                            .into(),
                    );
                }
                let stop = if frames_match {
                    (total + u64::from(DECODER_DELAY))
                        .saturating_sub(u64::from(l.padding))
                        .min(total)
                } else {
                    total
                };
                (skip, stop.max(skip))
            }
            None => (0, total),
        };

        let mut packets = Vec::new();
        let mut entry_frame = Vec::new();
        let mut entry_offset = Vec::new();
        for (k, f) in frames.iter().enumerate() {
            let fs = k as u64 * spf;
            let (a, b) = (fs.max(skip), (fs + spf).min(stop));
            if a >= b {
                continue;
            }
            packets.push(PacketInfo {
                byte_offset: f.offset,
                byte_len: f.header.frame_len() as u32,
                first_sample: a - skip,
                n_samples: (b - a) as u32,
                // Encoder delay and padding inside this frame: hidden by the
                // LAME tag, but played if the frame lands at a join.
                hidden_before: (a - fs) as u32,
                hidden_after: (fs + spf - b) as u32,
                flags: PacketFlags { independent: false },
            });
            entry_frame.push(k);
            entry_offset.push((a - fs) as u32);
        }
        let vbr = frames
            .iter()
            .any(|f| f.header.bitrate_index != first.bitrate_index);
        let index = PacketIndex::new(
            AudioStreamInfo {
                sample_rate: first.sample_rate(),
                channels: first.channels(),
            },
            packets,
        )
        .map_err(|e| ContainerError::InvalidPlan(format!("internal index error: {e}")))?;
        Ok(Self {
            frames,
            first,
            id3v2: 0..start,
            id3v1,
            id3,
            lame,
            entry_frame,
            entry_offset,
            vbr,
            decode_keep: skip..stop,
            index,
            warnings,
        })
    }

    /// The source's tags (see the module documentation).
    pub fn tags(&self) -> &Tags {
        &self.id3.tags
    }

    /// One entry per audio frame with audible samples.
    pub fn index(&self) -> &PacketIndex {
        &self.index
    }

    /// Problems that were tolerated while opening the file.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Human-readable stream description.
    pub fn details(&self) -> String {
        let v = match self.first.version {
            Version::Mpeg1 => "MPEG-1",
            Version::Mpeg2 => "MPEG-2",
            Version::Mpeg25 => "MPEG-2.5",
        };
        let rate = if self.vbr {
            "VBR".to_owned()
        } else {
            format!("{} kbit/s", self.first.bitrate())
        };
        let gapless = if self.lame.is_some() {
            ", gapless info"
        } else {
            ""
        };
        format!("{v} Layer III, {rate}{gapless}")
    }

    /// Sample-exact at the start and end of the output (LAME tag), packet
    /// granular at joins.
    pub fn capabilities(&self) -> CutCapabilities {
        CutCapabilities {
            preroll_packets: 0,
            can_trim_start: true,
            can_trim_end: true,
            lossless_reencode: false,
            trims_at_file_ends_only: true,
        }
    }

    /// A single section of all frames in file order (also those inside the
    /// encoder delay, which build up the bit reservoir).
    pub fn decode_sections<R: Read + Seek + ?Sized>(
        &self,
        _r: &mut R,
    ) -> Result<Vec<crate::DecodeSection>> {
        Ok(vec![crate::DecodeSection {
            codec: crate::Codec::Mp3,
            sample_rate: self.first.sample_rate(),
            channels: self.index.info().channels,
            extradata: Vec::new(),
            // libavcodec emits every frame's samples as soon as it is
            // decoded, also those of the encoder delay (they are cut away
            // by `keep`).
            packet_ends: crate::decode::running_ends(
                self.frames.iter().map(|f| u64::from(f.header.samples())),
            ),
            keep: self.decode_keep.clone(),
        }])
    }

    /// The bytes of frame `packet` exactly as in the file. Its main data
    /// may lie in earlier frames; the decoder has seen those before.
    pub fn read_decode_packet<R: Read + Seek + ?Sized>(
        &self,
        r: &mut R,
        section: usize,
        packet: usize,
    ) -> Result<Vec<u8>> {
        if section != 0 {
            return Err(crate::decode::no_section(section));
        }
        let f = self
            .frames
            .get(packet)
            .ok_or_else(|| ContainerError::InvalidPlan(format!("no frame {packet}")))?;
        r.seek(SeekFrom::Start(f.offset))?;
        let mut out = Vec::with_capacity(f.header.frame_len());
        // The last frame of a truncated file may be cut short: hand the
        // decoder what exists, padded with zeros (silence in the reservoir
        // model, like `main_data`).
        r.take(f.header.frame_len() as u64).read_to_end(&mut out)?;
        out.resize(f.header.frame_len(), 0);
        Ok(out)
    }

    /// What the decoder reads for index entry `entry` apart from the
    /// reservoir pointer: the side information with `main_data_begin`
    /// zeroed, followed by the frame's main data. Equal for a frame and its
    /// copy in a cut, which tests use to check that nothing is re-encoded.
    pub fn audio_data<R: Read + Seek + ?Sized>(&self, r: &mut R, entry: usize) -> Result<Vec<u8>> {
        let k = *self
            .entry_frame
            .get(entry)
            .ok_or_else(|| ContainerError::InvalidPlan(format!("no packet {entry}")))?;
        let f = &self.frames[k];
        let mut out = f.side.clone();
        frame::set_main_data_begin(&f.header, &mut out, 0);
        out.extend(self.main_data(r, k)?);
        Ok(out)
    }

    /// The main data of frame `k`, from the source reservoir. Bytes that
    /// are not in the file read as zeros.
    fn main_data<R: Read + Seek + ?Sized>(&self, r: &mut R, k: usize) -> Result<Vec<u8>> {
        let f = &self.frames[k];
        let mut out = vec![0u8; f.info.main_data_len];
        let start = f.voff as i64 - f.info.main_data_begin as i64;
        // First frame whose slot reaches beyond `start`.
        let mut j = self.frames[..=k]
            .partition_point(|g| (g.voff + g.header.slot_len() as u64) as i64 <= start);
        let mut pos = start;
        let end = start + out.len() as i64;
        while pos < end && j <= k {
            let g = &self.frames[j];
            let (s0, s1) = (g.voff as i64, (g.voff + g.header.slot_len() as u64) as i64);
            let a = pos.max(s0);
            let b = end.min(s1);
            if a < b {
                r.seek(SeekFrom::Start(g.slot_file_offset() + (a - s0) as u64))?;
                let dst = &mut out[(a - start) as usize..(b - start) as usize];
                r.read_exact(dst)?;
            }
            pos = pos.max(s1);
            j += 1;
        }
        Ok(out)
    }

    /// Writes the `actual` ranges of `plans` as one MP3 file, with `tags`
    /// if given, else with the source's tags (see the module
    /// documentation).
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
        let plans: Vec<&CutPlan> = plans.iter().filter(|p| !p.packets.is_empty()).collect();
        let (Some(first), Some(last)) = (plans.first(), plans.last()) else {
            return Ok(());
        };
        let n = self.entry_frame.len();
        for (i, p) in plans.iter().enumerate() {
            if p.packets.end > n {
                return Err(ContainerError::InvalidPlan(format!(
                    "packets {:?} outside the index (length {n})",
                    p.packets
                )));
            }
            if (p.trim_start != 0 && i != 0) || (p.trim_end != 0 && i != plans.len() - 1) {
                return Err(ContainerError::InvalidPlan(
                    "MP3 can only trim at the start and end of the file (use plan_cuts)".into(),
                ));
            }
        }
        let spf = self.first.samples() as usize;

        // Source frames to write: pre-roll, then every kept frame.
        let e0 = first.packets.start;
        let f0 = self.entry_frame[e0];
        let mut pre = f0;
        let wanted = if self.first.granules() == 2 { 1 } else { 2 };
        while f0 - pre < wanted && pre > 0 && !self.frames[pre].after_gap {
            pre -= 1;
        }
        let mut src: Vec<usize> = (pre..f0).collect();
        for p in &plans {
            src.extend(p.packets.clone().map(|e| self.entry_frame[e]));
        }

        // Raw samples of the output to skip and to drop at the end.
        let mut skip = (f0 - pre) * spf
            + self.entry_offset[e0] as usize
            + usize::try_from(first.trim_start).unwrap_or(usize::MAX);
        let e1 = last.packets.end - 1;
        let kept_in_last = self.entry_offset[e1] as usize
            + self.index.packets()[e1].n_samples as usize
            - usize::try_from(last.trim_end).unwrap_or(0);
        let padding = spf - kept_in_last + DECODER_DELAY as usize;

        // Silent carrier frames in front: needed when the delay would be
        // below the decoder delay, and to hold reservoir data for the first
        // frame if its own slot cannot.
        let mut carriers = usize::from(skip < DECODER_DELAY as usize);
        let layout = loop {
            match self.layout(&src, carriers) {
                Ok(l) => break l,
                Err(NeedCarrier) if carriers < 2 => carriers += 1,
                Err(NeedCarrier) => {
                    return Err(ContainerError::TooLarge(
                        "main data does not fit even at the highest bitrate".into(),
                    ));
                }
            }
        };
        skip += carriers * spf;
        let delay = skip - DECODER_DELAY as usize;
        if delay > 4095 || padding > 4095 {
            return Err(ContainerError::TooLarge(
                "encoder delay exceeds the LAME tag".into(),
            ));
        }

        match tags {
            Some(t) => {
                let tag = crate::tags::id3::write(t, &self.id3.other)
                    .map_err(ContainerError::TooLarge)?;
                w.write_all(&tag)?;
            }
            None => {
                r.seek(SeekFrom::Start(self.id3v2.start))?;
                io::copy(&mut (&mut *r).take(self.id3v2.end - self.id3v2.start), w)?;
            }
        }

        // Placeholder for the Xing frame, written at the end.
        let xh = self.xing_header();
        let xing_pos = w.stream_position()?;
        w.write_all(&vec![0u8; xh.frame_len()])?;

        let mut offsets = Vec::with_capacity(layout.len());
        let mut music_crc = 0u16;
        let mut pos = xh.frame_len() as u64;
        // Main data placed but not yet written, starting at virtual `base`.
        let mut buf: Vec<u8> = Vec::new();
        let mut base = 0usize;
        let mut next = 0;
        for (i, out) in layout.iter().enumerate() {
            let slot = out.header.slot_len();
            let slot_end = out.slot_start + slot;
            // Place the main data of every frame starting before this
            // slot's end.
            while next < layout.len() && layout[next].data_start < slot_end {
                let data = match layout[next].src {
                    Some(k) => self.main_data(r, k)?,
                    None => Vec::new(),
                };
                let at = layout[next].data_start - base;
                if buf.len() < at {
                    buf.resize(at, 0);
                }
                buf.truncate(at);
                buf.extend_from_slice(&data);
                next += 1;
            }
            let mut bytes = Vec::with_capacity(out.header.frame_len());
            bytes.extend(out.header.raw);
            let mut side = match out.src {
                Some(k) => self.frames[k].side.clone(),
                None => vec![0; out.header.side_info_len()],
            };
            frame::set_main_data_begin(&out.header, &mut side, out.slot_start - out.data_start);
            if out.header.protected {
                bytes.extend(frame::frame_crc(&out.header.raw, &side).to_be_bytes());
            }
            bytes.extend(&side);
            let from = out.slot_start - base;
            let have = buf.len().saturating_sub(from).min(slot);
            bytes.extend_from_slice(&buf[from.min(buf.len())..from.min(buf.len()) + have]);
            bytes.resize(out.header.frame_len(), 0);
            // Everything before the next slot is written.
            let drop = (slot_end - base).min(buf.len());
            buf.drain(..drop);
            base = slot_end;
            debug_assert_eq!(i, offsets.len());
            offsets.push(pos);
            music_crc = xing::crc16_arc_update(music_crc, &bytes);
            pos += bytes.len() as u64;
            w.write_all(&bytes)?;
        }
        let bytes = u32::try_from(pos)
            .map_err(|_| ContainerError::TooLarge("MP3 stream exceeds 4 GiB".into()))?;

        if let Some(at) = self.id3v1.filter(|_| tags.is_none()) {
            r.seek(SeekFrom::Start(at))?;
            io::copy(&mut (&mut *r).take(128), w)?;
        }
        let end = w.stream_position()?;

        // The Xing/Info frame with the LAME tag.
        let lame = LameTag {
            delay: delay as u32,
            padding: padding as u32,
            ..self.lame.clone().unwrap_or(LameTag {
                info: *b"LAME3.100\0\0\0\0\0\0\0\0\0\0\0\0",
                delay: 0,
                padding: 0,
                tail: [0; 4],
            })
        };
        let vbr = layout
            .iter()
            .any(|o| o.header.bitrate_index != layout[0].header.bitrate_index);
        let mut xf = vec![0u8; xh.frame_len()];
        xf[..4].copy_from_slice(&xh.raw);
        xing::write(
            &mut xf,
            xh.slot_offset(),
            &xing::XingWrite {
                vbr,
                frames: layout.len() as u32,
                bytes,
                frame_offsets: &offsets,
                lame: &lame,
                music_crc,
            },
        );
        w.seek(SeekFrom::Start(xing_pos))?;
        w.write_all(&xf)?;
        w.seek(SeekFrom::Start(end))?;
        Ok(())
    }

    /// Header of the Xing frame: the stream's parameters, no CRC, and the
    /// lowest bitrate whose frame holds the Xing header and LAME tag.
    fn xing_header(&self) -> FrameHeader {
        let t = self.first.unprotected();
        (1..=MAX_BITRATE_INDEX)
            .map(|b| t.with_bitrate(b, false))
            .find(|h| h.frame_len() >= h.slot_offset() + xing::LEN)
            .unwrap_or_else(|| t.with_bitrate(MAX_BITRATE_INDEX, false))
    }

    /// Lays out the main data of `carriers` silent frames followed by the
    /// source frames `src` over the output slots; see the module docs.
    fn layout(
        &self,
        src: &[usize],
        carriers: usize,
    ) -> std::result::Result<Vec<OutFrame>, NeedCarrier> {
        let carrier = self.first.unprotected().with_bitrate(1, false);
        let mut out: Vec<OutFrame> = (0..carriers)
            .map(|_| OutFrame {
                src: None,
                header: carrier,
                data_len: 0,
                slot_start: 0,
                data_start: 0,
            })
            .chain(src.iter().map(|&k| OutFrame {
                src: Some(k),
                header: self.frames[k].header,
                data_len: self.frames[k].info.main_data_len,
                slot_start: 0,
                data_start: 0,
            }))
            .collect();
        let max_mdb = self.first.max_main_data_begin();
        let mut i = 0;
        while i < out.len() {
            let (slot_start, prev_end) = match i {
                0 => (0, 0),
                _ => {
                    let p = &out[i - 1];
                    (
                        p.slot_start + p.header.slot_len(),
                        p.data_start + p.data_len,
                    )
                }
            };
            let o = &mut out[i];
            o.slot_start = slot_start;
            o.data_start = prev_end.max(slot_start.saturating_sub(max_mdb));
            if o.data_start + o.data_len <= slot_start + o.header.slot_len() {
                i += 1;
                continue;
            }
            // Does not fit. A larger slot for this frame always helps.
            // Otherwise, if the data start is held back by the previous
            // frame's data (not by the `main_data_begin` limit), a larger
            // earlier slot moves this slot back and makes room.
            let limited_by_prev = o.data_start > slot_start.saturating_sub(max_mdb);
            let j = if o.header.bitrate_index < MAX_BITRATE_INDEX {
                i
            } else if limited_by_prev {
                (0..i)
                    .rev()
                    .find(|&j| out[j].header.bitrate_index < MAX_BITRATE_INDEX)
                    .ok_or(NeedCarrier)?
            } else {
                return Err(NeedCarrier);
            };
            let h = out[j].header;
            out[j].header = h.with_bitrate(h.bitrate_index + 1, false);
            i = j;
        }
        Ok(out)
    }
}

/// One frame of the output.
#[derive(Debug, Clone)]
struct OutFrame {
    /// Source frame, or `None` for a silent carrier frame.
    src: Option<usize>,
    header: FrameHeader,
    data_len: usize,
    /// Virtual offset of this frame's slot in the output reservoir.
    slot_start: usize,
    /// Virtual offset of its main data.
    data_start: usize,
}

/// The layout needs (more) carrier frames in front.
#[derive(Debug)]
struct NeedCarrier;

struct Scan {
    frames: Vec<Frame>,
    xing: Option<xing::XingInfo>,
}

/// Parses the ID3v2 tag in `0..id3v2_end` and the ID3v1 tag at `id3v1`.
fn read_tags<R: Read + Seek + ?Sized>(
    r: &mut R,
    id3v2_end: u64,
    id3v1: Option<u64>,
    warnings: &mut Vec<String>,
) -> Result<crate::tags::id3::Id3v2> {
    let mut b = vec![0u8; usize::try_from(id3v2_end).unwrap_or(0)];
    r.seek(SeekFrom::Start(0))?;
    r.read_exact(&mut b)?;
    let mut id3 = crate::tags::id3::parse(&b);
    warnings.append(&mut id3.warnings);
    if let Some(at) = id3v1 {
        let mut v1 = [0u8; 128];
        r.seek(SeekFrom::Start(at))?;
        r.read_exact(&mut v1)?;
        crate::tags::id3::merge_v1(&mut id3.tags, &v1);
    }
    Ok(id3)
}

/// Finds a trailing ID3v1 tag and APE tag; returns the end of the audio
/// data and the ID3v1 offset.
fn trailing_tags<R: Read + Seek + ?Sized>(
    r: &mut R,
    start: u64,
    len: u64,
    warnings: &mut Vec<String>,
) -> Result<(u64, Option<u64>)> {
    let mut end = len;
    let mut id3v1 = None;
    let mut tag = [0u8; 3];
    if end >= start + 128 {
        r.seek(SeekFrom::Start(end - 128))?;
        r.read_exact(&mut tag)?;
        if &tag == b"TAG" {
            end -= 128;
            id3v1 = Some(end);
        }
    }
    // APEv2 footer: "APETAGEX", version, size (items + footer), count,
    // flags (bit 31: a header precedes the items).
    if end >= start + 32 {
        let mut f = [0u8; 32];
        r.seek(SeekFrom::Start(end - 32))?;
        r.read_exact(&mut f)?;
        if &f[..8] == b"APETAGEX" {
            let size = u64::from(u32::from_le_bytes([f[12], f[13], f[14], f[15]]));
            let header = if f[23] & 0x80 != 0 { 32 } else { 0 };
            end = end.saturating_sub(size + header).max(start);
            warnings.push("an APE tag at the end is not copied".into());
        }
    }
    Ok((end, id3v1))
}

/// Finds all frames in `start..end`.
fn scan<R: Read + Seek + ?Sized>(
    w: &mut Window<R>,
    start: u64,
    end: u64,
    warnings: &mut Vec<String>,
) -> Result<Scan> {
    let mut frames: Vec<Frame> = Vec::new();
    let mut xing = None;
    let mut lock: Option<FrameHeader> = None;
    let mut seen_other_layer = false;
    let mut seen_free_format = false;
    let mut pos = start;
    let mut voff = 0u64;
    let mut junk_since: Option<u64> = None;
    while pos + 4 <= end {
        w.keep = pos;
        let h = match FrameHeader::parse(w.bytes(pos, 4)?) {
            Ok(h) if lock.is_none_or(|l| l.compatible(&h)) => Some(h),
            Ok(_) => None,
            Err(HeaderError::OtherLayer) => {
                seen_other_layer = true;
                None
            }
            Err(HeaderError::FreeFormat) => {
                seen_free_format = true;
                None
            }
            Err(HeaderError::Invalid) => None,
        };
        // When resynchronising (at the start and after junk) a frame is
        // accepted only if another compatible frame follows it or it ends
        // at the end of the audio data. A frame directly following the
        // previous one needs no such confirmation.
        let contiguous = lock.is_some() && junk_since.is_none();
        let accepted = match h {
            Some(h) => {
                let next = pos + h.frame_len() as u64;
                if next > end {
                    if lock.is_some() {
                        warnings.push(format!("the last frame at byte {pos} is truncated"));
                        break;
                    }
                    None
                } else if contiguous || next == end || next + 4 > end {
                    Some(h)
                } else {
                    let n = FrameHeader::parse(w.bytes(next, 4)?);
                    n.is_ok_and(|n| h.compatible(&n)).then_some(h)
                }
            }
            None => None,
        };
        let Some(h) = accepted else {
            junk_since.get_or_insert(pos);
            pos += 1;
            continue;
        };
        let after_gap = junk_since.take().is_some_and(|j| {
            warnings.push(format!(
                "skipped {} bytes of non-MP3 data at byte {j}",
                pos - j
            ));
            true
        }) && !frames.is_empty();
        let bytes = w.bytes(pos, h.frame_len() as u64)?.to_vec();
        let side = bytes[h.side_info_offset()..h.slot_offset()].to_vec();
        if lock.is_none() {
            lock = Some(h);
            // The first frame may be a Xing/Info/VBRI frame without audio.
            if let Some(x) = xing::parse(&bytes, h.slot_offset()) {
                xing = Some(x);
                pos += h.frame_len() as u64;
                continue;
            }
        }
        let info = frame::parse_side_info(&h, &side).ok_or_else(|| ContainerError::Malformed {
            offset: pos,
            reason: "side information ends early".into(),
        })?;
        if after_gap {
            voff += GAP;
        }
        frames.push(Frame {
            offset: pos,
            header: h,
            side,
            info,
            voff,
            after_gap,
        });
        voff += h.slot_len() as u64;
        pos += h.frame_len() as u64;
    }
    if let Some(j) = junk_since
        && !frames.is_empty()
    {
        warnings.push(format!(
            "skipped {} bytes of non-MP3 data at byte {j}",
            end - j
        ));
    }
    if frames.is_empty() {
        let what = if seen_free_format {
            "free-format MP3 is not supported"
        } else if seen_other_layer {
            "MPEG audio Layer I/II is not supported"
        } else {
            "no MP3 frames found"
        };
        return Err(ContainerError::Unsupported(what.into()));
    }
    Ok(Scan { frames, xing })
}
