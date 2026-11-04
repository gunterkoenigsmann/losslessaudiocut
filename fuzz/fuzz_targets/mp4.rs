//! Fuzzes the MP4 demuxer and muxer directly, without format detection.
#![no_main]

use lac_container::mp4::Mp4File;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| lac_fuzz::run(data, |r| Mp4File::open(r)));
