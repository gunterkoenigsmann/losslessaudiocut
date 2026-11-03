//! Fuzzes the MP3 demuxer and muxer directly, without format detection.
#![no_main]

use lac_container::mp3::Mp3File;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| lac_fuzz::run(data, |r| Mp3File::open(r)));
