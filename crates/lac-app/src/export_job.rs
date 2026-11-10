//! Exporting in a background thread.
//!
//! Writing a cut of a multi-hour recording reads and copies the whole kept
//! audio, which can take seconds, so the GUI hands it to an [`ExportJob`]
//! and polls for the result from its timer. The job opens its own
//! [`Source`] (the GUI's peak and detail threads hold theirs), so nothing
//! is shared but the result.
//!
//! One job writes one file, or one file per track ([`track_tasks`]); the
//! source is opened once for all of them.
//!
//! Each file gets the source's tags with the task's [`TagEdits`] applied:
//! the track's changes from the editor. Unless the user set them, track
//! files are numbered among the files written (`track_number` 1..n,
//! `track_total` n), so a track that is deleted entirely leaves no gap in
//! a player's numbering.
//!
//! An export cannot be cancelled: stopping half way would leave a broken
//! file. Dropping the job only stops listening; the files are still
//! written.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use lac_core::{SamplePos, TagField};

use crate::editor::{Editor, Track, describe_export};
use crate::{CutRequest, KeptRange, Source, TagEdits};

/// What an export produced: per kept range what it became, or a message
/// for the user.
pub type ExportResult = Result<Vec<KeptRange>, String>;

/// One file to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportTask {
    /// What the file keeps of the source.
    pub request: CutRequest,
    /// Where it goes.
    pub output: PathBuf,
    /// Changes to the source's tags for this file; `None` keeps the
    /// source's tags as they are (copied verbatim where the format
    /// allows).
    pub tags: Option<TagEdits>,
}

/// How writing one file went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOutcome {
    /// The file.
    pub output: PathBuf,
    /// What it contains, or why it failed.
    pub result: ExportResult,
    /// Notes about how the file plays (see [`Source::cut_notes`]); empty
    /// when the export failed.
    pub notes: Vec<String>,
}

/// Where an export of `source` goes by default: next to it, as
/// `<stem>-cut.<ext>` (the extension is kept, an export never changes the
/// format).
pub fn default_output(source: &Path) -> PathBuf {
    let mut name = format!("{}-cut", stem(source));
    if let Some(ext) = source.extension() {
        name += ".";
        name += &ext.to_string_lossy();
    }
    source.with_file_name(name)
}

/// The file name of `source` without its extension.
fn stem(source: &Path) -> String {
    source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Replaces characters that are not allowed in file names on some system
/// (Windows is the strictest) and trims what Windows would trim anyway.
fn file_name_safe(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    s.trim().trim_end_matches('.').to_string()
}

/// The file for track `track` of `count` tracks of `source`, in `dir`:
/// `<stem>-<number>[ <name>].<ext>`, the number zero-padded to at least
/// two digits so that files sort in track order.
pub fn track_output(dir: &Path, source: &Path, track: &Track, count: usize) -> PathBuf {
    let width = count.to_string().len().max(2);
    let mut name = format!("{}-{:0width$}", stem(source), track.number);
    if let Some(n) = track
        .name
        .as_deref()
        .map(file_name_safe)
        .filter(|n| !n.is_empty())
    {
        name += " ";
        name += &n;
    }
    if let Some(ext) = source.extension() {
        name += ".";
        name += &ext.to_string_lossy();
    }
    dir.join(name)
}

/// The task for exporting all of `editor`'s kept audio as one file:
/// the source's tags with the changes made to the one track (none:
/// `tags` is `None` and the source's tags are copied as they are).
pub fn single_task(editor: &Editor, output: impl Into<PathBuf>) -> ExportTask {
    let tags = editor
        .tracks()
        .into_iter()
        .next()
        .map(|t| t.tags)
        .filter(|t| !t.is_empty());
    ExportTask {
        request: editor.cut_request(),
        output: output.into(),
        tags,
    }
}

/// Where an export of the selection of `source` goes by default: next to
/// it, as `<stem>-selection.<ext>`.
pub fn selection_output(source: &Path) -> PathBuf {
    let mut name = format!("{}-selection", stem(source));
    if let Some(ext) = source.extension() {
        name += ".";
        name += &ext.to_string_lossy();
    }
    source.with_file_name(name)
}

/// The task for exporting what is kept of `range` (the selection) as one
/// file (issue #101): deleted parts inside it stay out, as in a full
/// export. The file gets the tags of the track the range starts in, as
/// an export of that track would. `None` if nothing in `range` is kept.
pub fn selection_task(
    editor: &Editor,
    range: Range<SamplePos>,
    output: impl Into<PathBuf>,
) -> Option<ExportTask> {
    let keep: Vec<Range<SamplePos>> = editor
        .kept()
        .into_iter()
        .map(|k| k.start.max(range.start)..k.end.min(range.end))
        .filter(|k| k.start < k.end)
        .collect();
    if keep.is_empty() {
        return None;
    }
    let tags = editor
        .tracks()
        .into_iter()
        .nth(editor.track_at(range.start))
        .map(|t| t.tags)
        .filter(|t| !t.is_empty());
    Some(ExportTask {
        request: CutRequest::Keep(keep),
        output: output.into(),
        tags,
    })
}

/// The track numbers that files will get: (number, total) of each track
/// of `editor`, `None` for a track deleted entirely. Numbers count the
/// files written; a number or total the user set wins.
pub fn track_numbers(editor: &Editor) -> Vec<Option<(String, String)>> {
    let tracks = editor.tracks();
    let files = tracks.iter().filter(|t| !t.kept.is_empty()).count();
    let mut n = 0;
    tracks
        .iter()
        .map(|t| {
            if t.kept.is_empty() {
                return None;
            }
            n += 1;
            let get = |f, auto: usize| t.tags.get(f).map_or_else(|| auto.to_string(), String::from);
            Some((
                get(TagField::TrackNumber, n),
                get(TagField::TrackTotal, files),
            ))
        })
        .collect()
}

/// One task per track of `editor` that keeps anything, writing into `dir`
/// (see [`track_output`]), with the track's number among the written
/// files and its name as title (see the module documentation). Also
/// returns the numbers of the tracks that are deleted entirely and so
/// produce no file.
pub fn track_tasks(editor: &Editor, source: &Path, dir: &Path) -> (Vec<ExportTask>, Vec<usize>) {
    let tracks = editor.tracks();
    let count = tracks.len();
    let mut tasks = Vec::new();
    let mut skipped = Vec::new();
    for (t, numbers) in tracks.into_iter().zip(track_numbers(editor)) {
        let Some((number, total)) = numbers else {
            skipped.push(t.number);
            continue;
        };
        let mut tags = t.tags.clone();
        // Valid numbers (counted, or set by the user and checked then).
        let _ = tags.set(TagField::TrackNumber, &number);
        let _ = tags.set(TagField::TrackTotal, &total);
        tasks.push(ExportTask {
            output: track_output(dir, source, &t, count),
            request: CutRequest::Keep(t.kept),
            tags: Some(tags),
        });
    }
    (tasks, skipped)
}

/// A one-line report of an export for the status bar: for one file what
/// [`describe_export`] says, for several the number of files and their
/// joint report, and every failure.
pub fn describe_outcomes(outcomes: &[ExportOutcome], rate: u32) -> String {
    let file = |o: &ExportOutcome| {
        o.output
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let kept: Vec<KeptRange> = outcomes
        .iter()
        .filter_map(|o| o.result.as_ref().ok())
        .flatten()
        .cloned()
        .collect();
    let failed: Vec<String> = outcomes
        .iter()
        .filter_map(|o| {
            o.result
                .as_ref()
                .err()
                .map(|e| format!("{} failed: {e}", file(o)))
        })
        .collect();
    let ok = outcomes.len() - failed.len();
    let mut out = match (outcomes, ok) {
        ([one], 1) => format!("{}: {}", file(one), describe_export(&kept, rate)),
        (_, 0) => String::new(),
        (_, n) => format!("{n} files: {}", describe_export(&kept, rate)),
    };
    for f in failed {
        if !out.is_empty() {
            out += "; ";
        }
        out += &f;
    }
    // The notes concern the source, so all files share them: say each once.
    let mut notes: Vec<&String> = outcomes.iter().flat_map(|o| &o.notes).collect();
    notes.sort();
    notes.dedup();
    for n in notes {
        out += &format!(". Note: {n}");
    }
    out
}

/// A background thread writing one or more cuts of one source.
#[derive(Debug)]
pub struct ExportJob {
    outputs: Vec<PathBuf>,
    result: Arc<Mutex<Option<Vec<ExportOutcome>>>>,
    thread: Option<JoinHandle<()>>,
}

impl ExportJob {
    /// Starts writing the part of `source` that `request` keeps to `output`.
    pub fn start(
        source: impl Into<PathBuf>,
        request: CutRequest,
        output: impl Into<PathBuf>,
    ) -> Self {
        Self::start_all(
            source,
            vec![ExportTask {
                request,
                output: output.into(),
                tags: None,
            }],
        )
    }

    /// Starts writing every task, one after the other, from one opened
    /// `source`.
    pub fn start_all(source: impl Into<PathBuf>, tasks: Vec<ExportTask>) -> Self {
        let source = source.into();
        let outputs = tasks.iter().map(|t| t.output.clone()).collect();
        let result = Arc::new(Mutex::new(None));
        let thread = {
            let result = Arc::clone(&result);
            std::thread::spawn(move || {
                let outcomes = match Source::open(&source) {
                    Ok(mut s) => {
                        let notes = s.cut_notes();
                        tasks
                            .into_iter()
                            .map(|t| {
                                let tags = t.tags.as_ref().map(|e| e.apply_to(s.tags()));
                                let result = s
                                    .export(&t.request, tags.as_ref(), &t.output)
                                    .map_err(|e| e.to_string());
                                ExportOutcome {
                                    notes: if result.is_ok() {
                                        notes.clone()
                                    } else {
                                        Vec::new()
                                    },
                                    result,
                                    output: t.output,
                                }
                            })
                            .collect()
                    }
                    Err(e) => tasks
                        .into_iter()
                        .map(|t| ExportOutcome {
                            output: t.output,
                            result: Err(e.to_string()),
                            notes: Vec::new(),
                        })
                        .collect(),
                };
                *result.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcomes);
            })
        };
        ExportJob {
            outputs,
            result,
            thread: Some(thread),
        }
    }

    /// The files being written.
    pub fn outputs(&self) -> &[PathBuf] {
        &self.outputs
    }

    /// The outcomes once every file is written, `None` while it runs.
    /// Returns them only once.
    pub fn poll(&self) -> Option<Vec<ExportOutcome>> {
        self.result.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Waits for the export to finish. Mainly for tests and for not
    /// leaving truncated files when the program ends.
    pub fn wait(mut self) -> Vec<ExportOutcome> {
        let crashed = self.thread.take().is_some_and(|t| t.join().is_err());
        match self.poll() {
            Some(o) if !crashed => o,
            _ => self
                .outputs
                .iter()
                .map(|o| ExportOutcome {
                    output: o.clone(),
                    result: Err("the export thread crashed or was already polled".into()),
                    notes: Vec::new(),
                })
                .collect(),
        }
    }
}

/// Total length of what `outcomes` wrote.
pub fn written_len(outcomes: &[ExportOutcome]) -> SamplePos {
    outcomes
        .iter()
        .filter_map(|o| o.result.as_ref().ok())
        .flatten()
        .map(KeptRange::output_len)
        .sum()
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
    fn track_files_are_numbered_named_and_safe() {
        let track = |number, name: Option<&str>| Track {
            number,
            span: 0..1,
            kept: vec![],
            name: name.map(String::from),
            tags: TagEdits::new(),
        };
        let src = Path::new("/rec/show.mp3");
        let out = Path::new("/out");
        assert_eq!(
            track_output(out, src, &track(3, None), 9),
            Path::new("/out/show-03.mp3")
        );
        assert_eq!(
            track_output(out, src, &track(7, Some("A/B: why? ")), 120),
            Path::new("/out/show-007 A_B_ why_.mp3")
        );
        assert_eq!(
            track_output(out, src, &track(1, Some(" ... ")), 2),
            Path::new("/out/show-01.mp3"),
            "a name that is nothing after cleaning is left out"
        );
    }

    #[test]
    fn the_selection_keeps_only_its_kept_parts_and_its_tracks_tags() {
        assert_eq!(
            selection_output(Path::new("/rec/talk.opus")),
            Path::new("/rec/talk-selection.opus")
        );
        let mut e = Editor::new(100_000);
        e.select(20_000, 30_000);
        e.delete_selection();
        e.split_at(50_000);
        assert!(e.set_track_tag(1, TagField::Title, "Second").expect("text"));
        // 10..60 k: the deleted 20..30 k stays out.
        let task = selection_task(&e, 10_000..60_000, "out.flac").expect("kept");
        assert_eq!(
            task.request,
            CutRequest::Keep(vec![10_000..20_000, 30_000..60_000])
        );
        assert_eq!(task.output, Path::new("out.flac"));
        assert_eq!(task.tags, None, "track 1 has no changed tags");
        // Starting in track 2: its title.
        let task = selection_task(&e, 55_000..60_000, "x").expect("kept");
        assert_eq!(
            task.tags
                .and_then(|t| t.get(TagField::Title).map(String::from)),
            Some("Second".into())
        );
        // Nothing kept in it.
        assert_eq!(selection_task(&e, 21_000..29_000, "x"), None);
    }

    /// The exported selection decodes to exactly the selected samples.
    #[test]
    fn an_exported_selection_is_the_selected_audio() {
        let dir = temp_dir("selection");
        let src = fixture("flac_stereo16_44k.flac");
        let total = Source::open(&src).expect("open").total_samples();
        let e = Editor::new(total);
        let task = selection_task(&e, 5000..25_000, dir.join("sel.flac")).expect("kept");
        let outcomes = ExportJob::start_all(&src, vec![task]).wait();
        let Ok(kept) = &outcomes[0].result else {
            panic!("{outcomes:?}");
        };
        // FLAC cuts exactly.
        assert_eq!(
            kept.iter().map(|k| k.actual.clone()).collect::<Vec<_>>(),
            std::slice::from_ref(&(5000..25_000))
        );
        // Golden rule: bit for bit the selected samples.
        let mut out = Source::open(dir.join("sel.flac")).expect("open export");
        assert_eq!(out.total_samples(), 20_000);
        let want = Source::open(&src)
            .expect("open")
            .decode_range(5000..25_000)
            .expect("decode source");
        let got = out.decode_range(0..20_000).expect("decode export");
        assert!(got == want, "the export differs from the selection");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_the_same_file_as_a_direct_export() {
        let dir = temp_dir("same");
        let src = fixture("flac_stereo16_44k.flac");
        let request = CutRequest::Delete(vec![1000..20_000, 30_000..40_000]);
        let job = ExportJob::start(&src, request.clone(), dir.join("job.flac"));
        assert_eq!(job.outputs(), [dir.join("job.flac")]);
        let outcomes = job.wait();
        let direct = Source::open(&src)
            .expect("open")
            .export(&request, None, dir.join("direct.flac"))
            .expect("direct export");
        assert_eq!(outcomes[0].result, Ok(direct));
        assert_eq!(
            std::fs::read(dir.join("job.flac")).expect("read"),
            std::fs::read(dir.join("direct.flac")).expect("read")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cut of a file whose codec generates noise says so once in the
    /// report, also for several files; others say nothing extra.
    #[test]
    #[allow(clippy::single_range_in_vec_init)] // one deleted range
    fn reports_codec_noise_notes() {
        let dir = temp_dir("notes");
        let src = fixture("mp4_aac_pns_44k.m4a");
        let tasks = (0..2)
            .map(|i| ExportTask {
                request: CutRequest::Delete(vec![1000..20_000]),
                output: dir.join(format!("{i}.m4a")),
                tags: None,
            })
            .collect();
        let outcomes = ExportJob::start_all(&src, tasks).wait();
        let note = lac_container::mp4::aac::NOISE_NOTE;
        assert!(outcomes.iter().all(|o| o.notes == [note]), "{outcomes:?}");
        let text = describe_outcomes(&outcomes, 44_100);
        assert_eq!(text.matches(note).count(), 1, "{text}");
        assert!(text.contains(". Note: this AAC stream"), "{text}");

        let plain = ExportJob::start(
            fixture("flac_stereo16_44k.flac"),
            CutRequest::Delete(vec![1000..20_000]),
            dir.join("plain.flac"),
        )
        .wait();
        assert!(plain[0].notes.is_empty());
        assert!(!describe_outcomes(&plain, 44_100).contains("Note"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Splitting into tracks and exporting writes one file per track that
    /// keeps anything; each is what a single `Keep` export of its kept
    /// parts writes.
    #[test]
    fn exports_one_file_per_track() {
        let dir = temp_dir("tracks");
        let src = fixture("flac_stereo16_44k.flac");
        let total = Source::open(&src).expect("open").total_samples();
        let mut e = Editor::new(total);
        e.split_at(20_000);
        e.split_at(30_000);
        e.rename_track(0, "Intro");
        e.select(20_000, 30_000);
        e.delete_selection();
        let (tasks, skipped) = track_tasks(&e, &src, &dir);
        assert_eq!(skipped, [2], "the middle track is deleted entirely");
        let names: Vec<_> = tasks.iter().map(|t| t.output.clone()).collect();
        assert_eq!(
            names,
            [
                dir.join("flac_stereo16_44k-01 Intro.flac"),
                dir.join("flac_stereo16_44k-03.flac")
            ]
        );
        let outcomes = ExportJob::start_all(&src, tasks.clone()).wait();
        assert!(outcomes.iter().all(|o| o.result.is_ok()), "{outcomes:?}");
        assert_eq!(
            written_len(&outcomes),
            total - 10_000,
            "FLAC cuts are exact"
        );
        for t in &tasks {
            let reference = dir.join("reference.flac");
            let mut s = Source::open(&src).expect("open");
            let tags = t.tags.as_ref().map(|e| e.apply_to(s.tags()));
            s.export(&t.request, tags.as_ref(), &reference)
                .expect("export");
            assert_eq!(
                std::fs::read(&t.output).ok(),
                std::fs::read(&reference).ok()
            );
        }
        // Numbered among the written files, the name as title.
        let tags: Vec<_> = tasks
            .iter()
            .map(|t| Source::open(&t.output).expect("open").tags().clone())
            .collect();
        assert_eq!(tags[0].title.as_deref(), Some("Intro"));
        assert_eq!(
            (tags[0].track_number, tags[0].track_total),
            (Some(1), Some(2))
        );
        assert_eq!(tags[1].title, None);
        assert_eq!(
            (tags[1].track_number, tags[1].track_total),
            (Some(2), Some(2))
        );
        let report = describe_outcomes(&outcomes, 44_100);
        assert!(report.starts_with("2 files: exported 2 parts"), "{report}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Track files start from the source's tags: the fields nobody
    /// changed are the source's.
    #[test]
    fn track_files_keep_the_source_tags() {
        let dir = temp_dir("srctags");
        // TITLE=Fixture, ARTIST=losslessaudiocut.
        let src = fixture("flac_wasted_22k.flac");
        let total = Source::open(&src).expect("open").total_samples();
        let mut e = Editor::new(total);
        e.split_at(total / 2);
        e.rename_track(1, "Second half");
        let (tasks, _) = track_tasks(&e, &src, &dir);
        let outcomes = ExportJob::start_all(&src, tasks.clone()).wait();
        assert!(outcomes.iter().all(|o| o.result.is_ok()), "{outcomes:?}");
        let tags = |i: usize| Source::open(&tasks[i].output).expect("open").tags().clone();
        assert_eq!(tags(0).title.as_deref(), Some("Fixture"));
        assert_eq!(tags(1).title.as_deref(), Some("Second half"));
        for i in 0..2 {
            assert_eq!(tags(i).artist.as_deref(), Some("losslessaudiocut"));
        }

        // One file: the one track's name, no track number.
        let mut one = Editor::new(total);
        one.rename_track(0, "Everything");
        let task = single_task(&one, dir.join("one.flac"));
        let o = ExportJob::start_all(&src, vec![task]).wait();
        assert!(o[0].result.is_ok(), "{o:?}");
        let t = Source::open(dir.join("one.flac"))
            .expect("open")
            .tags()
            .clone();
        assert_eq!(t.title.as_deref(), Some("Everything"));
        assert_eq!(t.track_number, None);
        assert_eq!(single_task(&Editor::new(total), "x").tags, None);
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
        let outcomes = loop {
            if let Some(r) = job.poll() {
                break r;
            }
            std::thread::yield_now();
        };
        assert!(outcomes[0].result.is_ok(), "{outcomes:?}");
        assert!(job.poll().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn errors_become_messages() {
        let dir = temp_dir("err");
        let src = fixture("flac_stereo16_44k.flac");
        // Writing over the source is refused.
        let o = ExportJob::start(&src, CutRequest::Delete(vec![]), &src).wait();
        assert!(o[0].result.is_err());
        let report = describe_outcomes(&o, 44_100);
        assert!(
            report.starts_with("flac_stereo16_44k.flac failed: "),
            "{report}"
        );
        let o = ExportJob::start(
            "/nonexistent/x.flac",
            CutRequest::Delete(vec![]),
            dir.join("o"),
        )
        .wait();
        assert!(
            o[0].result
                .as_ref()
                .expect_err("missing input")
                .contains("x.flac")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
