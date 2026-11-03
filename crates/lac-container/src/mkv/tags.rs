//! The Matroska `Tags` element (RFC 9559 5.1.8, and the Matroska tagging
//! guidelines at <https://www.matroska.org/technical/tagging.html>).
//!
//! ```text
//! Tags
//!   Tag
//!     Targets      TargetTypeValue (50 = album, the default), TagTrackUID, ...
//!     SimpleTag    TagName, TagLanguage, TagString
//!     SimpleTag    ...
//! ```
//!
//! Only tags about the whole file are read: a `Tag` whose `Targets` name a
//! track, edition, chapter or attachment UID describes something an export
//! does not keep as is (ffmpeg, for instance, stores each track's
//! `DURATION` that way). Names follow the tagging guidelines where they
//! define one (`PART_NUMBER`, `TOTAL_PARTS`, `DATE_RELEASED`) and ffmpeg's
//! global tags otherwise (`ALBUM`, `ALBUM_ARTIST`), all in one `Tag`
//! without targets, which is what ffmpeg writes and reads.

use lac_core::Tags;

use super::{children, ebml};
use crate::tags::non_empty;

/// Element IDs of the `Tags` element (RFC 9559 5.1.8).
mod id {
    pub const TAGS: u32 = 0x1254_C367;
    pub const TAG: u32 = 0x7373;
    pub const TARGETS: u32 = 0x63C0;
    pub const TAG_TRACK_UID: u32 = 0x63C5;
    pub const TAG_EDITION_UID: u32 = 0x63C9;
    pub const TAG_CHAPTER_UID: u32 = 0x63C4;
    pub const TAG_ATTACHMENT_UID: u32 = 0x63C6;
    pub const SIMPLE_TAG: u32 = 0x67C8;
    pub const TAG_NAME: u32 = 0x45A3;
    pub const TAG_STRING: u32 = 0x4487;
}

/// Names that are not copied: they describe the source's length.
const DROPPED: [&str; 2] = ["DURATION", "NUMBER_OF_FRAMES"];

/// A string element's value; strings may be padded with NUL bytes
/// (RFC 8794 7.4).
pub fn string(b: &[u8]) -> String {
    let b = b.split(|&c| c == 0).next().unwrap_or_default();
    String::from_utf8_lossy(b).into_owned()
}

/// Parses the body of a `Tags` element.
pub fn parse(body: &[u8]) -> Tags {
    let mut t = Tags::default();
    for (i, tag) in children(body) {
        if i != id::TAG {
            continue;
        }
        let targeted = children(tag)
            .filter(|&(i, _)| i == id::TARGETS)
            .flat_map(|(_, b)| children(b))
            .any(|(i, _)| {
                matches!(
                    i,
                    id::TAG_TRACK_UID
                        | id::TAG_EDITION_UID
                        | id::TAG_CHAPTER_UID
                        | id::TAG_ATTACHMENT_UID
                )
            });
        if targeted {
            continue;
        }
        for (i, simple) in children(tag) {
            if i != id::SIMPLE_TAG {
                continue;
            }
            let mut name = None;
            let mut value = None;
            for (i, b) in children(simple) {
                match i {
                    id::TAG_NAME => name = Some(string(b)),
                    id::TAG_STRING => value = Some(string(b)),
                    _ => {}
                }
            }
            if let (Some(n), Some(v)) = (name, value) {
                add(&mut t, n, v);
            }
        }
    }
    t
}

/// Puts one SimpleTag into `t`: the first value of a known name fills
/// its field, anything else goes to the extras.
fn add(t: &mut Tags, name: String, value: String) {
    let upper = name.to_ascii_uppercase();
    if DROPPED.contains(&upper.as_str()) {
        return;
    }
    let number = |v: &str| v.trim().parse::<u32>().ok();
    let slot = match upper.as_str() {
        "TITLE" => &mut t.title,
        "ARTIST" => &mut t.artist,
        "ALBUM" => &mut t.album,
        "ALBUM_ARTIST" | "ALBUMARTIST" => &mut t.album_artist,
        "DATE_RELEASED" | "DATE_RECORDED" | "DATE" => &mut t.date,
        "GENRE" => &mut t.genre,
        "COMMENT" => &mut t.comment,
        "PART_NUMBER" | "TRACK" if t.track_number.is_none() && number(&value).is_some() => {
            t.track_number = number(&value);
            return;
        }
        "TOTAL_PARTS" if t.track_total.is_none() && number(&value).is_some() => {
            t.track_total = number(&value);
            return;
        }
        _ => {
            t.extra.push((name, value));
            return;
        }
    };
    if slot.is_none() && non_empty(&value).is_some() {
        *slot = non_empty(&value);
    } else {
        t.extra.push((name, value));
    }
}

/// The whole `Tags` element for `tags`, or nothing if they are empty.
pub fn element(tags: &Tags) -> Vec<u8> {
    let mut simple = Vec::new();
    let mut put = |name: &str, value: &str| {
        let mut s = Vec::new();
        ebml::put_element(&mut s, id::TAG_NAME, name.as_bytes());
        ebml::put_element(&mut s, id::TAG_STRING, value.as_bytes());
        ebml::put_element(&mut simple, id::SIMPLE_TAG, &s);
    };
    let fields = [
        ("TITLE", &tags.title),
        ("ARTIST", &tags.artist),
        ("ALBUM", &tags.album),
        ("ALBUM_ARTIST", &tags.album_artist),
        ("DATE_RELEASED", &tags.date),
        ("GENRE", &tags.genre),
        ("COMMENT", &tags.comment),
    ];
    for (name, v) in fields {
        if let Some(v) = v {
            put(name, v);
        }
    }
    if let Some(n) = tags.track_number {
        put("PART_NUMBER", &n.to_string());
    }
    if let Some(n) = tags.track_total {
        put("TOTAL_PARTS", &n.to_string());
    }
    for (k, v) in &tags.extra {
        put(k, v);
    }
    if simple.is_empty() {
        return simple;
    }
    // An empty Targets: TargetTypeValue defaults to 50, the whole file.
    let mut tag = Vec::new();
    ebml::put_element(&mut tag, id::TARGETS, &[]);
    tag.extend(simple);
    let mut tag_el = Vec::new();
    ebml::put_element(&mut tag_el, id::TAG, &tag);
    let mut out = Vec::new();
    ebml::put_element(&mut out, id::TAGS, &tag_el);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The body of a Tags element.
    fn body(el: &[u8]) -> &[u8] {
        let h = ebml::parse_header(el).expect("header");
        assert_eq!(h.id, id::TAGS);
        &el[h.len..]
    }

    #[test]
    fn round_trip() {
        let t = Tags {
            title: Some("T".into()),
            artist: Some("A".into()),
            album: Some("Al".into()),
            album_artist: Some("AA".into()),
            track_number: Some(3),
            track_total: Some(4),
            date: Some("2026-11-03".into()),
            genre: Some("G".into()),
            comment: Some("C".into()),
            extra: vec![("ENCODER".into(), "x".into())],
        };
        assert_eq!(parse(body(&element(&t))), t);
        assert!(element(&Tags::default()).is_empty());
    }

    #[test]
    fn track_targeted_tags_and_durations_are_ignored() {
        let mut simple = Vec::new();
        ebml::put_element(&mut simple, id::TAG_NAME, b"TITLE");
        ebml::put_element(&mut simple, id::TAG_STRING, b"track title");
        let mut st = Vec::new();
        ebml::put_element(&mut st, id::SIMPLE_TAG, &simple);
        let mut targets = Vec::new();
        ebml::put_uint(&mut targets, id::TAG_TRACK_UID, 7);
        let mut tag = Vec::new();
        ebml::put_element(&mut tag, id::TARGETS, &targets);
        tag.extend(&st);
        let mut tags = Vec::new();
        ebml::put_element(&mut tags, id::TAG, &tag);

        let mut simple = Vec::new();
        ebml::put_element(&mut simple, id::TAG_NAME, b"DURATION");
        ebml::put_element(&mut simple, id::TAG_STRING, b"00:01:00.000\0\0");
        let mut tag = Vec::new();
        ebml::put_element(&mut tag, id::SIMPLE_TAG, &simple);
        ebml::put_element(&mut tags, id::TAG, &tag);

        assert!(parse(&tags).is_empty());
    }

    #[test]
    fn garbage_never_panics() {
        let el = element(&Tags {
            title: Some("x".into()),
            ..Tags::default()
        });
        let b = body(&el);
        for n in 0..b.len() {
            let _ = parse(&b[..n]);
        }
    }
}
