//! The packages' file associations must match [`lac_app::file_types`].
//!
//! The Linux desktop files, the macOS `Info.plist` and the Windows
//! installer script each list the supported types in their own syntax.
//! These tests read them from the repository and compare, so adding a
//! format to `file_types` fails here until every package offers it, and a
//! package cannot claim a type the program does not open.

use std::collections::BTreeSet;
use std::path::PathBuf;

use lac_app::file_types::{FILE_TYPES, associated_extensions, desktop_mime_types};

/// Reads a file relative to the repository root.
fn repo_file(path: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// The value of `key=` in a desktop file.
fn desktop_value<'a>(file: &'a str, key: &str) -> Option<&'a str> {
    file.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
}

#[test]
fn desktop_files_offer_exactly_the_supported_mime_types() {
    for path in [
        "packaging/linux/losslessaudiocut.desktop",
        "snap/gui/losslessaudiocut.desktop",
    ] {
        let f = repo_file(path);
        assert_eq!(
            desktop_value(&f, "MimeType"),
            Some(desktop_mime_types().as_str()),
            "{path}"
        );
        // %f: the file manager passes one file, which the GUI opens.
        let exec = desktop_value(&f, "Exec").expect("Exec=");
        assert!(exec.ends_with(" %f"), "{path}: Exec={exec}");
    }
}

/// The `<string>` values between `start` and the next `</array>`.
fn plist_strings_after<'a>(text: &'a str, start: &str) -> Vec<&'a str> {
    let Some(i) = text.find(start) else {
        return Vec::new();
    };
    let rest = &text[i + start.len()..];
    let block = &rest[..rest.find("</array>").expect("</array>")];
    block
        .split("<string>")
        .skip(1)
        .map(|s| s.split("</string>").next().expect("</string>"))
        .collect()
}

/// UTIs that macOS defines itself, so `Info.plist` does not import them.
const SYSTEM_UTIS: &[&str] = &[
    "public.mp3",
    "com.microsoft.waveform-audio",
    "public.aiff-audio",
    "public.mpeg-4-audio",
    "public.ac3-audio",
    "public.aac-audio",
];

#[test]
fn macos_document_types_match() {
    let plist = repo_file("packaging/macos/Info.plist");
    let associated: Vec<_> = FILE_TYPES.iter().filter(|t| t.by_extension).collect();
    let utis: Vec<&str> = associated.iter().map(|t| t.uti).collect();
    assert_eq!(
        plist_strings_after(&plist, "<key>LSItemContentTypes</key>"),
        utis
    );
    assert!(
        plist.contains("<string>Alternate</string>"),
        "never the default handler"
    );

    // Every type macOS may not know is declared with its extensions and
    // MIME types; nothing else is declared.
    let imports = &plist[plist
        .find("<key>UTImportedTypeDeclarations</key>")
        .expect("imported types")..];
    let declared: Vec<&str> = imports
        .split("<key>UTTypeIdentifier</key>")
        .skip(1)
        .map(|decl| {
            let uti = plist_strings_after(decl, "")[0];
            let t = associated
                .iter()
                .find(|t| t.uti == uti)
                .unwrap_or_else(|| panic!("{uti} is not an associated type"));
            assert_eq!(
                plist_strings_after(decl, "<key>public.filename-extension</key>"),
                t.extensions,
                "{uti}"
            );
            assert_eq!(
                plist_strings_after(decl, "<key>public.mime-type</key>"),
                t.mime_types,
                "{uti}"
            );
            uti
        })
        .collect();
    let expected: Vec<&str> = utis
        .iter()
        .copied()
        .filter(|u| !SYSTEM_UTIS.contains(u))
        .collect();
    assert_eq!(declared, expected);
}

/// The extensions in Inno Setup registry lines that contain `marker`, taken
/// from the text between `before` and the next `\` or `"`.
fn iss_extensions(iss: &str, marker: &str, before: &str) -> BTreeSet<String> {
    iss.lines()
        .filter(|l| l.starts_with("Root:") && l.contains(marker))
        .map(|l| {
            let rest = &l[l.find(before).expect("extension") + before.len()..];
            rest[..rest.find(['\\', '"']).expect("end")].to_string()
        })
        .collect()
}

#[test]
fn windows_installer_offers_exactly_the_associated_extensions() {
    let iss = repo_file("packaging/windows/losslessaudiocut.iss");
    let expected: BTreeSet<String> = associated_extensions()
        .iter()
        .map(|e| e.to_string())
        .collect();
    assert_eq!(
        iss_extensions(&iss, "\\OpenWithProgids\"", "Software\\Classes\\."),
        expected
    );
    assert_eq!(
        iss_extensions(&iss, "\\SupportedTypes\"", "ValueName: \"."),
        expected
    );
    // Only "Open with" entries: no extension's default program changes.
    assert!(!iss.contains("ChangesAssociations=no"));
    for l in iss.lines().filter(|l| l.contains("Software\\Classes\\.")) {
        assert!(l.contains("\\OpenWithProgids\""), "{l}");
    }
}
