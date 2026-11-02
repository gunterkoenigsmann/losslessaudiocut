//! Helpers shared by the integration tests.

#![allow(dead_code)] // not every test binary uses every helper

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Path of a committed fixture (see `tests/fixtures/generate.sh`).
pub fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Contents of a committed fixture.
pub fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixture_path(name)).expect("fixture exists")
}

/// Deterministic pseudo-random bytes (xorshift), so failures reproduce.
pub fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// Writes `data` to a fresh temporary file and returns its path.
///
/// Every call gets its own file (`name` is prefixed with a counter), so
/// tests running in parallel never overwrite each other's files.
pub fn temp_file(name: &str, data: &[u8]) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("lac-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(format!("{n}-{name}"));
    std::fs::write(&path, data).expect("write temp file");
    path
}

/// Runs an external reference tool (ffmpeg, flac).
///
/// Returns `None` if the tool is not installed, unless the environment
/// variable `LAC_REQUIRE_TOOLS` is set (as in CI): then a missing tool fails
/// the test, so cross-checks can never be skipped silently there.
pub fn run_tool(program: &str, args: &[&std::ffi::OsStr]) -> Option<Output> {
    match Command::new(program).args(args).output() {
        Ok(o) => Some(o),
        Err(_) if std::env::var_os("LAC_REQUIRE_TOOLS").is_some() => {
            panic!("{program} is required (LAC_REQUIRE_TOOLS is set) but not installed")
        }
        Err(_) => {
            eprintln!("{program} not installed, skipping cross-check");
            None
        }
    }
}

/// Decodes `file` with ffmpeg to interleaved raw samples of `raw_format`
/// (e.g. `"s32le"`), or `None` if ffmpeg is unavailable.
pub fn ffmpeg_decode(file: &Path, raw_format: &str) -> Option<Vec<u8>> {
    let o = run_tool(
        "ffmpeg",
        &[
            "-v".as_ref(),
            "error".as_ref(),
            "-i".as_ref(),
            file.as_os_str(),
            "-f".as_ref(),
            raw_format.as_ref(),
            "-".as_ref(),
        ],
    )?;
    assert!(
        o.status.success(),
        "ffmpeg failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    Some(o.stdout)
}

/// Decodes an Ogg Vorbis file with the reference decoder (`oggdec` from
/// vorbis-tools, i.e. libvorbisfile) to interleaved 16-bit little-endian
/// samples, or `None` if it is not installed.
///
/// Used instead of ffmpeg for Vorbis because ffmpeg does not reset its
/// Vorbis decoder at the start of a chained stream and so outputs one
/// block overlap too many at every chain boundary.
pub fn oggdec_decode(file: &Path) -> Option<Vec<u8>> {
    let o = run_tool(
        "oggdec",
        &[
            "-Q".as_ref(),
            "-R".as_ref(),
            "-b".as_ref(),
            "16".as_ref(),
            "-o".as_ref(),
            "-".as_ref(),
            file.as_os_str(),
        ],
    )?;
    assert!(
        o.status.success(),
        "oggdec failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    Some(o.stdout)
}
