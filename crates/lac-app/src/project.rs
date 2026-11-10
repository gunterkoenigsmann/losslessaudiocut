//! Project files (issue #97): the edits of one recording, saved so that
//! work can go on later. File > Save project writes one; File > Open reads
//! it like an audio file and opens its recording with the edits.
//!
//! The format is plain UTF-8 text, one item per line, so that it can be
//! read, compared and repaired by hand:
//!
//! ```text
//! losslessaudiocut project 1
//! audio talk.mp3
//! frames 2646000
//! delete 44100 88200
//! split 1323000
//! tag * album Live at the Bar
//! tag 1323000 title Second set
//! view 0 2646000
//! ```
//!
//! - `audio`: the recording, relative to the project file's folder when it
//!   is inside it (so a folder can be moved as a whole), else absolute.
//! - `frames`: the recording's length when saved, to notice a recording
//!   that changed since.
//! - `delete START END`, `split POS`: sample frames of the source.
//! - `tag TRACK FIELD VALUE`: a tag change for all tracks (`*`) or for the
//!   track starting at `TRACK`; `FIELD` is [`TagField::key`]. An empty
//!   value clears the tag.
//! - `view START END`: the part of the waveform that was shown.
//!
//! Values escape `\` as `\\`, line breaks as `\n` and tabs as `\t`. Unknown
//! keywords are skipped, so a newer program's additions do not stop an
//! older one from reading the rest; a newer major version is refused.

use std::ops::Range;
use std::path::{Path, PathBuf};

use lac_core::{SamplePos, TagField};
use thiserror::Error;

use crate::editor::Edits;

/// The file name extension of project files.
pub const EXTENSION: &str = "lacproj";

/// The first line: what the file is, and the format's version.
const MAGIC: &str = "losslessaudiocut project";
/// The version this program writes and the newest it reads.
const VERSION: u32 = 1;

/// A recording and its edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The recording.
    pub audio: PathBuf,
    /// Its length in sample frames when the project was saved, if known.
    pub frames: Option<SamplePos>,
    /// What is deleted, split and tagged.
    pub edits: Edits,
    /// The part of the waveform that was shown, if any.
    pub view: Option<Range<SamplePos>>,
}

/// Why a project file cannot be read.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProjectError {
    /// The first line is not that of a project file.
    #[error("not a losslessaudiocut project")]
    NotAProject,
    /// Written by a newer program in a format this one does not know.
    #[error("project format {0} is newer than this program reads ({VERSION})")]
    TooNew(u32),
    /// A line that cannot be read.
    #[error("line {line}: cannot read {text:?}")]
    Syntax {
        /// 1-based line number.
        line: usize,
        /// The line.
        text: String,
    },
    /// The file names no recording.
    #[error("the project names no audio file")]
    NoAudio,
}

impl Project {
    /// The project file's text. `dir` is the folder the file is written
    /// to, which a recording inside it is named relative to.
    pub fn to_text(&self, dir: &Path) -> String {
        let audio = self.audio.strip_prefix(dir).unwrap_or(&self.audio);
        let mut out = format!(
            "{MAGIC} {VERSION}\naudio {}\n",
            escape(&audio.to_string_lossy())
        );
        if let Some(f) = self.frames {
            out += &format!("frames {f}\n");
        }
        for r in &self.edits.deleted {
            out += &format!("delete {} {}\n", r.start, r.end);
        }
        for s in &self.edits.splits {
            out += &format!("split {s}\n");
        }
        for (field, value) in self.edits.all_tags.iter() {
            out += &format!("tag * {} {}\n", field.key(), escape(value));
        }
        for (start, tags) in &self.edits.track_tags {
            for (field, value) in tags.iter() {
                out += &format!("tag {start} {} {}\n", field.key(), escape(value));
            }
        }
        if let Some(v) = &self.view {
            out += &format!("view {} {}\n", v.start, v.end);
        }
        out
    }

    /// Reads a project file's text. `dir` is the folder the file is in,
    /// which a relative recording path is relative to.
    pub fn parse(text: &str, dir: &Path) -> Result<Project, ProjectError> {
        let mut lines = text.lines().enumerate();
        let version = lines
            .next()
            .and_then(|(_, l)| l.trim_end().strip_prefix(MAGIC))
            .and_then(|v| v.trim().parse::<u32>().ok())
            .ok_or(ProjectError::NotAProject)?;
        if version > VERSION {
            return Err(ProjectError::TooNew(version));
        }
        let mut audio = None;
        let mut frames = None;
        let mut edits = Edits::default();
        let mut view = None;
        for (n, raw) in lines {
            let syntax = || ProjectError::Syntax {
                line: n + 1,
                text: raw.to_string(),
            };
            let line = raw.trim_end_matches('\r');
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let (keyword, rest) = line.split_once(' ').unwrap_or((line, ""));
            let number = |s: &str| s.parse::<SamplePos>().map_err(|_| syntax());
            let pair = |s: &str| -> Result<Range<SamplePos>, ProjectError> {
                let (a, b) = s.split_once(' ').ok_or_else(syntax)?;
                let r = number(a)?..number(b)?;
                if r.start < r.end {
                    Ok(r)
                } else {
                    Err(syntax())
                }
            };
            match keyword {
                "audio" => audio = Some(unescape(rest).ok_or_else(syntax)?),
                "frames" => frames = Some(number(rest)?),
                "delete" => edits.deleted.push(pair(rest)?),
                "split" => edits.splits.push(number(rest)?),
                "view" => view = Some(pair(rest)?),
                "tag" => {
                    let mut parts = rest.splitn(3, ' ');
                    let (Some(track), Some(field)) = (parts.next(), parts.next()) else {
                        return Err(syntax());
                    };
                    let field = TagField::from_key(field).ok_or_else(syntax)?;
                    let value = unescape(parts.next().unwrap_or("")).ok_or_else(syntax)?;
                    let tags = if track == "*" {
                        &mut edits.all_tags
                    } else {
                        edits.track_tags.entry(number(track)?).or_default()
                    };
                    tags.set(field, &value).map_err(|_| syntax())?;
                }
                // Something a newer program added.
                _ => {}
            }
        }
        let audio = PathBuf::from(
            audio
                .filter(|a| !a.is_empty())
                .ok_or(ProjectError::NoAudio)?,
        );
        Ok(Project {
            audio: dir.join(audio),
            frames,
            edits,
            view,
        })
    }
}

/// Whether `path` names a project file (by its extension).
pub fn is_project(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(EXTENSION))
}

/// The project file to suggest for `audio`: the same name with the
/// project extension.
pub fn default_path(audio: &Path) -> PathBuf {
    audio.with_extension(EXTENSION)
}

/// `s` with `\`, line breaks and tabs escaped.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// The escaped value `s` read back; `None` for an unknown escape.
fn unescape(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        out.push(match chars.next()? {
            '\\' => '\\',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            _ => return None,
        });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TagEdits;

    fn sample(audio: &Path) -> Project {
        let mut all = TagEdits::new();
        all.set(TagField::Album, "Live \\ at\tthe\nBar")
            .expect("album");
        let mut second = TagEdits::new();
        second.set(TagField::Title, "Second set").expect("title");
        second.set(TagField::Artist, "").expect("cleared");
        Project {
            audio: audio.to_path_buf(),
            frames: Some(2_646_000),
            edits: Edits {
                deleted: vec![44_100..88_200, 100_000..100_001],
                splits: vec![1_323_000],
                all_tags: all,
                track_tags: [(1_323_000, second)].into_iter().collect(),
            },
            view: Some(0..2_646_000),
        }
    }

    #[test]
    fn projects_round_trip() {
        let dir = Path::new("/rec");
        let p = sample(Path::new("/rec/talk.mp3"));
        let text = p.to_text(dir);
        assert!(text.contains("\naudio talk.mp3\n"), "relative: {text}");
        assert_eq!(Project::parse(&text, dir), Ok(p.clone()));
        // Moved with its folder.
        let moved = Project::parse(&text, Path::new("/backup/rec")).expect("valid");
        assert_eq!(moved.audio, Path::new("/backup/rec/talk.mp3"));
        // A recording elsewhere keeps its absolute path.
        let other = sample(Path::new("/music/talk.mp3"));
        let text = other.to_text(dir);
        assert!(text.contains("\naudio /music/talk.mp3\n"), "{text}");
        assert_eq!(Project::parse(&text, dir), Ok(other));
    }

    #[test]
    fn the_format_is_readable_text() {
        let text = sample(Path::new("/rec/talk.mp3")).to_text(Path::new("/rec"));
        assert_eq!(
            text,
            "losslessaudiocut project 1\n\
             audio talk.mp3\n\
             frames 2646000\n\
             delete 44100 88200\n\
             delete 100000 100001\n\
             split 1323000\n\
             tag * album Live \\\\ at\\tthe\\nBar\n\
             tag 1323000 title Second set\n\
             tag 1323000 artist \n\
             view 0 2646000\n"
        );
    }

    #[test]
    fn unknown_lines_are_skipped_and_comments_allowed() {
        let p = Project::parse(
            "losslessaudiocut project 1\r\n# made by hand\naudio a.wav\r\nzoom 3\n\nsplit 5\n",
            Path::new("/x"),
        )
        .expect("valid");
        assert_eq!(p.audio, Path::new("/x/a.wav"));
        assert_eq!(p.edits.splits, [5]);
        assert_eq!(p.frames, None);
    }

    #[test]
    fn broken_files_are_errors_not_panics() {
        let dir = Path::new("/x");
        let head = "losslessaudiocut project 1\naudio a.wav\n";
        assert_eq!(Project::parse("", dir), Err(ProjectError::NotAProject));
        assert_eq!(
            Project::parse("RIFF....WAVE", dir),
            Err(ProjectError::NotAProject)
        );
        assert_eq!(
            Project::parse("losslessaudiocut project 2\naudio a\n", dir),
            Err(ProjectError::TooNew(2))
        );
        assert_eq!(
            Project::parse("losslessaudiocut project 1\nsplit 3\n", dir),
            Err(ProjectError::NoAudio)
        );
        for bad in [
            "delete 5",
            "delete 9 3",
            "delete a b",
            "split -1",
            "split 99999999999999999999999",
            "tag * nofield x",
            "tag x title y",
            "tag *",
            "tag * title bad\\q",
            "tag * track two",
            "view 3 3",
        ] {
            let r = Project::parse(&format!("{head}{bad}\n"), dir);
            assert!(
                matches!(r, Err(ProjectError::Syntax { line: 3, .. })),
                "{bad}: {r:?}"
            );
        }
    }

    #[test]
    fn project_files_are_told_by_their_extension() {
        assert!(is_project(Path::new("/a/b.lacproj")));
        assert!(is_project(Path::new("B.LACPROJ")));
        assert!(!is_project(Path::new("b.mp3")));
        assert_eq!(
            default_path(Path::new("/r/talk.mp3")),
            Path::new("/r/talk.lacproj")
        );
    }
}
