//! The on-disk cache of waveform summaries, so that a recording is decoded
//! only once (`docs/architecture.md`, "Persistent cache").
//!
//! Entries live in the per-user cache directory, never next to the
//! recording. One file per recording, named by a hash of its key:
//!
//! - **Key**: canonical path, file size, modification time and a hash of
//!   the first and last 64 KiB. A changed or replaced file gets a new key
//!   and so never a stale waveform; a moved file is analysed again.
//! - **Content**: [`ENTRY_MAGIC`], [`ENTRY_VERSION`], the key (so that a
//!   hash collision is a miss, not a wrong waveform), the decoder warnings
//!   of the build, and [`PeakSummary::to_bytes`].
//! - **Eviction** ([`PeakCache::evict`]): entries whose recording is gone,
//!   entries not used for [`PeakCache::max_age`], then the least recently
//!   used ones until the total is below [`PeakCache::max_bytes`]. "Used"
//!   is the entry file's modification time, which a hit refreshes.
//!
//! The cache is only an accelerator. Every problem with it (unreadable,
//! damaged, another version, a full disk) means "decode again", never an
//! error the user has to deal with.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lac_peaks::PeakSummary;

use crate::Source;

/// First bytes of a cache entry.
pub const ENTRY_MAGIC: [u8; 8] = *b"LACCACHE";
/// Version of the entry layout around the summary; the summary has its
/// own ([`PeakSummary::FORMAT_VERSION`]).
pub const ENTRY_VERSION: u32 = 1;
/// File name extension of cache entries.
const EXTENSION: &str = "peaks";
/// Bytes hashed at each end of a recording for the key.
const HASHED_BYTES: u64 = 64 * 1024;

/// A cache directory with its limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeakCache {
    dir: PathBuf,
    max_bytes: u64,
    max_age: Duration,
}

/// What [`PeakCache::evict`] removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Eviction {
    /// Entries removed.
    pub removed: usize,
    /// Bytes of the entries kept.
    pub kept_bytes: u64,
}

impl PeakCache {
    /// Default size limit: 1 GiB.
    pub const DEFAULT_MAX_BYTES: u64 = 1 << 30;
    /// Default age limit: 90 days without use.
    pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(90 * 24 * 3600);

    /// A cache in `dir` with the default limits. The directory is created
    /// when the first entry is stored.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        PeakCache {
            dir: dir.into(),
            max_bytes: Self::DEFAULT_MAX_BYTES,
            max_age: Self::DEFAULT_MAX_AGE,
        }
    }

    /// The per-user cache directory of losslessaudiocut: on Linux
    /// `$XDG_CACHE_HOME/losslessaudiocut` (default `~/.cache/…`), on macOS
    /// `~/Library/Caches/…`, on Windows `%LOCALAPPDATA%\losslessaudiocut\cache`.
    /// `None` if the system has no home directory.
    pub fn default_dir() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "losslessaudiocut")
            .map(|d| d.cache_dir().join("peaks"))
    }

    /// Changes the size limit.
    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// Changes the age limit.
    pub fn with_max_age(mut self, max_age: Duration) -> Self {
        self.max_age = max_age;
        self
    }

    /// The cache directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The size limit in bytes.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// The age limit.
    pub fn max_age(&self) -> Duration {
        self.max_age
    }

    /// The stored summary of `source` and the warnings of its build, if
    /// there is a valid entry for the file as it is now. A hit counts as a
    /// use for eviction.
    pub fn load(&self, source: &Source) -> Option<(PeakSummary, Vec<String>)> {
        let key = key_of(source.path()).ok()?;
        let path = self.entry_path(&key);
        let bytes = fs::read(&path).ok()?;
        let (stored_key, warnings, peaks) = parse_entry(&bytes)?;
        if stored_key != key {
            return None;
        }
        let peaks = PeakSummary::from_bytes(peaks).ok()?;
        if !peaks.matches_index(source.index()) {
            return None;
        }
        // Refresh the use time; failing to do so only affects eviction.
        if let Ok(f) = File::options().write(true).open(&path) {
            let _ = f.set_modified(SystemTime::now());
        }
        Some((peaks, warnings))
    }

    /// Stores the complete summary `peaks` of `source`, then evicts. Only
    /// complete summaries are stored.
    pub fn store(
        &self,
        source: &Source,
        peaks: &PeakSummary,
        warnings: &[String],
    ) -> io::Result<()> {
        if !peaks.is_complete() {
            return Ok(());
        }
        let key = key_of(source.path())?;
        fs::create_dir_all(&self.dir)?;
        let mut entry = Vec::new();
        entry.extend(ENTRY_MAGIC);
        entry.extend(ENTRY_VERSION.to_le_bytes());
        put_bytes(&mut entry, key.as_bytes());
        entry.extend((warnings.len() as u32).to_le_bytes());
        for w in warnings {
            put_bytes(&mut entry, w.as_bytes());
        }
        entry.extend(peaks.to_bytes());
        // Write a temporary file and rename it, so that a reader never sees
        // half an entry (rename replaces atomically on all our platforms).
        let path = self.entry_path(&key);
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&entry)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;
        self.evict().map(|_| ())
    }

    /// Applies the eviction rules (see the module documentation).
    pub fn evict(&self) -> io::Result<Eviction> {
        let mut report = Eviction::default();
        let entries = match fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(report),
            Err(e) => return Err(e),
        };
        let now = SystemTime::now();
        let mut kept = Vec::new();
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != EXTENSION) {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            let used = meta.modified().unwrap_or(UNIX_EPOCH);
            let too_old = now.duration_since(used).is_ok_and(|age| age > self.max_age);
            let orphan = recorded_source(&path).is_none_or(|src| !src.exists());
            if too_old || orphan {
                if fs::remove_file(&path).is_ok() {
                    report.removed += 1;
                }
            } else {
                kept.push((used, meta.len(), path));
            }
        }
        // Least recently used first.
        kept.sort();
        let mut total: u64 = kept.iter().map(|k| k.1).sum();
        for (_, len, path) in kept {
            if total <= self.max_bytes {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                report.removed += 1;
                total -= len;
            }
        }
        report.kept_bytes = total;
        Ok(report)
    }

    /// Removes every entry (the "clear cache" command).
    pub fn clear(&self) -> io::Result<usize> {
        let mut n = 0;
        let entries = match fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_some_and(|x| x == EXTENSION) && fs::remove_file(&path).is_ok() {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Where the entry for `key` lives.
    fn entry_path(&self, key: &str) -> PathBuf {
        self.dir
            .join(format!("{:016x}.{EXTENSION}", fnv1a(key.as_bytes())))
    }
}

/// The cache key of the recording at `path`: canonical path, size,
/// modification time (ns since the epoch) and the hash of both ends,
/// separated by NUL bytes. The path comes first so that eviction can find
/// it ([`recorded_source`]). Non-UTF-8 paths are stored lossily; such an
/// entry may be evicted as an orphan, which only costs a rebuild.
fn key_of(path: &Path) -> io::Result<String> {
    let canonical = fs::canonicalize(path)?;
    let mut f = File::open(&canonical)?;
    let meta = f.metadata()?;
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    let mut head = Vec::new();
    (&mut f).take(HASHED_BYTES).read_to_end(&mut head)?;
    let mut tail = Vec::new();
    f.seek(SeekFrom::Start(size.saturating_sub(HASHED_BYTES)))?;
    f.take(HASHED_BYTES).read_to_end(&mut tail)?;
    head.extend(tail);
    Ok(format!(
        "{}\0{size}\0{mtime}\0{:016x}",
        canonical.to_string_lossy(),
        fnv1a(&head)
    ))
}

/// 64-bit FNV-1a: a stable hash (std's hasher may change between Rust
/// versions, which would orphan every entry). Not cryptographic; the full
/// key in each entry guards against collisions.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Appends a length-prefixed byte string.
fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend((b.len() as u32).to_le_bytes());
    out.extend(b);
}

/// Splits a length-prefixed byte string off `b`.
fn take_bytes<'a>(b: &mut &'a [u8]) -> Option<&'a [u8]> {
    let n = u32::from_le_bytes(b.get(..4)?.try_into().ok()?) as usize;
    let s = b.get(4..4 + n)?;
    *b = &b[4 + n..];
    Some(s)
}

/// Key, warnings and summary bytes of an entry, or `None` if it is not one
/// of [`ENTRY_VERSION`].
fn parse_entry(bytes: &[u8]) -> Option<(String, Vec<String>, &[u8])> {
    let mut b = bytes.strip_prefix(&ENTRY_MAGIC)?;
    let version = u32::from_le_bytes(b.get(..4)?.try_into().ok()?);
    if version != ENTRY_VERSION {
        return None;
    }
    b = &b[4..];
    let key = String::from_utf8(take_bytes(&mut b)?.to_vec()).ok()?;
    let n = u32::from_le_bytes(b.get(..4)?.try_into().ok()?);
    b = &b[4..];
    let mut warnings = Vec::new();
    for _ in 0..n {
        warnings.push(String::from_utf8(take_bytes(&mut b)?.to_vec()).ok()?);
    }
    Some((key, warnings, b))
}

/// The recording an entry file belongs to, read from its key without
/// reading the (possibly large) summary.
fn recorded_source(entry: &Path) -> Option<PathBuf> {
    let mut f = File::open(entry).ok()?;
    let mut head = [0u8; 16];
    f.read_exact(&mut head).ok()?;
    if head[..8] != ENTRY_MAGIC || u32::from_le_bytes(head[8..12].try_into().ok()?) != ENTRY_VERSION
    {
        return None;
    }
    let n = u32::from_le_bytes(head[12..16].try_into().ok()?) as u64;
    let mut key = Vec::new();
    f.take(n).read_to_end(&mut key).ok()?;
    let key = String::from_utf8(key).ok()?;
    key.split('\0').next().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../lac-container/tests/fixtures")
            .join(name)
    }

    /// A fresh directory for one test.
    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lac-cache-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("temp dir");
        d
    }

    /// Copies fixture `name` into `dir` and builds its summary.
    fn source_in(dir: &Path, name: &str) -> (PathBuf, Source, PeakSummary) {
        let path = dir.join(name);
        fs::copy(fixture(name), &path).expect("copy");
        let mut s = Source::open(&path).expect("opens");
        let (peaks, _) = s
            .build_peaks(|_| lac_decode::Flow::Continue)
            .expect("decodes");
        (path, s, peaks)
    }

    #[test]
    fn stores_and_loads() {
        let d = scratch("roundtrip");
        let cache = PeakCache::new(d.join("cache"));
        let (_, src, peaks) = source_in(&d, "mp3_vbr_44k.mp3");
        assert!(cache.load(&src).is_none(), "empty cache");
        cache
            .store(&src, &peaks, &["a warning".into()])
            .expect("stores");
        let (got, warnings) = cache.load(&src).expect("hit");
        assert_eq!(got, peaks);
        assert_eq!(warnings, vec!["a warning".to_string()]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_changed_file_is_a_miss() {
        let d = scratch("changed");
        let cache = PeakCache::new(d.join("cache"));
        let (path, src, peaks) = source_in(&d, "flac_stereo16_44k.flac");
        cache.store(&src, &peaks, &[]).expect("stores");
        // Same size, different content at the end.
        let mut bytes = fs::read(&path).expect("read");
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(&path, bytes).expect("write");
        assert!(cache.load(&src).is_none());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn damaged_entries_are_misses_not_errors() {
        let d = scratch("damaged");
        let cache = PeakCache::new(d.join("cache"));
        let (_, src, peaks) = source_in(&d, "ogg_vorbis_stereo_44k.ogg");
        cache.store(&src, &peaks, &[]).expect("stores");
        let entry = fs::read_dir(cache.dir())
            .expect("dir")
            .next()
            .expect("entry")
            .expect("entry")
            .path();
        let bytes = fs::read(&entry).expect("read");
        for cut in [0, 7, 20, bytes.len() / 2, bytes.len() - 1] {
            fs::write(&entry, &bytes[..cut]).expect("write");
            assert!(cache.load(&src).is_none(), "cut at {cut}");
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn eviction_removes_orphans_old_and_least_recently_used() {
        let d = scratch("evict");
        let cache = PeakCache::new(d.join("cache"));
        let mut entries = Vec::new();
        for name in [
            "mp3_cbr128_44k.mp3",
            "mp3_vbr_44k.mp3",
            "flac_stereo16_44k.flac",
        ] {
            let (path, src, peaks) = source_in(&d, name);
            cache.store(&src, &peaks, &[]).expect("stores");
            entries.push((path, src));
        }
        let entry_count = || fs::read_dir(cache.dir()).expect("dir").count();
        assert_eq!(entry_count(), 3);
        // Orphan: the recording is gone.
        fs::remove_file(&entries[0].0).expect("remove");
        assert_eq!(cache.evict().expect("evicts").removed, 1);
        assert_eq!(entry_count(), 2);
        // LRU: make the vbr entry the older one by using the flac one, then
        // allow only one entry's worth of bytes.
        let set_used = |src: &Source, t: SystemTime| {
            let key = key_of(src.path()).expect("key");
            File::options()
                .write(true)
                .open(cache.entry_path(&key))
                .expect("entry")
                .set_modified(t)
                .expect("mtime");
        };
        set_used(&entries[1].1, SystemTime::now() - Duration::from_secs(60));
        assert!(cache.load(&entries[2].1).is_some());
        let flac_len = fs::metadata(cache.entry_path(&key_of(entries[2].1.path()).expect("key")))
            .expect("meta")
            .len();
        let small = cache.clone().with_max_bytes(flac_len);
        assert_eq!(small.evict().expect("evicts").removed, 1);
        assert!(
            cache.load(&entries[1].1).is_none(),
            "least recently used went"
        );
        assert!(cache.load(&entries[2].1).is_some());
        // Age: an entry unused for longer than max_age goes.
        set_used(&entries[2].1, SystemTime::now() - Duration::from_secs(3600));
        let strict = cache.clone().with_max_age(Duration::from_secs(60));
        assert_eq!(strict.evict().expect("evicts").removed, 1);
        assert_eq!(entry_count(), 0);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn clear_removes_only_entries() {
        let d = scratch("clear");
        let cache = PeakCache::new(d.join("cache"));
        let (_, src, peaks) = source_in(&d, "mp3_cbr128_44k.mp3");
        cache.store(&src, &peaks, &[]).expect("stores");
        fs::write(cache.dir().join("notes.txt"), "keep").expect("write");
        assert_eq!(cache.clear().expect("clears"), 1);
        assert!(cache.dir().join("notes.txt").exists());
        assert_eq!(
            PeakCache::new(d.join("missing")).clear().expect("no dir"),
            0
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn incomplete_summaries_are_not_stored() {
        let d = scratch("partial");
        let cache = PeakCache::new(d.join("cache"));
        let path = d.join("x.mp3");
        fs::copy(fixture("mp3_cbr128_44k.mp3"), &path).expect("copy");
        let mut src = Source::open(&path).expect("opens");
        let mut partial = None;
        let _ = src.build_peaks(|s| {
            partial = Some(s.clone());
            lac_decode::Flow::Stop
        });
        cache
            .store(&src, &partial.expect("partial"), &[])
            .expect("no error");
        assert!(!cache.dir().exists() || fs::read_dir(cache.dir()).expect("dir").count() == 0);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn default_dir_is_named_after_the_app() {
        if let Some(d) = PeakCache::default_dir() {
            assert!(
                d.to_string_lossy()
                    .to_lowercase()
                    .contains("losslessaudiocut"),
                "{d:?}"
            );
        }
    }
}
