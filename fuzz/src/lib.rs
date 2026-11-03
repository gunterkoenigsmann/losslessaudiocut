//! Shared driver of the fuzz targets.
//!
//! A target opens the fuzzer's bytes as one container format and hands
//! the result to [`exercise`], which walks every read path the editor
//! uses on an opened file. The only property checked is the one from
//! AGENTS.md: malformed input must produce an error, never a panic, a
//! hang or an out-of-memory abort. Errors are therefore ignored.

use std::io::Cursor;

use lac_container::{Container, Result};

/// Upper bound on the packets fed to `read_decode_packet` per section, so
/// that a tiny input claiming millions of packets cannot turn one fuzz
/// iteration into minutes of work.
const MAX_PACKETS: usize = 4096;

/// Opens `data` with `open` and, if that succeeds, exercises the result.
pub fn run<C: Container>(data: &[u8], open: impl FnOnce(&mut Cursor<&[u8]>) -> Result<C>) {
    let mut r = Cursor::new(data);
    if let Ok(c) = open(&mut r) {
        exercise(&c, data);
    }
}

/// Calls everything a front end calls on an opened file: the metadata
/// getters, the decode setup, every packet read, and a cut of the whole
/// file plus one of its middle half (the latter forces trimming and, for
/// FLAC, boundary re-encoding).
pub fn exercise(c: &dyn Container, data: &[u8]) {
    let mut r = Cursor::new(data);
    let _ = c.format_name();
    let _ = c.details();
    let _ = c.warnings();
    let index = c.index();
    let caps = c.capabilities();

    if let Ok(sections) = c.decode_sections(&mut r) {
        for (s, section) in sections.iter().enumerate() {
            for p in 0..section.packet_ends.len().min(MAX_PACKETS) {
                let _ = c.read_decode_packet(&mut r, s, p);
            }
        }
    }

    let len = index.total_samples();
    for range in [0..len, len / 4..len - len / 4] {
        let Ok(plans) = lac_core::plan_cuts(index, caps, &[range]) else {
            continue;
        };
        let mut out = Cursor::new(Vec::new());
        let _ = c.write_cut(&mut r, &plans, None, &mut out);
        // Rewriting the tags read from the file.
        let mut out = Cursor::new(Vec::new());
        let _ = c.write_cut(&mut r, &plans, Some(c.tags()), &mut out);
    }
}
