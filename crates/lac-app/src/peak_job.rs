//! Building the waveform summary of a file in a background thread.
//!
//! Decoding a long recording takes seconds, so the GUI must not wait for
//! it. A [`PeakJob`] opens the file and runs [`Source::build_peaks`] on its
//! own thread. While the summary grows it publishes snapshots, at most one
//! per `min_interval`, so the display fills from left to right; the front
//! end polls [`PeakJob::poll`] (e.g. from a timer) and redraws when the
//! generation changed.
//!
//! The job knows nothing about the toolkit, so it is tested without a
//! display. Dropping it cancels the decoder at the next chunk.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lac_core::{AudioStreamInfo, Tags};
use lac_decode::{DecodeError, DecodeReport, Flow};
use lac_peaks::PeakSummary;

use crate::Source;
use crate::peak_cache::PeakCache;

/// Where a [`PeakJob`] is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    /// Still decoding; [`JobUpdate::peaks`] is partial.
    Running,
    /// The whole file is summarised.
    Done(DecodeReport),
    /// Opening or decoding failed; the message is for the user. Peaks
    /// published before the failure stay valid.
    Failed(String),
}

/// A published state of a [`PeakJob`].
#[derive(Debug, Clone)]
pub struct JobUpdate {
    /// Increases with every publication; equal generations are equal
    /// updates.
    pub generation: u64,
    /// The summary so far, `None` until the file is open.
    pub peaks: Option<Arc<PeakSummary>>,
    /// Running, done or failed.
    pub status: JobStatus,
    /// Whether the summary came from the [`PeakCache`] instead of being
    /// decoded.
    pub cached: bool,
    /// Sample rate and channels, `None` until the file is open.
    pub info: Option<AudioStreamInfo>,
    /// The file's tags, `None` until the file is open. Shared, because
    /// every poll copies the update and tags can hold pictures.
    pub tags: Option<Arc<Tags>>,
}

/// A background thread building the waveform summary of one file.
#[derive(Debug)]
pub struct PeakJob {
    shared: Arc<Mutex<JobUpdate>>,
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl PeakJob {
    /// Starts summarising `path`. Partial summaries are published at most
    /// every `min_interval` (they are copies, so this bounds the cost).
    ///
    /// With a `cache`, a stored summary of the file is used if there is a
    /// valid one; otherwise a completed build is stored there. Cache
    /// problems never fail the job, they only cost decoding time.
    pub fn start(
        path: impl Into<PathBuf>,
        min_interval: Duration,
        cache: Option<PeakCache>,
    ) -> Self {
        let path = path.into();
        let shared = Arc::new(Mutex::new(JobUpdate {
            generation: 0,
            peaks: None,
            status: JobStatus::Running,
            cached: false,
            info: None,
            tags: None,
        }));
        let cancel = Arc::new(AtomicBool::new(false));
        let thread = {
            let shared = Arc::clone(&shared);
            let cancel = Arc::clone(&cancel);
            std::thread::spawn(move || run(&path, min_interval, cache.as_ref(), &shared, &cancel))
        };
        PeakJob {
            shared,
            cancel,
            thread: Some(thread),
        }
    }

    /// The latest update if its generation differs from `seen`, so a
    /// caller that remembers the last generation it drew only redraws on
    /// changes. Pass `None` to always get the current state.
    pub fn poll(&self, seen: Option<u64>) -> Option<JobUpdate> {
        let u = lock(&self.shared);
        (seen != Some(u.generation)).then(|| u.clone())
    }

    /// Waits for the thread to finish and returns the final update.
    /// Mainly for tests and command line use.
    pub fn wait(mut self) -> JobUpdate {
        if let Some(t) = self.thread.take() {
            // A panic in the thread would be a bug; it is reported as
            // `Failed` by `run` where possible, so ignoring the join
            // error here only loses the panic payload.
            let _ = t.join();
        }
        lock(&self.shared).clone()
    }
}

impl Drop for PeakJob {
    fn drop(&mut self) {
        // Do not join: the decoder notices the flag at its next chunk and
        // the thread ends on its own, so closing a window never blocks.
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Locks `m`, recovering the data if a panicking thread poisoned it (the
/// state is replaced as a whole, so it is never half written).
fn lock(m: &Mutex<JobUpdate>) -> MutexGuard<'_, JobUpdate> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Replaces the shared state with a new generation.
fn publish(shared: &Mutex<JobUpdate>, peaks: Option<Arc<PeakSummary>>, status: JobStatus) {
    let mut u = lock(shared);
    u.generation += 1;
    if peaks.is_some() {
        u.peaks = peaks;
    }
    u.status = status;
}

/// Body of the job thread.
fn run(
    path: &std::path::Path,
    min_interval: Duration,
    cache: Option<&PeakCache>,
    shared: &Mutex<JobUpdate>,
    cancel: &AtomicBool,
) {
    let mut source = match Source::open(path) {
        Ok(s) => s,
        Err(e) => return publish(shared, None, JobStatus::Failed(e.to_string())),
    };
    // Become visible with the first publication below.
    lock(shared).info = Some(source.info());
    lock(shared).tags = Some(Arc::new(source.tags().clone()));
    if let Some((peaks, warnings)) = cache.and_then(|c| c.load(&source)) {
        let report = DecodeReport {
            frames: peaks.total_samples(),
            warnings,
        };
        // Set before publishing, so that no poller sees `Done` without it.
        lock(shared).cached = true;
        publish(shared, Some(Arc::new(peaks)), JobStatus::Done(report));
        return;
    }
    let mut last = Instant::now();
    let mut first = true;
    let result = source.build_peaks(|summary| {
        if cancel.load(Ordering::Relaxed) {
            return Flow::Stop;
        }
        // Publish the first chunk at once (the window learns the length
        // and channel count), then throttle.
        if first || last.elapsed() >= min_interval {
            first = false;
            last = Instant::now();
            publish(shared, Some(Arc::new(summary.clone())), JobStatus::Running);
        }
        Flow::Continue
    });
    match result {
        Ok((peaks, report)) => {
            if let Some(c) = cache {
                // A full disk or a read-only cache only means decoding
                // again next time.
                let _ = c.store(&source, &peaks, &report.warnings);
            }
            publish(shared, Some(Arc::new(peaks)), JobStatus::Done(report));
        }
        // Cancelled: nobody is listening any more.
        Err(DecodeError::Stopped) => {}
        Err(e) => publish(shared, None, JobStatus::Failed(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../lac-container/tests/fixtures")
            .join(name)
    }

    #[test]
    fn summarises_a_whole_file() {
        let job = PeakJob::start(fixture("flac_stereo16_44k.flac"), Duration::ZERO, None);
        let u = job.wait();
        let JobStatus::Done(report) = &u.status else {
            panic!("{:?}", u.status)
        };
        let peaks = u.peaks.expect("peaks");
        assert_eq!(peaks.packets_done(), peaks.packets());
        assert_eq!(report.frames, peaks.total_samples());
        assert_eq!(peaks.channels(), 2);
        let info = u.info.expect("info");
        assert_eq!((info.sample_rate, info.channels), (44_100, 2));
        assert!(u.tags.expect("tags").is_empty(), "the fixture has none");
    }

    #[test]
    fn partial_updates_grow_and_poll_skips_seen_generations() {
        let job = PeakJob::start(fixture("mp3_cbr128_44k.mp3"), Duration::ZERO, None);
        let mut seen = None;
        let mut done_before = 0;
        loop {
            let Some(u) = job.poll(seen) else {
                std::thread::yield_now();
                continue;
            };
            assert_ne!(Some(u.generation), seen);
            seen = Some(u.generation);
            if let Some(p) = &u.peaks {
                assert!(p.packets_done() >= done_before, "summary never shrinks");
                done_before = p.packets_done();
            }
            if u.status != JobStatus::Running {
                assert!(matches!(u.status, JobStatus::Done(_)));
                break;
            }
        }
        assert!(job.poll(seen).is_none(), "nothing new after the end");
        assert!(job.poll(None).is_some());
    }

    #[test]
    fn the_second_open_comes_from_the_cache() {
        let dir = std::env::temp_dir().join(format!("lac-job-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("a.mp3");
        std::fs::copy(fixture("mp3_vbr_44k.mp3"), &path).expect("copy");
        let cache = PeakCache::new(dir.join("cache"));
        let first = PeakJob::start(&path, Duration::ZERO, Some(cache.clone())).wait();
        assert!(!first.cached);
        let second = PeakJob::start(&path, Duration::ZERO, Some(cache)).wait();
        assert!(second.cached);
        assert!(matches!(second.status, JobStatus::Done(_)));
        assert_eq!(second.peaks, first.peaks);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_fails_with_a_message() {
        let u = PeakJob::start("/nonexistent/x.flac", Duration::ZERO, None).wait();
        assert!(u.peaks.is_none());
        let JobStatus::Failed(msg) = u.status else {
            panic!("{:?}", u.status)
        };
        assert!(msg.contains("x.flac"), "{msg}");
    }

    #[test]
    fn dropping_does_not_block() {
        let t = Instant::now();
        drop(PeakJob::start(
            fixture("ogg_vorbis_stereo_44k.ogg"),
            Duration::ZERO,
            None,
        ));
        assert!(t.elapsed() < Duration::from_secs(1));
    }
}
