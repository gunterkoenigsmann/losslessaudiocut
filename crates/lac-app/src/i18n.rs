//! Translations of the user interface (issue #103).
//!
//! The texts are English in the code and marked with [`tr`] (or [`trf`]
//! for texts with `{name}` placeholders). Translations are gettext `.po`
//! files in `crates/lac-app/po/`, built into the program, so nothing has
//! to be installed next to it and every platform behaves the same. The
//! `.po` format is what translators and their tools know; this module
//! reads the subset we need (`msgid`, `msgstr`, continued lines,
//! escapes, comments, the `fuzzy` flag). [`extract`] collects the marked
//! texts into the template `po/messages.pot`, which a weekly workflow
//! merges into the `.po` files.
//!
//! [`set_language`] picks a catalog once at start-up; until then, and for
//! texts a catalog lacks, [`tr`] returns the English text. The GUI also
//! loads wxWidgets' own catalog for its built-in texts (file dialogs,
//! standard buttons), which is installed with wxWidgets where available.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Display;
use std::sync::RwLock;

use thiserror::Error;

pub mod extract;

/// The translations built into the program: language code and `.po`
/// text. English needs none.
const CATALOGS: &[(&str, &str)] = &[("de", include_str!("../po/de.po"))];

/// The languages the user can choose: code and name in that language.
/// English first, as it is the language of the code.
pub const LANGUAGES: &[(&str, &str)] = &[("en", "English"), ("de", "Deutsch")];

/// The code in [`LANGUAGES`] of `stored` (a code from the config store);
/// `None` for anything else, which means the system's language.
pub fn language_code(stored: &str) -> Option<&'static str> {
    LANGUAGES.iter().map(|(c, _)| *c).find(|c| *c == stored)
}

/// The catalog in use, if any.
static CURRENT: RwLock<Option<Catalog>> = RwLock::new(None);

/// A malformed `.po` file.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PoError {
    /// A line that is neither a keyword, a continued string nor a comment.
    #[error("line {line}: cannot read {text:?}")]
    Syntax {
        /// 1-based line number.
        line: usize,
        /// The line.
        text: String,
    },
}

/// The translations of one language: English text to translated text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    messages: HashMap<String, String>,
}

impl Catalog {
    /// Reads a `.po` file. Entries without a translation (empty
    /// `msgstr`), the header (empty `msgid`) and plural forms are left
    /// out.
    pub fn parse(po: &str) -> Result<Catalog, PoError> {
        // Which string a continued line ("...") belongs to.
        #[derive(PartialEq)]
        enum Field {
            None,
            Id,
            Str,
            Other,
        }
        let mut messages = HashMap::new();
        let (mut id, mut text) = (String::new(), String::new());
        let mut field = Field::None;
        // `#, fuzzy`: msgmerge guessed the translation from a similar
        // text; until a translator checks it, English is safer.
        let mut fuzzy = false;
        let mut flush = |id: &mut String, text: &mut String, fuzzy: bool| {
            if !id.is_empty() && !text.is_empty() && !fuzzy {
                messages.insert(std::mem::take(id), std::mem::take(text));
            }
            id.clear();
            text.clear();
        };
        for (n, raw) in po.lines().enumerate() {
            let line = raw.trim();
            let syntax = || PoError::Syntax {
                line: n + 1,
                text: raw.to_string(),
            };
            if line.starts_with("#,") && line.contains("fuzzy") {
                // Comments come before their entry: end the last one.
                flush(&mut id, &mut text, fuzzy);
                fuzzy = true;
                field = Field::None;
                continue;
            }
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("msgid ") {
                if field != Field::None {
                    flush(&mut id, &mut text, fuzzy);
                    fuzzy = false;
                }
                id = unquote(rest).ok_or_else(syntax)?;
                field = Field::Id;
            } else if let Some(rest) = line.strip_prefix("msgstr ") {
                text = unquote(rest).ok_or_else(syntax)?;
                field = Field::Str;
            } else if line.starts_with("msgctxt ")
                || line.starts_with("msgid_plural ")
                || line.starts_with("msgstr[")
            {
                // Not used by this program; skipped with their
                // continuations. A plural entry thus has no `msgstr`.
                field = Field::Other;
            } else if line.starts_with('"') {
                let more = unquote(line).ok_or_else(syntax)?;
                match field {
                    Field::Id => id += &more,
                    Field::Str => text += &more,
                    Field::Other => {}
                    Field::None => return Err(syntax()),
                }
            } else {
                return Err(syntax());
            }
        }
        flush(&mut id, &mut text, fuzzy);
        Ok(Catalog { messages })
    }

    /// The translation of `english`, if the catalog has one.
    pub fn get(&self, english: &str) -> Option<&str> {
        self.messages.get(english).map(String::as_str)
    }

    /// The English texts the catalog translates.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.messages.keys().map(String::as_str)
    }
}

/// The contents of a quoted `.po` string (`"..."`) with its escapes
/// resolved; `None` if it is not one.
fn unquote(s: &str) -> Option<String> {
    let inner = s.trim().strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            // An unescaped quote inside the string is malformed.
            if c == '"' {
                return None;
            }
            out.push(c);
            continue;
        }
        out.push(match chars.next()? {
            'n' => '\n',
            't' => '\t',
            '"' => '"',
            '\\' => '\\',
            _ => return None,
        });
    }
    Some(out)
}

/// The built-in catalog for language `code` (`"de"`, or a full locale
/// such as `"de_DE.UTF-8"`, of which the language part counts).
pub fn catalog(code: &str) -> Option<Catalog> {
    let lang = code.split(['_', '-', '.']).next().unwrap_or(code);
    let (_, po) = CATALOGS.iter().find(|(c, _)| *c == lang)?;
    // The built-in catalogs are checked by the tests; a broken one would
    // leave the program in English rather than fail.
    Catalog::parse(po).ok()
}

/// Uses language `code` from now on; `None` or a language without a
/// catalog means English. Returns whether a catalog was found.
pub fn set_language(code: Option<&str>) -> bool {
    let cat = code.and_then(catalog);
    let found = cat.is_some();
    match CURRENT.write() {
        Ok(mut c) => *c = cat,
        Err(e) => *e.into_inner() = cat,
    }
    found
}

/// `english` in the chosen language, or as it is.
pub fn tr(english: &str) -> Cow<'_, str> {
    let current = CURRENT.read().unwrap_or_else(|e| e.into_inner());
    translate(current.as_ref(), english)
}

/// [`tr`] of `template`, with each `{name}` replaced by its value from
/// `args`. The placeholders keep their names in the translation, so a
/// language can put them in another order.
pub fn trf(template: &str, args: &[(&str, &dyn Display)]) -> String {
    fill(&tr(template), args)
}

/// A menu label translated without its accelerator: `"&Open…\tCtrl+O"`
/// is looked up as `"&Open…"`, so translators cannot break the key, and
/// the key stays the same in every language.
pub fn tr_menu(label: &str) -> String {
    match label.split_once('\t') {
        Some((text, key)) => format!("{}\t{key}", tr(text)),
        None => tr(label).into_owned(),
    }
}

/// Marks `english` for translation where it is defined (a table of
/// labels, a constant) without translating it there; the code that shows
/// it passes it to [`tr`]. Like gettext's `N_`.
pub const fn n_(english: &'static str) -> &'static str {
    english
}

/// `english` translated by `catalog`, or as it is.
fn translate<'a>(catalog: Option<&Catalog>, english: &'a str) -> Cow<'a, str> {
    match catalog.and_then(|c| c.get(english)) {
        Some(t) => Cow::Owned(t.to_string()),
        None => Cow::Borrowed(english),
    }
}

/// `template` with each `{name}` replaced by its value from `args`.
fn fill(template: &str, args: &[(&str, &dyn Display)]) -> String {
    let mut s = template.to_string();
    for (name, value) in args {
        s = s.replace(&format!("{{{name}}}"), &value.to_string());
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const PO: &str = r#"# A comment
msgid ""
msgstr ""
"Content-Type: text/plain; charset=UTF-8\n"

#: main.rs:1
msgid "Open"
msgstr "Öffnen"

msgid "Zoom to track {n}"
msgstr "Auf Spur {n} zoomen"

msgid ""
"Two "
"lines\n"
msgstr "Zwei \"Zeilen\"\n"

msgid "Untranslated"
msgstr ""

#: main.rs:2
#, fuzzy
msgid "Close it"
msgstr "Schließen"

#~ msgid "Gone"
#~ msgstr "Weg"

msgid "file"
msgid_plural "files"
msgstr[0] "Datei"
msgstr[1] "Dateien"
"#;

    #[test]
    fn reads_entries_continuations_and_escapes() {
        let c = Catalog::parse(PO).expect("valid");
        assert_eq!(c.get("Open"), Some("Öffnen"));
        assert_eq!(c.get("Two lines\n"), Some("Zwei \"Zeilen\"\n"));
        assert_eq!(c.get("Untranslated"), None, "empty translation");
        assert_eq!(c.get("Close it"), None, "fuzzy: not checked yet");
        assert_eq!(c.get("Gone"), None, "obsolete");
        assert_eq!(c.get(""), None, "the header is no message");
        assert_eq!(c.get("file"), None, "plurals are not used");
        assert_eq!(c.ids().count(), 3);
    }

    #[test]
    fn malformed_files_are_errors_not_panics() {
        for bad in [
            "msgid Open",
            "msgid \"a\"\nmsgstr \"b\\q\"",
            "\"orphan\"",
            "garbage",
            "msgid \"a\"b\"",
        ] {
            assert!(Catalog::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn only_offered_languages_are_kept() {
        assert_eq!(language_code("de"), Some("de"));
        assert_eq!(language_code("en"), Some("en"));
        assert_eq!(language_code(""), None, "the system's language");
        assert_eq!(language_code("klingon"), None);
    }

    #[test]
    fn locales_find_their_language() {
        assert!(catalog("de").is_some());
        assert!(catalog("de_DE.UTF-8").is_some());
        assert!(catalog("de-AT").is_some());
        assert!(catalog("en").is_none(), "English needs no catalog");
        assert!(catalog("xx").is_none());
    }

    /// Every built-in catalog reads without errors and keeps the
    /// placeholders of each text it translates.
    #[test]
    fn built_in_catalogs_are_valid() {
        let placeholders = |s: &str| {
            let mut v: Vec<String> = s
                .split('{')
                .skip(1)
                .filter_map(|p| p.split_once('}').map(|(n, _)| n.to_string()))
                .collect();
            v.sort();
            v
        };
        for (code, po) in CATALOGS {
            let c = Catalog::parse(po).unwrap_or_else(|e| panic!("{code}: {e}"));
            for id in c.ids() {
                let t = c.get(id).unwrap_or_default();
                assert_eq!(placeholders(id), placeholders(t), "{code}: {id:?} -> {t:?}");
            }
            assert!(
                LANGUAGES.iter().any(|(l, _)| l == code),
                "{code} is offered"
            );
        }
    }

    /// The pure parts of [`tr`] and [`trf`]: the chosen language is
    /// global to the process, and other tests run in parallel expect
    /// English, so the tests leave it alone.
    #[test]
    fn texts_are_translated_and_filled_in() {
        let c = Catalog::parse(PO).expect("valid");
        assert_eq!(translate(Some(&c), "Open"), "Öffnen");
        assert_eq!(translate(Some(&c), "No such text"), "No such text");
        assert_eq!(translate(None, "Open"), "Open");
        let t = translate(Some(&c), "Zoom to track {n}");
        assert_eq!(fill(&t, &[("n", &3)]), "Auf Spur 3 zoomen");
        assert_eq!(
            fill("{a} and {a}, {b}", &[("a", &1), ("b", &"x")]),
            "1 and 1, x"
        );
        // English stays English.
        assert!(!set_language(None));
        assert_eq!(tr("Open"), "Open");
    }
}
