//! `lac`: command line front end of losslessaudiocut.
//!
//! ```text
//! lac info recording.wav
//! lac cut recording.wav -o out.wav --delete 1:00..1:30 --delete 5:00..
//! lac cut recording.wav -o out.wav --keep 0:10..3:45
//! ```
//!
//! Supported formats: WAV and FLAC (detected from the content). Besides being useful for scripting, the CLI drives the end-to-end tests.

mod time;

use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use lac_container::Container;
use lac_core::{CutPlan, EditList, SamplePos, Segment, SourceId, Tags, plan_cut};

use crate::time::{TimeRange, format_samples};

#[derive(Parser)]
#[command(name = "lac", version, about = "Lossless audio cutter")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show format, length and warnings of a file.
    Info {
        /// The audio file.
        file: PathBuf,
    },
    /// Cut a file without re-encoding.
    ///
    /// Times are seconds (`90`, `90.5`), `m:ss`, `h:mm:ss` (optionally with
    /// a fraction) or `@N` for sample frame N. A range is `START..END`;
    /// either side may be omitted to mean the start or end of the file.
    Cut {
        /// The source file.
        input: PathBuf,
        /// Where to write the result. Must differ from the input.
        #[arg(short, long)]
        output: PathBuf,
        /// Delete this range; the rest is joined. May be repeated; ranges
        /// refer to the original file.
        #[arg(long, value_name = "RANGE", conflicts_with = "keep")]
        delete: Vec<TimeRange>,
        /// Keep only this range. May be repeated; the kept ranges are joined
        /// in the given order.
        #[arg(long, value_name = "RANGE")]
        keep: Vec<TimeRange>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Info { file } => info(&file),
        Command::Cut {
            input,
            output,
            delete,
            keep,
        } => cut(&input, &output, &delete, &keep),
    }
}

/// Opens a file of any supported format, detected from its content.
fn open(path: &Path) -> Result<(Box<dyn Container>, BufReader<File>)> {
    let mut r = BufReader::new(
        File::open(path).with_context(|| format!("cannot open {}", path.display()))?,
    );
    let c =
        lac_container::open(&mut r).with_context(|| format!("cannot read {}", path.display()))?;
    Ok((c, r))
}

fn info(path: &Path) -> Result<()> {
    let (c, _) = open(path)?;
    let i = c.index().info();
    let total = c.index().total_samples();
    println!("format:      {} ({})", c.format_name(), c.details());
    println!("sample rate: {} Hz", i.sample_rate);
    println!("channels:    {}", i.channels);
    println!(
        "length:      {} ({} sample frames)",
        format_samples(total, i.sample_rate),
        total
    );
    for w in c.warnings() {
        println!("warning:     {w}");
    }
    Ok(())
}

fn cut(input: &Path, output: &Path, delete: &[TimeRange], keep: &[TimeRange]) -> Result<()> {
    if same_file(input, output) {
        bail!("output must not be the input file");
    }
    let (wav, mut r) = open(input)?;
    let rate = wav.index().info().sample_rate;
    let total = wav.index().total_samples();
    let source = SourceId(0);

    let segments: Vec<Segment> = if keep.is_empty() {
        let mut edit = EditList::whole(source, total, Tags::default());
        // Deletes refer to the original timeline, so apply them from the
        // last to the first: earlier positions stay valid that way.
        let mut ranges = delete
            .iter()
            .map(|d| d.resolve(rate, total))
            .collect::<Result<Vec<_>>>()?;
        ranges.sort_by_key(|r| r.start);
        merge_overlapping(&mut ranges);
        for r in ranges.iter().rev() {
            edit.tracks[0].delete(r.clone())?;
        }
        edit.tracks.remove(0).segments
    } else {
        keep.iter()
            .map(|k| {
                Ok(Segment {
                    source,
                    range: k.resolve(rate, total)?,
                })
            })
            .collect::<Result<_>>()?
    };

    let segments: Vec<Segment> = segments.into_iter().filter(|s| !s.is_empty()).collect();
    let plans: Vec<CutPlan> = segments
        .iter()
        .map(|s| plan_cut(wav.index(), wav.capabilities(), s.range.clone()))
        .collect::<Result<_, _>>()?;

    let mut w = BufWriter::new(
        File::create(output).with_context(|| format!("cannot create {}", output.display()))?,
    );
    wav.write_cut(&mut r, &plans, &mut w)?;
    w.flush()?;

    // Exactness report: what was asked for and what the file contains.
    let mut out_len: SamplePos = 0;
    for (s, p) in segments.iter().zip(&plans) {
        let exact = if p.actual == s.range {
            "exact"
        } else {
            "widened"
        };
        println!(
            "kept {} .. {}  ({exact})",
            format_samples(p.actual.start, rate),
            format_samples(p.actual.end, rate)
        );
        out_len += p.actual.end - p.actual.start;
    }
    println!(
        "wrote {} ({})",
        output.display(),
        format_samples(out_len, rate)
    );
    Ok(())
}

/// Merges overlapping or touching ranges of a list sorted by start.
fn merge_overlapping(ranges: &mut Vec<std::ops::Range<SamplePos>>) {
    ranges.dedup_by(|b, a| {
        if b.start <= a.end {
            a.end = a.end.max(b.end);
            true
        } else {
            false
        }
    });
}

/// True if both paths name the same existing file.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_joins_overlaps() {
        let mut r = vec![0..10, 5..20, 20..25, 30..40];
        merge_overlapping(&mut r);
        assert_eq!(r, vec![0..25, 30..40]);
    }
}
