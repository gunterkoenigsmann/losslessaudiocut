//! iTunes-style metadata: `moov/udta/meta/ilst`.
//!
//! There is no ISO standard for it; the layout is Apple's ("QuickTime File
//! Format", "Metadata", iTunes item list), which every common tagger and
//! ffmpeg (`libavformat/movenc.c`, `mov.c`) follow:
//!
//! ```text
//! ilst
//!   <item type, e.g. "©nam">
//!     data: <u32 type indicator> <u32 locale> <value>
//!   "----"                                  (free-form item)
//!     mean: <u32 version/flags> "com.apple.iTunes"
//!     name: <u32 version/flags> <key>
//!     data: as above
//! ```
//!
//! Type indicator 1 is UTF-8 text, 0 binary (`trkn`: two zero bytes, the
//! track number and total as `u16`, two zero bytes).
//!
//! Items [`Tags`] holds are parsed; free-form items become extras (key =
//! their `name`). Everything else (`covr`, `gnre`, `cpil`, `©too`, ...)
//! is kept as raw bytes and written back unchanged. The free-form item
//! `iTunSMPB` (Apple's gapless information: encoder delay and padding of
//! the *source*) is dropped, because it would be wrong for any cut; the
//! edit list says the same for the output.

use lac_core::Tags;

use super::boxes::{self, Boxes, Reader};

/// `©` as stored in item types (Latin-1).
const C: u8 = 0xA9;

/// The free-form item dropped on reading (see the module documentation).
const GAPLESS_INFO: &str = "iTunSMPB";

/// The source's tags plus the items [`Tags`] does not hold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ilst {
    /// What [`Tags`] can hold.
    pub tags: Tags,
    /// Other items, as whole boxes (header included), in file order.
    pub other: Vec<Vec<u8>>,
}

/// The text fields of [`Tags`] and their item types.
fn text_items(t: &Tags) -> [([u8; 4], &Option<String>); 7] {
    [
        ([C, b'n', b'a', b'm'], &t.title),
        ([C, b'A', b'R', b'T'], &t.artist),
        ([C, b'a', b'l', b'b'], &t.album),
        (*b"aART", &t.album_artist),
        ([C, b'd', b'a', b'y'], &t.date),
        ([C, b'g', b'e', b'n'], &t.genre),
        ([C, b'c', b'm', b't'], &t.comment),
    ]
}

/// The `data` payload of an item: (type indicator, value).
fn data_of(item: &[u8]) -> Option<(u32, &[u8])> {
    let d = boxes::find(item, b"data")?;
    let mut r = Reader::new(d.body);
    let kind = r.u32()?;
    r.u32()?; // locale
    Some((kind & 0x00FF_FFFF, r.rest()))
}

/// The text of a `mean` or `name` box (after version and flags).
fn label(item: &[u8], kind: &[u8; 4]) -> Option<String> {
    let b = boxes::find(item, kind)?;
    Some(String::from_utf8_lossy(b.body.get(4..)?).into_owned())
}

/// Parses the body of a `meta` box (ISO full box, or QuickTime's plain
/// box that starts directly with its `hdlr`).
pub fn parse_meta(meta: &[u8]) -> Ilst {
    let children = match meta.get(4..8) {
        Some(b"hdlr") => meta,
        _ => meta.get(4..).unwrap_or_default(),
    };
    boxes::find(children, b"ilst").map_or_else(Ilst::default, |b| parse(b.body))
}

/// Parses the body of an `ilst` box.
pub fn parse(ilst: &[u8]) -> Ilst {
    let mut out = Ilst::default();
    let mut it = Boxes::new(ilst);
    for item in it.by_ref() {
        let raw = || {
            let mut b = Vec::new();
            boxes::put(&mut b, &item.kind, item.body);
            b
        };
        let text = || {
            data_of(item.body)
                .filter(|(k, _)| *k == 1)
                .and_then(|(_, v)| crate::tags::non_empty(&String::from_utf8_lossy(v)))
        };
        let raw_text = || {
            data_of(item.body)
                .filter(|(k, _)| *k == 1)
                .map(|(_, v)| crate::tags::trim_nul(&String::from_utf8_lossy(v)).to_owned())
                .filter(|v| !v.is_empty())
        };
        let t = &mut out.tags;
        let field = match &item.kind {
            [C, b'n', b'a', b'm'] => Some(&mut t.title),
            [C, b'A', b'R', b'T'] => Some(&mut t.artist),
            [C, b'a', b'l', b'b'] => Some(&mut t.album),
            b"aART" => Some(&mut t.album_artist),
            [C, b'd', b'a', b'y'] => Some(&mut t.date),
            [C, b'g', b'e', b'n'] => Some(&mut t.genre),
            [C, b'c', b'm', b't'] => Some(&mut t.comment),
            _ => None,
        };
        if let Some(f) = field {
            match text() {
                Some(v) if f.is_none() => *f = Some(v),
                // A second item of the same type, or a non-text one.
                _ => out.other.push(raw()),
            }
            continue;
        }
        match &item.kind {
            b"trkn" => match data_of(item.body) {
                Some((0, v)) if v.len() >= 6 => {
                    let n = u16::from_be_bytes([v[2], v[3]]);
                    let total = u16::from_be_bytes([v[4], v[5]]);
                    t.track_number = (n > 0).then_some(u32::from(n));
                    t.track_total = (total > 0).then_some(u32::from(total));
                }
                _ => out.other.push(raw()),
            },
            // Free-form values are kept as they are, like Vorbis comments.
            b"----" => match (label(item.body, b"name"), raw_text()) {
                (Some(k), _) if k == GAPLESS_INFO => {}
                (Some(k), Some(v)) if !k.is_empty() => t.extra.push((k, v)),
                _ => out.other.push(raw()),
            },
            _ => out.other.push(raw()),
        }
    }
    out
}

/// A text item.
fn put_text(out: &mut Vec<u8>, kind: &[u8; 4], value: &str) {
    let mut data = 1u32.to_be_bytes().to_vec();
    data.extend(0u32.to_be_bytes());
    data.extend(value.as_bytes());
    let mut item = Vec::new();
    boxes::put(&mut item, b"data", &data);
    boxes::put(out, kind, &item);
}

/// The `ilst` body for `tags` plus the raw `other` items. Empty when there
/// is nothing to write.
pub fn write(tags: &Tags, other: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (kind, v) in text_items(tags) {
        if let Some(v) = v {
            put_text(&mut out, &kind, v);
        }
    }
    if tags.track_number.is_some() || tags.track_total.is_some() {
        let clamp = |v: Option<u32>| v.map_or(0, |n| n.min(u32::from(u16::MAX)) as u16);
        let mut data = vec![0u8; 8]; // type 0 (binary), locale 0
        data.extend([0, 0]);
        data.extend(clamp(tags.track_number).to_be_bytes());
        data.extend(clamp(tags.track_total).to_be_bytes());
        data.extend([0, 0]);
        let mut item = Vec::new();
        boxes::put(&mut item, b"data", &data);
        boxes::put(&mut out, b"trkn", &item);
    }
    for (k, v) in &tags.extra {
        let mut item = Vec::new();
        boxes::put_full(&mut item, b"mean", 0, 0, b"com.apple.iTunes");
        boxes::put_full(&mut item, b"name", 0, 0, k.as_bytes());
        let mut data = 1u32.to_be_bytes().to_vec();
        data.extend(0u32.to_be_bytes());
        data.extend(v.as_bytes());
        boxes::put(&mut item, b"data", &data);
        boxes::put(&mut out, b"----", &item);
    }
    for o in other {
        out.extend_from_slice(o);
    }
    out
}

/// A complete `udta` box holding `meta/hdlr` and `ilst`, or nothing when
/// the item list is empty.
pub fn udta(ilst_body: &[u8]) -> Vec<u8> {
    if ilst_body.is_empty() {
        return Vec::new();
    }
    let mut meta = Vec::new();
    // hdlr: pre_defined, handler "mdir", reserved ("appl" as iTunes and
    // ffmpeg write it), empty name.
    let mut hdlr = 0u32.to_be_bytes().to_vec();
    hdlr.extend(b"mdirappl");
    hdlr.extend([0; 8]);
    hdlr.push(0);
    boxes::put_full(&mut meta, b"hdlr", 0, 0, &hdlr);
    boxes::put(&mut meta, b"ilst", ilst_body);
    let mut m = Vec::new();
    boxes::put_full(&mut m, b"meta", 0, 0, &meta);
    let mut u = Vec::new();
    boxes::put(&mut u, b"udta", &m);
    u
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn known_items_unknown_items_and_gapless_info() {
        let tags = Tags {
            title: Some("Tïtle".into()),
            track_number: Some(3),
            track_total: Some(9),
            extra: vec![("iTunNORM".into(), "x".into())],
            ..Tags::default()
        };
        let mut cover = Vec::new();
        boxes::put(
            &mut cover,
            b"covr",
            &[0, 0, 0, 9, b'd', b'a', b't', b'a', 1],
        );
        let mut body = write(&tags, std::slice::from_ref(&cover));
        // Apple's gapless info is dropped.
        let mut smpb = Vec::new();
        let gapless = Tags {
            extra: vec![(GAPLESS_INFO.into(), " 00000000 00000840".into())],
            ..Tags::default()
        };
        smpb.extend(write(&gapless, &[]));
        body.extend(smpb);
        let back = parse(&body);
        assert_eq!(back.tags, tags);
        assert_eq!(back.other, vec![cover]);
    }

    #[test]
    fn quicktime_and_iso_meta() {
        let tags = Tags {
            artist: Some("A".into()),
            ..Tags::default()
        };
        let u = udta(&write(&tags, &[]));
        let meta = boxes::path(&u, &[b"udta", b"meta"]).expect("meta");
        assert_eq!(parse_meta(meta.body).tags, tags);
        // QuickTime: no version/flags in front of hdlr.
        assert_eq!(parse_meta(&meta.body[4..]).tags, tags);
        assert!(udta(&[]).is_empty());
    }

    proptest! {
        #[test]
        fn round_trip(tags in crate::tags::arb_tags()) {
            let tags = Tags {
                track_number: tags.track_number.map(|n| n.min(999)),
                ..tags
            };
            prop_assert_eq!(parse(&write(&tags, &[])).tags, tags);
        }

        #[test]
        fn garbage_never_panics(b in proptest::collection::vec(any::<u8>(), 0..200)) {
            let _ = parse(&b);
            let _ = parse_meta(&b);
        }
    }
}
