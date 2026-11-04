//! Tags in every container: the source's tags are read, new tags written
//! by `write_cut` read back the same with our demuxers and with ffprobe,
//! and writing tags never changes a single audio packet (golden rule:
//! tags live outside the audio).

mod common;

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::Path;

use common::{fixture, run_tool, temp_file};
use lac_container::Container;
use lac_core::{CutPlan, Tags, plan_cuts};

/// Every format, with a fixture and its file extension.
const FIXTURES: [&str; 10] = [
    "aiff_s16_44k.aiff",
    "aifc_sowt_22k.aifc",
    "flac_wasted_22k.flac",
    "mp3_ffmpeg_id3v2.mp3",
    "mp3_vbr_44k.mp3",
    "ogg_vorbis_stereo_44k.ogg",
    "ogg_opus_chained.opus",
    "ogg_speex_16k.spx",
    "mkv_opus_stereo.webm",
    "mkv_opus_live.webm",
];

/// A small 16-bit stereo WAV with a `LIST/INFO` chunk.
fn tagged_wav() -> Vec<u8> {
    let mut fmt = Vec::new();
    fmt.extend(1u16.to_le_bytes()); // PCM
    fmt.extend(2u16.to_le_bytes());
    fmt.extend(8000u32.to_le_bytes());
    fmt.extend(32000u32.to_le_bytes());
    fmt.extend(4u16.to_le_bytes());
    fmt.extend(16u16.to_le_bytes());
    let data = common::noise(4 * 8000, 7);
    let info = b"INFO\
INAM\x06\x00\x00\x00Title\x00\
IART\x05\x00\x00\x00Wavy\x00\x00\
ISFT\x04\x00\x00\x00lac\x00";
    let mut body = b"WAVE".to_vec();
    for (id, payload) in [
        (b"fmt ", fmt.as_slice()),
        (b"LIST", info.as_slice()),
        (b"data", data.as_slice()),
    ] {
        body.extend_from_slice(id);
        body.extend((payload.len() as u32).to_le_bytes());
        body.extend_from_slice(payload);
    }
    let mut out = b"RIFF".to_vec();
    out.extend((body.len() as u32).to_le_bytes());
    out.extend(body);
    out
}

fn open(bytes: &[u8]) -> Box<dyn Container> {
    lac_container::open(&mut Cursor::new(bytes)).expect("opens")
}

/// Plans keeping the middle half of the file.
fn middle(c: &dyn Container) -> Vec<CutPlan> {
    let n = c.index().total_samples();
    let keep = n / 4..n - n / 4;
    plan_cuts(c.index(), c.capabilities(), std::slice::from_ref(&keep)).expect("plans")
}

fn export(c: &dyn Container, src: &[u8], tags: Option<&Tags>) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    c.write_cut(&mut Cursor::new(src), &middle(c), tags, &mut out)
        .expect("writes");
    out.into_inner()
}

/// Every audio packet as the decoder gets it.
fn packets(c: &dyn Container, bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut r = Cursor::new(bytes);
    let sections = c.decode_sections(&mut r).expect("sections");
    let mut out = Vec::new();
    for (s, section) in sections.iter().enumerate() {
        for p in 0..section.packet_ends.len() {
            out.push(c.read_decode_packet(&mut r, s, p).expect("packet"));
        }
    }
    out
}

/// New tags for a track, keeping the source's extras.
fn new_tags(source: &Tags) -> Tags {
    Tags {
        title: Some("Track twö".into()),
        artist: Some("Speaker".into()),
        album: Some("Session".into()),
        album_artist: Some("Ensemble".into()),
        track_number: Some(2),
        track_total: Some(5),
        date: Some("2026-11-03".into()),
        genre: Some("Speech".into()),
        comment: Some("cut losslessly".into()),
        extra: source.extra.clone(),
    }
}

/// The tags ffprobe sees (format and stream tags, keys in lower case).
fn ffprobe_tags(file: &Path) -> Option<BTreeMap<String, String>> {
    let o = run_tool(
        "ffprobe",
        &[
            "-v".as_ref(),
            "error".as_ref(),
            "-show_entries".as_ref(),
            "format_tags:stream_tags".as_ref(),
            "-of".as_ref(),
            "flat".as_ref(),
            file.as_os_str(),
        ],
    )?;
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    let mut tags = BTreeMap::new();
    for line in text.lines() {
        // e.g. format.tags.title="Track"
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.rsplit('.').next().unwrap_or(k).to_lowercase();
        let v = v.trim_matches('"').replace("\\\"", "\"");
        tags.entry(key).or_insert(v);
    }
    Some(tags)
}

fn check(name: &str, src: &[u8]) {
    let c = open(src);
    let source_tags = c.tags().clone();
    let tags = new_tags(&source_tags);

    let plain = export(c.as_ref(), src, None);
    let tagged = export(c.as_ref(), src, Some(&tags));

    // Without new tags the source's are kept, with them they are replaced.
    let p = open(&plain);
    let t = open(&tagged);
    assert_eq!(*p.tags(), source_tags, "{name}: source tags kept");
    assert_eq!(*t.tags(), tags, "{name}: new tags read back");

    // Not one audio packet changed.
    assert_eq!(
        packets(t.as_ref(), &tagged),
        packets(p.as_ref(), &plain),
        "{name}: audio packets"
    );
    assert_eq!(t.index().total_samples(), p.index().total_samples());

    // An independent reader agrees, and decodes the same audio.
    let ext = name.rsplit('.').next().unwrap_or("bin");
    let tagged_file = temp_file(&format!("tagged.{ext}"), &tagged);
    let plain_file = temp_file(&format!("plain.{ext}"), &plain);
    let Some(probed) = ffprobe_tags(&tagged_file) else {
        return;
    };
    let get = |k: &str| probed.get(k).map(String::as_str);
    assert_eq!(get("title"), Some("Track twö"), "{name}: {probed:?}");
    assert_eq!(get("artist"), Some("Speaker"), "{name}: {probed:?}");
    assert_eq!(get("album"), Some("Session"), "{name}: {probed:?}");
    let track = get("track").unwrap_or_default();
    assert!(track == "2" || track == "2/5", "{name}: {probed:?}");
    if !name.ends_with(".wav") {
        // INFO has no album artist; ffprobe reads only INFO from WAV.
        assert_eq!(get("album_artist"), Some("Ensemble"), "{name}: {probed:?}");
    }
    if let (Some(a), Some(b)) = (
        common::ffmpeg_decode(&tagged_file, "s32le"),
        common::ffmpeg_decode(&plain_file, "s32le"),
    ) {
        assert_eq!(a, b, "{name}: ffmpeg decodes the same audio");
    }
}

#[test]
fn every_container_writes_and_reads_tags() {
    for name in FIXTURES {
        check(name, &fixture(name));
    }
    check("tagged.wav", &tagged_wav());
}

#[test]
fn source_tags_are_read() {
    let flac = open(&fixture("flac_wasted_22k.flac"));
    assert_eq!(flac.tags().title.as_deref(), Some("Fixture"));
    assert_eq!(flac.tags().artist.as_deref(), Some("losslessaudiocut"));
    let mp3 = open(&fixture("mp3_ffmpeg_id3v2.mp3"));
    assert_eq!(mp3.tags().title.as_deref(), Some("Fixture"));
    let wav = open(&tagged_wav());
    assert_eq!(wav.tags().title.as_deref(), Some("Title"));
    assert_eq!(wav.tags().artist.as_deref(), Some("Wavy"));
    assert_eq!(wav.tags().extra, vec![("ISFT".into(), "lac".into())]);
    // ffmpeg's per-track DURATION is not a file tag.
    let webm = open(&fixture("mkv_opus_stereo.webm"));
    assert!(
        webm.tags().extra.iter().all(|(k, _)| k != "DURATION"),
        "{:?}",
        webm.tags()
    );
}

#[test]
fn mp3_without_new_tags_is_copied_verbatim_up_to_the_audio() {
    let src = fixture("mp3_ffmpeg_id3v2.mp3");
    let c = open(&src);
    let out = export(c.as_ref(), &src, None);
    let len = lac_container::tags::id3::tag_len(&src).expect("has ID3v2");
    assert_eq!(out[..len], src[..len]);
}

#[test]
fn clearing_every_field_writes_no_tags() {
    for name in ["flac_wasted_22k.flac", "mp3_ffmpeg_id3v2.mp3"] {
        let src = fixture(name);
        let c = open(&src);
        let out = export(c.as_ref(), &src, Some(&Tags::default()));
        assert!(open(&out).tags().is_empty(), "{name}");
    }
}
