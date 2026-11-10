//! The audio file types the program opens.
//!
//! One list for the GUI's open dialog and for the desktop integration of
//! the packages: the Linux desktop files' `MimeType=`, the macOS
//! `Info.plist` document types and the Windows installer's "Open with"
//! entries. Tests of the packaging files compare them with this list, so
//! a new format only has to be added here (and to those files, which the
//! tests then demand).
//!
//! The program registers as *a* handler, never as the default one: it is
//! offered under "Open with", and only becomes the default for a type that
//! nothing else on the system opens.

use crate::i18n::{n_, tr};

/// One kind of file the program opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileType {
    /// What the type is called in dialogs and in the macOS document types.
    pub name: &'static str,
    /// File name extensions, lower case and without the dot.
    pub extensions: &'static [&'static str],
    /// MIME types as freedesktop.org's shared-mime-info names them; the
    /// Linux desktop files list them in `MimeType=`. Only types that are
    /// audio by definition: Linux tells WebM audio (`audio/webm`) from
    /// WebM video by the content.
    pub mime_types: &'static [&'static str],
    /// Whether Windows and macOS, which associate by extension alone,
    /// offer the program for these extensions. False for containers that
    /// usually hold video (`.webm`, `.mkv`): an audio cutter in the "Open
    /// with" menu of every film would be a nuisance. They still open from
    /// the program's own dialog.
    pub by_extension: bool,
    /// The macOS uniform type identifier (UTI) of the type. System-defined
    /// where macOS has one (`public.mp3`), otherwise declared by our
    /// `Info.plist` as an imported type.
    pub uti: &'static str,
}

/// Everything the program opens, in the order of the open dialog.
pub const FILE_TYPES: &[FileType] = &[
    FileType {
        name: n_("MP3 audio"),
        extensions: &["mp3"],
        mime_types: &["audio/mpeg"],
        by_extension: true,
        uti: "public.mp3",
    },
    FileType {
        name: n_("MPEG-4 audio (AAC, ALAC)"),
        extensions: &["m4a", "m4b"],
        // audio/x-m4a is the older name, still used by some systems.
        mime_types: &["audio/mp4", "audio/x-m4a"],
        by_extension: true,
        // System-defined; it covers .m4a and .m4b.
        uti: "public.mpeg-4-audio",
    },
    FileType {
        name: n_("AC-3 audio"),
        extensions: &["ac3"],
        mime_types: &["audio/ac3"],
        by_extension: true,
        // System-defined (AVFoundation's AVFileTypeAC3).
        uti: "public.ac3-audio",
    },
    FileType {
        name: n_("AAC audio (ADTS)"),
        extensions: &["aac"],
        // audio/x-aac is the older name, still used by some systems.
        mime_types: &["audio/aac", "audio/x-aac"],
        by_extension: true,
        // System-defined.
        uti: "public.aac-audio",
    },
    FileType {
        name: n_("Ogg Vorbis audio"),
        extensions: &["ogg", "oga"],
        mime_types: &["audio/ogg", "audio/x-vorbis+ogg"],
        by_extension: true,
        uti: "org.xiph.ogg-audio",
    },
    FileType {
        name: n_("Opus audio"),
        extensions: &["opus"],
        mime_types: &["audio/x-opus+ogg"],
        by_extension: true,
        uti: "org.xiph.opus",
    },
    FileType {
        name: n_("Speex audio"),
        extensions: &["spx"],
        mime_types: &["audio/x-speex", "audio/x-speex+ogg"],
        by_extension: true,
        uti: "org.xiph.speex",
    },
    FileType {
        name: n_("FLAC audio"),
        extensions: &["flac"],
        // audio/x-flac is the older name, still used by some systems.
        mime_types: &["audio/flac", "audio/x-flac"],
        by_extension: true,
        uti: "org.xiph.flac",
    },
    FileType {
        name: n_("WAV audio"),
        extensions: &["wav"],
        mime_types: &["audio/x-wav", "audio/wav", "audio/vnd.wave"],
        by_extension: true,
        uti: "com.microsoft.waveform-audio",
    },
    FileType {
        name: n_("AIFF audio"),
        extensions: &["aiff", "aif", "aifc"],
        // shared-mime-info's names; audio/aiff is an alias of the first.
        mime_types: &["audio/x-aiff", "audio/x-aifc"],
        by_extension: true,
        // System-defined; macOS's public.aifc-audio conforms to it.
        uti: "public.aiff-audio",
    },
    FileType {
        name: n_("Matroska audio"),
        extensions: &["mka"],
        mime_types: &["audio/x-matroska"],
        by_extension: true,
        uti: "org.matroska.mka",
    },
    FileType {
        name: n_("WebM audio"),
        extensions: &["webm"],
        mime_types: &["audio/webm"],
        by_extension: false,
        uti: "org.webmproject.webm",
    },
    FileType {
        name: n_("Matroska video (audio track only)"),
        extensions: &["mkv"],
        // video/x-matroska would offer the program for every film.
        mime_types: &[],
        by_extension: false,
        uti: "org.matroska.mkv",
    },
    FileType {
        name: n_("MP4 video (audio track only)"),
        extensions: &["mp4"],
        // video/mp4 would offer the program for every film.
        mime_types: &[],
        by_extension: false,
        uti: "public.mpeg-4",
    },
];

/// All MIME types, in the form of a desktop file's `MimeType=` value
/// (each followed by `;`).
pub fn desktop_mime_types() -> String {
    FILE_TYPES
        .iter()
        .flat_map(|t| t.mime_types)
        .map(|m| format!("{m};"))
        .collect()
}

/// The extensions that Windows and macOS associate with the program.
pub fn associated_extensions() -> Vec<&'static str> {
    FILE_TYPES
        .iter()
        .filter(|t| t.by_extension)
        .flat_map(|t| t.extensions.iter().copied())
        .collect()
}

/// The wildcard of the open dialog (wxWidgets syntax: pairs of
/// `description|patterns`, separated by `|`): all supported files and
/// projects first, then each type, projects, then everything.
pub fn dialog_wildcard() -> String {
    let patterns = |t: &FileType| {
        t.extensions
            .iter()
            .map(|e| format!("*.{e}"))
            .collect::<Vec<_>>()
            .join(";")
    };
    let all: Vec<String> = FILE_TYPES.iter().map(patterns).collect();
    // Projects (`crate::project`) open like recordings.
    let project = format!("*.{}", crate::project::EXTENSION);
    let mut w = format!(
        "{}|{};{project}",
        tr("Audio files and projects"),
        all.join(";")
    );
    for t in FILE_TYPES {
        w.push_str(&format!("|{}|{}", tr(t.name), patterns(t)));
    }
    w.push_str(&format!("|{}|{project}", tr("losslessaudiocut projects")));
    w.push_str(&format!("|{}|*", tr("All files")));
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_are_unique_lower_case_and_dotless() {
        let mut seen = std::collections::HashSet::new();
        for t in FILE_TYPES {
            assert!(!t.extensions.is_empty(), "{}", t.name);
            for e in t.extensions {
                assert_eq!(*e, e.to_lowercase());
                assert!(!e.starts_with('.') && !e.is_empty());
                assert!(seen.insert(*e), "{e} listed twice");
            }
        }
    }

    #[test]
    fn only_audio_mime_types_are_claimed() {
        for t in FILE_TYPES {
            for m in t.mime_types {
                assert!(m.starts_with("audio/"), "{m} of {}", t.name);
            }
        }
        assert!(desktop_mime_types().starts_with(
            "audio/mpeg;audio/mp4;audio/x-m4a;audio/ac3;audio/aac;audio/x-aac;audio/ogg;"
        ));
        assert!(desktop_mime_types().ends_with("audio/webm;"));
    }

    #[test]
    fn video_containers_are_not_associated_by_extension() {
        let e = associated_extensions();
        assert!(e.contains(&"mp3") && e.contains(&"opus") && e.contains(&"mka"));
        assert!(!e.contains(&"webm") && !e.contains(&"mkv") && !e.contains(&"mp4"));
    }

    #[test]
    fn wildcard_lists_everything_first() {
        let w = dialog_wildcard();
        assert!(w.starts_with(
            "Audio files and projects|*.mp3;*.m4a;*.m4b;*.ac3;*.aac;*.ogg;*.oga;*.opus;*.spx;*.flac;*.wav;*.aiff;*.aif;*.aifc;*.mka;*.webm;*.mkv;*.mp4;*.lacproj|MP3 audio|*.mp3|"
        ));
        assert!(w.ends_with("|losslessaudiocut projects|*.lacproj|All files|*"));
        // Description and pattern alternate, so the count is even.
        assert_eq!(w.split('|').count() % 2, 0);
    }
}
