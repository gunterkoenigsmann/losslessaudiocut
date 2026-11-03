//! `lac`: command line front end of losslessaudiocut.
//!
//! ```text
//! lac info recording.wav
//! lac cut recording.wav -o out.wav --delete 1:00..1:30 --delete 5:00..
//! lac cut recording.wav -o out.wav --keep 0:10..3:45
//! ```
//!
//! Supported formats: WAV, FLAC, Ogg Vorbis/Opus/Speex and MP3 (detected from the content). Besides being useful for scripting, the CLI drives the end-to-end tests.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};
use lac_app::{CutRequest, Source, TimeRange, format_samples};
use lac_core::SamplePos;

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

fn info(path: &Path) -> Result<()> {
    let src = Source::open(path)?;
    let i = src.info();
    let total = src.total_samples();
    println!("format:      {} ({})", src.format_name(), src.details());
    println!("sample rate: {} Hz", i.sample_rate);
    println!("channels:    {}", i.channels);
    println!(
        "length:      {} ({} sample frames)",
        format_samples(total, i.sample_rate),
        total
    );
    for w in src.warnings() {
        println!("warning:     {w}");
    }
    Ok(())
}

fn cut(input: &Path, output: &Path, delete: &[TimeRange], keep: &[TimeRange]) -> Result<()> {
    let mut src = Source::open(input)?;
    let rate = src.info().sample_rate;
    let total = src.total_samples();
    let resolve = |ranges: &[TimeRange]| {
        ranges
            .iter()
            .map(|r| r.resolve(rate, total))
            .collect::<Result<Vec<_>, _>>()
    };
    let request = if keep.is_empty() {
        CutRequest::Delete(resolve(delete)?)
    } else {
        CutRequest::Keep(resolve(keep)?)
    };

    let kept = src.export(&request, output)?;

    // Exactness report: what was asked for and what the file contains.
    let mut out_len: SamplePos = 0;
    for k in &kept {
        let exact = if k.is_exact() { "exact" } else { "widened" };
        let padding = if k.padding > 0 {
            format!(", plus {} sample frames of encoder padding", k.padding)
        } else {
            String::new()
        };
        println!(
            "kept {} .. {}  ({exact}{padding})",
            format_samples(k.actual.start, rate),
            format_samples(k.actual.end, rate)
        );
        out_len += k.output_len();
    }
    println!(
        "wrote {} ({})",
        output.display(),
        format_samples(out_len, rate)
    );
    Ok(())
}
