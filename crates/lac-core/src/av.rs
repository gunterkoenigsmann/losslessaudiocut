//! Planning cuts of files with audio and video, keeping them in sync.
//!
//! The editor's timeline is the audio track's: kept ranges are audio
//! sample positions. A video track can only be cut frame-exactly at the
//! file's start and end (by the edit list); at a join inside the file the
//! part after the join starts at a keyframe (see [`crate::plan_video_cuts`]).
//! The audio must then follow the video, or picture and sound drift apart
//! by up to a GOP per join.
//!
//! [`plan_av_cuts`] therefore plans the video first and derives the audio
//! ranges from it. Audio packets (1024 samples for AAC, 21.3 ms at 48 kHz)
//! do not line up with video frames, so at a join the audio cut lands on
//! the nearest packet boundary. Each audio cut is chosen relative to where
//! the output *already* is, not where it should ideally be, so the errors
//! never add up: both the offset of sound against picture inside every
//! part and the difference of the audio and video lengths at every join
//! stay within half an audio packet (property-tested below). People
//! notice offsets from about 45 ms on.

use std::ops::Range;

use crate::{
    CutCapabilities, CutPlan, MediaTime, PacketIndex, PlanError, Rounding, SamplePos, VideoIndex,
    plan_cuts, plan_video_cuts, rescale,
};

/// Plans the joined output of `ranges` (audio sample positions, sorted and
/// not overlapping) for a file with an audio track `audio` (cut with
/// `caps`) and a video track `video`.
///
/// `video_offset` is the video media time shown at audio sample 0 (the
/// source's edit lists): video time = sample · timescale / rate +
/// `video_offset`.
///
/// Returns one plan per kept part, each with its [`CutPlan::video`].
/// Parts whose video would overlap after moving a join to a keyframe (a
/// deleted gap shorter than the distance to the keyframe before it) are
/// merged, so there may be fewer plans than ranges. The parts' `actual`
/// ranges are what the listener gets; they follow the video's keyframes at
/// joins.
pub fn plan_av_cuts(
    audio: &PacketIndex,
    caps: CutCapabilities,
    video: &VideoIndex,
    video_offset: MediaTime,
    ranges: &[Range<SamplePos>],
) -> Result<Vec<CutPlan>, PlanError> {
    let rate = audio.info().sample_rate;
    let scale = video.info().timescale;
    let total = audio.total_samples();
    for r in ranges {
        if r.start > r.end || r.end > total {
            return Err(PlanError::OutOfRange {
                start: r.start,
                end: r.end,
                len: total,
            });
        }
    }
    let mut ranges: Vec<Range<SamplePos>> =
        ranges.iter().filter(|r| !r.is_empty()).cloned().collect();
    if ranges.is_empty() || rate == 0 {
        return plan_cuts(audio, caps, &ranges);
    }
    let shown = video.display_range();
    let to_video = |s: SamplePos, r: Rounding| {
        let t = rescale(
            MediaTime::try_from(s).unwrap_or(MediaTime::MAX),
            rate,
            scale,
            r,
        )
        .saturating_add(video_offset);
        t.clamp(shown.start, shown.end)
    };
    let to_audio = |t: MediaTime| {
        let s = rescale(
            t.saturating_sub(video_offset),
            scale,
            rate,
            Rounding::Nearest,
        );
        SamplePos::try_from(s.max(0)).unwrap_or(0).min(total)
    };

    // Plan the video; merge parts that a join would make overlap, until
    // none do.
    let vplans = loop {
        let vranges: Vec<Range<MediaTime>> = ranges
            .iter()
            .map(|r| to_video(r.start, Rounding::Down)..to_video(r.end, Rounding::Up))
            .collect();
        let vplans = plan_video_cuts(video, &vranges, true)?;
        let overlap = (1..vplans.len()).find(|&i| {
            !vplans[i].frames.is_empty()
                && !vplans[i - 1].frames.is_empty()
                && vplans[i].actual.start < vplans[i - 1].actual.end
        });
        match overlap {
            Some(i) => {
                ranges[i - 1].end = ranges[i].end;
                ranges.remove(i);
            }
            None => break vplans,
        }
    };

    // Inner audio cuts: exact where the format can cut there exactly,
    // else on the nearest packet boundary.
    let exact_inside = caps.lossless_reencode
        || (caps.can_trim_start && caps.can_trim_end && !caps.trims_at_file_ends_only);
    let snap = |s: SamplePos| -> SamplePos {
        if exact_inside {
            return s;
        }
        match audio.packet_containing(s) {
            Some(p) => {
                let pr = audio.packets()[p].sample_range();
                if s - pr.start <= pr.end - s {
                    pr.start
                } else {
                    pr.end
                }
            }
            None => total,
        }
    };

    // Output positions so far, in audio samples: of the audio (exact) and
    // of the video (its accumulated length, converted once, so rounding
    // does not add up either).
    let last = ranges.len() - 1;
    let mut audio_ranges = Vec::with_capacity(ranges.len());
    let mut audio_out: SamplePos = 0;
    // Output time of the video (ticks, from the output's start), which
    // begins with the first part that has pictures.
    let mut video_ticks: MediaTime = 0;
    let mut video_started = false;
    for (i, (r, v)) in ranges.iter().zip(&vplans).enumerate() {
        if !video_started && !v.frames.is_empty() {
            video_started = true;
            // The video begins with the audio output so far plus, when
            // the kept range starts before the source's first picture,
            // the time until that picture (the muxer's empty edit).
            let at = rescale(
                MediaTime::try_from(r.start).unwrap_or(MediaTime::MAX),
                rate,
                scale,
                Rounding::Down,
            )
            .saturating_add(video_offset);
            let so_far = rescale(
                MediaTime::try_from(audio_out).unwrap_or(MediaTime::MAX),
                rate,
                scale,
                Rounding::Nearest,
            );
            video_ticks = so_far.saturating_add((v.actual.start - at).max(0));
        }
        let video_out_before = to_audio(video_ticks.saturating_add(video_offset));
        let video_len = v.actual.end - v.actual.start;
        video_ticks = video_ticks.saturating_add(video_len);
        let video_out_after = to_audio(video_ticks.saturating_add(video_offset));
        let start = if i == 0 || v.frames.is_empty() {
            r.start
        } else {
            // Sound in sync with the picture where this part's picture
            // starts: source position of the picture plus how far the
            // audio output is ahead of the video output.
            let pic = to_audio(v.actual.start);
            snap(
                (pic + audio_out)
                    .saturating_sub(video_out_before)
                    .min(total),
            )
        };
        let end = if i == last || v.frames.is_empty() {
            r.end
        } else {
            // Both outputs end this part at (nearly) the same position.
            snap(
                (start + video_out_after)
                    .saturating_sub(audio_out)
                    .min(total),
            )
        };
        // A part whose picture is shorter than half a packet may keep no
        // sound at all; the next part makes up for it.
        let end = end.max(start);
        audio_out += end - start;
        audio_ranges.push(start..end);
    }

    let mut plans = plan_cuts(audio, caps, &audio_ranges)?;
    for (p, v) in plans.iter_mut().zip(vplans) {
        p.video = Some(v);
    }
    Ok(plans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AudioStreamInfo, PacketFlags, PacketInfo, VideoCodec, VideoFrame, VideoStreamInfo,
    };
    use proptest::prelude::*;

    /// AAC-like audio: `n` packets of 1024 samples at 48 kHz.
    fn aac(n: usize) -> PacketIndex {
        let packets = (0..n)
            .map(|i| PacketInfo {
                byte_offset: i as u64 * 10,
                byte_len: 10,
                first_sample: i as u64 * 1024,
                n_samples: 1024,
                hidden_before: 0,
                hidden_after: 0,
                flags: PacketFlags::default(),
            })
            .collect();
        PacketIndex::new(
            AudioStreamInfo {
                sample_rate: 48_000,
                channels: 2,
            },
            packets,
        )
        .expect("contiguous")
    }

    const AAC_CAPS: CutCapabilities = CutCapabilities {
        preroll_packets: 0,
        can_trim_start: true,
        can_trim_end: true,
        lossless_reencode: false,
        trims_at_file_ends_only: true,
    };

    /// Video of `n` frames of `d` ticks (timescale 90 kHz), a keyframe
    /// every `gop` frames, no B-frames.
    fn video(n: usize, d: u32, gop: usize) -> VideoIndex {
        let frames = (0..n)
            .map(|i| VideoFrame {
                byte_offset: i as u64 * 100,
                byte_len: 100,
                duration: d,
                composition_offset: 0,
                sync: i % gop == 0,
            })
            .collect();
        VideoIndex::new(
            VideoStreamInfo {
                codec: VideoCodec::H264,
                width: 64,
                height: 64,
                timescale: 90_000,
                rotation: 0,
            },
            frames,
        )
        .expect("valid")
    }

    #[test]
    fn file_ends_are_exact_and_joins_follow_keyframes() {
        // 10 s of audio, 10 s of 30 fps video with a keyframe every second.
        let a = aac(470);
        let v = video(300, 3000, 30);
        // Keep 1.5..3.5 s and 6.5..8.25 s.
        let plans =
            plan_av_cuts(&a, AAC_CAPS, &v, 0, &[72_000..168_000, 312_000..396_000]).expect("plan");
        assert_eq!(plans.len(), 2);
        let v0 = plans[0].video.as_ref().expect("video");
        let v1 = plans[1].video.as_ref().expect("video");
        // File ends: exact for both tracks.
        assert_eq!(plans[0].actual.start, 72_000);
        assert_eq!(v0.actual.start, 135_000);
        assert_eq!(plans[1].actual.end, 396_000);
        assert_eq!(v1.actual.end, 742_500);
        // The part after the join starts at the keyframe at 6 s.
        assert_eq!(v1.actual.start, 540_000);
        // The audio follows to within half a packet.
        assert!(plans[1].actual.start.abs_diff(288_000) <= 512);
    }

    #[test]
    fn a_short_gap_before_a_far_keyframe_merges_the_parts() {
        let a = aac(470);
        let v = video(300, 3000, 30);
        // Deleting 6.2..6.4 s: the part after it would start at the
        // keyframe at 6 s, before the part in front of it ends.
        let plans =
            plan_av_cuts(&a, AAC_CAPS, &v, 0, &[0..297_600, 307_200..480_000]).expect("plan");
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].actual, 0..480_000);
    }

    #[test]
    fn errors_are_reported() {
        let a = aac(10);
        let v = video(30, 3000, 30);
        assert!(plan_av_cuts(&a, AAC_CAPS, &v, 0, &[0..20_000, 30_000..40_000]).is_err());
        assert!(
            plan_av_cuts(&a, AAC_CAPS, &v, 0, &[])
                .expect("empty")
                .is_empty()
        );
    }

    proptest! {
        /// Picture and sound stay within half an audio packet of each
        /// other in every part, and the outputs within half a packet at
        /// every join, however many joins there are.
        #[test]
        fn sync_errors_do_not_add_up(
            cuts in proptest::collection::vec(0u64..470 * 1024, 2..12),
            gop in 1usize..40,
            frame in prop_oneof![Just(3000u32), Just(3003), Just(1500), Just(3600)],
            // Negative: the video starts up to 1.5 packets after the audio.
            offset in -3000i64..9000,
        ) {
            let a = aac(470);
            // Video covering the whole audio (after the offset): where
            // there is no picture, there is nothing to keep in sync.
            let ticks = (470 * 1024 * 90_000 / 48_000 + offset.max(0)) as u64;
            let n = ticks.div_ceil(u64::from(frame)) as usize;
            let v = video(n, frame, gop);
            let mut cuts = cuts;
            cuts.sort_unstable();
            cuts.dedup();
            let ranges: Vec<Range<SamplePos>> =
                cuts.as_chunks::<2>().0.iter().map(|&[a, b]| a..b).collect();
            let plans = plan_av_cuts(&a, AAC_CAPS, &v, offset, &ranges).expect("plan");
            let to_samples = |t: MediaTime| t as f64 * 48_000.0 / 90_000.0;
            let mut audio_out = 0.0;
            let mut video_out = 0.0;
            let mut started = false;
            let half = 512.0 + 1.0;
            for (i, p) in plans.iter().enumerate() {
                let vp = p.video.as_ref().expect("video");
                if !started && !vp.frames.is_empty() {
                    // The video starts with the audio so far, or later when
                    // its first picture comes after the part's start.
                    started = true;
                    let picture = to_samples(vp.actual.start - offset);
                    video_out = audio_out + (picture - p.actual.start as f64).max(0.0);
                } else if started && i > 0 {
                    // Sync where the part starts: source audio position
                    // minus source picture position equals the output
                    // offset of audio over video.
                    let off = p.actual.start as f64 - to_samples(vp.actual.start - offset)
                        - (audio_out - video_out);
                    prop_assert!(off.abs() <= half, "part {} offset {}", i, off);
                }
                audio_out += (p.actual.end - p.actual.start) as f64;
                video_out += to_samples(vp.actual.end - vp.actual.start);
                if started && i + 1 < plans.len() {
                    prop_assert!((audio_out - video_out).abs() <= half,
                        "join {}: audio {} video {}", i, audio_out, video_out);
                }
                // Parts never overlap and stay in order.
                if i > 0 {
                    prop_assert!(plans[i - 1].actual.end <= p.actual.start);
                    let prev = plans[i - 1].video.as_ref().expect("video");
                    prop_assert!(vp.frames.is_empty() || prev.actual.end <= vp.actual.start);
                }
            }
        }
    }
}
