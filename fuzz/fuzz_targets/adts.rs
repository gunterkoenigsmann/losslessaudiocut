//! Fuzzes the ADTS (raw AAC) demuxer and muxer directly, without format
//! detection.
#![no_main]

use lac_container::adts::AdtsFile;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| lac_fuzz::run(data, |r| AdtsFile::open(r)));
