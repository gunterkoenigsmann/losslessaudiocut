//! ID3v2 tags (id3.org "ID3 tag version 2.3.0", "ID3v2.4.0 - Main
//! Structure" and "ID3v2.4.0 - Native Frames"; version 2.2 is read too)
//! and the 128-byte ID3v1 tag.
//!
//! ## Reading
//!
//! The text frames that [`Tags`] models are parsed into it; `TXXX`
//! frames become [`Tags::extra`] (description, value). Every other frame
//! (pictures, lyrics, private data, further text frames) is kept as a
//! [`RawFrame`] and written back unchanged, except
//!
//! - frames that describe positions in the audio, which a cut makes wrong
//!   (`TLEN`, `MLLT`, `ETCO`, `SYLT`, `SYTC`, `POSS`, `SEEK`, `ASPI`),
//! - version 2.3 frames that version 2.4 replaced (`TYER`, `TDAT`, ...;
//!   the date is read into [`Tags::date`]),
//! - frames whose "file alter" or "tag alter" preservation flag says to
//!   discard them when the audio or the tag changes (2.4 4.1.1),
//! - compressed, encrypted or grouped frames, which this reader does not
//!   unpack,
//! - all frames of a version 2.2 tag other than the modelled ones, since
//!   their three-letter IDs have no one-to-one 2.4 equivalent.
//!
//! ## Writing
//!
//! Always version 2.4, UTF-8 text (encoding 3), no unsynchronisation, no
//! extended header, no padding.

use lac_core::{Tags, parse_track_number};

use super::non_empty;

/// A frame copied verbatim from the source: its 2.4 ID and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFrame {
    /// Four-character frame ID, e.g. `APIC`.
    pub id: [u8; 4],
    /// The frame body, after undoing unsynchronisation.
    pub body: Vec<u8>,
}

/// A parsed ID3v2 tag.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Id3v2 {
    /// The modelled fields.
    pub tags: Tags,
    /// Frames to copy unchanged (see the module documentation).
    pub other: Vec<RawFrame>,
    /// What was dropped or could not be read.
    pub warnings: Vec<String>,
}

/// Frames that a cut invalidates, or that 2.4 replaced; never copied.
const DROPPED: [&[u8; 4]; 17] = [
    b"TLEN", b"MLLT", b"ETCO", b"SYLT", b"SYTC", b"POSS", b"SEEK", b"ASPI", b"TYER", b"TDAT",
    b"TIME", b"TRDA", b"TSIZ", b"IPLS", b"EQUA", b"RVAD", b"TORY",
];

/// Length of a tag starting at `b[0]` ("ID3" header), including its
/// header and footer, or `None` if `b` does not start with one.
pub fn tag_len(b: &[u8]) -> Option<usize> {
    if b.len() < 10 || &b[..3] != b"ID3" {
        return None;
    }
    let footer = if b[3] >= 4 && b[5] & 0x10 != 0 { 10 } else { 0 };
    Some(10 + syncsafe(&b[6..10]) + footer)
}

/// A 28-bit "syncsafe" integer: 7 bits per byte, most significant first.
fn syncsafe(b: &[u8]) -> usize {
    b.iter().fold(0, |a, &x| (a << 7) | usize::from(x & 0x7F))
}

/// Encodes `n` (below 2^28) as a syncsafe integer.
fn put_syncsafe(out: &mut Vec<u8>, n: usize) {
    out.extend([
        (n >> 21) as u8 & 0x7F,
        (n >> 14) as u8 & 0x7F,
        (n >> 7) as u8 & 0x7F,
        n as u8 & 0x7F,
    ]);
}

/// Undoes unsynchronisation (2.4 6.1): every `FF 00` becomes `FF`.
fn resync(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut prev_ff = false;
    for &x in b {
        if !(prev_ff && x == 0) {
            out.push(x);
        }
        prev_ff = x == 0xFF;
    }
    out
}

/// Decodes a string in ID3 text encoding `enc` (2.4 4.2): 0 ISO-8859-1,
/// 1 UTF-16 with BOM, 2 UTF-16BE, 3 UTF-8.
fn decode(enc: u8, b: &[u8]) -> String {
    match enc {
        1 | 2 => {
            let (be, b) = match b {
                [0xFE, 0xFF, rest @ ..] => (true, rest),
                [0xFF, 0xFE, rest @ ..] => (false, rest),
                // No BOM: 2.4 says big-endian for encoding 2; for 1 the BOM
                // is mandatory, so big-endian is as good a guess as any.
                _ => (true, b),
            };
            let units = b.chunks_exact(2).map(|c| {
                if be {
                    u16::from_be_bytes([c[0], c[1]])
                } else {
                    u16::from_le_bytes([c[0], c[1]])
                }
            });
            char::decode_utf16(units)
                .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect()
        }
        3 => String::from_utf8_lossy(b).into_owned(),
        _ => latin1(b),
    }
}

/// ISO-8859-1 bytes as a string (each byte is the code point).
pub(crate) fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| char::from(c)).collect()
}

/// Splits `b` at the first string terminator of encoding `enc` (one NUL
/// byte, or two aligned ones for UTF-16). Returns (string, rest).
fn split_terminated(enc: u8, b: &[u8]) -> (&[u8], &[u8]) {
    if enc == 1 || enc == 2 {
        let mut i = 0;
        while i + 1 < b.len() {
            if b[i] == 0 && b[i + 1] == 0 {
                return (&b[..i], &b[i + 2..]);
            }
            i += 2;
        }
        (b, &[])
    } else {
        match b.iter().position(|&c| c == 0) {
            Some(i) => (&b[..i], &b[i + 1..]),
            None => (b, &[]),
        }
    }
}

/// The value of a text frame body. Several values (2.4 separates them by
/// NUL) are joined with `"; "`.
fn text_frame(body: &[u8]) -> Option<String> {
    let (&enc, rest) = body.split_first()?;
    let s = decode(enc, rest);
    let values: Vec<&str> = s
        .split('\0')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .collect();
    (!values.is_empty()).then(|| values.join("; "))
}

/// (description, value) of a `TXXX` body, or (description, text) of a
/// `COMM` body after its language.
fn described(body: &[u8]) -> Option<(String, String)> {
    let (&enc, rest) = body.split_first()?;
    let (desc, value) = split_terminated(enc, rest);
    Some((
        decode(enc, desc).trim_end_matches('\0').to_string(),
        decode(enc, value).trim_end_matches('\0').to_string(),
    ))
}

/// The 2.4 frame ID for a 2.2 one, for the frames [`Tags`] models.
fn v22_id(id: &[u8]) -> Option<&'static [u8; 4]> {
    Some(match id {
        b"TT2" => b"TIT2",
        b"TP1" => b"TPE1",
        b"TAL" => b"TALB",
        b"TP2" => b"TPE2",
        b"TRK" => b"TRCK",
        b"TYE" => b"TYER",
        b"TCO" => b"TCON",
        b"COM" => b"COMM",
        b"TXX" => b"TXXX",
        _ => return None,
    })
}

/// Parses a whole tag (`b` starts with the "ID3" header). Damage inside
/// the tag ends parsing with a warning; it never fails the file, since
/// tags are not needed to cut audio.
pub fn parse(b: &[u8]) -> Id3v2 {
    let mut out = Id3v2::default();
    if tag_len(b).is_none() {
        return out;
    }
    let version = b[3];
    let flags = b[5];
    if !(2..=4).contains(&version) {
        out.warnings
            .push(format!("ID3v2.{version} tag not understood, not copied"));
        return out;
    }
    let end = 10 + syncsafe(&b[6..10]);
    if end > b.len() {
        out.warnings.push("the ID3v2 tag is truncated".into());
    }
    let end = end.min(b.len());
    // 2.3 unsynchronises the whole tag; 2.4 flags it per frame, with the
    // tag flag meaning "all frames" (2.4 3.1).
    let tag_unsync = flags & 0x80 != 0;
    let body = if version < 4 && tag_unsync {
        resync(&b[10..end])
    } else {
        b[10..end].to_vec()
    };
    let mut pos = 0usize;
    if version >= 3 && flags & 0x40 != 0 {
        // Extended header: 2.3 gives its size without the size field, 2.4
        // as a syncsafe size including it.
        let Some(h) = body.get(..4) else {
            return out;
        };
        pos = if version == 3 {
            4 + u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as usize
        } else {
            syncsafe(h)
        };
    }
    if version == 2 && flags & 0x40 != 0 {
        out.warnings
            .push("compressed ID3v2.2 tag not understood, not copied".into());
        return out;
    }

    let (id_len, head_len) = if version == 2 { (3, 6) } else { (4, 10) };
    let mut year: Option<String> = None;
    let mut day_month: Option<String> = None;
    while pos + head_len <= body.len() {
        let h = &body[pos..pos + head_len];
        // Padding (2.4 3.3) or garbage: frame IDs are A-Z and 0-9.
        if !h[..id_len]
            .iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        {
            break;
        }
        let size = match version {
            2 => (usize::from(h[3]) << 16) | (usize::from(h[4]) << 8) | usize::from(h[5]),
            3 => u32::from_be_bytes([h[4], h[5], h[6], h[7]]) as usize,
            _ => syncsafe(&h[4..8]),
        };
        let start = pos + head_len;
        let Some(raw) = start.checked_add(size).and_then(|e| body.get(start..e)) else {
            out.warnings.push("an ID3v2 frame is truncated".into());
            break;
        };
        pos = start + size;
        let (status, format) = if version == 2 { (0, 0) } else { (h[8], h[9]) };

        let id: [u8; 4] = if version == 2 {
            match v22_id(&h[..3]) {
                Some(id) => *id,
                None => {
                    continue;
                }
            }
        } else {
            [h[0], h[1], h[2], h[3]]
        };
        let name = String::from_utf8_lossy(&id).into_owned();

        // Format flags: 2.3 has compression 0x80, encryption 0x40,
        // grouping 0x20; 2.4 grouping 0x40, compression 0x08, encryption
        // 0x04, unsynchronisation 0x02, data length indicator 0x01.
        let (packed, unsync, dli) = match version {
            3 => (format & 0xE0 != 0, false, false),
            4 => (
                format & 0x4C != 0,
                tag_unsync || format & 0x02 != 0,
                format & 0x01 != 0,
            ),
            _ => (false, false, false),
        };
        if packed {
            out.warnings.push(format!(
                "compressed or encrypted ID3v2 frame {name} is dropped when the tags are rewritten"
            ));
            continue;
        }
        let mut frame = if unsync { resync(raw) } else { raw.to_vec() };
        if dli {
            if frame.len() < 4 {
                continue;
            }
            frame.drain(..4);
        }

        match &id {
            b"TIT2" => out.tags.title = out.tags.title.take().or(text_frame(&frame)),
            b"TPE1" => out.tags.artist = out.tags.artist.take().or(text_frame(&frame)),
            b"TALB" => out.tags.album = out.tags.album.take().or(text_frame(&frame)),
            b"TPE2" => {
                out.tags.album_artist = out.tags.album_artist.take().or(text_frame(&frame));
            }
            b"TCON" => out.tags.genre = out.tags.genre.take().or(text_frame(&frame)),
            b"TDRC" => out.tags.date = out.tags.date.take().or(text_frame(&frame)),
            b"TYER" => year = text_frame(&frame),
            b"TDAT" => day_month = text_frame(&frame),
            b"TRCK" => {
                if let Some(s) = text_frame(&frame) {
                    let (n, total) = parse_track_number(&s);
                    out.tags.track_number = out.tags.track_number.or(n);
                    out.tags.track_total = out.tags.track_total.or(total);
                }
            }
            b"TXXX" => {
                if let Some((d, v)) = described(&frame) {
                    out.tags.extra.push((d, v));
                }
            }
            b"COMM" if out.tags.comment.is_none() && frame.len() >= 4 => {
                // Encoding, 3-byte language, description, text.
                let mut b = vec![frame[0]];
                b.extend_from_slice(&frame[4..]);
                match described(&b) {
                    Some((d, v)) if d.is_empty() => out.tags.comment = non_empty(&v),
                    _ => out.other.push(RawFrame { id, body: frame }),
                }
            }
            _ if version == 2 => {}
            _ if DROPPED.contains(&&id) => {}
            // File alter preservation (2.3 0x40, 2.4 0x20) and tag alter
            // preservation (2.3 0x80, 2.4 0x40): "discard this frame when
            // the file (or the tag) is altered".
            _ if (version == 3 && status & 0xC0 != 0) || (version == 4 && status & 0x60 != 0) => {}
            _ => out.other.push(RawFrame { id, body: frame }),
        }
    }
    // 2.3 keeps the year and DDMM apart; 2.4's TDRC is ISO 8601.
    if out.tags.date.is_none() {
        out.tags.date = match (year, day_month) {
            (Some(y), Some(dm)) if dm.len() == 4 && dm.bytes().all(|c| c.is_ascii_digit()) => {
                Some(format!("{y}-{}-{}", &dm[2..], &dm[..2]))
            }
            (y, _) => y,
        };
    }
    out
}

/// Fills the fields of `tags` that are still empty from an ID3v1 tag
/// (`b` is the 128 bytes starting with "TAG"). Version 1.1 stores the
/// track number in the last byte of the comment.
pub fn merge_v1(tags: &mut Tags, b: &[u8]) {
    if b.len() != 128 || &b[..3] != b"TAG" {
        return;
    }
    let field = |r: std::ops::Range<usize>| {
        let s = &b[r];
        let s = &s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())];
        non_empty(&latin1(s))
    };
    tags.title = tags.title.take().or(field(3..33));
    tags.artist = tags.artist.take().or(field(33..63));
    tags.album = tags.album.take().or(field(63..93));
    tags.date = tags.date.take().or(field(93..97));
    tags.comment = tags.comment.take().or(field(97..127));
    if b[125] == 0 && b[126] != 0 {
        tags.track_number = tags.track_number.or(Some(u32::from(b[126])));
    }
}

/// Appends a 2.4 frame.
fn put_frame(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) -> Result<(), String> {
    if body.len() >= 1 << 28 {
        return Err(format!(
            "ID3v2 frame {} above 256 MiB",
            String::from_utf8_lossy(id)
        ));
    }
    out.extend_from_slice(id);
    put_syncsafe(out, body.len());
    out.extend([0, 0]);
    out.extend_from_slice(body);
    Ok(())
}

/// Writes `tags` and the copied frames `other` as an ID3v2.4 tag. Returns
/// an empty vector if there is nothing to write.
pub fn write(tags: &Tags, other: &[RawFrame]) -> Result<Vec<u8>, String> {
    let mut frames = Vec::new();
    let text = |s: &str| {
        let mut b = vec![3u8];
        b.extend_from_slice(s.as_bytes());
        b
    };
    let fields: [(&[u8; 4], &Option<String>); 7] = [
        (b"TIT2", &tags.title),
        (b"TPE1", &tags.artist),
        (b"TALB", &tags.album),
        (b"TPE2", &tags.album_artist),
        (b"TDRC", &tags.date),
        (b"TCON", &tags.genre),
        (b"TRCK", &None),
    ];
    for (id, v) in fields {
        if let Some(v) = v {
            put_frame(&mut frames, id, &text(v))?;
        }
    }
    // 2.4 4.2.1 TRCK: "number" or "number/total".
    if let Some(n) = tags.track_number {
        let s = match tags.track_total {
            Some(t) => format!("{n}/{t}"),
            None => n.to_string(),
        };
        put_frame(&mut frames, b"TRCK", &text(&s))?;
    }
    if let Some(c) = &tags.comment {
        // Encoding, language, empty description, text.
        let mut b = vec![3u8];
        b.extend_from_slice(b"eng\0");
        b.extend_from_slice(c.as_bytes());
        put_frame(&mut frames, b"COMM", &b)?;
    }
    for (k, v) in &tags.extra {
        let mut b = vec![3u8];
        b.extend_from_slice(k.as_bytes());
        b.push(0);
        b.extend_from_slice(v.as_bytes());
        put_frame(&mut frames, b"TXXX", &b)?;
    }
    for f in other {
        put_frame(&mut frames, &f.id, &f.body)?;
    }
    if frames.is_empty() {
        return Ok(Vec::new());
    }
    if frames.len() >= 1 << 28 {
        return Err("ID3v2 tag above 256 MiB".into());
    }
    let mut out = b"ID3\x04\x00\x00".to_vec();
    put_syncsafe(&mut out, frames.len());
    out.extend(frames);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2.3 frame with a plain 32-bit size.
    fn v23_frame(id: &[u8; 4], flags: [u8; 2], body: &[u8]) -> Vec<u8> {
        let mut f = id.to_vec();
        f.extend((body.len() as u32).to_be_bytes());
        f.extend(flags);
        f.extend_from_slice(body);
        f
    }

    fn v23_tag(flags: u8, frames: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = frames.concat();
        let mut t = vec![b'I', b'D', b'3', 3, 0, flags];
        put_syncsafe(&mut t, body.len());
        t.extend(body);
        t
    }

    fn sample_tags() -> Tags {
        Tags {
            title: Some("Tïtle".into()),
            artist: Some("Artist".into()),
            album: Some("Album".into()),
            album_artist: Some("Band".into()),
            track_number: Some(2),
            track_total: Some(7),
            date: Some("2026-11-03".into()),
            genre: Some("Speech".into()),
            comment: Some("Comment".into()),
            extra: vec![("MOOD".into(), "calm".into())],
        }
    }

    #[test]
    fn written_tags_read_back() {
        let pic = RawFrame {
            id: *b"APIC",
            body: b"\x00image/png\x00\x03\x00PNGDATA".to_vec(),
        };
        let b = write(&sample_tags(), std::slice::from_ref(&pic)).expect("fits");
        assert_eq!(tag_len(&b), Some(b.len()));
        let p = parse(&b);
        assert_eq!(p.tags, sample_tags());
        assert_eq!(p.other, vec![pic]);
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn nothing_to_write_writes_nothing() {
        assert!(write(&Tags::default(), &[]).expect("fits").is_empty());
    }

    #[test]
    fn version_2_3_with_utf16_and_split_date() {
        let mut utf16 = vec![1, 0xFF, 0xFE];
        for u in "Grüße".encode_utf16() {
            utf16.extend(u.to_le_bytes());
        }
        let tag = v23_tag(
            0,
            &[
                v23_frame(b"TIT2", [0, 0], &utf16),
                v23_frame(b"TYER", [0, 0], b"\x002024"),
                v23_frame(b"TDAT", [0, 0], b"\x002512"),
                v23_frame(b"TRCK", [0, 0], b"\x0005"),
                v23_frame(b"TLEN", [0, 0], b"\x0012345"),
                v23_frame(b"TSSE", [0, 0], b"\x00LAME"),
                // File alter preservation set: discarded on a cut.
                v23_frame(b"PRIV", [0x40, 0], b"x\x00data"),
                // Compressed.
                v23_frame(b"TPE1", [0, 0x80], b"\x00\x00\x00\x05zzzzz"),
            ],
        );
        let p = parse(&tag);
        assert_eq!(p.tags.title.as_deref(), Some("Grüße"));
        assert_eq!(p.tags.date.as_deref(), Some("2024-12-25"));
        assert_eq!(p.tags.track_number, Some(5));
        assert_eq!(p.tags.artist, None);
        assert_eq!(p.other.len(), 1);
        assert_eq!(&p.other[0].id, b"TSSE");
        assert_eq!(p.warnings.len(), 1, "{:?}", p.warnings);
    }

    #[test]
    fn unsynchronised_2_3_tag() {
        // An APIC body with FF E0 must be stored as FF 00 E0.
        let body = b"\x00image/jpeg\x00\x03\x00\xFF\xE0".to_vec();
        let frame = v23_frame(b"APIC", [0, 0], &body);
        let mut unsynced = Vec::new();
        for (i, &c) in frame.iter().enumerate() {
            unsynced.push(c);
            if c == 0xFF
                && frame
                    .get(i + 1)
                    .is_some_and(|&n| n & 0xE0 == 0xE0 || n == 0)
            {
                unsynced.push(0);
            }
        }
        let p = parse(&v23_tag(0x80, &[unsynced]));
        assert_eq!(p.other, vec![RawFrame { id: *b"APIC", body }]);
    }

    #[test]
    fn version_2_2_text_frames() {
        let mut body = Vec::new();
        for (id, v) in [(b"TT2", "Old"), (b"TRK", "4/9"), (b"PIC", "xx")] {
            body.extend_from_slice(id);
            let len = v.len() + 1;
            body.extend([0, 0, len as u8, 0]);
            body.extend_from_slice(v.as_bytes());
        }
        let mut tag = vec![b'I', b'D', b'3', 2, 0, 0];
        put_syncsafe(&mut tag, body.len());
        tag.extend(body);
        let p = parse(&tag);
        assert_eq!(p.tags.title.as_deref(), Some("Old"));
        assert_eq!(
            (p.tags.track_number, p.tags.track_total),
            (Some(4), Some(9))
        );
        assert!(p.other.is_empty());
    }

    #[test]
    fn id3v1_fills_gaps_only() {
        let mut v1 = vec![0u8; 128];
        v1[..3].copy_from_slice(b"TAG");
        v1[3..8].copy_from_slice(b"Title");
        v1[33..39].copy_from_slice(b"Artist");
        v1[93..97].copy_from_slice(b"1999");
        v1[126] = 3;
        let mut t = Tags {
            title: Some("From v2".into()),
            ..Tags::default()
        };
        merge_v1(&mut t, &v1);
        assert_eq!(t.title.as_deref(), Some("From v2"));
        assert_eq!(t.artist.as_deref(), Some("Artist"));
        assert_eq!(t.date.as_deref(), Some("1999"));
        assert_eq!(t.track_number, Some(3));
    }

    proptest::proptest! {
        #[test]
        fn any_tags_round_trip(t in crate::tags::arb_tags()) {
            // TXXX descriptions are free text, so any extra key works.
            let b = write(&t, &[]).expect("fits");
            proptest::prop_assert_eq!(parse(&b).tags, t);
        }
    }

    #[test]
    fn damaged_tags_never_panic() {
        let b = write(&sample_tags(), &[]).expect("fits");
        for n in 0..b.len() {
            let _ = parse(&b[..n]);
        }
        let mut bad = b.clone();
        for i in 10..bad.len() {
            bad[i] = 0xFF;
            let _ = parse(&bad);
        }
    }
}
