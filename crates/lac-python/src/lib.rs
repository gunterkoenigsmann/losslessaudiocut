//! # lac-python
//!
//! The native part of the Python package `losslessaudiocut`: the extension
//! module `losslessaudiocut._native`, built by maturin (`pyproject.toml`).
//!
//! It is deliberately thin. Every function takes and returns plain Python
//! values (strings, numbers, lists, dicts); the typed, documented API that
//! users see (`info()`, `cut()`, `detect()`, `decode()` and their result
//! classes) is the pure Python layer in `python/losslessaudiocut/`. So the
//! user-facing API can change its shape without touching Rust, and this
//! crate only maps the use cases of `lac-app` to Python.
//!
//! Positions are sample frames (`int`) at the source's sample rate, as
//! everywhere in losslessaudiocut; ranges arrive in the CLI's syntax
//! (`"1:00..1:30"`, `"@44100.."`) so both front ends parse times the same
//! way ([`lac_app::TimeRange`]).
//!
//! The work runs with the interpreter's lock released (`Python::detach`),
//! so other Python threads keep running during a long cut or analysis.

use std::ffi::OsString;
use std::ops::Range;
use std::path::PathBuf;

use lac_analysis::{DetectParams, RegionKind, SplitReason};
use lac_app::{AppError, CutRequest, Source, TagEdits, TimeRange};
use lac_core::{SamplePos, TagField};
use pyo3::exceptions::{PyException, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

pyo3::create_exception!(
    _native,
    Error,
    PyException,
    "A file could not be read, cut or written (losslessaudiocut.Error)."
);

/// What can go wrong in a call, before it becomes a Python exception.
enum CallError {
    /// A bad argument: unknown tag field, malformed time. `ValueError`.
    Value(String),
    /// Reading, decoding or writing failed. `losslessaudiocut.Error`.
    File(String),
}

impl From<CallError> for PyErr {
    fn from(e: CallError) -> PyErr {
        match e {
            CallError::Value(m) => PyValueError::new_err(m),
            CallError::File(m) => Error::new_err(m),
        }
    }
}

impl From<AppError> for CallError {
    fn from(e: AppError) -> Self {
        match e {
            // A range outside the file is a bad argument, not a bad file.
            AppError::Edit(_) | AppError::Plan(_) | AppError::OutputIsInput => {
                CallError::Value(chain(&e))
            }
            _ => CallError::File(chain(&e)),
        }
    }
}

impl From<lac_decode::DecodeError> for CallError {
    fn from(e: lac_decode::DecodeError) -> Self {
        CallError::File(chain(&e))
    }
}

/// The error and its causes as one line, `outer: inner: innermost`, the
/// way the CLI prints them (`AppError` keeps the details in its sources).
fn chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        s.push_str(": ");
        s.push_str(&c.to_string());
        cur = c.source();
    }
    s
}

/// Parses ranges in the CLI's `START..END` syntax.
fn parse_ranges(ranges: &[String]) -> Result<Vec<TimeRange>, CallError> {
    ranges
        .iter()
        .map(|r| r.parse().map_err(|e| CallError::Value(format!("{e}"))))
        .collect()
}

/// Resolves parsed ranges against the source's rate and length.
fn resolve(src: &Source, ranges: &[TimeRange]) -> Result<Vec<Range<SamplePos>>, CallError> {
    let rate = src.info().sample_rate;
    let total = src.total_samples();
    ranges
        .iter()
        .map(|r| {
            r.resolve(rate, total)
                .map_err(|e| CallError::Value(e.to_string()))
        })
        .collect()
}

/// Tag edits from `(field, value)` pairs; checks fields and values the way
/// `lac cut --tag` does.
fn tag_edits(tags: &[(String, String)]) -> Result<TagEdits, CallError> {
    let mut edits = Vec::with_capacity(tags.len());
    for (k, v) in tags {
        let field = TagField::from_key(k.trim()).ok_or_else(|| {
            let keys: Vec<&str> = TagField::ALL.iter().map(|f| f.key()).collect();
            CallError::Value(format!(
                "unknown tag field {k:?} (known: {})",
                keys.join(", ")
            ))
        })?;
        TagEdits::new()
            .set(field, v)
            .map_err(|e| CallError::Value(format!("tag {k}: {e}")))?;
        edits.push((field, v.clone()));
    }
    Ok(edits.into_iter().collect())
}

/// What [`info`] reports, gathered without the interpreter lock.
struct InfoData {
    format: &'static str,
    details: String,
    sample_rate: u32,
    channels: u16,
    samples: SamplePos,
    video: Option<String>,
    tags: Vec<(&'static str, String)>,
    extra_tags: Vec<(String, String)>,
    warnings: Vec<String>,
    notes: Vec<String>,
}

/// Format, stream parameters, tags and warnings of a file, as a dict.
#[pyfunction]
fn info<'py>(py: Python<'py>, path: PathBuf) -> PyResult<Bound<'py, PyDict>> {
    let data = py.detach(|| -> Result<InfoData, CallError> {
        let mut src = Source::open(&path)?;
        let i = src.info();
        let tags = src.tags();
        Ok(InfoData {
            format: src.format_name(),
            details: src.details(),
            sample_rate: i.sample_rate,
            channels: i.channels,
            samples: src.total_samples(),
            video: src.video().map(lac_cli::describe_video),
            tags: TagField::ALL
                .iter()
                .filter_map(|&f| tags.get(f).map(|v| (f.key(), v)))
                .collect(),
            extra_tags: tags.extra.clone(),
            warnings: src.warnings().to_vec(),
            notes: src.cut_notes(),
        })
    })?;
    let d = PyDict::new(py);
    d.set_item("format", data.format)?;
    d.set_item("details", data.details)?;
    d.set_item("sample_rate", data.sample_rate)?;
    d.set_item("channels", data.channels)?;
    d.set_item("samples", data.samples)?;
    d.set_item("video", data.video)?;
    d.set_item("tags", data.tags.into_py_dict(py)?)?;
    d.set_item("extra_tags", data.extra_tags)?;
    d.set_item("warnings", data.warnings)?;
    d.set_item("notes", data.notes)?;
    Ok(d)
}

/// Small helper: a list of pairs as a dict.
trait IntoPyDict {
    /// The pairs as a Python dict, later keys winning.
    fn into_py_dict(self, py: Python<'_>) -> PyResult<Bound<'_, PyDict>>;
}

impl<K: for<'a> IntoPyObject<'a>, V: for<'a> IntoPyObject<'a>> IntoPyDict for Vec<(K, V)> {
    fn into_py_dict(self, py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
        let d = PyDict::new(py);
        for (k, v) in self {
            d.set_item(k, v)?;
        }
        Ok(d)
    }
}

/// What [`cut`] reports, gathered without the interpreter lock.
struct CutData {
    sample_rate: u32,
    kept: Vec<lac_app::KeptRange>,
    notes: Vec<String>,
}

/// Cuts `input` into `output` without re-encoding.
///
/// Exactly one of `delete` and `keep` is used: `keep` if it is not empty.
/// Both are lists of ranges in the CLI's syntax that refer to the source.
/// `tags` are `(field, value)` pairs; an empty value removes the tag.
#[pyfunction]
#[pyo3(signature = (input, output, delete, keep, tags))]
fn cut<'py>(
    py: Python<'py>,
    input: PathBuf,
    output: PathBuf,
    delete: Vec<String>,
    keep: Vec<String>,
    tags: Vec<(String, String)>,
) -> PyResult<Bound<'py, PyDict>> {
    if !delete.is_empty() && !keep.is_empty() {
        return Err(PyValueError::new_err(
            "give either delete or keep, not both",
        ));
    }
    let data = py.detach(|| -> Result<CutData, CallError> {
        let delete = parse_ranges(&delete)?;
        let keep = parse_ranges(&keep)?;
        let edits = tag_edits(&tags)?;
        let mut src = Source::open(&input)?;
        let request = if keep.is_empty() {
            CutRequest::Delete(resolve(&src, &delete)?)
        } else {
            CutRequest::Keep(resolve(&src, &keep)?)
        };
        let new_tags = (!edits.is_empty()).then(|| edits.apply_to(src.tags()));
        let kept = src.export(&request, new_tags.as_ref(), &output)?;
        Ok(CutData {
            sample_rate: src.info().sample_rate,
            kept,
            notes: src.cut_notes(),
        })
    })?;
    let kept = PyList::empty(py);
    for k in &data.kept {
        let d = PyDict::new(py);
        d.set_item("requested_start", k.requested.start)?;
        d.set_item("requested_end", k.requested.end)?;
        d.set_item("start", k.actual.start)?;
        d.set_item("end", k.actual.end)?;
        d.set_item("padding", k.padding)?;
        d.set_item("exact", k.is_exact())?;
        kept.append(d)?;
    }
    let d = PyDict::new(py);
    d.set_item("sample_rate", data.sample_rate)?;
    d.set_item(
        "samples",
        data.kept.iter().map(|k| k.output_len()).sum::<SamplePos>(),
    )?;
    d.set_item("kept", kept)?;
    d.set_item("notes", data.notes)?;
    Ok(d)
}

/// The defaults of [`detect`]'s parameters, so the Python layer documents
/// and uses the same numbers as `lac detect`.
#[pyfunction]
fn detect_defaults(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
    let p = DetectParams::default();
    let d = PyDict::new(py);
    d.set_item("min_gap", p.min_track_gap_secs)?;
    d.set_item("min_pause", p.min_pause_secs)?;
    Ok(d)
}

/// Suggests pauses to delete and track starts, like `lac detect`.
///
/// `timbre` and `key` switch the two kinds of track starts without a pause
/// on or off, as `--no-timbre` and `--key` do.
#[pyfunction]
#[pyo3(signature = (path, min_gap, min_pause, timbre, key))]
fn detect<'py>(
    py: Python<'py>,
    path: PathBuf,
    min_gap: f32,
    min_pause: f32,
    timbre: bool,
    key: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let defaults = DetectParams::default();
    let params = DetectParams {
        min_track_gap_secs: min_gap,
        min_pause_secs: min_pause,
        min_novelty_db: if timbre {
            defaults.min_novelty_db
        } else {
            f32::INFINITY
        },
        min_key_distance: if key {
            lac_analysis::KEY_DISTANCE
        } else {
            defaults.min_key_distance
        },
        ..defaults
    };
    let (rate, warnings, det) = py.detach(|| -> Result<_, CallError> {
        let mut src = Source::open(&path)?;
        let rate = src.info().sample_rate;
        let (features, report) = src.analyze(|_| lac_app::Flow::Continue)?;
        Ok((
            rate,
            report.warnings,
            lac_analysis::detect(&features, &params),
        ))
    })?;
    let pauses = PyList::empty(py);
    for r in &det.regions {
        let d = PyDict::new(py);
        d.set_item("start", r.range.start)?;
        d.set_item("end", r.range.end)?;
        d.set_item(
            "kind",
            match r.kind {
                RegionKind::Silence => "silence",
                RegionKind::Noise => "noise",
            },
        )?;
        d.set_item("level_db", r.level_db)?;
        pauses.append(d)?;
    }
    let tracks = PyList::empty(py);
    for s in &det.splits {
        let d = PyDict::new(py);
        d.set_item("position", s.pos)?;
        d.set_item("confidence", s.confidence)?;
        match &s.reason {
            SplitReason::Gap { gap } => {
                d.set_item("reason", "gap")?;
                d.set_item("gap_start", gap.start)?;
                d.set_item("gap_end", gap.end)?;
            }
            SplitReason::TimbreChange { novelty_db } => {
                d.set_item("reason", "timbre")?;
                d.set_item("novelty_db", novelty_db)?;
            }
            SplitReason::KeyChange { distance } => {
                d.set_item("reason", "key")?;
                d.set_item("key_distance", distance)?;
            }
        }
        tracks.append(d)?;
    }
    let t = det.thresholds;
    let d = PyDict::new(py);
    d.set_item("sample_rate", rate)?;
    d.set_item("quiet_db", t.quiet_db)?;
    d.set_item("floor_db", t.floor_db)?;
    d.set_item("loud_db", t.loud_db)?;
    d.set_item("warnings", warnings)?;
    d.set_item("pauses", pauses)?;
    d.set_item("tracks", tracks)?;
    Ok(d)
}

/// Decodes `range` of a file (the whole file if `None`) to interleaved
/// 32-bit float samples in machine byte order.
///
/// Returns `(sample_rate, channels, data)`; `data` is `bytes`, ready for
/// `array.array("f", data)` or `numpy.frombuffer(data, numpy.float32)`.
#[pyfunction]
#[pyo3(signature = (path, range=None))]
fn decode<'py>(
    py: Python<'py>,
    path: PathBuf,
    range: Option<String>,
) -> PyResult<(u32, u16, Bound<'py, PyBytes>)> {
    let (rate, channels, samples) = py.detach(|| -> Result<_, CallError> {
        let range = parse_ranges(&range.into_iter().collect::<Vec<_>>())?;
        let mut src = Source::open(&path)?;
        let r = match range.first() {
            Some(r) => resolve(&src, std::slice::from_ref(r))?.remove(0),
            None => 0..src.total_samples(),
        };
        let info = src.info();
        let samples = src.decode_range(r)?;
        Ok((info.sample_rate, info.channels, samples))
    })?;
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_ne_bytes()).collect();
    Ok((rate, channels, PyBytes::new(py, &bytes)))
}

/// Runs the command line tool `lac` with `argv` (program name first) and
/// returns its exit status. Output goes straight to the process's stdout
/// and stderr. Behind the `lac` command of the package.
#[pyfunction]
fn run_cli(py: Python<'_>, argv: Vec<OsString>) -> i32 {
    py.detach(|| lac_cli::run(argv))
}

/// The version of the bundled FFmpeg (or the system's, in a source build).
#[pyfunction]
fn ffmpeg_version() -> String {
    lac_decode::ffmpeg_version()
}

/// The extension module `losslessaudiocut._native`.
#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add("Error", m.py().get_type::<Error>())?;
    m.add_function(wrap_pyfunction!(info, m)?)?;
    m.add_function(wrap_pyfunction!(cut, m)?)?;
    m.add_function(wrap_pyfunction!(detect, m)?)?;
    m.add_function(wrap_pyfunction!(detect_defaults, m)?)?;
    m.add_function(wrap_pyfunction!(decode, m)?)?;
    m.add_function(wrap_pyfunction!(run_cli, m)?)?;
    m.add_function(wrap_pyfunction!(ffmpeg_version, m)?)?;
    Ok(())
}
