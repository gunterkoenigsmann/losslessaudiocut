//! Opening a source file and exporting a cut of it.
//!
//! This is the "open" and "export" use case of the architecture: the CLI
//! calls it once per invocation, the GUI keeps a [`Source`] open while the
//! user edits and calls [`Source::export`] when they save.

use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

use lac_container::{Container, ContainerError};
use lac_core::{
    AudioStreamInfo, CutPlan, EditError, EditList, PacketIndex, PlanError, SamplePos, SourceId,
    Tags, plan_cuts,
};

use crate::ranges;

/// Errors of the use cases in this module.
///
/// Each variant carries enough context (usually the path) for its message
/// to be shown to the user as it is.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The file could not be opened for reading.
    #[error("cannot open {}", path.display())]
    Open {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        #[source]
        source: std::io::Error,
    },
    /// The file was opened but is not a supported or valid audio file.
    #[error("cannot read {}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// What the container parser reported.
        #[source]
        source: ContainerError,
    },
    /// The output file could not be created or written.
    #[error("cannot write {}", path.display())]
    Write {
        /// The output file.
        path: PathBuf,
        /// What went wrong.
        #[source]
        source: ContainerError,
    },
    /// Exporting onto the source file would destroy it while it is read.
    #[error("output must not be the input file")]
    OutputIsInput,
    /// A requested range does not fit the file.
    #[error(transparent)]
    Edit(#[from] EditError),
    /// The cut planner rejected a range.
    #[error(transparent)]
    Plan(#[from] PlanError),
}

/// What to cut out of a source, in sample frames of the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CutRequest {
    /// Delete these ranges and join the rest. Ranges refer to the original
    /// timeline and may overlap or come in any order.
    Delete(Vec<Range<SamplePos>>),
    /// Keep only these ranges, joined in the given order. Empty ranges are
    /// ignored.
    Keep(Vec<Range<SamplePos>>),
}

/// One stretch of the source that ended up in an exported file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptRange {
    /// The range that was asked for.
    pub requested: Range<SamplePos>,
    /// The range the file actually contains: equal to `requested`, or
    /// widened to packet boundaries where the format cannot cut exactly.
    pub actual: Range<SamplePos>,
    /// Sample frames played around `actual` that are not part of the source
    /// timeline (MP3 encoder delay or padding that a join cannot hide).
    pub padding: SamplePos,
}

impl KeptRange {
    /// True if the file contains exactly the requested samples.
    pub fn is_exact(&self) -> bool {
        self.requested == self.actual && self.padding == 0
    }

    /// Sample frames this range occupies in the output file.
    pub fn output_len(&self) -> SamplePos {
        self.actual.end - self.actual.start + self.padding
    }
}

/// An opened source file: its parsed container plus the reader that the
/// muxer copies packets from.
pub struct Source {
    path: PathBuf,
    container: Box<dyn Container>,
    reader: BufReader<File>,
}

impl Source {
    /// Opens a file of any supported format, detected from its content.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AppError> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path).map_err(|source| AppError::Open {
            path: path.clone(),
            source,
        })?;
        let mut reader = BufReader::new(file);
        let container = lac_container::open(&mut reader).map_err(|source| AppError::Read {
            path: path.clone(),
            source,
        })?;
        Ok(Source {
            path,
            container,
            reader,
        })
    }

    /// The path the source was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Short format name, e.g. `FLAC` or `Ogg (Speex)`.
    pub fn format_name(&self) -> &'static str {
        self.container.format_name()
    }

    /// Format details such as bit depth or bit rate, for display.
    pub fn details(&self) -> String {
        self.container.details()
    }

    /// Problems found while parsing that did not prevent opening the file.
    pub fn warnings(&self) -> &[String] {
        self.container.warnings()
    }

    /// The packet index, the single source of truth for positions.
    pub fn index(&self) -> &PacketIndex {
        self.container.index()
    }

    /// Sample rate, channel count etc. of the audio stream.
    pub fn info(&self) -> AudioStreamInfo {
        self.index().info()
    }

    /// Length of the stream in sample frames.
    pub fn total_samples(&self) -> SamplePos {
        self.index().total_samples()
    }

    /// Decodes the whole source once and builds its waveform summary.
    ///
    /// `progress` is called after every decoded chunk with the summary so
    /// far (the GUI shows it while it grows); returning
    /// [`lac_decode::Flow::Stop`] cancels, e.g. when the file is closed.
    pub fn build_peaks(
        &mut self,
        mut progress: impl FnMut(&lac_peaks::PeakSummary) -> lac_decode::Flow,
    ) -> Result<(lac_peaks::PeakSummary, lac_decode::DecodeReport), lac_decode::DecodeError> {
        let mut builder = lac_peaks::PeakBuilder::new(self.container.index());
        let report = lac_decode::decode_all(self.container.as_ref(), &mut self.reader, |chunk| {
            builder.push(chunk);
            progress(builder.summary())
        })?;
        Ok((builder.finish(), report))
    }

    /// Decodes the whole source once and computes its analysis features
    /// (level, spectral flatness, band spectrum per ~20 ms), from which
    /// [`lac_analysis::detect`] suggests deletable pauses and track starts.
    ///
    /// `progress` is called after every decoded chunk with the number of
    /// sample frames analysed so far; returning [`lac_decode::Flow::Stop`]
    /// cancels.
    pub fn analyze(
        &mut self,
        mut progress: impl FnMut(SamplePos) -> lac_decode::Flow,
    ) -> Result<(lac_analysis::Features, lac_decode::DecodeReport), lac_decode::DecodeError> {
        let info = self.info();
        let channels = usize::from(info.channels);
        let mut extractor = lac_analysis::FeatureExtractor::new(info.sample_rate, channels);
        let mut done: SamplePos = 0;
        let report = lac_decode::decode_all(self.container.as_ref(), &mut self.reader, |chunk| {
            extractor.push(chunk);
            done += (chunk.len() / channels.max(1)) as SamplePos;
            progress(done)
        })?;
        Ok((extractor.finish(), report))
    }

    /// Decodes the sample frames in `range` (clamped to the timeline) as
    /// interleaved `f32`, without decoding from the start of the file; see
    /// [`lac_decode::decode_range`].
    pub fn decode_range(
        &mut self,
        range: Range<SamplePos>,
    ) -> Result<Vec<f32>, lac_decode::DecodeError> {
        lac_decode::decode_range(self.container.as_ref(), &mut self.reader, range)
    }

    /// The ranges of the source that `request` keeps, in output order,
    /// without empty ones.
    pub fn kept_ranges(&self, request: &CutRequest) -> Result<Vec<Range<SamplePos>>, AppError> {
        let total = self.total_samples();
        let kept = match request {
            CutRequest::Delete(delete) => {
                let mut delete = delete.clone();
                ranges::normalize(&mut delete);
                let mut edit = EditList::whole(SourceId(0), total, Tags::default());
                // Deletes refer to the original timeline, so apply them from
                // the last to the first: earlier positions stay valid that way.
                for r in delete.iter().rev() {
                    edit.tracks[0].delete(r.clone())?;
                }
                edit.tracks
                    .swap_remove(0)
                    .segments
                    .into_iter()
                    .map(|s| s.range)
                    .collect()
            }
            CutRequest::Keep(keep) => {
                for r in keep {
                    if r.start > r.end || r.end > total {
                        return Err(EditError::OutOfRange {
                            start: r.start,
                            end: r.end,
                            len: total,
                        }
                        .into());
                    }
                }
                keep.clone()
            }
        };
        Ok(kept.into_iter().filter(|r| r.start < r.end).collect())
    }

    /// Writes the part of the source selected by `request` to `output`,
    /// without re-encoding, and reports what each kept range became.
    pub fn export(
        &mut self,
        request: &CutRequest,
        output: impl AsRef<Path>,
    ) -> Result<Vec<KeptRange>, AppError> {
        let output = output.as_ref();
        if same_file(&self.path, output) {
            return Err(AppError::OutputIsInput);
        }
        let kept = self.kept_ranges(request)?;
        let plans: Vec<CutPlan> =
            plan_cuts(self.container.index(), self.container.capabilities(), &kept)?;

        let write_err = |source: ContainerError| AppError::Write {
            path: output.to_path_buf(),
            source,
        };
        let file = File::create(output).map_err(|e| write_err(e.into()))?;
        let mut w = BufWriter::new(file);
        self.container
            .write_cut(&mut self.reader, &plans, &mut w)
            .map_err(write_err)?;
        w.flush().map_err(|e| write_err(e.into()))?;

        Ok(kept
            .into_iter()
            .zip(plans)
            .map(|(requested, p)| KeptRange {
                requested,
                padding: p.padding_before + p.padding_after,
                actual: p.actual,
            })
            .collect())
    }
}

/// True if both paths name the same existing file.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}
