//! Video tracks of MP4, QuickTime (`.mov`) and 3GPP (`.3gp`) files, as
//! phones and cameras record them.
//!
//! The video track is read into a [`VideoIndex`]: every sample (= frame)
//! with its position, `stts` duration, `ctts` composition offset and
//! whether it is a sync sample (`stss`, or the sample flags of a movie
//! fragment). Cutting copies frames verbatim, so all a codec needs here is
//! a way to tell its keyframes (see `docs/architecture.md`, section 13).
//!
//! ## Keyframes are checked, not trusted
//!
//! Muxers mark a sample as sync when the encoder says "key", and some
//! encoders say that for frames a decoder cannot start at: x264 with
//! `open-gop` flags its non-IDR I-frames, whose following frames may refer
//! to frames before them. A cut starting there would show broken pictures.
//! So for the codecs whose bitstream says it cheaply, each claimed
//! keyframe's first bytes are read and checked:
//!
//! | Codec | Keyframe when |
//! |---|---|
//! | H.264 | the first slice is an IDR slice (`nal_unit_type` 5, ISO/IEC 14496-10 7.4.1.2) |
//! | HEVC | the first slice is an IRAP picture (`nal_unit_type` 16..=23, ISO/IEC 23008-2 7.4.2.2): BLA, IDR or CRA |
//! | AV1 | the first frame header has `frame_type` `KEY_FRAME` and does not show an existing frame (AV1 spec 5.9.2) |
//! | VP9 | the frame header has `frame_type` `KEY_FRAME` and does not show an existing frame (VP9 spec 6.2) |
//! | MPEG-4 Visual | the first VOP is intra-coded (`vop_coding_type` 0, ISO/IEC 14496-2 6.2.5) |
//!
//! A claimed keyframe that is none is demoted with a warning. Other codecs
//! (ProRes, APV, H.263, ...) are trusted; ProRes and APV only have intra
//! frames anyway. An HEVC CRA is a keyframe, but its leading RASL pictures
//! refer to the previous GOP; [`VideoIndex::is_clean_start`] finds those
//! from the display order, so no slice types are needed for that.

use std::io::{Read, Seek};
use std::ops::Range;

use lac_core::{MediaTime, Rounding, VideoCodec, VideoFrame, VideoIndex, VideoStreamInfo, rescale};

use super::boxes::{self, BoxRef, Reader};
use super::{Fragments, Samples, malformed, parse_elst, read_samples, read_up_to};
use crate::{ContainerError, Result};

/// NAL units looked at per sample before giving up on finding a slice
/// (access unit delimiters, parameter sets and SEI messages come first).
const MAX_NAL_UNITS: usize = 32;

/// Bytes read from the start of an MPEG-4 Visual sample to find its first
/// VOP start code (behind the VOL headers of a keyframe).
const MPEG4_SEARCH: usize = 512;

/// The video track of an MP4 file.
#[derive(Debug, Clone)]
pub struct Mp4Video {
    index: VideoIndex,
    /// The `stsd` box (header included), copied to exports.
    stsd: Vec<u8>,
    /// ISO 639-2 language code as packed in `mdhd`.
    language: u16,
    /// The part of the track the edit list shows, in media ticks.
    shown: Range<MediaTime>,
    /// Ticks (of the video timescale) before the track starts on the
    /// movie timeline: a leading empty edit.
    delay: MediaTime,
    /// Frames marked as sync samples that are no keyframes.
    demoted: usize,
    /// The `tkhd` display matrix, as stored.
    tkhd_matrix: [u8; 36],
    /// The `tkhd` width and height (16.16), as stored.
    tkhd_size: [u8; 8],
}

impl Mp4Video {
    /// The frames of the track.
    pub fn index(&self) -> &VideoIndex {
        &self.index
    }

    /// The `stsd` box of the track, header included.
    pub fn stsd(&self) -> &[u8] {
        &self.stsd
    }

    /// ISO 639-2 language code as packed in `mdhd`.
    pub fn language(&self) -> u16 {
        self.language
    }

    /// The span of presentation times the source's edit list shows, in the
    /// video timescale. Its start is the media time shown at the start of
    /// the movie (after [`Mp4Video::delay`]). Without an edit list, media
    /// time is movie time, so the span starts at 0, even if the first frame
    /// is shown later.
    pub fn shown(&self) -> Range<MediaTime> {
        self.shown.clone()
    }

    /// Ticks of the video timescale between the start of the movie and the
    /// first shown frame (a leading empty edit), usually 0.
    pub fn delay(&self) -> MediaTime {
        self.delay
    }

    /// The video media time that belongs to the movie's start (audio
    /// sample 0): the first shown media time minus the delay. Negative
    /// when the video starts later than the movie.
    pub fn video_offset(&self) -> MediaTime {
        self.shown.start - self.delay
    }

    /// The track's display matrix (`tkhd`), as stored, for exports.
    pub fn tkhd_matrix(&self) -> [u8; 36] {
        self.tkhd_matrix
    }

    /// The track's display width and height (`tkhd`, 16.16 fixed point),
    /// as stored, for exports.
    pub fn tkhd_size(&self) -> [u8; 8] {
        self.tkhd_size
    }

    /// Number of frames the file marks as keyframes although decoding
    /// cannot start at them; they are not used as keyframes.
    pub fn demoted_keyframes(&self) -> usize {
        self.demoted
    }
}

/// The codec of a visual sample entry and how to look into its frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    codec: VideoCodec,
    width: u32,
    height: u32,
    /// Bytes of the NAL unit length prefix (H.264, HEVC), 1, 2 or 4.
    nal_length: usize,
    /// AV1 `reduced_still_picture_header` from the sequence header in
    /// `av1C`: every frame is then a key frame without a frame header.
    av1_reduced: bool,
}

/// Parses the `trak` of a video track (handler `vide`).
pub(super) fn parse_video_track<R: Read + Seek + ?Sized>(
    r: &mut R,
    trak: BoxRef,
    movie_timescale: u32,
    frag: &Fragments,
    file_len: u64,
    warnings: &mut Vec<String>,
) -> Result<Mp4Video> {
    let at = |what: &str| malformed(0, &format!("video track: {what}"));
    let mdia = boxes::find(trak.body, b"mdia").ok_or_else(|| at("no mdia box"))?;
    let mdhd = boxes::find(mdia.body, b"mdhd").ok_or_else(|| at("no mdhd box"))?;
    let (timescale, language) = super::parse_mdhd(mdhd.body).ok_or_else(|| at("invalid mdhd"))?;
    let stbl = boxes::path(mdia.body, &[b"minf", b"stbl"]).ok_or_else(|| at("no sample table"))?;
    let stsd = boxes::find(stbl.body, b"stsd").ok_or_else(|| at("no stsd box"))?;
    let entry = parse_visual_entry(stsd.body)?;
    let mut stsd_box = Vec::new();
    boxes::put(&mut stsd_box, b"stsd", stsd.body);
    let tkhd = boxes::find(trak.body, b"tkhd").map(|b| b.body);
    let (tkhd_matrix, tkhd_size) = tkhd.and_then(tkhd_layout).unwrap_or_else(|| {
        // Identity matrix and the coded size.
        let mut m = [0u8; 36];
        m[0..4].copy_from_slice(&0x1_0000u32.to_be_bytes());
        m[16..20].copy_from_slice(&0x1_0000u32.to_be_bytes());
        m[32..36].copy_from_slice(&0x4000_0000u32.to_be_bytes());
        let mut size = [0u8; 8];
        size[0..4].copy_from_slice(&(entry.width << 16).to_be_bytes());
        size[4..8].copy_from_slice(&(entry.height << 16).to_be_bytes());
        (m, size)
    });
    let rotation = tkhd.and_then(tkhd_rotation).unwrap_or_else(|| {
        warnings.push("the video's display matrix is no plain rotation; ignoring it".into());
        0
    });

    let Samples {
        offsets,
        sizes,
        durations,
        ctts,
        mut sync,
    } = read_samples(trak, stbl.body, frag, file_len, "video", warnings)?;
    let mut count = offsets.len();
    if let Some(cut) =
        (0..count).find(|&i| offsets[i].saturating_add(u64::from(sizes[i])) > file_len)
    {
        warnings.push(format!(
            "the file is truncated; {cut} of {count} video frames are present"
        ));
        count = cut;
    }
    // A frame without duration (some muxers write 0 for the last one)
    // would be shown for no time; give it its predecessor's.
    let mut durations = durations;
    durations.truncate(count);
    for i in 0..count {
        if durations[i] == 0 {
            durations[i] = if i > 0 { durations[i - 1] } else { 1 };
        }
    }

    // Check the claimed keyframes (the first frame is left alone: the
    // source itself is decoded from there).
    let mut demoted = 0;
    for i in 1..count {
        if sync[i] && is_keyframe(r, &entry, offsets[i], sizes[i])? == Some(false) {
            sync[i] = false;
            demoted += 1;
        }
    }
    if demoted > 0 {
        warnings.push(format!(
            "{demoted} video frame(s) marked as keyframes cannot start decoding \
             (open GOP); cuts start at the keyframe before"
        ));
    }

    let frames: Vec<VideoFrame> = (0..count)
        .map(|i| VideoFrame {
            byte_offset: offsets[i],
            byte_len: sizes[i],
            duration: durations[i],
            composition_offset: ctts[i],
            sync: sync[i],
        })
        .collect();
    let info = VideoStreamInfo {
        codec: entry.codec,
        width: entry.width,
        height: entry.height,
        timescale,
        rotation,
    };
    let index = VideoIndex::new(info, frames)
        .map_err(|e| ContainerError::Unsupported(format!("video track: {e}")))?;

    // The edit list (ISO/IEC 14496-12 8.6.6): a leading empty edit delays
    // the track, the first non-empty edit selects what is shown.
    let all = index.display_range();
    let edits = boxes::path(trak.body, &[b"edts", b"elst"])
        .and_then(|b| parse_elst(b.body))
        .unwrap_or_default();
    let to_media = |d: u64| {
        rescale(
            MediaTime::try_from(d).unwrap_or(MediaTime::MAX),
            movie_timescale,
            timescale,
            Rounding::Nearest,
        )
    };
    let delay = edits
        .iter()
        .take_while(|e| e.1 < 0)
        .map(|e| to_media(e.0))
        .fold(0, MediaTime::saturating_add);
    let mut real = edits.iter().filter(|e| e.1 >= 0);
    let shown = match real.next() {
        // Without an edit list, media time is movie time: a track whose
        // first frame has a presentation time above 0 (composition offsets
        // without an edit list to compensate) starts that much later.
        None => 0..all.end.max(0),
        Some(&(dur, media_time)) => {
            if real.next().is_some() {
                warnings.push("only the first edit of the video's edit list is used".into());
            }
            let start = media_time.clamp(all.start, all.end);
            let end = if dur == 0 {
                all.end
            } else {
                start.saturating_add(to_media(dur)).min(all.end)
            };
            start..end
        }
    };
    Ok(Mp4Video {
        index,
        stsd: stsd_box,
        language,
        shown,
        delay,
        demoted,
        tkhd_matrix,
        tkhd_size,
    })
}

/// The display matrix and the width and height of a `tkhd` body, as
/// stored (ISO/IEC 14496-12 8.3.2).
fn tkhd_layout(b: &[u8]) -> Option<([u8; 36], [u8; 8])> {
    let at = if *b.first()? == 1 { 52 } else { 40 };
    let matrix = b.get(at..at + 36)?.try_into().ok()?;
    let size = b.get(at + 36..at + 44)?.try_into().ok()?;
    Some((matrix, size))
}

/// Parses the first entry of a video `stsd` body (ISO/IEC 14496-12
/// 12.1.3 VisualSampleEntry; QuickTime's video sample description has the
/// same layout).
fn parse_visual_entry(b: &[u8]) -> Result<Entry> {
    let bad = |what: &str| malformed(0, &format!("video sample description: {what}"));
    let entry = boxes::Boxes::new(b.get(8..).unwrap_or_default())
        .next()
        .ok_or_else(|| bad("no entry"))?;
    // 6 reserved bytes, data_reference_index, then 16 bytes of
    // pre_defined/reserved fields before width and height; the child
    // boxes follow 70 bytes later (resolutions, frame count, compressor
    // name, depth, pre_defined).
    let mut r = Reader::new(entry.body);
    r.bytes(24).ok_or_else(|| bad("short entry"))?;
    let width = u32::from(r.u16().ok_or_else(|| bad("short entry"))?);
    let height = u32::from(r.u16().ok_or_else(|| bad("short entry"))?);
    let children = entry.body.get(78..).unwrap_or_default();
    let mut e = Entry {
        codec: VideoCodec::Other(entry.kind),
        width,
        height,
        nal_length: 4,
        av1_reduced: false,
    };
    match &entry.kind {
        // Dolby Vision streams (dva1, dvav, dvh1, dvhe, dav1) carry a base
        // layer of the codec they are named after.
        b"avc1" | b"avc2" | b"avc3" | b"avc4" | b"dva1" | b"dvav" => {
            // AVCDecoderConfigurationRecord (ISO/IEC 14496-15 5.3.3.1):
            // lengthSizeMinusOne in the low 2 bits of byte 4.
            let c = boxes::find(children, b"avcC")
                .and_then(|c| c.body.get(4).copied())
                .ok_or_else(|| bad("H.264 entry without avcC"))?;
            e.codec = VideoCodec::H264;
            e.nal_length = nal_length(c)?;
        }
        b"hvc1" | b"hev1" | b"dvh1" | b"dvhe" => {
            // HEVCDecoderConfigurationRecord (ISO/IEC 14496-15 8.3.3.1):
            // lengthSizeMinusOne in the low 2 bits of byte 21.
            let c = boxes::find(children, b"hvcC")
                .and_then(|c| c.body.get(21).copied())
                .ok_or_else(|| bad("HEVC entry without hvcC"))?;
            e.codec = VideoCodec::Hevc;
            e.nal_length = nal_length(c)?;
        }
        b"av01" | b"dav1" => {
            e.codec = VideoCodec::Av1;
            // AV1CodecConfigurationRecord: 4 bytes, then configOBUs, which
            // usually hold the sequence header.
            if let Some(obus) = boxes::find(children, b"av1C").and_then(|c| c.body.get(4..)) {
                e.av1_reduced = av1_obus(obus)
                    .find(|o| o.kind == AV1_SEQUENCE_HEADER)
                    .and_then(|o| o.payload.first().map(|&b| b & 0x08 != 0))
                    .unwrap_or(false);
            }
        }
        b"vp09" => e.codec = VideoCodec::Vp9,
        b"mp4v" => {
            // mp4v holds whatever its esds names (ISO/IEC 14496-1 Table 5):
            // 0x20 is MPEG-4 Visual; JPEG (0x6C) and MPEG-1/2 video use the
            // entry too.
            let oti = boxes::find(children, b"esds")
                .and_then(|b| super::esds_decoder_config(b.body.get(4..)?))
                .map(|(oti, _)| oti);
            e.codec = match oti {
                Some(0x20) => VideoCodec::Mpeg4Visual,
                Some(0x6C) => VideoCodec::Other(*b"jpeg"),
                Some(0x60..=0x65) => VideoCodec::Other(*b"mp2v"),
                Some(0x6A) => VideoCodec::Other(*b"mp1v"),
                _ => VideoCodec::Other(*b"mp4v"),
            };
        }
        b"encv" => {
            return Err(ContainerError::Unsupported("encrypted video".into()));
        }
        _ => {}
    }
    Ok(e)
}

/// The NAL unit length prefix from `lengthSizeMinusOne` (low 2 bits); a
/// value of 2 (3 bytes) is not allowed.
fn nal_length(b: u8) -> Result<usize> {
    match b & 3 {
        2 => Err(malformed(0, "video: NAL unit length size of 3 bytes")),
        n => Ok(usize::from(n) + 1),
    }
}

/// Clockwise rotation in degrees from a `tkhd` body's display matrix
/// (ISO/IEC 14496-12 8.3.2), or `None` if it is not a plain rotation by a
/// multiple of 90 degrees (a mirror image, a scale). The matrix
/// `{a b u, c d v, x y w}` maps (p, q) to (a·p + c·q + x, b·p + d·q + y);
/// a, b, c and d are 16.16 fixed point.
fn tkhd_rotation(b: &[u8]) -> Option<u16> {
    let mut r = Reader::new(b);
    let version = r.u8()?;
    r.bytes(3)?;
    // Times, track ID, reserved, duration; then reserved (8), layer,
    // alternate group, volume, reserved (2 each).
    r.bytes(if version == 1 { 32 } else { 20 })?;
    r.bytes(16)?;
    let a = r.u32()? as i32;
    let b = r.u32()? as i32;
    r.u32()?;
    let c = r.u32()? as i32;
    let d = r.u32()? as i32;
    const ONE: i32 = 0x1_0000;
    const MINUS_ONE: i32 = -0x1_0000;
    match (a, b, c, d) {
        (ONE, 0, 0, ONE) => Some(0),
        (0, ONE, MINUS_ONE, 0) => Some(90),
        (MINUS_ONE, 0, 0, MINUS_ONE) => Some(180),
        (0, MINUS_ONE, ONE, 0) => Some(270),
        // A track without a meaningful matrix (all zero) is shown as is.
        (0, 0, 0, 0) => Some(0),
        _ => None,
    }
}

/// Whether the frame at `offset` (`size` bytes) is a keyframe a decoder
/// can start at; `None` when the codec has no cheap check or the data
/// does not say. Reads only the headers it needs.
fn is_keyframe<R: Read + Seek + ?Sized>(
    r: &mut R,
    e: &Entry,
    offset: u64,
    size: u32,
) -> Result<Option<bool>> {
    let size = u64::from(size);
    match e.codec {
        VideoCodec::H264 | VideoCodec::Hevc => {
            let mut pos = 0u64;
            for _ in 0..MAX_NAL_UNITS {
                let mut head = [0u8; 6];
                let want = e.nal_length + 1;
                if pos + want as u64 > size {
                    break;
                }
                if read_up_to(r, offset + pos, &mut head[..want])? < want {
                    break;
                }
                let len = head[..e.nal_length]
                    .iter()
                    .fold(0u64, |a, &b| (a << 8) | u64::from(b));
                if let Some(irap) = slice_is_random_access(e.codec, head[e.nal_length]) {
                    return Ok(Some(irap));
                }
                pos += e.nal_length as u64 + len;
            }
            Ok(None)
        }
        VideoCodec::Av1 => {
            // A temporal unit is small enough to read whole when it is a
            // key frame's; only its OBU headers and the first bytes of the
            // frame header are needed, so read a bounded prefix.
            let mut buf = vec![0u8; size.min(4096) as usize];
            let n = read_up_to(r, offset, &mut buf)?;
            Ok(av1_is_key(&buf[..n], e.av1_reduced))
        }
        VideoCodec::Vp9 => {
            let mut head = [0u8; 1];
            if size == 0 || read_up_to(r, offset, &mut head)? < 1 {
                return Ok(None);
            }
            Ok(vp9_is_key(head[0]))
        }
        VideoCodec::Mpeg4Visual => {
            let mut buf = vec![0u8; size.min(MPEG4_SEARCH as u64) as usize];
            let n = read_up_to(r, offset, &mut buf)?;
            // VOP start code 00 00 01 B6, then vop_coding_type (2 bits).
            Ok(buf[..n]
                .windows(5)
                .find(|w| w[..4] == [0, 0, 1, 0xB6])
                .map(|w| w[4] >> 6 == 0))
        }
        VideoCodec::Other(_) => Ok(None),
    }
}

/// For a NAL unit header byte: `Some(irap)` if it is a slice (a coded
/// picture), `None` for other NAL units.
fn slice_is_random_access(codec: VideoCodec, header: u8) -> Option<bool> {
    if codec == VideoCodec::H264 {
        // forbidden_zero_bit, nal_ref_idc (2), nal_unit_type (5); slices
        // are types 1..=5, 5 being an IDR picture.
        let t = header & 0x1F;
        (1..=5).contains(&t).then_some(t == 5)
    } else {
        // forbidden_zero_bit, nal_unit_type (6), ...; types 0..=31 are VCL
        // (slice segments), 16..=23 IRAP.
        let t = (header >> 1) & 0x3F;
        (t < 32).then_some((16..=23).contains(&t))
    }
}

/// AV1 OBU type of a sequence header (AV1 spec 6.2.2).
const AV1_SEQUENCE_HEADER: u8 = 1;
/// AV1 OBU type of a frame header.
const AV1_FRAME_HEADER: u8 = 3;
/// AV1 OBU type of a frame (header plus tile group).
const AV1_FRAME: u8 = 6;

/// One open bitstream unit.
struct Obu<'a> {
    kind: u8,
    payload: &'a [u8],
}

/// The OBUs of `b` (AV1 spec 5.3); stops at the first damaged one. An OBU
/// without `obu_has_size_field` takes the rest of the data.
fn av1_obus(mut b: &[u8]) -> impl Iterator<Item = Obu<'_>> {
    std::iter::from_fn(move || {
        let h = *b.first()?;
        let kind = (h >> 3) & 0x0F;
        let mut pos = 1 + usize::from(h & 0x04 != 0); // obu_extension_flag
        let len = if h & 0x02 != 0 {
            // leb128 (4.10.5): 7 bits per byte, low group first.
            let mut v = 0usize;
            for i in 0..8 {
                let byte = *b.get(pos)?;
                pos += 1;
                v |= usize::from(byte & 0x7F) << (7 * i);
                if byte & 0x80 == 0 {
                    break;
                }
            }
            v
        } else {
            b.len().checked_sub(pos)?
        };
        let end = pos.checked_add(len)?;
        // A payload cut off by the read limit still gives its first bytes.
        let payload = b.get(pos..end.min(b.len()))?;
        b = b.get(end..).unwrap_or_default();
        Some(Obu { kind, payload })
    })
}

/// Whether an AV1 temporal unit starts with a key frame (AV1 spec 5.9.2:
/// `show_existing_frame`, then `frame_type`, 0 being `KEY_FRAME`).
fn av1_is_key(tu: &[u8], reduced_default: bool) -> Option<bool> {
    let mut reduced = reduced_default;
    for o in av1_obus(tu) {
        match o.kind {
            AV1_SEQUENCE_HEADER => {
                // seq_profile (3), still_picture (1),
                // reduced_still_picture_header (1).
                reduced = o.payload.first()? & 0x08 != 0;
            }
            AV1_FRAME_HEADER | AV1_FRAME => {
                if reduced {
                    return Some(true);
                }
                let h = *o.payload.first()?;
                let show_existing = h & 0x80 != 0;
                let frame_type = (h >> 5) & 0x03;
                return Some(!show_existing && frame_type == 0);
            }
            _ => {}
        }
    }
    None
}

/// Whether a VP9 frame (or the first frame of a superframe) is a key
/// frame, from its first header byte (VP9 spec 6.2: frame_marker (2),
/// profile_low_bit, profile_high_bit, a reserved zero bit for profile 3,
/// show_existing_frame, frame_type).
fn vp9_is_key(b: u8) -> Option<bool> {
    if b >> 6 != 2 {
        return None; // no frame marker
    }
    let profile = ((b >> 5) & 1) | (((b >> 4) & 1) << 1);
    let rest = if profile == 3 { b << 5 } else { b << 4 };
    let show_existing = rest & 0x80 != 0;
    let frame_type = (rest >> 6) & 1;
    Some(!show_existing && frame_type == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nal_unit_types() {
        // H.264: 0x65 = IDR slice, 0x41 = non-IDR slice, 0x67 = SPS.
        assert_eq!(slice_is_random_access(VideoCodec::H264, 0x65), Some(true));
        assert_eq!(slice_is_random_access(VideoCodec::H264, 0x41), Some(false));
        assert_eq!(slice_is_random_access(VideoCodec::H264, 0x67), None);
        // HEVC: type 19 (IDR_W_RADL) = 0x26, 21 (CRA) = 0x2A, 1 (TRAIL_R)
        // = 0x02, 32 (VPS) = 0x40.
        assert_eq!(slice_is_random_access(VideoCodec::Hevc, 0x26), Some(true));
        assert_eq!(slice_is_random_access(VideoCodec::Hevc, 0x2A), Some(true));
        assert_eq!(slice_is_random_access(VideoCodec::Hevc, 0x02), Some(false));
        assert_eq!(slice_is_random_access(VideoCodec::Hevc, 0x40), None);
    }

    #[test]
    fn vp9_frame_headers() {
        // 10 0 0 0 0 = marker, profile 0, show_existing 0, key frame.
        assert_eq!(vp9_is_key(0b1000_0000), Some(true));
        assert_eq!(vp9_is_key(0b1000_0100), Some(false)); // inter frame
        assert_eq!(vp9_is_key(0b1000_1000), Some(false)); // show existing
        // Profile 3 has a reserved bit before show_existing_frame.
        assert_eq!(vp9_is_key(0b1011_0000), Some(true));
        assert_eq!(vp9_is_key(0b1011_0010), Some(false));
        assert_eq!(vp9_is_key(0), None);
    }

    /// An OBU with a size field.
    fn obu(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![(kind << 3) | 0x02, payload.len() as u8];
        out.extend(payload);
        out
    }

    #[test]
    fn av1_temporal_units() {
        let td = obu(2, &[]);
        let seq = obu(AV1_SEQUENCE_HEADER, &[0x00, 0x00]);
        let key = obu(AV1_FRAME, &[0x10, 0xFF]); // show_existing 0, type 0
        let inter = obu(AV1_FRAME, &[0x30, 0xFF]); // type 1
        let shown = obu(AV1_FRAME_HEADER, &[0x80]); // show_existing_frame
        assert_eq!(
            av1_is_key(&[td.clone(), seq.clone(), key].concat(), false),
            Some(true)
        );
        assert_eq!(
            av1_is_key(&[td.clone(), inter.clone()].concat(), false),
            Some(false)
        );
        assert_eq!(
            av1_is_key(&[td.clone(), shown].concat(), false),
            Some(false)
        );
        assert_eq!(av1_is_key(&td, false), None);
        // A reduced still picture header makes every frame a key frame.
        let still = obu(AV1_SEQUENCE_HEADER, &[0x18]);
        assert_eq!(
            av1_is_key(&[still, inter.clone()].concat(), false),
            Some(true)
        );
        assert_eq!(av1_is_key(&inter, true), Some(true));
        // A damaged size ends the walk without a panic.
        assert_eq!(av1_is_key(&[0x32, 0x80], false), None);
    }

    /// A version 0 `tkhd` body with the given a, b, c, d matrix entries.
    fn tkhd(a: i32, b: i32, c: i32, d: i32) -> Vec<u8> {
        let mut t = vec![0u8; 4 + 20 + 16];
        for v in [a, b, 0, c, d, 0, 0, 0, 0x4000_0000] {
            t.extend(v.to_be_bytes());
        }
        t.extend([0u8; 8]);
        t
    }

    #[test]
    fn display_matrix_rotations() {
        const ONE: i32 = 0x1_0000;
        assert_eq!(tkhd_rotation(&tkhd(ONE, 0, 0, ONE)), Some(0));
        assert_eq!(tkhd_rotation(&tkhd(0, ONE, -ONE, 0)), Some(90));
        assert_eq!(tkhd_rotation(&tkhd(-ONE, 0, 0, -ONE)), Some(180));
        assert_eq!(tkhd_rotation(&tkhd(0, -ONE, ONE, 0)), Some(270));
        assert_eq!(tkhd_rotation(&tkhd(-ONE, 0, 0, ONE)), None); // mirrored
        assert_eq!(tkhd_rotation(&[0; 10]), None);
    }
}
