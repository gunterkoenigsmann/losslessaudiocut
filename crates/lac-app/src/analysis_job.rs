//! Detecting pauses and track starts in a background thread.
//!
//! Analysis decodes the whole file (as the waveform summary does), so it
//! runs on its own thread like [`crate::peak_job::PeakJob`]. The front end
//! polls [`AnalysisJob::poll`] from a timer; when it is done, the
//! [`Detection`] holds the deletable regions and suggested track starts,
//! which the editor applies with [`crate::editor::Editor::split_at_all`].
//!
//! The [`Features`] are kept in the result, so the suggestions can be
//! recomputed with other [`DetectParams`] (a settings slider) without
//! decoding again: [`lac_analysis::detect`] takes milliseconds.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use lac_analysis::{DetectParams, Detection, Features};
use lac_core::SamplePos;
use lac_decode::{DecodeError, Flow};

use crate::Source;

/// Where an [`AnalysisJob`] is.
#[derive(Debug, Clone)]
pub enum AnalysisStatus {
    /// Decoding; `done` of `total` sample frames are analysed (`total` is
    /// 0 until the file is open).
    Running {
        /// Sample frames analysed so far.
        done: SamplePos,
        /// Length of the recording.
        total: SamplePos,
    },
    /// Finished.
    Done {
        /// The features, for running [`lac_analysis::detect`] again with
        /// other parameters.
        features: Arc<Features>,
        /// The suggestions for the parameters the job was started with.
        detection: Arc<Detection>,
        /// Decoder warnings; positions after one may be off.
        warnings: Vec<String>,
    },
    /// Opening or decoding failed; the message is for the user.
    Failed(String),
}

#[derive(Debug)]
struct Shared {
    generation: u64,
    status: AnalysisStatus,
}

/// A background thread analysing one file.
#[derive(Debug)]
pub struct AnalysisJob {
    shared: Arc<Mutex<Shared>>,
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AnalysisJob {
    /// Starts analysing `path` with `params`.
    pub fn start(path: impl Into<PathBuf>, params: DetectParams) -> Self {
        let path = path.into();
        let shared = Arc::new(Mutex::new(Shared {
            generation: 0,
            status: AnalysisStatus::Running { done: 0, total: 0 },
        }));
        let cancel = Arc::new(AtomicBool::new(false));
        let thread = {
            let shared = Arc::clone(&shared);
            let cancel = Arc::clone(&cancel);
            std::thread::spawn(move || run(&path, &params, &shared, &cancel))
        };
        AnalysisJob {
            shared,
            cancel,
            thread: Some(thread),
        }
    }

    /// The current status and its generation if the generation differs
    /// from `seen` (pass `None` to always get it). Progress is published
    /// about once per percent.
    pub fn poll(&self, seen: Option<u64>) -> Option<(u64, AnalysisStatus)> {
        let s = lock(&self.shared);
        (seen != Some(s.generation)).then(|| (s.generation, s.status.clone()))
    }

    /// Waits for the thread and returns the final status. Mainly for tests
    /// and the command line.
    pub fn wait(mut self) -> AnalysisStatus {
        if let Some(t) = self.thread.take() {
            // `run` reports its errors as `Failed`; a panic would be a bug
            // and leaves the last published status.
            let _ = t.join();
        }
        lock(&self.shared).status.clone()
    }
}

impl Drop for AnalysisJob {
    fn drop(&mut self) {
        // Do not join: the decoder notices the flag at its next chunk.
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Locks `m`, recovering from poisoning (the status is replaced as a
/// whole, so it is never half written).
fn lock(m: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn publish(shared: &Mutex<Shared>, status: AnalysisStatus) {
    let mut s = lock(shared);
    s.generation += 1;
    s.status = status;
}

/// Body of the job thread.
fn run(path: &Path, params: &DetectParams, shared: &Mutex<Shared>, cancel: &AtomicBool) {
    let mut source = match Source::open(path) {
        Ok(s) => s,
        Err(e) => return publish(shared, AnalysisStatus::Failed(e.to_string())),
    };
    let total = source.total_samples();
    publish(shared, AnalysisStatus::Running { done: 0, total });
    let step = (total / 100).max(1);
    let mut next = step;
    let result = source.analyze(|done| {
        if cancel.load(Ordering::Relaxed) {
            return Flow::Stop;
        }
        if done >= next {
            next = done + step;
            publish(shared, AnalysisStatus::Running { done, total });
        }
        Flow::Continue
    });
    match result {
        Ok((features, report)) => {
            let detection = lac_analysis::detect(&features, params);
            publish(
                shared,
                AnalysisStatus::Done {
                    features: Arc::new(features),
                    detection: Arc::new(detection),
                    warnings: report.warnings,
                },
            );
        }
        Err(DecodeError::Stopped) => {}
        Err(e) => publish(shared, AnalysisStatus::Failed(e.to_string())),
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
    fn analyses_a_whole_file() {
        let path = fixture("ogg_vorbis_stereo_44k.ogg");
        let total = Source::open(&path).expect("open").total_samples();
        let AnalysisStatus::Done {
            features,
            detection,
            ..
        } = AnalysisJob::start(&path, DetectParams::default()).wait()
        else {
            panic!("not done")
        };
        assert_eq!(features.total_samples(), total);
        assert_eq!(features.frames().len() as SamplePos, total.div_ceil(1024));
        for s in &detection.splits {
            assert!(s.pos > 0 && s.pos < total);
        }
    }

    #[test]
    fn a_missing_file_fails_with_a_message() {
        let s = AnalysisJob::start("/nonexistent/y.flac", DetectParams::default()).wait();
        let AnalysisStatus::Failed(msg) = s else {
            panic!("{s:?}")
        };
        assert!(msg.contains("y.flac"), "{msg}");
    }
}
