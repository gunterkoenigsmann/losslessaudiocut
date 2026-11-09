//! The list of recently opened files behind File > Open recent.
//!
//! wxWidgets has `wxFileHistory` for this, but wxdragon does not bind it
//! (nor the `wxDocManager` that usually owns one), so the list is kept
//! here: toolkit independent and unit-tested. The GUI stores it in its
//! config store and turns [`RecentFiles::menu_entries`] into menu items.
//!
//! The newest file comes first; opening a file again moves it to the
//! front instead of listing it twice.

use std::path::{Path, PathBuf};

/// How many files the menu lists, like `wxFileHistory`'s default. Nine
/// also means every entry gets a single-digit mnemonic (`&1` … `&9`).
pub const MAX_FILES: usize = 9;

/// Recently opened files, newest first, at most [`MAX_FILES`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecentFiles {
    /// Newest first, no duplicates.
    paths: Vec<PathBuf>,
}

/// One item of the Open recent submenu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEntry {
    /// The label, with a mnemonic and `&` escaped as `&&`.
    pub label: String,
    /// The full path, for the status bar while the item is highlighted.
    pub help: String,
}

impl RecentFiles {
    /// A list from stored paths, newest first. Duplicates and paths
    /// beyond [`MAX_FILES`] are dropped, so a hand-edited or corrupt
    /// store still gives a valid list.
    pub fn from_paths(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut list = RecentFiles::default();
        // Adding moves to the front, so add the oldest first.
        let paths: Vec<PathBuf> = paths.into_iter().collect();
        for p in paths.into_iter().rev() {
            list.add(p);
        }
        list
    }

    /// The files, newest first.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Whether no file is listed.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// The file of menu entry `index` (0 = newest), if there is one.
    pub fn get(&self, index: usize) -> Option<&Path> {
        self.paths.get(index).map(PathBuf::as_path)
    }

    /// Puts `path` at the front: either new, or moved up from further
    /// down. The oldest file drops off when the list is full. Empty paths
    /// are ignored.
    ///
    /// Callers should pass an absolute path, so that the same file opened
    /// from different working directories is listed once.
    pub fn add(&mut self, path: PathBuf) {
        if path.as_os_str().is_empty() {
            return;
        }
        self.paths.retain(|p| *p != path);
        self.paths.insert(0, path);
        self.paths.truncate(MAX_FILES);
    }

    /// Takes `path` off the list (e.g. because it no longer exists).
    /// Returns whether it was listed.
    pub fn remove(&mut self, path: &Path) -> bool {
        let before = self.paths.len();
        self.paths.retain(|p| p != path);
        self.paths.len() != before
    }

    /// Empties the list.
    pub fn clear(&mut self) {
        self.paths.clear();
    }

    /// The submenu's items, newest first.
    ///
    /// A label is `&N name`: the file name alone keeps the menu narrow.
    /// Where two listed files have the same name (the same recording in
    /// different folders, a common case with "recording.m4a"), both show
    /// their full path instead, so they can be told apart.
    pub fn menu_entries(&self) -> Vec<MenuEntry> {
        let name = |p: &Path| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string_lossy().into_owned())
        };
        self.paths
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let short = name(p);
                let ambiguous = self
                    .paths
                    .iter()
                    .filter(|q| *q != p)
                    .any(|q| name(q) == short);
                let shown = if ambiguous {
                    p.to_string_lossy().into_owned()
                } else {
                    short
                };
                MenuEntry {
                    label: format!("&{} {}", i + 1, escape_mnemonics(&shown)),
                    help: p.to_string_lossy().into_owned(),
                }
            })
            .collect()
    }
}

/// Escapes `&` (which marks a mnemonic in wxWidgets menu labels) and
/// replaces tabs (which start the accelerator part of a label).
fn escape_mnemonics(s: &str) -> String {
    s.replace('&', "&&").replace('\t', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn newest_first_without_duplicates() {
        let mut r = RecentFiles::default();
        r.add(p("/a/one.mp3"));
        r.add(p("/a/two.flac"));
        r.add(p("/a/one.mp3"));
        assert_eq!(r.paths(), [p("/a/one.mp3"), p("/a/two.flac")]);
        assert_eq!(r.get(1), Some(Path::new("/a/two.flac")));
        assert_eq!(r.get(2), None);
    }

    #[test]
    fn oldest_drops_off_when_full() {
        let mut r = RecentFiles::default();
        for i in 0..=MAX_FILES {
            r.add(p(&format!("/f{i}.wav")));
        }
        assert_eq!(r.paths().len(), MAX_FILES);
        assert_eq!(r.get(0), Some(Path::new("/f9.wav")));
        // f0 was the oldest.
        assert!(!r.paths().contains(&p("/f0.wav")));
    }

    #[test]
    fn empty_paths_are_ignored() {
        let mut r = RecentFiles::default();
        r.add(PathBuf::new());
        assert!(r.is_empty());
    }

    #[test]
    fn remove_and_clear() {
        let mut r = RecentFiles::from_paths([p("/x.ogg"), p("/y.ogg")]);
        assert!(r.remove(Path::new("/x.ogg")));
        assert!(!r.remove(Path::new("/x.ogg")));
        assert_eq!(r.paths(), [p("/y.ogg")]);
        r.clear();
        assert!(r.is_empty());
    }

    #[test]
    fn from_paths_keeps_order_and_repairs_bad_stores() {
        let stored: Vec<PathBuf> = [
            "/new.mp3", "/mid.mp3", "/new.mp3", "/old.mp3", "/1", "/2", "/3", "/4", "/5", "/6",
            "/7",
        ]
        .iter()
        .map(|s| p(s))
        .collect();
        let r = RecentFiles::from_paths(stored);
        assert_eq!(r.paths().len(), MAX_FILES);
        // The first occurrence of a duplicate wins: it was the newer one.
        assert_eq!(
            &r.paths()[..3],
            [p("/new.mp3"), p("/mid.mp3"), p("/old.mp3")]
        );
        assert_eq!(r.get(8), Some(Path::new("/6")));
    }

    #[test]
    fn labels_have_mnemonics_and_escape_ampersands() {
        let r = RecentFiles::from_paths([p("/rec/Tom & Jerry.m4a"), p("/rec/news.mp3")]);
        let e = r.menu_entries();
        assert_eq!(e[0].label, "&1 Tom && Jerry.m4a");
        assert_eq!(e[0].help, "/rec/Tom & Jerry.m4a");
        assert_eq!(e[1].label, "&2 news.mp3");
    }

    #[test]
    fn same_names_show_full_paths() {
        let r = RecentFiles::from_paths([
            p("/mon/recording.m4a"),
            p("/tue/recording.m4a"),
            p("/tue/other.m4a"),
        ]);
        let labels: Vec<String> = r.menu_entries().into_iter().map(|e| e.label).collect();
        assert_eq!(
            labels,
            [
                "&1 /mon/recording.m4a",
                "&2 /tue/recording.m4a",
                "&3 other.m4a"
            ]
        );
    }
}
