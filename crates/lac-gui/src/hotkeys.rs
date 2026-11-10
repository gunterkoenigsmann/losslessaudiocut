//! Checks that keep the rule of issue #99: every keyboard shortcut comes
//! from a menu item (its `\tCtrl+X` part), so it is translated with the
//! menu, shown next to the command and found without the manual. A toolbar
//! button or control with a shortcut names the menu item's.
//!
//! The checks read the GUI's own source, so a new file is covered without
//! being listed here.

use std::collections::BTreeMap;
use std::path::PathBuf;

use lac_app::perspective::Perspective;

/// Window events that would bind a key outside any menu.
const RAW_KEY_HANDLERS: [&str; 6] = [
    "on_key_down(",
    "on_key_up(",
    "on_char(",
    "on_char_hook(",
    "AcceleratorTable",
    "AcceleratorEntry",
];

/// Every source file of the GUI, with its name and text.
fn sources() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("src is readable")
        .map(|e| e.expect("directory entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .map(|p| {
            let name = p
                .file_name()
                .expect("a file has a name")
                .to_string_lossy()
                .into_owned();
            (
                name,
                std::fs::read_to_string(&p).expect("source is readable"),
            )
        })
        .collect();
    files.sort();
    files
}

/// The shortcut of a menu label's `\t` part, in one spelling: modifiers
/// in a fixed order and everything upper case, so "Shift+Ctrl+k" and
/// "Ctrl+Shift+K" are the same.
fn normalized(accel: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    // "Ctrl++" is Ctrl and the plus key.
    let (mods, key) = match accel.strip_suffix("++") {
        Some(m) => (m, "+"),
        None => accel.rsplit_once('+').unwrap_or(("", accel)),
    };
    for m in ["CTRL", "ALT", "SHIFT"] {
        if mods.split('+').any(|p| p.eq_ignore_ascii_case(m)) {
            parts.push(m.to_string());
        }
    }
    parts.push(key.to_ascii_uppercase());
    parts.join("+")
}

/// The shortcuts in `text`: the parts after `\t` in string literals, up
/// to the closing quote. A `{}` (the perspectives' numbers) stands for
/// 1 to the number of perspectives.
fn shortcuts(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (i, _) in text.match_indices("\\t") {
        let rest = &text[i + 2..];
        let Some(end) = rest.find('"') else { continue };
        let accel = &rest[..end];
        // Escaped tabs in comments or format strings without a key are
        // not shortcuts.
        if accel.is_empty() || accel.contains(' ') {
            continue;
        }
        if let Some(prefix) = accel.strip_suffix("{}") {
            for n in 1..=Perspective::ALL.len() {
                found.push(normalized(&format!("{prefix}{n}")));
            }
        } else {
            found.push(normalized(accel));
        }
    }
    found
}

/// No window catches keys itself: shortcuts go through menus.
#[test]
fn no_key_is_bound_outside_the_menus() {
    for (name, text) in sources() {
        if name == "hotkeys.rs" {
            continue;
        }
        for handler in RAW_KEY_HANDLERS {
            assert!(
                !text.contains(handler),
                "{name} uses {handler}: give the key to a menu item instead (issue #99)"
            );
        }
    }
}

/// No two menu items share a shortcut (only the first one would work).
#[test]
fn menu_shortcuts_are_distinct() {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for (name, text) in sources() {
        if name == "hotkeys.rs" {
            continue;
        }
        for s in shortcuts(&text) {
            if let Some(first) = seen.insert(s.clone(), name.clone()) {
                panic!("{s} is used in {first} and again in {name}");
            }
        }
    }
    // The menus have shortcuts at all, so the scan found them.
    for expected in ["CTRL+O", "SPACE", "DEL", "CTRL++", "CTRL+1", "CTRL+L"] {
        assert!(
            seen.contains_key(expected),
            "{expected} not found: {seen:?}"
        );
    }
}

#[test]
fn shortcut_spellings_are_normalized() {
    assert_eq!(normalized("Shift+Ctrl+k"), "CTRL+SHIFT+K");
    assert_eq!(normalized("Ctrl++"), "CTRL++");
    assert_eq!(normalized("Ctrl+-"), "CTRL+-");
    assert_eq!(normalized("Del"), "DEL");
    assert_eq!(normalized("Ctrl+Alt+E"), "CTRL+ALT+E");
    assert_eq!(
        shortcuts(r#"x("&Open…\tCtrl+O") y("{}\tCtrl+{}")"#).len(),
        1 + Perspective::ALL.len()
    );
}
