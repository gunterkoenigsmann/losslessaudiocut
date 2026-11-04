//! Fuzzes the AIFF demuxer and muxer directly, without format detection.
#![no_main]

use lac_container::aiff::AiffFile;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| lac_fuzz::run(data, |r| AiffFile::open(r)));
