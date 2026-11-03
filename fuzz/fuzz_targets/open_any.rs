//! Fuzzes format detection (`lac_container::open`) and whatever format it
//! picks, the path every file takes in the editor.
#![no_main]

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(c) = lac_container::open(&mut Cursor::new(data)) {
        lac_fuzz::exercise(c.as_ref(), data);
    }
});
