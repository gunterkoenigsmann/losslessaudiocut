//! Exporting in a background thread.
//!
//! Writing a cut of a multi-hour recording reads and copies the whole kept
//! audio, which can take seconds, so the GUI hands it to an [`ExportJob`]
//! and polls for the result from its timer. The job opens its own
//! [`Source`] (the GUI's peak and detail threads hold theirs), so nothing
//! is shared but the result.
//!
//! An export cannot be cancelled: stopping half way would leave a broken
//! file. Dropping the job only stops listening; the file is still written.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::{CutRequest, KeptRange, Source};

/// What an export produced: per kept range what it became, or a message
/// for the user.
pub type ExportResult = Result<Vec<KeptRange>, String>;

/// Where an export of `source` goes by default: next to it, as
/// `<stem>-cut.<ext>` (the extension is kept, an export never changes the
/// format).
pub fn default_output(source: &Path) -> PathBuf {
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut name = format!("{stem}-cut");
    if let Some(ext) = source.extension() {
        name += ".";
        name += &ext.to_string_lossy();
    }
    source.with_file_name(name)
}

/// A background thread writing one cut.
#[derive(Debug)]
pub struct ExportJob {
    output: PathBuf,
    result: Arc<Mutex<Option<ExportResult>>>,
    thread: Option<JoinHandle<()>>,
}

impl ExportJob {
    /// Starts writing the part of `source` that `request` keeps to `output`.
    pub fn start(
        source: impl Into<PathBuf>,
        request: CutRequest,
        output: impl Into<PathBuf>,
    ) -> Self {
        let (source, output) = (source.into(), output.into());
        let result = Arc::new(Mutex::new(None));
        let thread = {
            let result = Arc::clone(&result);
            let output = output.clone();
            std::thread::spawn(move || {
                let r = Source::open(&source)
                    .and_then(|mut s| s.export(&request, &output))
                    .map_err(|e| e.to_string());
                *result.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
            })
        };
        ExportJob {
            output,
            result,
            thread: Some(thread),
        }
    }

    /// The file being written.
    pub fn output(&self) -> &Path {
        &self.output
    }

    /// The result once the export has finished, `None` while it runs.
    /// Returns it only once.
    pub fn poll(&self) -> Option<ExportResult> {
        self.result.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Waits for the export to finish. Mainly for tests.
    pub fn wait(mut self) -> ExportResult {
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            return Err("the export thread crashed".to_string());
        }
        self.poll()
            .unwrap_or_else(|| Err("the export result was already taken".to_string()))
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

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lac-export-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn default_output_sits_next_to_the_source() {
        assert_eq!(
            default_output(Path::new("/rec/talk.2024.opus")),
            Path::new("/rec/talk.2024-cut.opus")
        );
        assert_eq!(default_output(Path::new("noext")), Path::new("noext-cut"));
    }

    #[test]
    fn writes_the_same_file_as_a_direct_export() {
        let dir = temp_dir("same");
        let src = fixture("flac_stereo16_44k.flac");
        let request = CutRequest::Delete(vec![1000..20_000, 30_000..40_000]);
        let job = ExportJob::start(&src, request.clone(), dir.join("job.flac"));
        assert_eq!(job.output(), dir.join("job.flac"));
        let kept = job.wait().expect("export");
        let direct = Source::open(&src)
            .expect("open")
            .export(&request, dir.join("direct.flac"))
            .expect("direct export");
        assert_eq!(kept, direct);
        assert_eq!(
            std::fs::read(dir.join("job.flac")).expect("read"),
            std::fs::read(dir.join("direct.flac")).expect("read")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn polling_returns_the_result_once() {
        let dir = temp_dir("poll");
        let job = ExportJob::start(
            fixture("mp3_cbr128_44k.mp3"),
            CutRequest::Delete(vec![]),
            dir.join("out.mp3"),
        );
        let result = loop {
            if let Some(r) = job.poll() {
                break r;
            }
            std::thread::yield_now();
        };
        assert!(result.is_ok(), "{result:?}");
        assert!(job.poll().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn errors_become_messages() {
        let dir = temp_dir("err");
        let src = fixture("flac_stereo16_44k.flac");
        // Writing over the source is refused.
        let r = ExportJob::start(&src, CutRequest::Delete(vec![]), &src).wait();
        assert!(r.is_err());
        let r = ExportJob::start(
            "/nonexistent/x.flac",
            CutRequest::Delete(vec![]),
            dir.join("o"),
        )
        .wait();
        assert!(r.expect_err("missing input").contains("x.flac"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
