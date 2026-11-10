//! Tests of reading video tracks from MP4, QuickTime and 3GPP files.
//!
//! Fixtures come from `tests/fixtures/generate.sh`: 2 s of 64x64 video at
//! 30 fps with a keyframe every 15 frames, in each codec phones record
//! with, plus AAC audio.
//!
//! Oracles:
//! 1. What the encoder was asked for: codec, size, rotation, frame count,
//!    keyframe positions, and which keyframes are clean starts (the open
//!    GOP fixtures have keyframes that are not, see
//!    `lac_core::VideoIndex::is_clean_start`).
//! 2. ffprobe (an independent demuxer) lists the same packets in the same
//!    order: sizes, decode and presentation times (ffmpeg shifts both by
//!    the edit list's media time) and keyframe flags, except for the
//!    keyframes we demote because decoding cannot start there.

mod common;

use std::io::Cursor;

use common::{fixture, fixture_path, run_tool};
use lac_container::mp4::Mp4File;
use lac_core::{MediaTime, VideoCodec};

/// Expected properties of a video fixture.
struct Expected {
    name: &'static str,
    codec: VideoCodec,
    rotation: u16,
    /// Decode indexes of the keyframes after checking them.
    keyframes: &'static [usize],
    /// Keyframes that are no clean starts (open GOP with leading frames).
    unclean: &'static [usize],
    /// Keyframes the file claims that are none.
    demoted: usize,
}

/// Every 15th frame, as the fixtures were encoded.
const GOP15: &[usize] = &[0, 15, 30, 45];

const FIXTURES: &[Expected] = &[
    Expected {
        name: "video_h264_aac.mp4",
        codec: VideoCodec::H264,
        rotation: 0,
        keyframes: GOP15,
        unclean: &[],
        demoted: 0,
    },
    // x264's open GOP: only the first I-frame is an IDR picture; the
    // others (at 13, 30 and 43 in decode order) are marked as sync samples
    // by the muxer but are plain I-slices.
    Expected {
        name: "video_h264_opengop_rot90.mp4",
        codec: VideoCodec::H264,
        rotation: 270, // ffmpeg's "90 degrees counter-clockwise"
        keyframes: &[0],
        unclean: &[],
        demoted: 3,
    },
    Expected {
        name: "video_h264_fragmented.mp4",
        codec: VideoCodec::H264,
        rotation: 0,
        keyframes: GOP15,
        unclean: &[],
        demoted: 0,
    },
    // x265's open GOP: CRA pictures at 15, 30 and 43; only the last one is
    // followed by RASL pictures (shown before it, referring to the GOP
    // before), so it is a keyframe but no clean start.
    Expected {
        name: "video_hevc_opengop.mov",
        codec: VideoCodec::Hevc,
        rotation: 0,
        keyframes: &[0, 15, 30, 43],
        unclean: &[43],
        demoted: 0,
    },
    Expected {
        name: "video_av1.mp4",
        codec: VideoCodec::Av1,
        rotation: 0,
        keyframes: GOP15,
        unclean: &[],
        demoted: 0,
    },
    Expected {
        name: "video_vp9.mp4",
        codec: VideoCodec::Vp9,
        rotation: 0,
        keyframes: GOP15,
        unclean: &[],
        demoted: 0,
    },
    // ProRes 422 Proxy: every frame is a keyframe, the file has no stss.
    Expected {
        name: "video_prores.mov",
        codec: VideoCodec::Other(*b"apco"),
        rotation: 0,
        keyframes: &[
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45,
            46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59,
        ],
        unclean: &[],
        demoted: 0,
    },
    Expected {
        name: "video_mpeg4.3gp",
        codec: VideoCodec::Mpeg4Visual,
        rotation: 0,
        keyframes: GOP15,
        unclean: &[],
        demoted: 0,
    },
];

fn open(name: &str) -> Mp4File {
    Mp4File::open(&mut Cursor::new(fixture(name))).unwrap_or_else(|e| panic!("{name}: {e}"))
}

#[test]
fn video_tracks_are_read() {
    for x in FIXTURES {
        let f = open(x.name);
        let v = f.video().unwrap_or_else(|| panic!("{}: no video", x.name));
        let i = v.index();
        let info = i.info();
        assert_eq!(info.codec, x.codec, "{}", x.name);
        assert_eq!((info.width, info.height), (64, 64), "{}", x.name);
        assert_eq!(info.rotation, x.rotation, "{}", x.name);
        assert_eq!(i.frames().len(), 60, "{}", x.name);
        // 2 s of 30 fps video, whatever the timescale.
        let range = i.display_range();
        assert_eq!(
            (range.end - range.start) * 30,
            MediaTime::from(info.timescale) * 60,
            "{}",
            x.name
        );
        // The edit list (if any) shows everything; without one (the
        // fragmented file) media time is movie time.
        assert_eq!(v.shown().end, range.end, "{}", x.name);
        assert!(
            v.shown().start == range.start || v.shown().start == 0,
            "{}",
            x.name
        );
        assert_eq!(v.delay(), 0, "{}", x.name);
        let keys: Vec<usize> = (0..60).filter(|&k| i.frames()[k].sync).collect();
        assert_eq!(keys, x.keyframes, "{}: keyframes", x.name);
        let unclean: Vec<usize> = keys
            .iter()
            .copied()
            .filter(|&k| !i.is_clean_start(k))
            .collect();
        assert_eq!(
            unclean, x.unclean,
            "{}: keyframes that are no clean start",
            x.name
        );
        assert_eq!(v.demoted_keyframes(), x.demoted, "{}", x.name);
        let warned = f.warnings().iter().any(|w| w.contains("open GOP"));
        assert_eq!(warned, x.demoted > 0, "{}: {:?}", x.name, f.warnings());
        // The audio is unaffected by the video next to it.
        assert_eq!(f.index().info().sample_rate, 48_000, "{}", x.name);
    }
}

/// ffprobe's view of the packets: (pts, dts, size, keyframe).
fn ffprobe_packets(name: &str) -> Option<Vec<(i64, i64, u32, bool)>> {
    let path = fixture_path(name);
    let out = run_tool(
        "ffprobe",
        &[
            "-v".as_ref(),
            "error".as_ref(),
            "-select_streams".as_ref(),
            "v:0".as_ref(),
            "-show_entries".as_ref(),
            "packet=pts,dts,size,flags".as_ref(),
            "-of".as_ref(),
            "csv=p=0".as_ref(),
            path.as_os_str(),
        ],
    )?;
    assert!(out.status.success(), "ffprobe failed on {name}");
    let text = String::from_utf8(out.stdout).expect("utf-8");
    Some(
        text.lines()
            .map(|l| {
                let f: Vec<&str> = l.split(',').collect();
                (
                    f[0].parse().expect("pts"),
                    f[1].parse().expect("dts"),
                    f[2].parse().expect("size"),
                    f[3].starts_with('K'),
                )
            })
            .collect(),
    )
}

#[test]
fn packets_match_ffprobe() {
    for x in FIXTURES {
        let Some(probe) = ffprobe_packets(x.name) else {
            return;
        };
        let f = open(x.name);
        let v = f.video().expect("video");
        let i = v.index();
        assert_eq!(probe.len(), i.frames().len(), "{}", x.name);
        // ffmpeg moves the edit list's media time to 0 (0 without one).
        let shift = v.shown().start;
        let mut demoted = 0;
        for (k, (frame, &(pts, dts, size, key))) in i.frames().iter().zip(&probe).enumerate() {
            let at = format!("{} frame {k}", x.name);
            assert_eq!(frame.byte_len, size, "{at}: size");
            assert_eq!(i.decode_time(k) - shift, dts, "{at}: dts");
            assert_eq!(i.presentation_time(k) - shift, pts, "{at}: pts");
            match (frame.sync, key) {
                (true, true) | (false, false) => {}
                (false, true) => demoted += 1,
                (true, false) => panic!("{at}: a keyframe ffmpeg does not know"),
            }
        }
        assert_eq!(demoted, x.demoted, "{}", x.name);
    }
}

/// A video track that cannot be read is skipped with a warning; the audio
/// still opens.
#[test]
fn unreadable_video_keeps_the_audio() {
    let mut data = fixture("video_h264_aac.mp4");
    let at = data
        .windows(4)
        .position(|w| w == b"avcC")
        .expect("avcC box");
    data[at..at + 4].copy_from_slice(b"xxxx");
    let f = Mp4File::open(&mut Cursor::new(data)).expect("audio opens");
    assert!(f.video().is_none());
    assert!(
        f.warnings()
            .iter()
            .any(|w| w.contains("ignoring the video track")),
        "{:?}",
        f.warnings()
    );
    assert_eq!(f.index().info().sample_rate, 48_000);
}

/// The phone fixture of the audio tests has a Motion JPEG track, which
/// MP4 files store in an `mp4v` entry; it is not MPEG-4 Visual.
#[test]
fn mp4v_entries_name_their_object_type() {
    let f = open("mp4_aac_with_video.mp4");
    let v = f.video().expect("video");
    assert_eq!(v.index().info().codec, VideoCodec::Other(*b"jpeg"));
}
