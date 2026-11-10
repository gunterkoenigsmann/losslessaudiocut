//! Video streams and keyframe-aware cut planning.
//!
//! A video stream differs from an audio stream in two ways that matter for
//! lossless cutting (see `docs/architecture.md`, section 13):
//!
//! 1. **Only keyframes are independent.** A frame can be decoded only after
//!    the keyframe of its group of pictures (GOP) and every frame decoded in
//!    between, so a kept part always starts at a keyframe. Phones write one
//!    about every second.
//! 2. **Decode order is not display order.** With B-frames, a frame shown
//!    later may be decoded (and therefore stored) earlier, because frames
//!    shown in between refer to it. Every frame is stored after all frames
//!    it refers to, which is what [`plan_video_cut`] relies on: copying a
//!    contiguous run of frames *in decode order*, starting at a suitable
//!    keyframe, copies every reference of every frame in it.
//!
//! Frames the cut does not want but has to copy anyway are hidden by the
//! container's edit list at the file's start and end (MP4 `elst`, ISO/IEC
//! 14496-12 8.6.6), just like audio pre-roll. At a join inside a file there
//! is nothing that can hide them, so [`plan_video_cuts`] widens the ends
//! that meet at a join until every copied frame is also shown.
//!
//! Times are integer ticks of the stream's own timescale ([`MediaTime`]),
//! e.g. 1/90000 s; [`rescale`] converts between timescales, such as the
//! audio sample positions of the editor's timeline and video ticks.

use std::ops::Range;

/// A position or a duration in ticks of a video stream's timescale.
///
/// Signed, because a source's edit list may place frames before the
/// presentation start (0), and MP4 composition offsets may be negative.
pub type MediaTime = i64;

/// The compression format of a video stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    /// H.264 / AVC (ISO/IEC 14496-10).
    H264,
    /// H.265 / HEVC (ISO/IEC 23008-2).
    Hevc,
    /// Anything else, named by its four-character code (e.g. `mp4v`).
    /// Such streams can be copied, but keyframes are only known from the
    /// container.
    Other([u8; 4]),
}

/// Basic parameters of a video stream, independent of its container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoStreamInfo {
    /// The compression format.
    pub codec: VideoCodec,
    /// Width of the coded picture in pixels, before rotation.
    pub width: u32,
    /// Height of the coded picture in pixels, before rotation.
    pub height: u32,
    /// Ticks per second of all [`MediaTime`] values of the stream.
    pub timescale: u32,
    /// Clockwise rotation a player applies for display: 0, 90, 180 or 270
    /// degrees. Phones record portrait videos as landscape frames plus a
    /// rotation in the track's display matrix.
    pub rotation: u16,
}

/// One compressed video frame (an MP4 sample) as found in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFrame {
    /// Position of the frame's data in the source file, in bytes.
    pub byte_offset: u64,
    /// Length of the frame's data in bytes.
    pub byte_len: u32,
    /// Ticks until the next frame in decode order (MP4 `stts`). The decode
    /// time of a frame is the sum of the durations of all frames before it.
    pub duration: u32,
    /// Presentation time minus decode time (MP4 `ctts`), 0 without
    /// B-frames.
    pub composition_offset: i32,
    /// The frame is a keyframe: decoding can start here (MP4 `stss`).
    pub sync: bool,
}

/// Error raised when a frame list does not form a valid video stream.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VideoIndexError {
    /// A timescale of 0 ticks per second makes every time meaningless.
    #[error("the video timescale is 0")]
    ZeroTimescale,
    /// The first frame in decode order must be a keyframe, otherwise the
    /// stream cannot be decoded from its start.
    #[error("the first video frame is not a keyframe")]
    FirstFrameNotSync,
    /// Every frame must last at least one tick.
    #[error("video frame {index} has a duration of 0")]
    ZeroDuration {
        /// Decode index of the offending frame.
        index: usize,
    },
    /// Two frames would be shown at the same time.
    #[error("video frames {first} and {second} have the same presentation time {time}")]
    DuplicatePresentationTime {
        /// Decode index of the earlier frame.
        first: usize,
        /// Decode index of the later frame.
        second: usize,
        /// The shared presentation time.
        time: MediaTime,
    },
}

/// The immutable sequence of frames of one video stream, in decode order.
///
/// Built once by a demuxer. Besides the frames it holds the display order
/// and, per frame, whether decoding may start there without showing
/// anything out of place (a *clean start*, see
/// [`VideoIndex::is_clean_start`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoIndex {
    info: VideoStreamInfo,
    frames: Vec<VideoFrame>,
    /// Decode time of each frame (prefix sums of the durations).
    decode_times: Vec<MediaTime>,
    /// Decode indexes sorted by presentation time.
    display: Vec<usize>,
    /// Position of each frame (by decode index) in `display`.
    rank: Vec<usize>,
    /// Per decode index: the frame is a keyframe, every frame decoded
    /// before it is shown before it, and every frame decoded after it is
    /// shown after it.
    clean: Vec<bool>,
}

impl VideoIndex {
    /// Creates an index after checking the stream's invariants.
    pub fn new(info: VideoStreamInfo, frames: Vec<VideoFrame>) -> Result<Self, VideoIndexError> {
        if info.timescale == 0 {
            return Err(VideoIndexError::ZeroTimescale);
        }
        if frames.first().is_some_and(|f| !f.sync) {
            return Err(VideoIndexError::FirstFrameNotSync);
        }
        let mut decode_times = Vec::with_capacity(frames.len());
        let mut t: MediaTime = 0;
        for (index, f) in frames.iter().enumerate() {
            if f.duration == 0 {
                return Err(VideoIndexError::ZeroDuration { index });
            }
            decode_times.push(t);
            t += MediaTime::from(f.duration);
        }
        let pts = |i: usize| decode_times[i] + MediaTime::from(frames[i].composition_offset);
        let mut display: Vec<usize> = (0..frames.len()).collect();
        display.sort_by_key(|&i| (pts(i), i));
        for w in display.windows(2) {
            if pts(w[0]) == pts(w[1]) {
                return Err(VideoIndexError::DuplicatePresentationTime {
                    first: w[0],
                    second: w[1],
                    time: pts(w[0]),
                });
            }
        }
        let mut rank = vec![0; frames.len()];
        for (r, &i) in display.iter().enumerate() {
            rank[i] = r;
        }
        // A keyframe k is a clean start when the frames decoded before it are
        // exactly the frames shown before it. That rules out the leading
        // pictures of an open GOP (HEVC RASL frames after a CRA, H.264 B-frames
        // after a non-IDR I-frame): they are decoded after k, shown before k,
        // and refer to frames of the previous GOP. In ranks: the highest rank
        // among frames 0..k is below k's rank, and the lowest rank among
        // frames k.. is k's rank.
        let mut clean = vec![false; frames.len()];
        let mut suffix_min = usize::MAX;
        let mut suffix_mins = vec![0; frames.len()];
        for i in (0..frames.len()).rev() {
            suffix_min = suffix_min.min(rank[i]);
            suffix_mins[i] = suffix_min;
        }
        let mut prefix_max: Option<usize> = None;
        for i in 0..frames.len() {
            clean[i] = frames[i].sync
                && prefix_max.is_none_or(|m| m < rank[i])
                && suffix_mins[i] == rank[i];
            prefix_max = Some(prefix_max.map_or(rank[i], |m| m.max(rank[i])));
        }
        Ok(Self {
            info,
            frames,
            decode_times,
            display,
            rank,
            clean,
        })
    }

    /// Stream parameters.
    pub fn info(&self) -> VideoStreamInfo {
        self.info
    }

    /// All frames in decode order (the order they are stored and copied in).
    pub fn frames(&self) -> &[VideoFrame] {
        &self.frames
    }

    /// Decode time of the frame with decode index `i`.
    ///
    /// # Panics
    /// If `i` is not a valid frame index.
    pub fn decode_time(&self, i: usize) -> MediaTime {
        self.decode_times[i]
    }

    /// Presentation time of the frame with decode index `i`.
    ///
    /// # Panics
    /// If `i` is not a valid frame index.
    pub fn presentation_time(&self, i: usize) -> MediaTime {
        self.decode_times[i] + MediaTime::from(self.frames[i].composition_offset)
    }

    /// Decode indexes in display order.
    pub fn display_order(&self) -> &[usize] {
        &self.display
    }

    /// The time span during which the frame with decode index `i` is shown:
    /// from its presentation time to the next frame's, or for the last
    /// frame shown, for its own duration.
    ///
    /// # Panics
    /// If `i` is not a valid frame index.
    pub fn display_interval(&self, i: usize) -> Range<MediaTime> {
        self.rank_interval(self.rank[i])
    }

    /// The time span covered by all frames, empty for an empty stream.
    pub fn display_range(&self) -> Range<MediaTime> {
        match (self.display.first(), self.display.last()) {
            (Some(&first), Some(&last)) => {
                self.presentation_time(first)..self.display_interval(last).end
            }
            _ => 0..0,
        }
    }

    /// Decode index of the frame shown at time `t`, or `None` if `t` is
    /// outside [`VideoIndex::display_range`].
    pub fn frame_at(&self, t: MediaTime) -> Option<usize> {
        if !self.display_range().contains(&t) {
            return None;
        }
        // The last frame whose presentation time is at or before t.
        let r = self
            .display
            .partition_point(|&i| self.presentation_time(i) <= t);
        Some(self.display[r - 1])
    }

    /// True if decoding may start at the frame with decode index `i` and
    /// the copied frames from there on show a gap-free picture sequence
    /// starting with it.
    ///
    /// That holds for a keyframe of a *closed* GOP (H.264 IDR, HEVC IDR or
    /// a CRA without leading pictures). It does not hold for a keyframe
    /// whose GOP is open, because frames decoded after it are shown before
    /// it and refer to the previous GOP.
    ///
    /// # Panics
    /// If `i` is not a valid frame index.
    pub fn is_clean_start(&self, i: usize) -> bool {
        self.clean[i]
    }

    /// Presentation times of all clean starts, in display order: the points
    /// a kept part may start at without an edit list (for the GUI's
    /// snapping and keyframe ticks).
    pub fn clean_start_times(&self) -> impl Iterator<Item = MediaTime> + '_ {
        // Clean starts are shown in the same order as they are decoded.
        (0..self.frames.len())
            .filter(|&i| self.clean[i])
            .map(|i| self.presentation_time(i))
    }

    fn rank_interval(&self, r: usize) -> Range<MediaTime> {
        let i = self.display[r];
        let start = self.presentation_time(i);
        let end = match self.display.get(r + 1) {
            Some(&next) => self.presentation_time(next),
            None => start + MediaTime::from(self.frames[i].duration),
        };
        start..end
    }
}

/// The result of planning one kept range of a video stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoCutPlan {
    /// Frames to copy, as decode indexes. Starts at a keyframe and contains
    /// every frame that a frame in it refers to. Empty for an empty range.
    pub frames: Range<usize>,
    /// The time span shown by the copied frames. Contains `actual`.
    pub copied: Range<MediaTime>,
    /// The time span the viewer will actually get. Equals the requested
    /// range at ends that may be trimmed; elsewhere it is widened to
    /// `copied`.
    pub actual: Range<MediaTime>,
}

impl VideoCutPlan {
    /// Ticks the container must hide in front of `actual` (the edit list's
    /// media time, relative to the first copied frame shown).
    pub fn trim_start(&self) -> MediaTime {
        self.actual.start - self.copied.start
    }

    /// Ticks the container must hide after `actual`.
    pub fn trim_end(&self) -> MediaTime {
        self.copied.end - self.actual.end
    }
}

/// Errors of [`plan_video_cut`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VideoPlanError {
    /// The requested range is reversed or reaches beyond the stream.
    #[error("range {start}..{end} is outside the video stream ({first}..{last})")]
    OutOfRange {
        /// Start of the requested range.
        start: MediaTime,
        /// End of the requested range.
        end: MediaTime,
        /// Start of the stream's display range.
        first: MediaTime,
        /// End of the stream's display range.
        last: MediaTime,
    },
}

/// Plans the lossless extraction of the time span `range` of a video
/// stream.
///
/// `trim_start` and `trim_end` say whether the container can hide copied
/// frames before and after the range (an MP4 edit list can at the file's
/// start and end). Where it cannot, the range is widened outwards so that
/// every copied frame is shown: no wanted frame is ever lost.
///
/// The copied frames start at the nearest clean start (see
/// [`VideoIndex::is_clean_start`]) at or before the first wanted frame in
/// decode order, or at the stream's first frame if there is none. They end
/// at the last wanted frame in decode order, extended until the frames
/// shown form one gap-free sequence. Because every frame is stored after
/// the frames it refers to, this copies every reference.
pub fn plan_video_cut(
    index: &VideoIndex,
    range: Range<MediaTime>,
    trim_start: bool,
    trim_end: bool,
) -> Result<VideoCutPlan, VideoPlanError> {
    let all = index.display_range();
    if range.start > range.end || range.start < all.start || range.end > all.end {
        return Err(VideoPlanError::OutOfRange {
            start: range.start,
            end: range.end,
            first: all.start,
            last: all.end,
        });
    }
    if range.is_empty() {
        return Ok(VideoCutPlan {
            frames: 0..0,
            copied: range.start..range.start,
            actual: range.clone(),
        });
    }
    // Both lookups succeed: the range is non-empty and inside the stream.
    let first_rank = index.frame_at(range.start).map_or(0, |i| index.rank[i]);
    let last_rank = index
        .frame_at(range.end - 1)
        .map_or(index.display.len() - 1, |i| index.rank[i]);
    let wanted = &index.display[first_rank..=last_rank];
    let lo = wanted.iter().copied().min().unwrap_or(0);
    let mut hi = wanted.iter().copied().max().unwrap_or(0);
    let start = (0..=lo).rev().find(|&i| index.clean[i]).unwrap_or(0);

    // Close the copied run [start, hi] over display order: every frame
    // shown between the earliest and latest copied frame must be copied
    // too, or the output would show the frame before it for longer.
    // `hi` only grows and is bounded by the last frame, so this terminates.
    // No frame shown in between was decoded before `start`: for a clean
    // start by definition, and otherwise `start` is 0.
    let (min_rank, max_rank) = loop {
        let (min_rank, max_rank) = rank_span(index, start..hi + 1);
        let needed = index.display[min_rank..=max_rank]
            .iter()
            .copied()
            .max()
            .unwrap_or(hi);
        if needed <= hi {
            break (min_rank, max_rank);
        }
        hi = needed;
    };
    let copied = index.rank_interval(min_rank).start..index.rank_interval(max_rank).end;
    let actual = (if trim_start {
        range.start
    } else {
        copied.start
    })..(if trim_end { range.end } else { copied.end });
    Ok(VideoCutPlan {
        frames: start..hi + 1,
        copied,
        actual,
    })
}

/// Lowest and highest display rank of the frames with decode indexes in
/// `frames` (non-empty).
fn rank_span(index: &VideoIndex, frames: Range<usize>) -> (usize, usize) {
    index.rank[frames]
        .iter()
        .fold((usize::MAX, 0), |(lo, hi), &r| (lo.min(r), hi.max(r)))
}

/// Plans the ranges that are joined into one output file, in order.
///
/// With `trim_file_ends` (the container has an edit list), the start of the
/// first and the end of the last range are frame-exact; the ends meeting at
/// a join are widened so that every copied frame is shown, which puts the
/// start of a part after a join on a keyframe. Without it, every end is
/// widened.
pub fn plan_video_cuts(
    index: &VideoIndex,
    ranges: &[Range<MediaTime>],
    trim_file_ends: bool,
) -> Result<Vec<VideoCutPlan>, VideoPlanError> {
    let last = ranges.len().saturating_sub(1);
    ranges
        .iter()
        .enumerate()
        .map(|(i, r)| {
            plan_video_cut(
                index,
                r.clone(),
                trim_file_ends && i == 0,
                trim_file_ends && i == last,
            )
        })
        .collect()
}

/// How [`rescale`] rounds a result that falls between two ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rounding {
    /// Towards negative infinity.
    Down,
    /// Towards positive infinity.
    Up,
    /// To the nearest tick, halves away from zero.
    Nearest,
}

/// Converts `t` ticks of a `from` timescale to ticks of a `to` timescale,
/// e.g. audio sample positions (`from` = sample rate) to video ticks.
///
/// Exact integer arithmetic; the only rounding is the one asked for.
/// Saturates at the limits of [`MediaTime`]. A `from` of 0 gives 0.
pub fn rescale(t: MediaTime, from: u32, to: u32, rounding: Rounding) -> MediaTime {
    if from == 0 {
        return 0;
    }
    let num = i128::from(t) * i128::from(to);
    let den = i128::from(from);
    let q = match rounding {
        Rounding::Down => num.div_euclid(den),
        Rounding::Up => -(-num).div_euclid(den),
        Rounding::Nearest => {
            let q = (2 * num.abs() + den) / (2 * den);
            if num < 0 { -q } else { q }
        }
    };
    MediaTime::try_from(q).unwrap_or(if q < 0 {
        MediaTime::MIN
    } else {
        MediaTime::MAX
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const INFO: VideoStreamInfo = VideoStreamInfo {
        codec: VideoCodec::H264,
        width: 160,
        height: 120,
        timescale: 90_000,
        rotation: 0,
    };

    /// A generated stream plus, per frame, the decode indexes it refers to.
    #[derive(Debug, Clone)]
    struct Stream {
        index: VideoIndex,
        refs: Vec<Vec<usize>>,
    }

    /// One GOP of the generator: anchors (I or P frames), B-frames between
    /// two anchors, and whether the GOP is open (leading B-frames shown
    /// before its keyframe that refer to the previous GOP).
    #[derive(Debug, Clone, Copy)]
    struct Gop {
        anchors: usize,
        b_frames: usize,
        open: bool,
    }

    /// Builds a stream the way an encoder lays it out: per GOP, decode order
    /// I [leading B..] P B.. P B.., display order [leading B..] I B.. P B.. P.
    /// `durations` are used per frame when there are no B-frames (variable
    /// frame rate); with B-frames, all frames last `durations[0]`.
    fn build(gops: &[Gop], durations: &[u32]) -> Stream {
        // (display slot, sync, refs) in decode order.
        let mut out: Vec<(usize, bool, Vec<usize>)> = Vec::new();
        let mut slot = 0;
        let mut prev_anchor: Option<usize> = None;
        for (g, gop) in gops.iter().enumerate() {
            let leading = if g > 0 && gop.open { gop.b_frames } else { 0 };
            let key = out.len();
            out.push((slot + leading, true, Vec::new()));
            for l in 0..leading {
                let refs = prev_anchor.into_iter().chain([key]).collect();
                out.push((slot + l, false, refs));
            }
            let mut anchor = key;
            let mut anchor_slot = slot + leading;
            for _ in 1..gop.anchors {
                let p_slot = anchor_slot + gop.b_frames + 1;
                let p = out.len();
                out.push((p_slot, false, vec![anchor]));
                for b in 0..gop.b_frames {
                    out.push((anchor_slot + 1 + b, false, vec![anchor, p]));
                }
                anchor = p;
                anchor_slot = p_slot;
            }
            prev_anchor = Some(anchor);
            slot = anchor_slot + 1;
        }
        let with_b = gops
            .iter()
            .any(|g| g.b_frames > 0 && (g.anchors > 1 || g.open));
        let frames: Vec<VideoFrame> = if with_b {
            // Constant frame rate: decode time i·d, presentation time slot·d.
            let d = durations[0];
            out.iter()
                .enumerate()
                .map(|(i, &(s, sync, _))| VideoFrame {
                    byte_offset: i as u64 * 100,
                    byte_len: 100,
                    duration: d,
                    composition_offset: (s as i32 - i as i32) * d as i32,
                    sync,
                })
                .collect()
        } else {
            out.iter()
                .enumerate()
                .map(|(i, &(_, sync, _))| VideoFrame {
                    byte_offset: i as u64 * 100,
                    byte_len: 100,
                    duration: durations[i % durations.len()],
                    composition_offset: 0,
                    sync,
                })
                .collect()
        };
        Stream {
            index: VideoIndex::new(INFO, frames).expect("generated stream is valid"),
            refs: out.into_iter().map(|(_, _, r)| r).collect(),
        }
    }

    fn closed(anchors: usize, b_frames: usize) -> Gop {
        Gop {
            anchors,
            b_frames,
            open: false,
        }
    }

    #[test]
    fn display_order_follows_composition_offsets() {
        // I P B B P B B, shown as I B B P B B P.
        let s = build(&[closed(3, 2)], &[3000]);
        assert_eq!(s.index.display_order(), [0, 2, 3, 1, 5, 6, 4]);
        assert_eq!(s.index.display_range(), 0..7 * 3000);
        assert_eq!(s.index.frame_at(3000), Some(2));
        assert_eq!(s.index.frame_at(3 * 3000 + 1), Some(1));
        assert_eq!(s.index.frame_at(7 * 3000), None);
        assert_eq!(s.index.display_interval(4), 6 * 3000..7 * 3000);
    }

    #[test]
    fn open_gop_keyframes_are_not_clean_starts() {
        let open = Gop {
            anchors: 2,
            b_frames: 2,
            open: true,
        };
        let s = build(&[closed(2, 2), open, closed(2, 2)], &[1000]);
        let keys: Vec<usize> = (0..s.index.frames().len())
            .filter(|&i| s.index.frames()[i].sync)
            .collect();
        assert_eq!(keys.len(), 3);
        assert!(s.index.is_clean_start(keys[0]));
        assert!(!s.index.is_clean_start(keys[1]));
        assert!(s.index.is_clean_start(keys[2]));
        // A cut starting at the open GOP's keyframe starts one GOP earlier.
        let t = s.index.presentation_time(keys[1]);
        let plan = plan_video_cut(&s.index, t..t + 1000, true, true).expect("in range");
        assert_eq!(plan.frames.start, keys[0]);
        assert_eq!(plan.actual, t..t + 1000);
    }

    #[test]
    fn cut_without_trimming_snaps_to_keyframes() {
        // Three closed GOPs of 30 frames at 30 fps (3000 ticks of 90 kHz).
        let s = build(&[closed(30, 0); 3], &[3000]);
        let gop = 30 * 3000;
        let plan = plan_video_cut(&s.index, gop + 4500..gop + 9000, false, false).expect("ok");
        assert_eq!(plan.frames, 30..33);
        assert_eq!(plan.actual, gop..gop + 9000);
        let plan = plan_video_cut(&s.index, gop + 4500..gop + 9000, true, true).expect("ok");
        assert_eq!(plan.actual, gop + 4500..gop + 9000);
        assert_eq!((plan.trim_start(), plan.trim_end()), (4500, 0));
    }

    #[test]
    fn joins_are_widened_and_file_ends_trimmed() {
        let s = build(&[closed(30, 0); 3], &[3000]);
        let gop = 30 * 3000;
        let plans = plan_video_cuts(
            &s.index,
            &[1000..gop / 2, gop + gop / 2..2 * gop + 1000],
            true,
        )
        .expect("ok");
        assert_eq!(plans[0].actual, 1000..gop / 2);
        // The part after the join starts at its keyframe, and its end is
        // the end of the file, so it is trimmed.
        assert_eq!(plans[1].actual, gop..2 * gop + 1000);
    }

    #[test]
    fn invalid_streams_are_rejected() {
        let frame = VideoFrame {
            byte_offset: 0,
            byte_len: 1,
            duration: 1,
            composition_offset: 0,
            sync: true,
        };
        let no_key = VideoFrame {
            sync: false,
            ..frame
        };
        assert_eq!(
            VideoIndex::new(INFO, vec![no_key]),
            Err(VideoIndexError::FirstFrameNotSync)
        );
        assert_eq!(
            VideoIndex::new(
                INFO,
                vec![
                    frame,
                    VideoFrame {
                        duration: 0,
                        ..frame
                    }
                ]
            ),
            Err(VideoIndexError::ZeroDuration { index: 1 })
        );
        assert_eq!(
            VideoIndex::new(
                INFO,
                vec![
                    frame,
                    VideoFrame {
                        composition_offset: -1,
                        ..frame
                    }
                ]
            ),
            Err(VideoIndexError::DuplicatePresentationTime {
                first: 0,
                second: 1,
                time: 0
            })
        );
        assert_eq!(
            VideoIndex::new(
                VideoStreamInfo {
                    timescale: 0,
                    ..INFO
                },
                vec![frame]
            ),
            Err(VideoIndexError::ZeroTimescale)
        );
        let empty = VideoIndex::new(INFO, Vec::new()).expect("valid");
        assert_eq!(empty.display_range(), 0..0);
        assert!(
            plan_video_cut(&empty, 0..0, true, true)
                .expect("empty")
                .frames
                .is_empty()
        );
        assert!(plan_video_cut(&empty, 0..1, true, true).is_err());
    }

    #[test]
    fn rescale_rounds_as_asked() {
        // One 48 kHz sample is 1.875 ticks of 90 kHz.
        assert_eq!(rescale(1, 48_000, 90_000, Rounding::Down), 1);
        assert_eq!(rescale(1, 48_000, 90_000, Rounding::Up), 2);
        assert_eq!(rescale(1, 48_000, 90_000, Rounding::Nearest), 2);
        assert_eq!(rescale(-1, 48_000, 90_000, Rounding::Down), -2);
        assert_eq!(rescale(-1, 48_000, 90_000, Rounding::Up), -1);
        assert_eq!(rescale(-1, 48_000, 90_000, Rounding::Nearest), -2);
        assert_eq!(rescale(48_000, 48_000, 90_000, Rounding::Down), 90_000);
        assert_eq!(
            rescale(MediaTime::MAX, 1, 2, Rounding::Down),
            MediaTime::MAX
        );
        assert_eq!(rescale(5, 0, 2, Rounding::Down), 0);
    }

    fn gop() -> impl Strategy<Value = Gop> {
        (1usize..6, 0usize..3, any::<bool>()).prop_map(|(anchors, b_frames, open)| Gop {
            anchors,
            b_frames,
            open,
        })
    }

    fn stream() -> impl Strategy<Value = Stream> {
        (
            proptest::collection::vec(gop(), 1..6),
            proptest::collection::vec(1u32..4000, 1..8),
        )
            .prop_map(|(gops, durations)| build(&gops, &durations))
    }

    proptest! {
        #[test]
        fn video_plan_invariants(
            s in stream(),
            a in 0.0f64..=1.0, b in 0.0f64..=1.0,
            trim_start in any::<bool>(), trim_end in any::<bool>(),
        ) {
            let all = s.index.display_range();
            let len = all.end - all.start;
            let at = |x: f64| all.start + (x * len as f64) as MediaTime;
            let range = at(a.min(b))..at(a.max(b));
            let plan = plan_video_cut(&s.index, range.clone(), trim_start, trim_end)
                .expect("in range");
            if range.is_empty() {
                prop_assert!(plan.frames.is_empty());
                return Ok(());
            }
            let frames = plan.frames.clone();
            // Decoding starts at a keyframe and every reference is copied.
            prop_assert!(s.index.frames()[frames.start].sync);
            for i in frames.clone() {
                for &r in &s.refs[i] {
                    prop_assert!(frames.contains(&r), "frame {} refers to {} outside {:?}", i, r, frames);
                }
            }
            // Every wanted frame is copied.
            for i in 0..s.index.frames().len() {
                let shown = s.index.display_interval(i);
                if shown.start < range.end && range.start < shown.end {
                    prop_assert!(frames.contains(&i), "wanted frame {} not in {:?}", i, frames);
                }
            }
            // The copied frames are shown without gaps, and `copied` is
            // exactly their span.
            let mut ranks: Vec<usize> = frames.clone().map(|i| s.index.rank[i]).collect();
            ranks.sort_unstable();
            prop_assert!(ranks.windows(2).all(|w| w[1] == w[0] + 1));
            prop_assert_eq!(plan.copied.start, s.index.rank_interval(ranks[0]).start);
            prop_assert_eq!(plan.copied.end, s.index.rank_interval(ranks[ranks.len() - 1]).end);
            // No wanted time is lost, trimmed ends are exact, untrimmed ends
            // show every copied frame.
            prop_assert!(plan.actual.start <= range.start && plan.actual.end >= range.end);
            prop_assert!(plan.copied.start <= plan.actual.start && plan.actual.end <= plan.copied.end);
            prop_assert_eq!(plan.actual.start, if trim_start { range.start } else { plan.copied.start });
            prop_assert_eq!(plan.actual.end, if trim_end { range.end } else { plan.copied.end });
        }

        #[test]
        fn rescale_round_trips_within_one_tick(
            t in -10_000_000i64..10_000_000, from in 1u32..200_000, to in 1u32..200_000,
        ) {
            let down = rescale(t, from, to, Rounding::Down);
            let up = rescale(t, from, to, Rounding::Up);
            let near = rescale(t, from, to, Rounding::Nearest);
            prop_assert!(up - down <= 1);
            prop_assert!(down <= near && near <= up);
            // Exact: down·from ≤ t·to < (down+1)·from.
            let (t, from, to) = (i128::from(t), i128::from(from), i128::from(to));
            prop_assert!(i128::from(down) * from <= t * to);
            prop_assert!(t * to < (i128::from(down) + 1) * from);
        }
    }
}
