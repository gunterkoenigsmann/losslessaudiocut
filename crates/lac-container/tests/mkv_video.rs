//! Tests of cutting Matroska and WebM files with video.
//!
//! Fixtures come from `tests/fixtures/generate.sh`: 2 s of 64x64 video at
//! 30 fps with a keyframe every 15 frames and Opus sound, in VP8, VP9 and
//! AV1 (WebM, as browsers record) and H.264 and HEVC with B-frames
//! (Matroska, as OBS writes).
//!
//! Oracles:
//! 1. What the encoder was asked for: codec, size, frame count, keyframe
//!    positions and which keyframes are clean starts.
//! 2. ffprobe (an independent demuxer) lists the same packets in the same
//!    order: sizes, presentation times and keyframe flags.
//! 3. Cuts copy frames verbatim: re-opening an export yields exactly the
//!    planned source frames, byte for byte, with their keyframe flags.
//! 4. The golden rule for video, checked with ffmpeg (an independent
//!    demuxer and decoder): the pictures of an export, decoded, are the
//!    source's pictures of the planned ranges, bit for bit (frame MD5s).
//! 5. Matroska has no edit list, so an export starts with a keyframe, and
//!    the sound starts with it (within a millisecond, the timestamp
//!    unit); sound and picture end together, within half an audio packet.

mod common;

use std::io::Cursor;
use std::ops::Range;
use std::path::Path;

use common::{fixture, fixture_path, run_tool, temp_file};
use lac_container::mkv::MkvFile;
use lac_core::{MediaTime, SamplePos, VideoCodec, VideoIndex};

/// Expected properties of a video fixture.
struct Expected {
    name: &'static str,
    codec: VideoCodec,
    /// Decode indexes of the keyframes.
    keyframes: &'static [usize],
    /// Keyframes that are no clean starts (open GOP with leading frames).
    unclean: &'static [usize],
}

const GOP15: &[usize] = &[0, 15, 30, 45];

const FIXTURES: &[Expected] = &[
    Expected {
        name: "video_vp8_opus.webm",
        codec: VideoCodec::Vp8,
        keyframes: GOP15,
        unclean: &[],
    },
    Expected {
        name: "video_vp9_opus.webm",
        codec: VideoCodec::Vp9,
        keyframes: GOP15,
        unclean: &[],
    },
    Expected {
        name: "video_av1_opus.webm",
        codec: VideoCodec::Av1,
        keyframes: GOP15,
        unclean: &[],
    },
    Expected {
        name: "video_h264_opus.mkv",
        codec: VideoCodec::H264,
        keyframes: GOP15,
        unclean: &[],
    },
    Expected {
        name: "video_hevc_opus.mkv",
        codec: VideoCodec::Hevc,
        // With B-frames the last keyframe is stored two frames early.
        keyframes: &[0, 15, 30, 43],
        unclean: &[43],
    },
];

/// One 30 fps frame in 48 kHz samples.
const FRAME: SamplePos = 1600;

fn open(name: &str) -> MkvFile {
    MkvFile::open(&mut Cursor::new(fixture(name))).unwrap_or_else(|e| panic!("{name}: {e}"))
}

#[test]
fn video_tracks_are_read() {
    for x in FIXTURES {
        let f = open(x.name);
        let v = f.video().unwrap_or_else(|| panic!("{}: no video", x.name));
        let i = &v.index;
        let info = i.info();
        assert_eq!(info.codec, x.codec, "{}", x.name);
        assert_eq!((info.width, info.height), (64, 64), "{}", x.name);
        assert_eq!(info.timescale, 1000, "{}", x.name);
        assert_eq!(i.frames().len(), 60, "{}", x.name);
        // 2 s of 30 fps video in ms (33 or 34 ms per frame).
        assert!(i.display_range().end.abs_diff(2000) <= 1, "{}", x.name);
        let keys: Vec<usize> = (0..60).filter(|&k| i.frames()[k].sync).collect();
        assert_eq!(keys, x.keyframes, "{}: keyframes", x.name);
        let unclean: Vec<usize> = keys
            .iter()
            .copied()
            .filter(|&k| !i.is_clean_start(k))
            .collect();
        assert_eq!(unclean, x.unclean, "{}: no clean starts", x.name);
        // Sound and picture start together in these files.
        assert!(f.video_offset().abs() <= 1, "{}", x.name);
        assert!(f.warnings().is_empty(), "{}: {:?}", x.name, f.warnings());
        assert!(f.other_tracks() == 0, "{}", x.name);
    }
}

/// ffprobe's view of the video packets: (pts, size, keyframe).
fn ffprobe_packets(file: &Path) -> Option<Vec<(i64, u32, bool)>> {
    let out = run_tool(
        "ffprobe",
        &[
            "-v".as_ref(),
            "error".as_ref(),
            "-select_streams".as_ref(),
            "v:0".as_ref(),
            "-show_entries".as_ref(),
            "packet=pts,size,flags".as_ref(),
            "-of".as_ref(),
            "csv=p=0".as_ref(),
            file.as_os_str(),
        ],
    )?;
    assert!(out.status.success(), "ffprobe failed on {}", file.display());
    let text = String::from_utf8(out.stdout).expect("utf-8");
    Some(
        text.lines()
            .map(|l| {
                let f: Vec<&str> = l.split(',').collect();
                (
                    f[0].parse().expect("pts"),
                    f[1].parse().expect("size"),
                    f[2].starts_with('K'),
                )
            })
            .collect(),
    )
}

#[test]
fn packets_match_ffprobe() {
    for x in FIXTURES {
        let Some(probe) = ffprobe_packets(&fixture_path(x.name)) else {
            return;
        };
        let f = open(x.name);
        let v = f.video().expect("video");
        let i = &v.index;
        assert_eq!(probe.len(), i.frames().len(), "{}", x.name);
        for (k, (frame, &(pts, size, key))) in i.frames().iter().zip(&probe).enumerate() {
            let at = format!("{} frame {k}", x.name);
            assert_eq!(frame.byte_len, size, "{at}: size");
            assert_eq!(v.origin + i.presentation_time(k), pts, "{at}: pts");
            assert_eq!(frame.sync, key, "{at}: keyframe");
        }
    }
}

/// Cuts `name` to `keep` (audio sample ranges) and returns the output.
fn cut(name: &str, keep: &[Range<SamplePos>]) -> (MkvFile, Vec<lac_core::CutPlan>, Vec<u8>) {
    let data = fixture(name);
    let f = open(name);
    let plans = f.plan(keep).unwrap_or_else(|e| panic!("{name}: {e}"));
    let mut out = Cursor::new(Vec::new());
    f.write_cut(&mut Cursor::new(&data), &plans, None, &mut out)
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    (f, plans, out.into_inner())
}

/// The source frames (decode indexes, in display order) whose display
/// starts inside `range`.
fn shown_frames(index: &VideoIndex, range: &Range<MediaTime>) -> Vec<usize> {
    index
        .display_order()
        .iter()
        .copied()
        .filter(|&k| range.contains(&index.presentation_time(k)))
        .collect()
}

/// MD5 of each picture ffmpeg decodes from `file`, in display order, or
/// `None` without ffmpeg.
fn frame_md5s(file: &Path) -> Option<Vec<String>> {
    let o = run_tool(
        "ffmpeg",
        &[
            "-v".as_ref(),
            "error".as_ref(),
            "-i".as_ref(),
            file.as_os_str(),
            "-map".as_ref(),
            "0:v:0".as_ref(),
            "-f".as_ref(),
            "framemd5".as_ref(),
            "-".as_ref(),
        ],
    )?;
    assert!(
        o.status.success(),
        "ffmpeg failed on {}: {}",
        file.display(),
        String::from_utf8_lossy(&o.stderr)
    );
    let text = String::from_utf8(o.stdout).expect("utf-8");
    Some(
        text.lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| l.rsplit(',').next().map(|m| m.trim().to_owned()))
            .collect(),
    )
}

/// Checks oracles 3 to 5 for one cut.
fn check_cut(name: &str, keep: &[Range<SamplePos>]) {
    let (f, plans, out) = cut(name, keep);
    let index = &f.video().expect("video").index;
    let o = MkvFile::open(&mut Cursor::new(&out)).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert!(o.warnings().is_empty(), "{name}: {:?}", o.warnings());
    assert_eq!(o.doc_type(), f.doc_type(), "{name}");
    let ov = o
        .video()
        .unwrap_or_else(|| panic!("{name}: export without video"));

    // 3. Verbatim frames, in the planned order.
    let planned: Vec<usize> = plans
        .iter()
        .filter_map(|p| p.video.as_ref())
        .flat_map(|p| p.frames.clone())
        .collect();
    assert_eq!(ov.index.frames().len(), planned.len(), "{name}");
    let data = fixture(name);
    for (k, (src, dst)) in planned.iter().zip(ov.index.frames()).enumerate() {
        let s = &index.frames()[*src];
        let a = &data[s.byte_offset as usize..][..s.byte_len as usize];
        let b = &out[dst.byte_offset as usize..][..dst.byte_len as usize];
        assert!(a == b, "{name}: frame {k} differs from source frame {src}");
        assert_eq!(s.sync, dst.sync, "{name}: keyframe flag of frame {k}");
    }

    // 5. Start at a keyframe with the sound, end together.
    assert!(index.frames()[planned[0]].sync, "{name}: first frame");
    // Where the sound starts relative to the first picture (ms): as in
    // the source, which is 0 unless the keyframe lies before the
    // source's first sound sample.
    let first = plans
        .iter()
        .find_map(|p| p.video.as_ref().filter(|v| !v.frames.is_empty()))
        .expect("video");
    let sound_at = (plans[0].actual.start as f64 / 48.0).round() as MediaTime;
    let want = (sound_at + f.video_offset() - first.actual.start).max(0);
    assert!(
        (o.video_offset() - want).abs() <= 1,
        "{name}: sound at {} ms, want {want}",
        o.video_offset()
    );
    let audio_ms = o.index().total_samples() as f64 / 48.0;
    let video_ms = (ov.index.display_range().end - o.video_offset()) as f64;
    // Half a 20 ms Opus packet, plus the rounding of ms timestamps.
    assert!(
        (audio_ms - video_ms).abs() <= 11.0,
        "{name}: audio {audio_ms} ms, video {video_ms} ms"
    );

    // 4. The decoded pictures (ffmpeg).
    let Some(src) = frame_md5s(&fixture_path(name)) else {
        return;
    };
    let ext = name.rsplit('.').next().unwrap_or("mkv");
    let out_path = temp_file(&format!("mkv-video-cut.{ext}"), &out);
    let got = frame_md5s(&out_path);
    let _ = std::fs::remove_file(&out_path);
    let Some(got) = got else { return };
    let all = shown_frames(index, &index.display_range());
    assert_eq!(src.len(), all.len(), "{name}: source frames");
    let rank = |k: usize| all.iter().position(|&x| x == k).expect("shown");
    let want: Vec<&String> = plans
        .iter()
        .filter_map(|p| p.video.as_ref())
        .flat_map(|p| shown_frames(index, &p.actual))
        .map(|k| &src[rank(k)])
        .collect();
    let got: Vec<&String> = got.iter().collect();
    assert_eq!(got, want, "{name}: decoded pictures of {keep:?}");

    // The export's packets as ffprobe sees them: the planned frames.
    if let Some(probe) = ffprobe_packets(&temp_file(&format!("mkv-video-probe.{ext}"), &out)) {
        assert_eq!(probe.len(), planned.len(), "{name}: ffprobe frames");
    }
}

/// Trimming both ends: the start moves back to the keyframe (frame 15
/// here), the end is exact.
#[test]
fn trimmed_videos_start_at_a_keyframe() {
    for x in FIXTURES {
        let (_, plans, _) = cut(x.name, std::slice::from_ref(&(20 * FRAME..51 * FRAME)));
        let v = plans[0].video.as_ref().expect("video");
        assert_eq!(v.frames.start, 15, "{}", x.name);
        assert!(
            plans[0].actual.start.abs_diff(15 * FRAME) <= 48,
            "{}",
            x.name
        );
        check_cut(x.name, std::slice::from_ref(&(20 * FRAME..51 * FRAME)));
    }
}

/// Deleting parts: each part after a join starts at a keyframe (or the
/// parts merge where it lies before the deleted part).
#[test]
fn joins_start_at_keyframes() {
    for x in FIXTURES {
        check_cut(x.name, &[0..20 * FRAME, 36 * FRAME..60 * FRAME]);
        check_cut(
            x.name,
            &[
                3 * FRAME..10 * FRAME,
                17 * FRAME..40 * FRAME,
                47 * FRAME..58 * FRAME,
            ],
        );
    }
}

/// Sound that starts after the picture: an export from the file's start
/// keeps the picture before the sound (the sound is delayed in the
/// export, as Matroska timestamps cannot be negative).
#[test]
fn late_sound_stays_late() {
    let name = "video_vp8_late_sound.webm";
    let f = open(name);
    assert!(f.video_offset().abs_diff(100) <= 1, "{}", f.video_offset());
    check_cut(name, std::slice::from_ref(&(0..40 * FRAME)));
    check_cut(name, &[0..20 * FRAME, 36 * FRAME..50 * FRAME]);
    let (_, _, out) = cut(name, std::slice::from_ref(&(0..40 * FRAME)));
    let o = MkvFile::open(&mut Cursor::new(&out)).expect("export");
    assert!(o.video_offset().abs_diff(100) <= 1, "{}", o.video_offset());
}

/// The decoder configuration is the track's `CodecPrivate`: `avcC`,
/// `hvcC` and `av1C` bodies; VP8 and VP9 have none.
#[test]
fn codec_configs_are_read() {
    for (name, start) in [
        ("video_h264_opus.mkv", Some(&[1u8][..])),
        ("video_hevc_opus.mkv", Some(&[1][..])),
        // av1C: marker bit and version 1.
        ("video_av1_opus.webm", Some(&[0x81][..])),
        ("video_vp8_opus.webm", None),
        ("video_vp9_opus.webm", None),
    ] {
        let f = open(name);
        let config = &f.video().expect("video").track.codec_private;
        match start {
            Some(s) => assert!(config.starts_with(s), "{name}: {config:02x?}"),
            None => assert!(config.is_empty(), "{name}: {config:02x?}"),
        }
    }
}

/// A video codec we cannot cut (Motion JPEG) is ignored with a warning;
/// the sound is cut as before.
#[test]
fn other_video_codecs_are_ignored() {
    let f = open("mkv_opus_with_video.mkv");
    assert!(f.video().is_none());
    assert_eq!(f.other_tracks(), 1);
}
