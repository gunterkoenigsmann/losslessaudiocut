//! Autosave for crash recovery (issue #97): while a recording has edits
//! that no export or project file contains, a copy of them is kept in the
//! user's data folder, so that a crash or a power cut loses (almost)
//! nothing, even before the user has saved a project.
//!
//! - [`Recovery`] is the folder: one project file (the format of
//!   [`crate::project`]) per recording, named by a hash of the recording's
//!   path, which the file itself also names.
//! - [`Autosaver`] decides, on each tick of the GUI's timer, whether the
//!   copy is to be written or removed: written when the edits changed (at
//!   most every [`INTERVAL`]), removed once nothing unsaved is left.
//!
//! When the recording is opened again and a copy with other edits exists,
//! the GUI offers to restore it. Saving, exporting, discarding or a normal
//! close removes the copy, so one that is still there at start means the
//! program did not end normally.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::editor::Edits;
use crate::peak_cache::fnv1a;
use crate::project::{self, Project};

/// The least time between two writes of the copy while editing goes on.
pub const INTERVAL: Duration = Duration::from_secs(2);

/// The folder of recovery copies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovery {
    dir: PathBuf,
}

/// A recovery copy found in the folder.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// The edits and the recording they belong to.
    pub project: Project,
    /// When the copy was last written.
    pub modified: SystemTime,
}

impl Recovery {
    /// Copies kept in `dir`; it is created when the first is written.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Recovery { dir: dir.into() }
    }

    /// The per-user folder: on Linux `$XDG_DATA_HOME/losslessaudiocut/recovery`
    /// (default `~/.local/share/…`), on macOS
    /// `~/Library/Application Support/…`, on Windows
    /// `%LOCALAPPDATA%\losslessaudiocut\data\recovery`. Not the cache
    /// folder: cleaning caches must not throw away the user's work. `None`
    /// if the system has no home directory.
    pub fn default_dir() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "losslessaudiocut")
            .map(|d| d.data_local_dir().join("recovery"))
    }

    /// The folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The copy's file for the recording at `audio` (an absolute path, so
    /// that every way of naming the file finds the same copy).
    pub fn path_for(&self, audio: &Path) -> PathBuf {
        let hash = fnv1a(audio.to_string_lossy().as_bytes());
        self.dir.join(format!("{hash:016x}.{}", project::EXTENSION))
    }

    /// Writes the copy of `project` (its `audio` absolute). The text goes
    /// to a temporary file first and replaces the copy in one rename, so a
    /// crash while writing leaves the old copy, not half a new one.
    pub fn store(&self, project: &Project) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.path_for(&project.audio);
        let tmp = path.with_extension("tmp");
        // The folder is never the recording's, so the path stays absolute.
        std::fs::write(&tmp, project.to_text(&self.dir))?;
        std::fs::rename(&tmp, &path)
    }

    /// The copy for the recording at `audio`, if there is one that can be
    /// read and is really that recording's (not a hash collision).
    pub fn load(&self, audio: &Path) -> Option<Found> {
        self.read(&self.path_for(audio))
            .filter(|f| f.project.audio == audio)
    }

    /// Removes the copy for the recording at `audio`; none is fine.
    pub fn remove(&self, audio: &Path) -> io::Result<()> {
        match std::fs::remove_file(self.path_for(audio)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// Every readable copy, the newest first: the edits a crash left. A
    /// file that is no copy is skipped.
    pub fn all(&self) -> Vec<Found> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut found: Vec<Found> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| project::is_project(p))
            .filter_map(|p| self.read(&p))
            .collect();
        found.sort_by_key(|f| std::cmp::Reverse(f.modified));
        found
    }

    /// The copy in the file at `path`.
    fn read(&self, path: &Path) -> Option<Found> {
        let text = std::fs::read_to_string(path).ok()?;
        let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
        let project = Project::parse(&text, &self.dir).ok()?;
        Some(Found { project, modified })
    }
}

/// What to do with the recovery copy now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Leave it as it is.
    Keep,
    /// Write these edits.
    Write(Edits),
    /// Remove it: nothing unsaved is left.
    Remove,
}

/// Decides when the recovery copy of the open recording is written or
/// removed. Starts afresh for every recording opened.
#[derive(Debug, Clone, Default)]
pub struct Autosaver {
    /// The edits last written, if a copy exists.
    written: Option<Edits>,
    /// When they were written.
    at: Option<Instant>,
}

impl Autosaver {
    /// A saver for a recording whose copy (if any) is already there with
    /// `edits`, e.g. restored from it; [`Autosaver::default`] for one
    /// without a copy.
    pub fn with_copy(edits: Edits) -> Self {
        Autosaver {
            written: Some(edits),
            at: None,
        }
    }

    /// The step for the current `edits` at `now`; `unsaved` tells whether
    /// they are in no export or project file
    /// ([`crate::editor::Editor::has_unexported_changes`]).
    pub fn step(&mut self, edits: &Edits, unsaved: bool, now: Instant) -> Step {
        if !unsaved {
            return if self.written.take().is_some() {
                self.at = None;
                Step::Remove
            } else {
                Step::Keep
            };
        }
        if self.written.as_ref() == Some(edits)
            || self
                .at
                .is_some_and(|t| now.saturating_duration_since(t) < INTERVAL)
        {
            return Step::Keep;
        }
        self.written = Some(edits.clone());
        self.at = Some(now);
        Step::Write(edits.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lac-recovery-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn edits(deleted: u64) -> Edits {
        Edits {
            deleted: std::iter::once(0..deleted).collect(),
            ..Edits::default()
        }
    }

    /// An absolute path on every system (a bare `/a/b` is not one on
    /// Windows), outside the copies' folder.
    fn abs(path: &str) -> PathBuf {
        std::env::temp_dir().join("lac-recordings").join(path)
    }

    fn project(audio: &str, deleted: u64) -> Project {
        Project {
            audio: abs(audio),
            frames: Some(48_000),
            edits: edits(deleted),
            view: None,
        }
    }

    #[test]
    fn a_copy_survives_until_removed() {
        let dir = scratch("roundtrip");
        let r = Recovery::new(&dir);
        let audio = &abs("music/talk.mp3");
        assert_eq!(r.load(audio), None);
        let p = project("music/talk.mp3", 100);
        r.store(&p).expect("stored");
        let found = r.load(audio).expect("found");
        assert_eq!(found.project, p);
        // A newer write replaces it, and no temporary file is left.
        r.store(&project("music/talk.mp3", 200)).expect("stored");
        assert_eq!(r.load(audio).expect("found").project.edits, edits(200));
        assert_eq!(std::fs::read_dir(&dir).expect("dir").count(), 1);
        r.remove(audio).expect("removed");
        assert_eq!(r.load(audio), None);
        // Removing what is not there is no error.
        r.remove(audio).expect("nothing to remove");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Recordings of the same name in different folders keep their own
    /// copies; a copy naming another recording (a hash collision) is not
    /// taken for this one's.
    #[test]
    fn copies_belong_to_one_recording() {
        let dir = scratch("separate");
        let r = Recovery::new(&dir);
        r.store(&project("a/talk.mp3", 1)).expect("stored");
        r.store(&project("b/talk.mp3", 2)).expect("stored");
        assert_eq!(
            r.load(&abs("a/talk.mp3")).expect("a").project.edits,
            edits(1)
        );
        assert_eq!(
            r.load(&abs("b/talk.mp3")).expect("b").project.edits,
            edits(2)
        );
        // Fake a collision: /c's copy file holds /a's edits.
        std::fs::copy(
            r.path_for(&abs("a/talk.mp3")),
            r.path_for(&abs("c/talk.mp3")),
        )
        .expect("copied");
        assert_eq!(r.load(&abs("c/talk.mp3")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn all_lists_readable_copies_newest_first() {
        let dir = scratch("all");
        let r = Recovery::new(&dir);
        assert!(r.all().is_empty(), "no folder yet");
        r.store(&project("old.wav", 1)).expect("stored");
        // File times may be coarse; make the first one clearly older.
        let old = r.path_for(&abs("old.wav"));
        let f = std::fs::File::options()
            .write(true)
            .open(&old)
            .expect("open");
        f.set_modified(SystemTime::now() - Duration::from_secs(60))
            .expect("set time");
        r.store(&project("new.wav", 2)).expect("stored");
        std::fs::write(dir.join("junk.lacproj"), "not a project").expect("junk");
        std::fs::write(dir.join("notes.txt"), "hello").expect("notes");
        let audio: Vec<_> = r.all().into_iter().map(|f| f.project.audio).collect();
        assert_eq!(audio, [abs("new.wav"), abs("old.wav")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The first change is written at once, later ones at most every
    /// INTERVAL; unchanged edits are not written again, and the copy goes
    /// once everything is saved.
    #[test]
    fn the_saver_writes_changes_and_removes_saved_ones() {
        let t0 = Instant::now();
        let mut s = Autosaver::default();
        // Nothing edited: no copy.
        assert_eq!(s.step(&Edits::default(), false, t0), Step::Keep);
        assert_eq!(s.step(&edits(1), true, t0), Step::Write(edits(1)));
        assert_eq!(s.step(&edits(1), true, t0 + INTERVAL), Step::Keep);
        // A change soon after waits for the interval.
        let soon = t0 + INTERVAL / 2;
        assert_eq!(s.step(&edits(2), true, soon), Step::Keep);
        assert_eq!(
            s.step(&edits(2), true, t0 + INTERVAL),
            Step::Write(edits(2))
        );
        // Saved (or exported): the copy goes, once.
        let later = t0 + 3 * INTERVAL;
        assert_eq!(s.step(&edits(2), false, later), Step::Remove);
        assert_eq!(s.step(&edits(2), false, later), Step::Keep);
        // A new change after saving is written at once.
        assert_eq!(s.step(&edits(3), true, later), Step::Write(edits(3)));
    }

    /// A recording opened with its copy (restored) does not write the same
    /// edits again, but removes the copy when they are saved.
    #[test]
    fn a_restored_copy_is_not_written_again() {
        let t0 = Instant::now();
        let mut s = Autosaver::with_copy(edits(5));
        assert_eq!(s.step(&edits(5), true, t0), Step::Keep);
        assert_eq!(s.step(&edits(6), true, t0), Step::Write(edits(6)));
        let mut s = Autosaver::with_copy(edits(5));
        assert_eq!(s.step(&edits(5), false, t0), Step::Remove);
    }
}
