//! MP4 / M4A files (ISO/IEC 14496-12, the ISO base media file format, and
//! 14496-14, the MP4 file format) with AAC or ALAC audio.
//!
//! Phones, iTunes and most podcast tools record and distribute AAC this
//! way (`.m4a`, `.m4b`, `.mp4`); Apple Lossless (ALAC) uses the same
//! container. QuickTime `.mov` files share the box structure and open too.
//!
//! ## Structure
//!
//! A sequence of boxes: `ftyp` (brands), `moov` (all metadata) and `mdat`
//! (the media data), in either order. Inside `moov` each `trak` describes
//! one track; its sample table (`mdia/minf/stbl`) lists every sample
//! (= packet): sizes (`stsz`), durations (`stts`), and where they lie in
//! the file (`stco`/`co64` chunk offsets plus `stsc`, which maps chunks to
//! samples). Only the first audio track is used; other tracks (video,
//! chapters as text tracks, ...) are ignored with a warning and not
//! written.
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
//! always yields the same number of samples (1024 for AAC-LC, the largest
//! `stts` duration of the track), an ALAC frame its own `stts` duration.
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
//! A cut is a fresh file: the source's `ftyp`, a `moov` with one track
//! (the source's sample description `stsd` verbatim, new sample tables, an
//! edit list), and one `mdat` with the kept samples, verbatim. `moov`
//! comes first, so players can start before the whole file is read.
//!
//! - **Start:** for AAC, [`AAC_PREROLL_FRAMES`] samples before the first kept
//!   one are written as pre-roll (the MDCT overlap of the next frame needs
//!   them), and hidden, together with the head of the first kept sample,
//!   by the edit list's `media_time`, so the start is sample-exact.
//! - **End:** the tail of the last sample is hidden by the edit list's
//!   duration (and, as ffmpeg writes it, by a shorter last `stts`
//!   duration), so the end is sample-exact.
//! - **Joins:** an edit list could join trimmed parts, but players handle
//!   multi-entry edit lists poorly, so kept parts are joined in one
//!   stream without pre-roll, and inner cut points are widened to sample
//!   boundaries ([`CutCapabilities::trims_at_file_ends_only`]). For AAC the
//!   first frame after a join overlaps with a foreign predecessor: a
//!   transient of one frame. ALAC frames are independent, so ALAC joins
//!   are bit-exact.
//!
//! ## Tags
//!
//! iTunes-style metadata in `moov/udta/meta/ilst`, see [`ilst`]. Chapter
//! lists and other `udta` content are not copied.

pub mod aac;
pub mod boxes;
pub mod ilst;

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;

use lac_core::{
    AudioStreamInfo, CutCapabilities, CutPlan, PacketFlags, PacketIndex, PacketInfo, Tags,
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

/// Samples per chunk in written files (ffmpeg writes about one second's
/// worth; any count is valid).
const CHUNK_SAMPLES: usize = 64;

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
}

impl Mp4Codec {
    /// The codec's name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Aac { .. } => "AAC",
            Self::Alac { .. } => "ALAC",
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
    /// ISO 639-2 language code as packed in `mdhd`.
    language: u16,
    raw: Vec<RawPacket>,
    /// Decoder output samples that belong to the timeline.
    audible: Range<u64>,
    /// Raw packet of each index entry.
    entry_raw: Vec<usize>,
    index: PacketIndex,
    other_tracks: usize,
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
        let mut other_tracks = 0;
        for trak in boxes::Boxes::new(&moov).filter(|b| &b.kind == b"trak") {
            let handler = boxes::path(trak.body, &[b"mdia", b"hdlr"])
                .and_then(|h| h.body.get(8..12))
                .unwrap_or_default();
            if handler != b"soun" || track.is_some() {
                other_tracks += 1;
                continue;
            }
            let frag = Fragments {
                moofs: &fragments,
                defaults: parse_trex(&moov),
            };
            track = Some(parse_track(
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
                "ignoring {other_tracks} other track(s); only the first audio track is cut"
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
                    independent: matches!(t.codec, Mp4Codec::Alac { .. }),
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
            language: t.language,
            raw: t.raw,
            audible: t.audible,
            entry_raw,
            index,
            other_tracks,
            tags,
            ilst_other: other,
            warnings,
        })
    }

    /// The audio codec.
    pub fn codec(&self) -> &Mp4Codec {
        &self.codec
    }

    /// Number of tracks besides the audio track, which are ignored.
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

    /// Sample-exact at the start and end of the output file (edit list);
    /// joins inside the file are packet-granular. Pre-roll is added and
    /// hidden inside [`Mp4File::write_cut`].
    pub fn capabilities(&self) -> CutCapabilities {
        CutCapabilities {
            preroll_packets: 0,
            can_trim_start: true,
            can_trim_end: true,
            lossless_reencode: false,
            trims_at_file_ends_only: true,
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

    /// Writes the kept samples of `plans`, joined, as a new MP4 file. Only
    /// the start of the first and the end of the last non-empty plan may
    /// be trimmed (see [`Mp4File::capabilities`]). `tags`, or else the
    /// source's tags, are written as the `ilst`.
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
                    "MP4 can only trim at the start and end of the file".into(),
                ));
            }
        }
        let tags = tags.unwrap_or(&self.tags);
        let (Some(first), Some(last)) = (kept.first(), kept.last()) else {
            // Nothing kept: a valid file with an empty track.
            return self.write_file(r, &[], 0, 0, tags, w);
        };

        // Raw packets to write: pre-roll, then the kept entries in order.
        let r_first = self.entry_raw[first.packets.start];
        let preroll = match self.codec {
            Mp4Codec::Aac { .. } => AAC_PREROLL_FRAMES,
            Mp4Codec::Alac { .. } => 0,
        };
        let r0 = r_first.saturating_sub(preroll);
        let mut order: Vec<usize> = (r0..r_first).collect();
        for p in &kept {
            order.extend(p.packets.clone().map(|e| self.entry_raw[e]));
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
        self.write_file(r, &order, media_time, discard, tags, w)
    }

    /// Writes `ftyp`, `moov` and `mdat` for the raw packets `order`, of
    /// whose decoder output the first `media_time` and the last `discard`
    /// samples are hidden.
    fn write_file<R, W>(
        &self,
        r: &mut R,
        order: &[usize],
        media_time: u64,
        discard: u64,
        tags: &Tags,
        w: &mut W,
    ) -> Result<()>
    where
        R: Read + Seek + ?Sized,
        W: Write + Seek + ?Sized,
    {
        let packets: Vec<&RawPacket> = order.iter().map(|&i| &self.raw[i]).collect();
        // Sample durations: the decoder output. For AAC, whose frames all
        // decode to the same length, the last one is shortened by the
        // discarded tail, as ffmpeg writes it. An ALAC frame's duration is
        // its real length (a reader takes it as such, see `parse_track`),
        // so there only the edit list states the trim.
        let mut durations: Vec<u32> = packets.iter().map(|p| p.out).collect();
        let decoded: u64 = durations.iter().map(|&d| u64::from(d)).sum();
        if let (Some(d), Mp4Codec::Aac { .. }) = (durations.last_mut(), &self.codec) {
            // discard < out: the last packet keeps at least one sample.
            *d -= discard as u32;
        }
        let media_duration: u64 = durations.iter().map(|&d| u64::from(d)).sum();
        let duration = decoded - media_time - discard;
        let data_len: u64 = packets.iter().map(|p| u64::from(p.len)).sum();
        let udta = ilst::udta(&ilst::write(tags, &self.ilst_other));

        let table = SampleTable {
            durations: &durations,
            sizes: packets.iter().map(|p| p.len).collect(),
            media_time,
            duration,
            media_duration,
        };
        // co64 and a 64-bit mdat header once the file may pass 4 GiB.
        let large = data_len + (1 << 24) + table.sizes.len() as u64 * 16 > u64::from(u32::MAX);
        let mdat_header = if large { 16 } else { 8 };
        let probe = self.moov(&table, 0, large, &udta);
        let data_start = (self.ftyp.len() + probe.len() + mdat_header) as u64;
        let moov = self.moov(&table, data_start, large, &udta);
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
        for p in packets {
            w.write_all(&read_raw(r, p)?)?;
        }
        Ok(())
    }

    /// The `moov` box for `t`, with the media data starting at file offset
    /// `data_start`.
    fn moov(&self, t: &SampleTable, data_start: u64, large: bool, udta: &[u8]) -> Vec<u8> {
        let fits = |v: u64| u32::try_from(v).is_ok();
        let v1 = !fits(t.media_duration) || !fits(t.duration);
        let time_fields = |out: &mut Vec<u8>, duration: u64| {
            if v1 {
                out.extend([0; 16]); // creation and modification time
                return duration.to_be_bytes().to_vec();
            }
            out.extend([0; 8]);
            (duration as u32).to_be_bytes().to_vec()
        };
        let version = u8::from(v1);
        let matrix: [u32; 9] = [0x1_0000, 0, 0, 0, 0x1_0000, 0, 0, 0, 0x4000_0000];

        // mvhd: the movie timescale is the sample rate, so the edit list
        // is sample-exact.
        let mut mvhd = Vec::new();
        let d = time_fields(&mut mvhd, t.duration);
        mvhd.extend(self.rate.to_be_bytes());
        mvhd.extend(d);
        mvhd.extend(0x1_0000u32.to_be_bytes()); // rate 1.0
        mvhd.extend(0x0100u16.to_be_bytes()); // volume 1.0
        mvhd.extend([0; 10]);
        matrix.iter().for_each(|m| mvhd.extend(m.to_be_bytes()));
        mvhd.extend([0; 24]); // pre_defined
        mvhd.extend(2u32.to_be_bytes()); // next_track_ID

        // tkhd (enabled, in movie): track 1.
        let mut tkhd = Vec::new();
        let d = time_fields(&mut tkhd, t.duration);
        tkhd.extend(1u32.to_be_bytes());
        tkhd.extend([0; 4]);
        tkhd.extend(d);
        tkhd.extend([0; 8]);
        tkhd.extend(0u16.to_be_bytes()); // layer
        tkhd.extend(1u16.to_be_bytes()); // alternate group
        tkhd.extend(0x0100u16.to_be_bytes()); // volume
        tkhd.extend([0; 2]);
        matrix.iter().for_each(|m| tkhd.extend(m.to_be_bytes()));
        tkhd.extend([0; 8]); // width, height

        // elst: one edit with the trims.
        let mut elst = 1u32.to_be_bytes().to_vec();
        if v1 {
            elst.extend(t.duration.to_be_bytes());
            elst.extend(t.media_time.to_be_bytes());
        } else {
            elst.extend((t.duration as u32).to_be_bytes());
            elst.extend((t.media_time as u32).to_be_bytes());
        }
        elst.extend(0x1_0000u32.to_be_bytes()); // media rate 1.0
        let mut edts = Vec::new();
        boxes::put_full(&mut edts, b"elst", version, 0, &elst);

        let mut mdhd = Vec::new();
        let d = time_fields(&mut mdhd, t.media_duration);
        mdhd.extend(self.rate.to_be_bytes());
        mdhd.extend(d);
        mdhd.extend(self.language.to_be_bytes());
        mdhd.extend([0; 2]);
        let mut hdlr = 0u32.to_be_bytes().to_vec();
        hdlr.extend(b"soun");
        hdlr.extend([0; 12]);
        hdlr.extend(b"SoundHandler\0");

        let mut smhd = Vec::new();
        boxes::put_full(&mut smhd, b"smhd", 0, 0, &[0; 4]);
        let mut url = Vec::new();
        boxes::put_full(&mut url, b"url ", 0, 1, &[]);
        let mut dref = 1u32.to_be_bytes().to_vec();
        dref.extend(url);
        let mut dinf = Vec::new();
        boxes::put_full(&mut dinf, b"dref", 0, 0, &dref);

        let mut stbl = self.stsd.clone();
        // stts: runs of equal durations.
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for &d in t.durations {
            match runs.last_mut() {
                Some((n, v)) if *v == d => *n += 1,
                _ => runs.push((1, d)),
            }
        }
        let mut stts = (runs.len() as u32).to_be_bytes().to_vec();
        runs.iter()
            .for_each(|(n, d)| stts.extend([n.to_be_bytes(), d.to_be_bytes()].concat()));
        boxes::put_full(&mut stbl, b"stts", 0, 0, &stts);
        // stsc: full chunks of CHUNK_SAMPLES, then the remainder.
        let n = t.sizes.len();
        let chunks = n.div_ceil(CHUNK_SAMPLES);
        let mut stsc_entries = Vec::new();
        if n >= CHUNK_SAMPLES {
            stsc_entries.push((1u32, CHUNK_SAMPLES as u32));
        }
        if !n.is_multiple_of(CHUNK_SAMPLES) {
            stsc_entries.push((chunks as u32, (n % CHUNK_SAMPLES) as u32));
        }
        let mut stsc = (stsc_entries.len() as u32).to_be_bytes().to_vec();
        for (c, s) in stsc_entries {
            stsc.extend(c.to_be_bytes());
            stsc.extend(s.to_be_bytes());
            stsc.extend(1u32.to_be_bytes()); // sample description 1
        }
        boxes::put_full(&mut stbl, b"stsc", 0, 0, &stsc);
        let mut stsz = 0u32.to_be_bytes().to_vec();
        stsz.extend((n as u32).to_be_bytes());
        t.sizes.iter().for_each(|s| stsz.extend(s.to_be_bytes()));
        boxes::put_full(&mut stbl, b"stsz", 0, 0, &stsz);
        let mut co = (chunks as u32).to_be_bytes().to_vec();
        let mut off = data_start;
        for c in t.sizes.chunks(CHUNK_SAMPLES) {
            if large {
                co.extend(off.to_be_bytes());
            } else {
                co.extend((off as u32).to_be_bytes());
            }
            off += c.iter().map(|&s| u64::from(s)).sum::<u64>();
        }
        boxes::put_full(&mut stbl, if large { b"co64" } else { b"stco" }, 0, 0, &co);
        if matches!(self.codec, Mp4Codec::Aac { .. }) && n > 0 {
            // ISO/IEC 14496-12 10.1: every AAC sample needs its
            // predecessor (roll distance -1), as ffmpeg writes it, so
            // seeking players decode one sample ahead.
            let mut sgpd = b"roll".to_vec();
            sgpd.extend(2u32.to_be_bytes()); // default_length
            sgpd.extend(1u32.to_be_bytes());
            sgpd.extend((-1i16).to_be_bytes());
            boxes::put_full(&mut stbl, b"sgpd", 1, 0, &sgpd);
            let mut sbgp = b"roll".to_vec();
            sbgp.extend(1u32.to_be_bytes());
            sbgp.extend((n as u32).to_be_bytes());
            sbgp.extend(1u32.to_be_bytes());
            boxes::put_full(&mut stbl, b"sbgp", 0, 0, &sbgp);
        }

        let mut minf = smhd;
        boxes::put(&mut minf, b"dinf", &dinf);
        boxes::put(&mut minf, b"stbl", &stbl);
        let mut mdia = Vec::new();
        boxes::put_full(&mut mdia, b"mdhd", version, 0, &mdhd);
        boxes::put_full(&mut mdia, b"hdlr", 0, 0, &hdlr);
        boxes::put(&mut mdia, b"minf", &minf);
        let mut trak = Vec::new();
        boxes::put_full(&mut trak, b"tkhd", version, 3, &tkhd);
        boxes::put(&mut trak, b"edts", &edts);
        boxes::put(&mut trak, b"mdia", &mdia);
        let mut moov = Vec::new();
        boxes::put_full(&mut moov, b"mvhd", version, 0, &mvhd);
        boxes::put(&mut moov, b"trak", &trak);
        moov.extend_from_slice(udta);
        let mut out = Vec::new();
        boxes::put(&mut out, b"moov", &moov);
        out
    }
}

/// What the sample table of an output file holds.
struct SampleTable<'a> {
    durations: &'a [u32],
    sizes: Vec<u32>,
    /// Edit list start, in samples.
    media_time: u64,
    /// Edit list duration, in samples.
    duration: u64,
    /// Sum of the sample durations.
    media_duration: u64,
}

/// The audio track as parsed from its `trak` box.
struct Track {
    codec: Mp4Codec,
    channels: u16,
    rate: u32,
    stsd: Vec<u8>,
    language: u16,
    raw: Vec<RawPacket>,
    audible: Range<u64>,
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
/// `frag`.
fn parse_track(
    trak: BoxRef,
    movie_timescale: u32,
    frag: &Fragments,
    file_len: u64,
    warnings: &mut Vec<String>,
) -> Result<Track> {
    let at = |what: &str| malformed(0, &format!("audio track: {what}"));
    let mdia = boxes::find(trak.body, b"mdia").ok_or_else(|| at("no mdia box"))?;
    let mdhd = boxes::find(mdia.body, b"mdhd").ok_or_else(|| at("no mdhd box"))?;
    let (timescale, language) = {
        let mut r = Reader::new(mdhd.body);
        let v = r.u8().ok_or_else(|| at("short mdhd"))?;
        r.bytes(3);
        r.bytes(if v == 1 { 16 } else { 8 });
        let ts = r
            .u32()
            .filter(|&t| t > 0)
            .ok_or_else(|| at("invalid timescale"))?;
        r.bytes(if v == 1 { 8 } else { 4 });
        (ts, r.u16().unwrap_or(0x55C4))
    };
    let stbl = boxes::path(mdia.body, &[b"minf", b"stbl"]).ok_or_else(|| at("no sample table"))?;
    let stsd = boxes::find(stbl.body, b"stsd").ok_or_else(|| at("no stsd box"))?;
    let (codec, channels) = parse_stsd(stsd.body)?;
    let mut stsd_box = Vec::new();
    boxes::put(&mut stsd_box, b"stsd", stsd.body);

    let get = |k: &[u8; 4]| boxes::find(stbl.body, k).map(|b| b.body);
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
            "the sample table lists {} durations but {count} sizes; using {}",
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
            "the chunk table covers only {} of {count} samples",
            offsets.len()
        ));
        count = offsets.len();
    }
    let (mut sizes, mut durations) = (sizes, durations);
    sizes.truncate(count);
    durations.truncate(count);
    offsets.truncate(count);

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
        }
        count = offsets.len();
    }

    let fixed_out = match codec {
        // An AAC frame always decodes to the same length; the encoder
        // may shorten the last sample's duration to state its padding.
        Mp4Codec::Aac { .. } => durations[..count].iter().copied().max(),
        Mp4Codec::Alac { .. } => None,
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
        let out = fixed_out.unwrap_or(durations[i]);
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
    let total = start;

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
    Ok(Track {
        codec,
        channels,
        rate: timescale,
        stsd: stsd_box,
        language,
        raw,
        audible,
    })
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
}

impl Default for SampleDefaults {
    /// What a track without `trex` gets: the first sample description and
    /// no duration or size (each `trun` must then give them).
    fn default() -> Self {
        Self {
            description: 1,
            duration: 0,
            size: 0,
        }
    }
}

/// A sample found in a movie fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FragmentSample {
    offset: u64,
    size: u32,
    duration: u32,
}

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
            Some((
                id,
                SampleDefaults {
                    description,
                    duration,
                    size,
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
        // 0x20 (default sample flags) and 0x1_0000 (duration-is-empty,
        // which comes without trun boxes) need no handling.

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
            if flags & 0x04 != 0 {
                r.u32().ok_or(Invalid)?; // first sample flags
            }
            if mine && out.len().saturating_add(n) > max {
                return Err(Invalid);
            }
            for _ in 0..n {
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
                if flags & 0x400 != 0 {
                    r.u32().ok_or(Invalid)?; // sample flags
                }
                if flags & 0x800 != 0 {
                    r.u32().ok_or(Invalid)?; // composition time offset
                }
                if mine {
                    out.push(FragmentSample {
                        offset: data,
                        size,
                        duration,
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

/// Parses an `stsd` body: the codec and channel count of its first entry.
fn parse_stsd(b: &[u8]) -> Result<(Mp4Codec, u16)> {
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
            Ok((Mp4Codec::Aac { asc }, channels))
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
            Ok((Mp4Codec::Alac { cookie: out }, channels))
        }
        other => Err(ContainerError::Unsupported(format!(
            "MP4 audio codec {:?} (only AAC and ALAC are supported)",
            String::from_utf8_lossy(other)
        ))),
    }
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

/// The AudioSpecificConfig from an `esds` payload (after version/flags):
/// ES_Descriptor (tag 3) > DecoderConfigDescriptor (tag 4) >
/// DecoderSpecificInfo (tag 5), ISO/IEC 14496-1 7.2.6.
fn parse_esds(b: &[u8]) -> Result<Vec<u8>> {
    let bad = || malformed(0, "invalid esds box");
    let mut r = Reader::new(b);
    let (tag, _) = descriptor(&mut r).ok_or_else(bad)?;
    if tag != 3 {
        return Err(bad());
    }
    r.u16().ok_or_else(bad)?; // ES_ID
    let flags = r.u8().ok_or_else(bad)?;
    if flags & 0x80 != 0 {
        r.u16().ok_or_else(bad)?; // dependsOn_ES_ID
    }
    if flags & 0x40 != 0 {
        let n = r.u8().ok_or_else(bad)?;
        r.bytes(usize::from(n)).ok_or_else(bad)?; // URL
    }
    if flags & 0x20 != 0 {
        r.u16().ok_or_else(bad)?; // OCR_ES_ID
    }
    let (tag, _) = descriptor(&mut r).ok_or_else(bad)?;
    if tag != 4 {
        return Err(bad());
    }
    let object_type = r.u8().ok_or_else(bad)?;
    // 0x40: MPEG-4 audio; 0x66-0x68: MPEG-2 AAC profiles.
    if !matches!(object_type, 0x40 | 0x66..=0x68) {
        return Err(ContainerError::Unsupported(format!(
            "MP4 audio object type {object_type:#04x} (only AAC is supported in mp4a)"
        )));
    }
    r.bytes(12).ok_or_else(bad)?; // stream type, buffer size, bitrates
    let (tag, len) = descriptor(&mut r).ok_or_else(bad)?;
    if tag != 5 || len == 0 {
        return Err(bad());
    }
    Ok(r.bytes(len).ok_or_else(bad)?.to_vec())
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
        }
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
