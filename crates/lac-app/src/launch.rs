//! The command line of the GUI program `losslessaudiocut`.
//!
//! File managers start it with the file to open (`Exec=losslessaudiocut %f`
//! in the Linux desktop file, `"%1"` in the Windows registration). On macOS
//! the Finder passes files through an "open documents" event instead, which
//! the GUI handles itself; old macOS versions also add a `-psn_…` argument
//! (the process serial number) that is dropped here.
//!
//! The definition lives here rather than in `lac-gui` so that `lac
//! manpages` can write the GUI's manpage too, without a display.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{CommandFactory, Parser, ValueHint};

/// Arguments of `losslessaudiocut`.
#[derive(Debug, Parser, PartialEq, Eq)]
#[command(
    name = "losslessaudiocut",
    version,
    about = "Cut audio recordings without re-encoding them",
    long_about = "Shows the waveform of an audio recording, plays it, and \
                  deletes parts of it or splits it into tracks without \
                  re-encoding: what is kept decodes to exactly the same \
                  samples as the original.\n\n\
                  Supported formats: WAV, AIFF, FLAC, Ogg Vorbis/Opus/Speex, \
                  MP3, MP4/M4A (AAC, ALAC) and Matroska/WebM Opus, detected from \
                  the content. The \
                  command line tool lac(1) does the same cuts without a \
                  window."
)]
pub struct GuiArgs {
    /// The audio file to open.
    #[arg(value_hint = ValueHint::FilePath)]
    pub file: Option<PathBuf>,
}

impl GuiArgs {
    /// Parses the program's arguments (`args` includes the program name,
    /// as from [`std::env::args_os`]).
    ///
    /// On `--help`, `--version` or a malformed command line the error says
    /// what to print and with which exit code ([`clap::Error::exit`]).
    pub fn try_parse_os(
        args: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Result<Self, clap::Error> {
        // Finder on macOS 10.8 and older passes -psn_0_12345.
        let args = args
            .into_iter()
            .map(Into::into)
            .filter(|a| !a.to_string_lossy().starts_with("-psn_"));
        Self::try_parse_from(args)
    }

    /// The clap definition, for the manpage.
    pub fn command() -> clap::Command {
        <Self as CommandFactory>::command()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<GuiArgs, clap::Error> {
        GuiArgs::try_parse_os(std::iter::once("losslessaudiocut").chain(args.iter().copied()))
    }

    #[test]
    fn no_arguments_open_nothing() {
        assert_eq!(parse(&[]).expect("valid"), GuiArgs { file: None });
    }

    #[test]
    fn a_file_is_opened() {
        let a = parse(&["/music/show.mp3"]).expect("valid");
        assert_eq!(a.file, Some(PathBuf::from("/music/show.mp3")));
    }

    #[test]
    fn a_file_named_like_an_option_follows_a_double_dash() {
        let a = parse(&["--", "-intro.wav"]).expect("valid");
        assert_eq!(a.file, Some(PathBuf::from("-intro.wav")));
    }

    #[test]
    fn macos_process_serial_number_is_ignored() {
        assert_eq!(parse(&["-psn_0_1234567"]).expect("valid").file, None);
        let a = parse(&["-psn_0_42", "a.flac"]).expect("valid");
        assert_eq!(a.file, Some(PathBuf::from("a.flac")));
    }

    #[test]
    fn help_and_version_are_answered_by_clap() {
        for (arg, kind) in [
            ("--help", clap::error::ErrorKind::DisplayHelp),
            ("-h", clap::error::ErrorKind::DisplayHelp),
            ("--version", clap::error::ErrorKind::DisplayVersion),
        ] {
            assert_eq!(parse(&[arg]).expect_err("not a file").kind(), kind);
        }
    }

    #[test]
    fn unknown_options_and_extra_files_are_errors() {
        assert!(parse(&["--frobnicate"]).is_err());
        assert!(parse(&["a.mp3", "b.mp3"]).is_err());
    }
}
