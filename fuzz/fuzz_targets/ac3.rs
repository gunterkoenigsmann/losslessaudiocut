//! Fuzzes the AC-3 demuxer and muxer directly, without format detection.
#![no_main]

use lac_container::ac3::Ac3File;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| lac_fuzz::run(data, |r| Ac3File::open(r)));
