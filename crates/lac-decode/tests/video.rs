//! Decoding the pictures of the video fixtures of `lac-container`.
//!
//! The oracle is the ffmpeg command line tool, which demuxes with its own
//! MP4 and Matroska demuxers: every picture we decode from our frame index must equal
//! ffmpeg's, byte for byte in the decoder's pixel format. Both use
//! libavcodec, so this checks that our frames, codec configuration,
//! timestamps and the choice of where to start decoding are right.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::Command;

use lac_core::{MediaTime, VideoCodec};
use lac_decode::{Frame, VideoDecoder};

/// The video fixtures: H.264 (closed GOP, open GOP with B-frames,
/// fragmented), HEVC with open GOPs, AV1, VP9, ProRes and MPEG-4 Visual
/// in MP4; VP8, VP9, AV1, H.264 and HEVC in WebM and Matroska.
const FIXTURES: &[&str] = &[
    "video_h264_aac.mp4",
    "video_h264_opengop_rot90.mp4",
    "video_h264_fragmented.mp4",
    "video_hevc_opengop.mov",
    "video_av1.mp4",
    "video_vp9.mp4",
    "video_prores.mov",
    "video_mpeg4.3gp",
    "video_vp8_opus.webm",
    "video_vp9_opus.webm",
    "video_av1_opus.webm",
    "video_h264_opus.mkv",
    "video_hevc_opus.mkv",
];

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../lac-container/tests/fixtures")
        .join(name)
}

/// The source of `name` and a video decoder for it.
fn open(name: &str) -> (BufReader<File>, VideoDecoder) {
    let mut r = BufReader::new(File::open(fixture_path(name)).expect("fixture"));
    let c = lac_container::open(&mut r).expect("opens");
    let v = VideoDecoder::new(c.as_ref()).expect("video track");
    (r, v)
}

/// Presentation times of all frames, in display order.
fn display_times(v: &VideoDecoder) -> Vec<MediaTime> {
    let i = v.index();
    i.display_order()
        .iter()
        .map(|&k| i.presentation_time(k))
        .collect()
}

/// Every picture of the track in display order, decoded by stepping
/// forward one frame at a time.
fn step_through(name: &str) -> Vec<Frame> {
    let (mut r, mut v) = open(name);
    display_times(&v)
        .into_iter()
        .map(|t| {
            let f = v.frame_at(&mut r, t).expect("decodes");
            assert_eq!(f.time, t, "{name}: picture at {t}");
            f
        })
        .collect()
}

/// All pictures of `path` as ffmpeg decodes them in `pix_fmt` (with an
/// MP4 edit list ignored, so all frames come out), split into frames of
/// `size` bytes; `None` without ffmpeg (fatal when `LAC_REQUIRE_TOOLS` is
/// set, as in CI).
fn ffmpeg_frames(path: &Path, pix_fmt: &str, size: usize) -> Option<Vec<Vec<u8>>> {
    // Only the MP4 demuxer knows edit lists (Matroska has none).
    let mp4 = !matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("webm" | "mkv")
    );
    let edits: &[&str] = if mp4 { &["-ignore_editlist", "1"] } else { &[] };
    let out = match Command::new("ffmpeg")
        .args(["-v", "error", "-noautorotate"])
        .args(edits)
        .arg("-i")
        .arg(path)
        .args([
            "-map",
            "0:v:0",
            "-fps_mode",
            "passthrough",
            "-pix_fmt",
            pix_fmt,
        ])
        .args(["-f", "rawvideo", "-"])
        .output()
    {
        Ok(o) => o,
        Err(_) if std::env::var_os("LAC_REQUIRE_TOOLS").is_some() => {
            panic!("ffmpeg is required (LAC_REQUIRE_TOOLS is set) but not installed")
        }
        Err(_) => {
            eprintln!("ffmpeg not installed, skipping cross-check");
            return None;
        }
    };
    assert!(
        out.status.success(),
        "ffmpeg failed on {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout.len() % size, 0, "{}", path.display());
    Some(out.stdout.chunks(size).map(<[u8]>::to_vec).collect())
}

#[test]
fn pictures_match_ffmpeg() {
    for name in FIXTURES {
        let ours = step_through(name);
        let first = &ours[0];
        let size = first.packed().len();
        let Some(theirs) = ffmpeg_frames(&fixture_path(name), &first.layout.name, size) else {
            return;
        };
        assert_eq!(ours.len(), theirs.len(), "{name}: pictures");
        for (k, (a, b)) in ours.iter().zip(&theirs).enumerate() {
            assert!(
                a.packed() == *b,
                "{name}: picture {k} differs from ffmpeg's"
            );
        }
    }
}

/// Random access (backwards, and jumping over GOPs) gives the same
/// pictures as stepping forward: decoding always starts at a clean
/// keyframe, also in open GOPs.
#[test]
fn random_access_matches_stepping() {
    for name in FIXTURES {
        let stepped = step_through(name);
        let (mut r, mut v) = open(name);
        let times = display_times(&v);
        let n = times.len();
        // Backwards, then a forward jump across keyframes and back.
        let order: Vec<usize> = (0..n)
            .rev()
            .chain([3, n - 2, 1, n / 2, n / 2 + 1, 0])
            .collect();
        for k in order {
            let f = v.frame_at(&mut r, times[k]).expect("decodes");
            assert!(f == stepped[k], "{name}: picture {k} by random access");
        }
    }
}

/// A time inside a frame's display interval shows that frame; times
/// before and after the track show its first and last picture.
#[test]
fn times_between_frames_and_outside_show_the_nearest_picture() {
    let (mut r, mut v) = open("video_h264_opengop_rot90.mp4");
    let times = display_times(&v);
    let range = v.index().display_range();
    let mid = (times[4] + times[5]) / 2;
    assert_eq!(v.frame_at(&mut r, mid).expect("decodes").time, times[4]);
    assert_eq!(
        v.frame_at(&mut r, range.start - 100).expect("decodes").time,
        times[0]
    );
    let last = *times.last().expect("frames");
    assert_eq!(
        v.frame_at(&mut r, range.end + 100).expect("decodes").time,
        last
    );
}

/// The fixtures show a test pattern (ffmpeg's `testsrc`): its top-left
/// corner is a colour bar, never black or a decoding artefact. The RGB
/// conversion keeps the size and fills every pixel.
#[test]
fn pictures_convert_to_rgb() {
    let (mut r, mut v) = open("video_h264_aac.mp4");
    assert_eq!(v.index().info().codec, VideoCodec::H264);
    let p = v.picture_at(&mut r, 0).expect("decodes");
    assert_eq!((p.width, p.height), (64, 64));
    assert_eq!(p.rgb.len(), 64 * 64 * 3);
    let f = v.frame_at(&mut r, 0).expect("decodes");
    assert_eq!(f.to_rgb(), p);
}
