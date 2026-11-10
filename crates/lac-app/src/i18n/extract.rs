//! Finds the texts marked for translation in the sources and writes them
//! as a gettext template (`po/messages.pot`), like `xgettext` does for C.
//!
//! The template is not kept in step by hand or by a test: the weekly
//! workflow `.github/workflows/translations.yml` runs
//! `cargo run -p lac-app --example update_pot`, merges the template into
//! each `.po` file with gettext's `msgmerge` (which keeps the translations,
//! marks texts that changed a little as fuzzy and moves removed ones to the
//! end as obsolete) and offers the result as a pull request. Code changes
//! thus never conflict in the `.po` files, and a text without a
//! translation simply shows in English.

use std::path::{Path, PathBuf};

/// The source folders, relative to `crates/`, whose texts are shown to
/// the user.
pub const SOURCE_DIRS: &[&str] = &["lac-app/src", "lac-gui/src"];

/// The functions that mark a text: the first argument is a string
/// literal to translate. Called with anything else (`tr(label)`), they
/// translate a text marked elsewhere.
const MARKERS: &[&str] = &["tr(", "trf(", "tr_menu(", "n_("];

/// A text to translate and where it is used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The English text, as `tr` looks it up.
    pub id: String,
    /// The places that use it (`path:line`), for translators.
    pub refs: Vec<String>,
}

/// A literal after a marker that is no plain string literal (e.g. a raw
/// string), so its text cannot be known.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: cannot read the marked text {start:?}")]
pub struct Unreadable {
    /// 1-based line number.
    pub line: usize,
    /// The start of the literal.
    pub start: String,
}

/// The texts marked in Rust source `source`, with their 1-based line
/// numbers, in the order they appear. Comment lines and the unit tests
/// (everything after `#[cfg(test)]` + `mod tests`) are skipped: tests may
/// use texts nobody sees. A label for [`crate::i18n::tr_menu`] is looked
/// up without its accelerator, so that is cut off.
pub fn marked(source: &str) -> Result<Vec<(String, usize)>, Unreadable> {
    let code = source
        .split("#[cfg(test)]\nmod tests")
        .next()
        .unwrap_or(source);
    // Comment lines become empty, so line numbers stay right.
    let code: String = code
        .lines()
        .map(|l| {
            if l.trim_start().starts_with("//") {
                ""
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = Vec::new();
    for marker in MARKERS {
        let mut from = 0;
        while let Some(i) = code[from..].find(marker) {
            let at = from + i;
            from = at + marker.len();
            let before = code[..at].chars().next_back();
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                continue; // another function, e.g. `attr("`
            }
            // rustfmt may put the literal on the next line.
            let rest = code[from..].trim_start();
            let Some(rest) = rest.strip_prefix('"') else {
                continue;
            };
            let line = code[..code.len() - rest.len()].matches('\n').count() + 1;
            let text = rust_literal(rest).ok_or_else(|| Unreadable {
                line,
                start: rest.chars().take(40).collect(),
            })?;
            let text = match (*marker, text.split_once('\t')) {
                ("tr_menu(", Some((label, _))) => label.to_string(),
                _ => text,
            };
            out.push((text, line));
        }
    }
    out.sort_by_key(|(_, line)| *line);
    Ok(out)
}

/// The value of the Rust string literal whose body starts `s` (after the
/// opening quote); `None` for an escape this does not know.
fn rust_literal(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                '0' => out.push('\0'),
                c @ ('"' | '\\' | '\'') => out.push(c),
                'u' => {
                    // \u{XXXX}
                    let hex: String = chars.by_ref().skip(1).take_while(|&c| c != '}').collect();
                    out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                }
                // A line continuation drops the line break and the
                // indentation of the next line.
                '\n' => {
                    while chars.peek().is_some_and(|c| c.is_whitespace()) {
                        chars.next();
                    }
                }
                _ => return None,
            },
            c => out.push(c),
        }
    }
    None
}

/// Every marked text of the sources under `crates` (the workspace's
/// `crates/` folder), each once, in the order first used; plus the tag
/// names, which `lac-core` defines and the GUI translates.
pub fn messages(crates: &Path) -> std::io::Result<Vec<Message>> {
    let mut out: Vec<Message> = Vec::new();
    let mut add = |id: String, place: String| match out.iter_mut().find(|m| m.id == id) {
        Some(m) => m.refs.push(place),
        None => out.push(Message {
            id,
            refs: vec![place],
        }),
    };
    for dir in SOURCE_DIRS {
        let mut files: Vec<PathBuf> = std::fs::read_dir(crates.join(dir))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "rs"))
            .collect();
        files.sort();
        for f in files {
            let text = std::fs::read_to_string(&f)?;
            let name = f.file_name().unwrap_or_default().to_string_lossy();
            let found = marked(&text).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{dir}/{name}: {e}"),
                )
            })?;
            for (id, line) in found {
                add(id, format!("crates/{dir}/{name}:{line}"));
            }
        }
    }
    for f in lac_core::TagField::ALL {
        add(
            f.label().to_string(),
            "crates/lac-core (TagField::label)".into(),
        );
    }
    Ok(out)
}

/// `messages` as a gettext template.
pub fn template(messages: &[Message]) -> String {
    let mut out = String::from(
        "# Texts of losslessaudiocut to translate.\n\
         # Generated by `cargo run -p lac-app --example update_pot`; do not edit.\n\
         msgid \"\"\n\
         msgstr \"\"\n\
         \"Project-Id-Version: losslessaudiocut\\n\"\n\
         \"MIME-Version: 1.0\\n\"\n\
         \"Content-Type: text/plain; charset=UTF-8\\n\"\n\
         \"Content-Transfer-Encoding: 8bit\\n\"\n",
    );
    for m in messages {
        out.push('\n');
        for r in &m.refs {
            out += &format!("#: {r}\n");
        }
        // gettext tools would otherwise take `{n}` for nothing special and
        // `%` for a C format; say neither applies.
        if m.id.contains('%') {
            out += "#, no-c-format\n";
        }
        out += &format!("msgid \"{}\"\nmsgstr \"\"\n", escape(&m.id));
    }
    out
}

/// `s` as the inside of a quoted `.po` string.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Catalog;

    #[test]
    fn marked_texts_are_found() {
        let src = "let a = tr(\"Open\");\n// tr(\"comment\")\nlet b = trf(\n    \"Track {n}\", &[]);\n\
                   let c = tr_menu(\"&Quit\\tCtrl+Q\");\nlet d = attr(\"no\");\n\
                   const E: &str = n_(\"Long \\\n    text\");\n#[cfg(test)]\nmod tests { tr(\"test\") }";
        let found = marked(src).expect("readable");
        assert_eq!(
            found,
            [
                ("Open".to_string(), 1),
                ("Track {n}".to_string(), 4),
                ("&Quit".to_string(), 5),
                ("Long text".to_string(), 7),
            ]
        );
        assert_eq!(
            marked("tr(\"bad \\q\")").map_err(|e| e.line),
            Err(1),
            "an unknown escape is reported"
        );
    }

    /// The real sources can be read: every marked text is a plain
    /// literal, and the template of them reads back as the same texts.
    #[test]
    fn the_sources_give_a_valid_template() {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let messages = messages(&crates).expect("sources");
        assert!(messages.iter().any(|m| m.id == "&Open…"));
        assert!(messages.iter().any(|m| m.id == "Album"), "tag names");
        // A template has no translations; give each one to read the ids
        // back.
        // The header's text becomes "x" plus its lines, and is skipped.
        let filled = template(&messages).replace("msgstr \"\"\n", "msgstr \"x\"\n");
        let c = Catalog::parse(&filled).expect("valid .po");
        let mut ids: Vec<&str> = c.ids().collect();
        ids.sort_unstable();
        let mut want: Vec<&str> = messages.iter().map(|m| m.id.as_str()).collect();
        want.sort_unstable();
        assert_eq!(ids, want);
    }
}
