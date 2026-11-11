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
//! 3. Cuts copy frames verbatim: re-opening an export yields exactly the
//!    planned source frames, byte for byte.
//! 4. The golden rule for video, checked with ffmpeg (an independent
//!    demuxer and decoder): the pictures of an export, decoded, are the
//!    source's pictures of the planned ranges, bit for bit (frame MD5s).
//!    H.264, HEVC and the other codecs are decoded bit-exactly by any
//!    conforming decoder, so there is no window at joins.
//! 5. Audio and video of an export are equally long, to within half an
//!    audio packet.

mod common;

use std::io::Cursor;

use std::ops::Range;
use std::path::Path;

use common::{fixture, fixture_path, run_tool, temp_file};
use lac_container::mp4::Mp4File;
use lac_core::{MediaTime, SamplePos, VideoCodec, VideoIndex};

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
    // The same video with Opus sound (20 ms packets instead of AAC's
    // 1024 samples).
    Expected {
        name: "video_h264_opus.mp4",
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

/// ffmpeg's fragmented files without an edit list stretch the first AAC
/// sample's duration over the B-frame delay (3200 instead of 1024 ticks
/// here). Each AAC frame still decodes to 1024 samples, and the audio
/// plays that much later than the movie's start (the video's alignment).
#[test]
fn a_stretched_first_aac_sample_is_one_frame() {
    let f = open("video_h264_fragmented.mp4");
    // 95 AAC frames, as ffprobe lists them.
    assert_eq!(f.index().packets().len(), 95);
    assert!(f.index().packets().iter().all(|p| p.n_samples == 1024));
    assert_eq!(f.index().total_samples(), 95 * 1024);
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

// ------------------------------------------------------------ cutting

/// One 30 fps frame at 48 kHz, in audio samples.
const FRAME: SamplePos = 1600;

/// Cuts `name` to `keep` (audio sample ranges) and returns the output.
fn cut(name: &str, keep: &[Range<SamplePos>]) -> (Mp4File, Vec<lac_core::CutPlan>, Vec<u8>) {
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
/// `None` without ffmpeg (or without the decoder `decoder`).
fn frame_md5s(file: &Path, decoder: Option<&str>) -> Option<Vec<String>> {
    use std::ffi::OsStr;
    let mut args: Vec<&OsStr> = vec![OsStr::new("-v"), OsStr::new("error")];
    if let Some(d) = decoder {
        args.extend([OsStr::new("-c:v"), OsStr::new(d)]);
    }
    args.push(OsStr::new("-i"));
    args.push(file.as_os_str());
    for a in ["-map", "0:v:0", "-f", "framemd5", "-"] {
        args.push(OsStr::new(a));
    }
    let o = run_tool("ffmpeg", &args)?;
    if !o.status.success() {
        let err = String::from_utf8_lossy(&o.stderr);
        if decoder.is_some() && err.contains("Unknown decoder") {
            eprintln!("ffmpeg lacks the decoder {decoder:?}, skipping");
            return None;
        }
        panic!("ffmpeg failed on {}: {err}", file.display());
    }
    let text = String::from_utf8(o.stdout).expect("utf-8");
    Some(
        text.lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| l.rsplit(',').next().map(|m| m.trim().to_owned()))
            .collect(),
    )
}

/// The decoder to compare `codec` with. ffmpeg 6.1 with libdav1d shows
/// the frames an edit list hides in front of a cut and drops as many
/// after them (it counts discarded frames without the decoder's delay);
/// libaom, like every other decoder here, honours the edit list.
fn decoder_for(codec: VideoCodec) -> Option<&'static str> {
    (codec == VideoCodec::Av1).then_some("libaom-av1")
}

/// Checks oracles 3 to 5 for one cut.
fn check_cut(name: &str, keep: &[Range<SamplePos>]) {
    let (f, plans, out) = cut(name, keep);
    let v = f.video().expect("video");
    let index = v.index();
    let o = Mp4File::open(&mut Cursor::new(&out)).unwrap_or_else(|e| panic!("{name}: {e}"));
    let ov = o
        .video()
        .unwrap_or_else(|| panic!("{name}: export without video"));

    // 3. Verbatim frames, in the planned order.
    let planned: Vec<usize> = plans
        .iter()
        .filter_map(|p| p.video.as_ref())
        .flat_map(|p| p.frames.clone())
        .collect();
    assert_eq!(ov.index().frames().len(), planned.len(), "{name}");
    let data = fixture(name);
    for (k, (src, dst)) in planned.iter().zip(ov.index().frames()).enumerate() {
        let s = &index.frames()[*src];
        let a = &data[s.byte_offset as usize..][..s.byte_len as usize];
        let b = &out[dst.byte_offset as usize..][..dst.byte_len as usize];
        assert!(a == b, "{name}: frame {k} differs from source frame {src}");
        assert_eq!(s.sync, dst.sync, "{name}: keyframe flag of frame {k}");
    }

    // 5. Audio and video end together (in seconds, within half a packet
    // of 1024 samples at 48 kHz); a delayed video counts from 0.
    let audio_s = o.index().total_samples() as f64 / 48_000.0;
    let shown = ov.shown();
    let video_s =
        (ov.delay() + shown.end - shown.start) as f64 / f64::from(ov.index().info().timescale);
    assert!(
        (audio_s - video_s).abs() <= 512.0 / 48_000.0 + 1e-3,
        "{name}: audio {audio_s} s, video {video_s} s"
    );

    // 4. The decoded pictures (ffmpeg).
    let decoder = decoder_for(index.info().codec);
    let src_path = fixture_path(name);
    let Some(src) = frame_md5s(&src_path, decoder) else {
        return;
    };
    let ext = name.rsplit('.').next().unwrap_or("mp4");
    let out_path = temp_file(&format!("video-cut.{ext}"), &out);
    let Some(got) = frame_md5s(&out_path, decoder) else {
        return;
    };
    let _ = std::fs::remove_file(&out_path);
    // ffmpeg lists the source's frames from the first one shown.
    let first_shown = shown_frames(index, &v.shown());
    assert_eq!(src.len(), first_shown.len(), "{name}: source frames");
    let rank = |k: usize| first_shown.iter().position(|&x| x == k).expect("shown");
    let want: Vec<&String> = plans
        .iter()
        .filter_map(|p| p.video.as_ref())
        .flat_map(|p| shown_frames(index, &p.actual))
        .map(|k| &src[rank(k)])
        .collect();
    let got: Vec<&String> = got.iter().collect();
    assert_eq!(got, want, "{name}: decoded pictures of {keep:?}");
}

/// Trimming both ends: frame-exact (the edit list hides the frames
/// before the start back to the keyframe).
#[test]
fn trimmed_videos_show_exactly_the_kept_frames() {
    for x in FIXTURES {
        check_cut(x.name, std::slice::from_ref(&(9 * FRAME..51 * FRAME)));
    }
}

/// Deleting a part: the part after the join starts at a keyframe (all
/// frames are keyframes in ProRes), or the parts merge where the keyframe
/// lies before the deleted part (the H.264 open GOP has only one).
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

#[test]
fn join_points_are_where_the_keyframes_are() {
    let (f, plans, _) = cut(
        "video_h264_aac.mp4",
        &[0..20 * FRAME, 36 * FRAME..60 * FRAME],
    );
    let index = f.video().expect("video").index();
    let starts: Vec<MediaTime> = plans
        .iter()
        .map(|p| p.video.as_ref().expect("video").actual.start)
        .collect();
    // 512 ticks per frame; the second part starts at the keyframe 30.
    assert_eq!(starts, [0, 30 * 512]);
    assert!(index.frames()[30].sync);
    // The audio follows: its second part starts at about 1 s.
    assert!(plans[1].actual.start.abs_diff(30 * FRAME) <= 512);

    let (_, plans, _) = cut(
        "video_h264_opengop_rot90.mp4",
        &[0..20 * FRAME, 36 * FRAME..60 * FRAME],
    );
    assert_eq!(plans.len(), 1, "one keyframe only: the parts merge");
}

/// The rotation and the edit list survive a cut.
#[test]
fn exports_keep_rotation_and_codec() {
    let (_, _, out) = cut(
        "video_h264_opengop_rot90.mp4",
        std::slice::from_ref(&(9 * FRAME..51 * FRAME)),
    );
    let o = Mp4File::open(&mut Cursor::new(&out)).expect("open");
    let info = o.video().expect("video").index().info();
    assert_eq!(info.rotation, 270);
    assert_eq!(info.codec, VideoCodec::H264);
    assert!(o.warnings().is_empty(), "{:?}", o.warnings());
}

/// The decoder configuration comes from the sample entry: `avcC`, `hvcC`
/// and `av1C` bodies (which start with configuration version 1), the
/// VOL headers of MPEG-4 Visual (a start code); VP9 and ProRes have none.
#[test]
fn codec_configs_are_read() {
    for (name, start) in [
        ("video_h264_aac.mp4", Some(&[1u8][..])),
        ("video_hevc_opengop.mov", Some(&[1][..])),
        // av1C: marker bit and version 1.
        ("video_av1.mp4", Some(&[0x81][..])),
        ("video_mpeg4.3gp", Some(&[0, 0, 1][..])),
        ("video_vp9.mp4", None),
        ("video_prores.mov", None),
    ] {
        let f = open(name);
        let config = f.video().expect("video").codec_config();
        match start {
            Some(s) => assert!(config.starts_with(s), "{name}: {config:02x?}"),
            None => assert!(config.is_empty(), "{name}: {config:02x?}"),
        }
    }
}
