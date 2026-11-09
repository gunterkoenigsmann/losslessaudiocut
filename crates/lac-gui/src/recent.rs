//! File > Open recent: the submenu of recently opened files and where
//! the list is stored between runs.
//!
//! The list itself is [`lac_app::recent_files::RecentFiles`]; this module
//! only stores it in the config store (as `/RecentFiles/File1` …
//! `File9`, the layout `wxFileHistory` would use) and rebuilds the
//! submenu from it.

use std::path::PathBuf;

use lac_app::recent_files::{MAX_FILES, RecentFiles};
use wxdragon::prelude::*;

use crate::options::config;

/// Config group of the list.
const GROUP: &str = "/RecentFiles";

/// Menu id of the newest file; the others follow, up to
/// `ID_FIRST + MAX_FILES - 1`. Below the panes' ids (`ID_HIGHEST + 100`).
pub const ID_FIRST: Id = ID_HIGHEST + 60;
/// Menu id of "Clear list".
pub const ID_CLEAR: Id = ID_HIGHEST + 59;

/// Config key of entry `index` (0 = newest).
fn key(index: usize) -> String {
    format!("{GROUP}/File{}", index + 1)
}

/// The stored list (empty if there is none).
pub fn load() -> RecentFiles {
    let c = config();
    let paths = (0..MAX_FILES)
        .map(|i| c.read_string(&key(i), ""))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    RecentFiles::from_paths(paths)
}

/// Stores the list. As with the options, failing to store it only means
/// it is not remembered.
pub fn save(list: &RecentFiles) {
    let c = config();
    for i in 0..MAX_FILES {
        match list.get(i) {
            Some(p) => {
                c.write_string(&key(i), &p.to_string_lossy());
            }
            None => {
                c.delete_entry(&key(i), true);
            }
        }
    }
    c.flush(false);
}

/// The empty submenu; [`refresh`] fills it.
pub fn submenu() -> Menu {
    Menu::builder()
        .append_separator()
        .append_item(ID_CLEAR, "&Clear list", "Forget the recently opened files")
        .build()
}

/// The index into the list of a menu id of [`submenu`], if it is one of
/// the file entries.
pub fn index_of(id: Id) -> Option<usize> {
    let i = usize::try_from(id - ID_FIRST).ok()?;
    (i < MAX_FILES).then_some(i)
}

/// Rebuilds the file entries of the submenu in `bar` from `list`.
pub fn refresh(bar: &MenuBar, list: &RecentFiles) {
    // The submenu is the menu holding "Clear list"; there is no need to
    // keep a handle of it.
    let Some((_, menu)) = bar.find_item_and_menu(ID_CLEAR) else {
        return;
    };
    for i in 0..MAX_FILES {
        let id = ID_FIRST + i as Id;
        if menu.find_item(id).is_some() {
            menu.delete(id);
        }
    }
    for (i, e) in list.menu_entries().iter().enumerate() {
        menu.insert(i, ID_FIRST + i as Id, &e.label, &e.help, ItemKind::Normal);
    }
    menu.enable_item(ID_CLEAR, !list.is_empty());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_map_to_indices_and_stay_clear_of_other_commands() {
        assert_eq!(index_of(ID_FIRST), Some(0));
        assert_eq!(
            index_of(ID_FIRST + MAX_FILES as Id - 1),
            Some(MAX_FILES - 1)
        );
        assert_eq!(index_of(ID_FIRST + MAX_FILES as Id), None);
        assert_eq!(index_of(ID_CLEAR), None);
        // The panes' ids start at ID_HIGHEST + 100.
        assert!(ID_FIRST + (MAX_FILES as Id) < ID_HIGHEST + 100);
    }

    #[test]
    fn keys_are_numbered_from_one() {
        assert_eq!(key(0), "/RecentFiles/File1");
        assert_eq!(key(8), "/RecentFiles/File9");
    }
}
