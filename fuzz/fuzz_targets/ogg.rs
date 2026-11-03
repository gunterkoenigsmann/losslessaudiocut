//! Fuzzes the OGG demuxer and muxer directly, without format detection.
#![no_main]

use lac_container::ogg::OggFile;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| lac_fuzz::run(data, |r| OggFile::open(r)));
