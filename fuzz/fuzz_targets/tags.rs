//! Fuzzes the tag parsers (ID3v2, Vorbis comments, RIFF INFO, Matroska) and writes
//! what they read back: malformed tags must never panic.
#![no_main]

use lac_container::mkv::tags as mkv_tags;
use lac_container::tags::{id3, riff, vorbis::VorbisComment};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let parsed = id3::parse(data);
    let _ = id3::write(&parsed.tags, &parsed.other);
    let mut t = parsed.tags.clone();
    id3::merge_v1(&mut t, data);
    if let Ok((vc, _)) = VorbisComment::parse(data) {
        let _ = vc.with_tags(&vc.tags()).to_bytes();
    }
    let info = riff::parse_info(data);
    let _ = riff::write_info(&info);
    let _ = mkv_tags::element(&mkv_tags::parse(data));
});
