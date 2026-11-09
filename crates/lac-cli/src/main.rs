//! `lac`: command line front end of losslessaudiocut.
//!
//! ```text
//! lac info recording.wav
//! lac cut recording.wav -o out.wav --delete 1:00..1:30 --delete 5:00..
//! lac cut recording.wav -o out.wav --keep 0:10..3:45
//! lac cut recording.flac -o part.flac --keep 1:00.. --tag title="Part 2" --tag track=2
//! lac detect recording.mp3 --min-gap 3
//! lac completions bash > ~/.local/share/bash-completion/completions/lac
//! ```
//!
//! Supported formats: WAV, AIFF, FLAC, Ogg Vorbis/Opus/Speex, MP3, MP4/M4A (AAC, ALAC), AC-3 and Matroska/WebM Opus (detected from the content). Besides being useful for scripting, the CLI drives the end-to-end tests.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueHint};
use clap_complete::Shell;
use lac_analysis::{DetectParams, RegionKind, SplitReason};
use lac_app::launch::GuiArgs;
use lac_app::{CutRequest, Source, TagEdits, TimeRange, format_samples};
use lac_core::{SamplePos, TagField};

#[derive(Parser)]
#[command(
    name = "lac",
    version,
    about = "Lossless audio cutter",
    long_about = "Cuts audio recordings without re-encoding them: what is kept \
                  decodes to exactly the same samples as the original.\n\n\
                  Supported formats: WAV, AIFF, FLAC, Ogg Vorbis/Opus/Speex, \
                  MP3, MP4/M4A (AAC, ALAC), AC-3 and Matroska/WebM Opus, detected from \
                  the content.",
    after_long_help = EXAMPLES
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show format, length and warnings of a file.
    Info {
        /// The audio file.
        #[arg(value_hint = ValueHint::FilePath)]
        file: PathBuf,
    },
    /// Cut a file without re-encoding.
    ///
    /// Times are seconds (`90`, `90.5`), `m:ss`, `h:mm:ss` (optionally with
    /// a fraction) or `@N` for sample frame N. A range is `START..END`;
    /// either side may be omitted to mean the start or end of the file.
    Cut {
        /// The source file.
        #[arg(value_hint = ValueHint::FilePath)]
        input: PathBuf,
        /// Where to write the result. Must differ from the input.
        #[arg(short, long, value_hint = ValueHint::FilePath)]
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
        #[arg(value_hint = ValueHint::FilePath)]
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
        /// Also suggest track starts at changes of key or harmony. Off by
        /// default: on real music it found few extra track starts but
        /// many inside songs.
        #[arg(long)]
        key: bool,
    },
    /// Print a shell completion script.
    ///
    /// Bash: `lac completions bash > ~/.local/share/bash-completion/completions/lac`.
    /// Zsh: write `_lac` into a directory of `$fpath`. Fish:
    /// `lac completions fish > ~/.config/fish/completions/lac.fish`.
    /// PowerShell: add `lac completions powershell | Out-String | Invoke-Expression`
    /// to your `$PROFILE`.
    Completions {
        /// The shell to complete in.
        shell: Shell,
        /// The command name to complete, if `lac` is called by another
        /// name (an alias, or `losslessaudiocut.cli` in the snap).
        #[arg(long, value_name = "NAME", default_value = "lac")]
        bin_name: String,
    },
    /// Write the manpages (lac.1, lac-cut.1, ...) into a directory.
    ///
    /// For packagers; hidden from the help.
    #[command(hide = true)]
    Manpages {
        /// Where to write the pages; created if missing.
        #[arg(value_hint = ValueHint::DirPath)]
        dir: PathBuf,
    },
}

/// Examples at the end of `lac --help` and in the manpage.
const EXAMPLES: &str = "\
Examples:
  lac info recording.wav
  lac cut recording.wav -o out.wav --delete 1:00..1:30 --delete 5:00..
  lac cut recording.wav -o out.wav --keep 0:10..3:45
  lac cut recording.flac -o part.flac --keep 1:00.. --tag title=\"Part 2\" --tag track=2
  lac detect recording.mp3 --min-gap 3
  lac completions bash > ~/.local/share/bash-completion/completions/lac";

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
            key,
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
                min_key_distance: if key {
                    lac_analysis::KEY_DISTANCE
                } else {
                    DetectParams::default().min_key_distance
                },
                ..DetectParams::default()
            },
        ),
        Command::Completions { shell, bin_name } => {
            clap_complete::generate(shell, &mut Cli::command(), bin_name, &mut std::io::stdout());
            Ok(())
        }
        Command::Manpages { dir } => manpages(&dir),
    }
}

/// Writes one manpage for `lac`, one for each visible subcommand
/// (`lac-cut.1` and so on, the naming git uses) and `losslessaudiocut.1`
/// for the GUI into `dir`.
///
/// `lac.1` is assembled from clap_mangen's sections plus EXAMPLES and
/// SEE ALSO: its default page would put the examples under a heading
/// "EXTRA" and reflow them into one paragraph.
fn manpages(dir: &Path) -> Result<()> {
    use clap_mangen::roff::{Roff, roman};

    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let cmd = Cli::command().disable_help_subcommand(true);
    clap_mangen::generate_to(cmd.clone(), dir)
        .with_context(|| format!("writing manpages to {}", dir.display()))?;

    let mut cmd = cmd;
    cmd.build();
    let gui = GuiArgs::command();
    clap_mangen::Man::new(gui.clone())
        .generate_to(dir)
        .with_context(|| format!("writing manpages to {}", dir.display()))?;

    let mut see_also: Vec<String> = cmd
        .get_subcommands()
        .filter(|s| !s.is_hide_set())
        .map(|s| format!("lac-{}(1)", s.get_name()))
        .collect();
    see_also.push(format!("{}(1)", gui.get_name()));
    let man = clap_mangen::Man::new(cmd);
    let mut page = Vec::new();
    man.render_title(&mut page)?;
    man.render_name_section(&mut page)?;
    man.render_synopsis_section(&mut page)?;
    man.render_description_section(&mut page)?;
    man.render_options_section(&mut page)?;
    man.render_subcommands_section(&mut page)?;
    let mut extra = Roff::new();
    // .nf/.fi: no filling, so that each example stays on its own line.
    extra.control("SH", ["EXAMPLES"]).control("nf", []);
    for line in EXAMPLES.lines().skip(1) {
        extra.text([roman(line.trim_start())]);
    }
    extra.control("fi", []);
    extra
        .control("SH", ["SEE ALSO"])
        .text([roman(see_also.join(", "))]);
    page.extend(extra.render().into_bytes());
    man.render_version_section(&mut page)?;
    let path = dir.join("lac.1");
    std::fs::write(&path, page).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn info(path: &Path) -> Result<()> {
    let mut src = Source::open(path)?;
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
    for n in src.cut_notes() {
        println!("note:        {n}");
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
    for n in src.cut_notes() {
        println!("note: {n}");
    }
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
