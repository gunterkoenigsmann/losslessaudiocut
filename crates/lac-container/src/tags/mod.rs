//! Reading and writing tags in the containers' native formats.
//!
//! Tags live outside the audio packets, so rewriting them never touches
//! audio. Each format module parses its source's tags into a
//! [`lac_core::Tags`] when the file is opened, and writes a given `Tags`
//! value when a cut is exported (see `docs/architecture.md`, "Tags per
//! container"). Everything a [`lac_core::Tags`] cannot hold as text
//! (pictures, binary ID3 frames) is copied from the source unchanged, so
//! editing the title never loses a cover image.
//!
//! - [`vorbis`]: Vorbis comments, used by FLAC and by Ogg Vorbis, Opus and
//!   Speex;
//! - [`id3`]: ID3v2, used by MP3 and inside WAV's `id3 ` chunk;
//! - [`riff`]: the `LIST/INFO` chunk of WAV.
//!
//! Matroska's `Tags` element is handled in [`crate::mkv`], next to the
//! other EBML code.

pub mod id3;
pub mod riff;
pub mod vorbis;

/// Strips trailing NUL bytes, which many taggers append to strings.
pub(crate) fn trim_nul(s: &str) -> &str {
    s.trim_end_matches('\0')
}

/// `Some(s)` for a non-empty `s`.
pub(crate) fn non_empty(s: &str) -> Option<String> {
    let s = trim_nul(s).trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Random [`lac_core::Tags`] as tag readers return them: values trimmed
/// and non-empty, without NUL characters.
#[cfg(test)]
pub(crate) fn arb_tags() -> impl proptest::strategy::Strategy<Value = lac_core::Tags> {
    use proptest::prelude::*;
    let text = || proptest::option::of("[^\\x00\\s][^\\x00]{0,20}[^\\x00\\s]|[^\\x00\\s]");
    let key = "[A-Z][A-Z0-9_]{1,10}";
    (
        (text(), text(), text(), text(), text()),
        (text(), text()),
        (
            proptest::option::of(1u32..999),
            proptest::option::of(1u32..999),
        ),
        proptest::collection::vec((key, "[^\\x00]{1,20}"), 0..4),
    )
        .prop_map(
            |((title, artist, album, album_artist, date), (genre, comment), (n, total), extra)| {
                lac_core::Tags {
                    title,
                    artist,
                    album,
                    album_artist,
                    track_number: n,
                    // A total alone has nowhere to go in ID3's TRCK.
                    track_total: n.and(total),
                    date,
                    genre,
                    comment,
                    extra,
                }
            },
        )
}
