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
//! written. Fragmented files (`moof`) are not supported yet.
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
//! ffmpeg honours the edit list's start but decodes the last frame whole;
//! Apple's players honour both ends. Our decoding follows the edit list,
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
                b"moof" => {
                    return Err(ContainerError::Unsupported(
                        "fragmented MP4 (moof boxes)".into(),
                    ));
                }
                _ => {}
            }
            pos = end;
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
            track = Some(parse_track(trak, movie_timescale, len, &mut warnings)?);
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
        // Sample durations: the decoder output, the last one shortened by
        // the discarded tail (as ffmpeg writes it).
        let mut durations: Vec<u32> = packets.iter().map(|p| p.out).collect();
        if let Some(d) = durations.last_mut() {
            // discard < out: the last packet keeps at least one sample.
            *d -= discard as u32;
        }
        let media_duration: u64 = durations.iter().map(|&d| u64::from(d)).sum();
        let duration = media_duration - media_time;
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

/// Parses an audio `trak` box.
fn parse_track(
    trak: BoxRef,
    movie_timescale: u32,
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
    if stsc.iter().any(|e| e.2 != 1) {
        return Err(ContainerError::Unsupported(
            "MP4 track with several sample descriptions".into(),
        ));
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
            .chunks_exact(4)
            .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
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
                .chunks_exact(2)
                .map(|c| u32::from(u16::from_be_bytes([c[0], c[1]])))
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
}
