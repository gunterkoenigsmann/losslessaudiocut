//! The video track of a Matroska or WebM file (RFC 9559 5.1.4.1.28,
//! codec IDs from the Matroska codec mappings).
//!
//! Matroska stores a timestamp per block, which is the frame's
//! *presentation* time; blocks are stored in decode order. There are no
//! decode times, so the demuxer derives them the usual way: the i-th
//! decode time is the i-th smallest presentation time. Codecs without
//! reordering (VP8, VP9, AV1) then have decode = presentation time; with
//! B-frames (H.264, HEVC) the composition offsets of some frames are
//! negative, which [`VideoIndex`] allows.
//!
//! Keyframes are the blocks a SimpleBlock flags as such, or BlockGroups
//! without `ReferenceBlock`. As for MP4, a claimed keyframe that the
//! frame's own header says cannot start decoding (an open-GOP picture) is
//! demoted with a warning.
//!
//! Matroska has no edit list, so an export cannot hide copied frames:
//! cuts plan with `trim_file_ends = false` (see
//! [`lac_core::plan_av_cuts`]) and the output file starts at a keyframe.

use std::io::{Read, Seek};

use lac_core::{MediaTime, VideoCodec, VideoFrame, VideoIndex, VideoStreamInfo};

use super::{children, ebml, id};
use crate::Result;
use crate::mp4::video::{KeyframeCheck, is_keyframe};

/// Element IDs of a video TrackEntry (RFC 9559 5.1.4.1.28).
mod vid {
    pub const VIDEO: u32 = 0xE0;
    pub const PIXEL_WIDTH: u32 = 0xB0;
    pub const PIXEL_HEIGHT: u32 = 0xBA;
    pub const DEFAULT_DURATION: u32 = 0x23_E383;
}

/// The video track of the source.
#[derive(Debug, Clone)]
pub struct VideoTrack {
    /// The source's track number.
    pub number: u64,
    /// The codec, from the `CodecID`.
    pub codec: VideoCodec,
    /// `CodecPrivate`: the `avcC`, `hvcC` or `av1C` body (ISO/IEC 14496-15
    /// and the AV1 ISOBMFF binding, which the Matroska mappings reuse);
    /// empty for VP8 and VP9.
    pub codec_private: Vec<u8>,
    /// The body of the source's TrackEntry, copied into exports with a new
    /// track number so that colour, projection and similar metadata stay.
    pub entry: Vec<u8>,
    /// Pixel size from the `Video` element.
    pub width: u32,
    /// Pixel size from the `Video` element.
    pub height: u32,
    /// `DefaultDuration` in nanoseconds, if given (the frame duration).
    pub default_duration: Option<u64>,
}

/// The codec of a Matroska video `CodecID`, if it is one we can cut.
fn codec(codec_id: &str) -> Option<VideoCodec> {
    Some(match codec_id {
        "V_VP8" => VideoCodec::Vp8,
        "V_VP9" => VideoCodec::Vp9,
        "V_AV1" => VideoCodec::Av1,
        "V_MPEG4/ISO/AVC" => VideoCodec::H264,
        "V_MPEGH/ISO/HEVC" => VideoCodec::Hevc,
        _ => return None,
    })
}

impl VideoTrack {
    /// Reads a video TrackEntry body. `None` if its codec is not one we
    /// support (the track is then ignored like any other).
    pub(super) fn parse(entry: &[u8]) -> Option<Self> {
        let mut t = Self {
            number: 0,
            codec: VideoCodec::Vp8,
            codec_private: Vec::new(),
            entry: entry.to_vec(),
            width: 0,
            height: 0,
            default_duration: None,
        };
        let mut codec_id = None;
        for (i, b) in children(entry) {
            match i {
                id::TRACK_NUMBER => t.number = ebml::read_uint(b).unwrap_or(0),
                id::CODEC_ID => {
                    codec_id = codec(String::from_utf8_lossy(b).trim_end_matches('\0'));
                }
                id::CODEC_PRIVATE => t.codec_private = b.to_vec(),
                // Compression or encryption: the stored bytes are not the
                // frames.
                id::CONTENT_ENCODINGS => return None,
                vid::DEFAULT_DURATION => t.default_duration = ebml::read_uint(b),
                vid::VIDEO => {
                    for (i, b) in children(b) {
                        let v = ebml::read_uint(b).and_then(|v| u32::try_from(v).ok());
                        match i {
                            vid::PIXEL_WIDTH => t.width = v.unwrap_or(0),
                            vid::PIXEL_HEIGHT => t.height = v.unwrap_or(0),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        t.codec = codec_id?;
        (t.number != 0).then_some(t)
    }

    /// The body of the TrackEntry for an export, as track `number`: the
    /// source's entry with its number replaced.
    pub(super) fn output_entry(&self, number: u64) -> Vec<u8> {
        let mut out = Vec::new();
        ebml::put_uint(&mut out, id::TRACK_NUMBER, number);
        let mut p = 0usize;
        while let Ok(h) = ebml::parse_header(&self.entry[p..]) {
            let Some(end) = h
                .size
                .and_then(|s| usize::try_from(s).ok())
                .and_then(|s| (p + h.len).checked_add(s))
                .filter(|&e| e <= self.entry.len())
            else {
                break;
            };
            if h.id != id::TRACK_NUMBER {
                out.extend_from_slice(&self.entry[p..end]);
            }
            p = end;
        }
        out
    }
}

/// A video block as found in the source.
#[derive(Debug, Clone, Copy)]
pub(super) struct RawFrame {
    /// File offset and length of the frame data.
    pub offset: u64,
    /// See `offset`.
    pub len: u32,
    /// Absolute presentation time in `TimestampScale` units.
    pub pts: i64,
    /// Flagged as a keyframe by the muxer.
    pub key: bool,
}

/// The source's video: its track, its frames and where they are.
#[derive(Debug, Clone)]
pub struct MkvVideo {
    /// The track as the source describes it.
    pub track: VideoTrack,
    /// The frames in decode order; times are ticks of
    /// [`VideoStreamInfo::timescale`] from the smallest presentation time.
    pub index: VideoIndex,
    /// The absolute presentation time (`TimestampScale` units) of the
    /// index's time 0.
    pub origin: i64,
}

/// Builds the video index from the frames of the video track, in file
/// order. `timescale` is ticks per second (10^9 / `TimestampScale`).
/// Returns a warning instead when the frames do not form a stream we can
/// cut; the file then opens with its audio only.
pub(super) fn build<R: Read + Seek + ?Sized>(
    r: &mut R,
    track: VideoTrack,
    frames: &[RawFrame],
    timescale: u32,
    scale_ns: u64,
    warnings: &mut Vec<String>,
) -> Result<std::result::Result<MkvVideo, String>> {
    if frames.is_empty() {
        return Ok(Err("the video track has no frames".into()));
    }
    // Decode times: the presentation times in ascending order.
    let mut sorted: Vec<i64> = frames.iter().map(|f| f.pts).collect();
    sorted.sort_unstable();
    if sorted.windows(2).any(|w| w[0] == w[1]) {
        return Ok(Err(
            "two video frames share a timestamp (laced or duplicated frames)".into(),
        ));
    }
    let origin = sorted[0];
    let last = sorted.len() - 1;
    let fallback = track
        .default_duration
        .map(|ns| ns / scale_ns.max(1))
        .filter(|&d| d > 0);
    let mut durations: Vec<u32> = sorted
        .windows(2)
        .map(|w| u32::try_from(w[1] - w[0]).unwrap_or(u32::MAX))
        .collect();
    let last_duration = fallback
        .and_then(|d| u32::try_from(d).ok())
        .or_else(|| durations.last().copied())
        .unwrap_or(1);
    durations.push(last_duration);
    debug_assert_eq!(durations.len(), last + 1);

    let check = KeyframeCheck::new(track.codec, &track.codec_private).ok_or_else(|| {
        crate::ContainerError::Malformed {
            offset: 0,
            reason: format!("{} CodecPrivate is malformed", track.codec.name()),
        }
    })?;
    let mut demoted = 0;
    let mut out = Vec::with_capacity(frames.len());
    for (i, f) in frames.iter().enumerate() {
        // The first frame is left alone: the source itself is decoded
        // from there.
        let mut sync = f.key;
        if i > 0 && sync && is_keyframe(r, &check, f.offset, f.len)? == Some(false) {
            sync = false;
            demoted += 1;
        }
        let offset = f.pts - sorted[i];
        let Ok(composition_offset) = i32::try_from(offset) else {
            return Ok(Err("video frames are stored far out of order".into()));
        };
        out.push(VideoFrame {
            byte_offset: f.offset,
            byte_len: f.len,
            duration: durations[i],
            composition_offset,
            sync,
        });
    }
    if demoted > 0 {
        warnings.push(format!(
            "{demoted} video frame(s) marked as keyframes cannot start decoding \
             (open GOP); cuts start at the keyframe before"
        ));
    }
    let info = VideoStreamInfo {
        codec: track.codec,
        width: track.width,
        height: track.height,
        timescale,
        rotation: 0,
    };
    Ok(match VideoIndex::new(info, out) {
        Ok(index) => Ok(MkvVideo {
            track,
            index,
            origin,
        }),
        Err(e) => Err(format!("the video track cannot be cut: {e}")),
    })
}

/// Converts `t` ticks of the video timescale to milliseconds, the
/// timestamp unit of exports.
pub(super) fn to_ms(t: MediaTime, timescale: u32) -> i64 {
    lac_core::rescale(t, timescale, 1000, lac_core::Rounding::Nearest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn track(codec: VideoCodec) -> VideoTrack {
        VideoTrack {
            number: 2,
            codec,
            codec_private: Vec::new(),
            entry: Vec::new(),
            width: 64,
            height: 64,
            default_duration: None,
        }
    }

    fn frame(pts: i64, key: bool) -> RawFrame {
        RawFrame {
            offset: 0,
            len: 1,
            pts,
            key,
        }
    }

    #[test]
    fn codec_ids_are_mapped() {
        assert_eq!(codec("V_VP8"), Some(VideoCodec::Vp8));
        assert_eq!(codec("V_MPEGH/ISO/HEVC"), Some(VideoCodec::Hevc));
        assert_eq!(codec("V_MJPEG"), None);
    }

    #[test]
    fn decode_times_are_the_sorted_presentation_times() {
        // I P B B as stored: shown at 0, 100, 33, 67 (H.264 with
        // B-frames as ffmpeg writes it to Matroska).
        let frames = [
            frame(1000, true),
            frame(1100, false),
            frame(1033, false),
            frame(1067, false),
        ];
        // The VP8 check reads byte 0 of a frame: all data is zero, which
        // VP8 calls a key frame, so nothing is demoted.
        let mut r = Cursor::new(vec![0u8; 4]);
        let mut w = Vec::new();
        let v = build(
            &mut r,
            track(VideoCodec::Vp8),
            &frames,
            1000,
            1_000_000,
            &mut w,
        )
        .expect("io")
        .expect("valid");
        assert_eq!(v.origin, 1000);
        let ix = &v.index;
        assert_eq!(
            (0..4).map(|i| ix.presentation_time(i)).collect::<Vec<_>>(),
            [0, 100, 33, 67]
        );
        assert_eq!(
            (0..4).map(|i| ix.decode_time(i)).collect::<Vec<_>>(),
            [0, 33, 67, 100]
        );
        // The last decode slot lasts as long as the one before it; the
        // picture shown last (at 100) keeps its slot's 34 ms.
        assert_eq!(ix.display_range(), 0..134);
        assert!(w.is_empty());
    }

    #[test]
    fn duplicate_timestamps_drop_the_video() {
        let frames = [frame(0, true), frame(0, false)];
        let mut r = Cursor::new(vec![0u8; 4]);
        let v = build(
            &mut r,
            track(VideoCodec::Vp9),
            &frames,
            1000,
            1_000_000,
            &mut Vec::new(),
        )
        .expect("io");
        assert!(v.is_err());
    }

    #[test]
    fn output_entries_get_a_new_number() {
        let mut entry = Vec::new();
        ebml::put_uint(&mut entry, id::TRACK_NUMBER, 7);
        ebml::put_element(&mut entry, id::CODEC_ID, b"V_VP9");
        let mut video = Vec::new();
        ebml::put_uint(&mut video, vid::PIXEL_WIDTH, 64);
        ebml::put_uint(&mut video, vid::PIXEL_HEIGHT, 48);
        ebml::put_element(&mut entry, vid::VIDEO, &video);
        let t = VideoTrack::parse(&entry).expect("video track");
        assert_eq!(
            (t.number, t.codec, t.width, t.height),
            (7, VideoCodec::Vp9, 64, 48)
        );
        let out = VideoTrack::parse(&t.output_entry(2)).expect("still valid");
        assert_eq!((out.number, out.codec, out.width), (2, VideoCodec::Vp9, 64));
    }
}
