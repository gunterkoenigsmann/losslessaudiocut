//! `lac`: command line front end of losslessaudiocut.
//!
//! ```text
//! lac info recording.wav
//! lac cut recording.wav -o out.wav --delete 1:00..1:30 --delete 5:00..
//! lac cut recording.wav -o out.wav --keep 0:10..3:45
//! lac cut recording.flac -o part.flac --keep 1:00.. --tag title="Part 2" --tag track=2
//! lac detect recording.mp3 --min-gap 3
//! ```
//!
//! Supported formats: WAV, FLAC, Ogg Vorbis/Opus/Speex, MP3 and Matroska/WebM Opus (detected from the content). Besides being useful for scripting, the CLI drives the end-to-end tests.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};
use lac_analysis::{DetectParams, RegionKind, SplitReason};
use lac_app::{CutRequest, Source, TagEdits, TimeRange, format_samples};
use lac_core::{SamplePos, TagField};

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
        /// Set a tag of the output, e.g. `title=Intro` or `track=2`; an
        /// empty value removes it. Fields: title, artist, album,
        /// album_artist, track, tracktotal, date, genre, comment. May be
        /// repeated; other tags are copied from the input.
        #[arg(long, value_name = "FIELD=VALUE", value_parser = parse_tag)]
        tag: Vec<(TagField, String)>,
    },
    /// Suggest pauses to delete and track starts.
    ///
    /// Pauses are stretches that are quiet or only noise (a flat
    /// spectrum: hiss, wind, murmur); a new track is suggested after each
    /// long pause and where the timbre or the key changes without a pause.
    Detect {
        /// The audio file.
        file: PathBuf,
        /// Pauses at least this long (seconds) suggest a new track.
        #[arg(long, value_name = "SECONDS", default_value_t = DetectParams::default().min_track_gap_secs)]
        min_gap: f32,
        /// Pauses shorter than this (seconds) are not listed.
        #[arg(long, value_name = "SECONDS", default_value_t = DetectParams::default().min_pause_secs)]
        min_pause: f32,
        /// Do not suggest track starts at changes of timbre.
        #[arg(long)]
        no_timbre: bool,
        /// Do not suggest track starts at changes of key or harmony.
        #[arg(long)]
        no_key: bool,
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
            tag,
        } => cut(&input, &output, &delete, &keep, &tag),
        Command::Detect {
            file,
            min_gap,
            min_pause,
            no_timbre,
            no_key,
        } => detect(
            &file,
            &DetectParams {
                min_track_gap_secs: min_gap,
                min_pause_secs: min_pause,
                min_novelty_db: if no_timbre {
                    f32::INFINITY
                } else {
                    DetectParams::default().min_novelty_db
                },
                min_key_distance: if no_key {
                    f32::INFINITY
                } else {
                    DetectParams::default().min_key_distance
                },
                ..DetectParams::default()
            },
        ),
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
    let tags = src.tags();
    for f in TagField::ALL {
        if let Some(v) = tags.get(f) {
            println!("{:<12} {v}", format!("{}:", f.key()));
        }
    }
    for (k, v) in &tags.extra {
        // Pictures and other binary data hidden in text tags can be huge.
        let v: String = v.chars().take(60).collect();
        println!("tag:         {k}={v}");
    }
    for w in src.warnings() {
        println!("warning:     {w}");
    }
    Ok(())
}

/// Parses `FIELD=VALUE` of `--tag`.
fn parse_tag(s: &str) -> Result<(TagField, String), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("expected FIELD=VALUE, got {s:?}"))?;
    let field = TagField::from_key(k.trim()).ok_or_else(|| {
        let keys: Vec<&str> = TagField::ALL.iter().map(|f| f.key()).collect();
        format!("unknown field {k:?} (known: {})", keys.join(", "))
    })?;
    TagEdits::new().set(field, v).map_err(|e| e.to_string())?;
    Ok((field, v.to_string()))
}

fn cut(
    input: &Path,
    output: &Path,
    delete: &[TimeRange],
    keep: &[TimeRange],
    tag: &[(TagField, String)],
) -> Result<()> {
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

    let edits: TagEdits = tag.iter().cloned().collect();
    let tags = (!edits.is_empty()).then(|| edits.apply_to(src.tags()));
    let kept = src.export(&request, tags.as_ref(), output)?;

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

fn detect(path: &Path, params: &DetectParams) -> Result<()> {
    let mut src = Source::open(path)?;
    let rate = src.info().sample_rate;
    let (features, report) = src.analyze(|_| lac_app::Flow::Continue)?;
    for w in &report.warnings {
        println!("warning: {w}");
    }
    let d = lac_analysis::detect(&features, params);
    let t = d.thresholds;
    println!(
        "quiet below {:.1} dBFS (noise floor {:.1}, loud {:.1})",
        t.quiet_db, t.floor_db, t.loud_db
    );
    for r in &d.regions {
        let kind = match r.kind {
            RegionKind::Silence => "silence",
            RegionKind::Noise => "noise",
        };
        println!(
            "pause {} .. {}  {kind}, {:.1} dBFS",
            format_samples(r.range.start, rate),
            format_samples(r.range.end, rate),
            r.level_db
        );
    }
    for s in &d.splits {
        let why = match &s.reason {
            SplitReason::Gap { gap } => format!(
                "after a pause of {}",
                format_samples(gap.end - gap.start, rate)
            ),
            SplitReason::TimbreChange { novelty_db } => {
                format!("timbre changes by {novelty_db:.1} dB")
            }
            SplitReason::KeyChange { distance } => {
                format!("key or harmony changes (distance {distance:.2})")
            }
        };
        println!(
            "track {}  {why} (confidence {:.2})",
            format_samples(s.pos, rate),
            s.confidence
        );
    }
    Ok(())
}
