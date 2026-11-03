//! Fuzzes the WAV demuxer and muxer directly, without format detection.
#![no_main]

use lac_container::wav::WavFile;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| lac_fuzz::run(data, |r| WavFile::open(r)));
