//! The `LIST` chunk of type `INFO` in RIFF files (Microsoft "Multimedia
//! Programming Interface and Data Specifications 1.0", 3-58 "INFO List
//! Chunk").
//!
//! ```text
//! "LIST" <u32 le size> "INFO"
//!   <4-byte ID> <u32 le size> <NUL-terminated text>  [pad byte if odd]
//!   ...
//! ```
//!
//! The specification knows no album artist, track number or encoding;
//! `ITRK` (track number) is a common extension (used by ffmpeg, Windows
//! Explorer and others), and text is read and written as UTF-8, which is
//! what current software does.

use lac_core::{Tags, parse_track_number};

use super::id3::latin1;
use super::non_empty;

/// Whether `key` looks like an INFO ID: `I` and three upper-case letters
/// or digits (`ISFT`, `ICOP`, ...). Extras with such keys are written to
/// the INFO list, others only to the `id3 ` chunk.
pub fn is_info_id(key: &str) -> bool {
    key.len() == 4
        && key.starts_with('I')
        && key
            .bytes()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// Text of an INFO value: UTF-8 if valid, else ISO-8859-1 (the original
/// specification's assumption).
fn text(b: &[u8]) -> String {
    let b = &b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())];
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => latin1(b),
    }
}

/// Parses the body of a `LIST` chunk after its `INFO` type. Unknown IDs
/// become [`Tags::extra`] under their ID. Damage ends parsing quietly.
pub fn parse_info(b: &[u8]) -> Tags {
    let mut t = Tags::default();
    let mut pos = 0;
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let size = u32::from_le_bytes([b[pos + 4], b[pos + 5], b[pos + 6], b[pos + 7]]) as usize;
        let start = pos + 8;
        let Some(v) = start.checked_add(size).and_then(|e| b.get(start..e)) else {
            break;
        };
        pos = start + size + (size & 1);
        let v = text(v);
        let slot = match id {
            b"INAM" => &mut t.title,
            b"IART" => &mut t.artist,
            b"IPRD" => &mut t.album,
            b"ICRD" => &mut t.date,
            b"IGNR" => &mut t.genre,
            b"ICMT" => &mut t.comment,
            b"ITRK" => {
                let (n, total) = parse_track_number(&v);
                t.track_number = t.track_number.or(n);
                t.track_total = t.track_total.or(total);
                continue;
            }
            _ => {
                if let Some(v) = non_empty(&v) {
                    t.extra.push((String::from_utf8_lossy(id).into_owned(), v));
                }
                continue;
            }
        };
        if slot.is_none() {
            *slot = non_empty(&v);
        }
    }
    t
}

/// The whole `LIST/INFO` chunk for `tags` (padded to even length), or
/// nothing if no field fits INFO.
pub fn write_info(tags: &Tags) -> Vec<u8> {
    let mut items = Vec::new();
    let mut put = |id: &[u8], v: &str| {
        let len = v.len() + 1;
        items.extend_from_slice(id);
        items.extend((len as u32).to_le_bytes());
        items.extend_from_slice(v.as_bytes());
        items.push(0);
        if len & 1 == 1 {
            items.push(0);
        }
    };
    let fields: [(&[u8], &Option<String>); 6] = [
        (b"INAM", &tags.title),
        (b"IART", &tags.artist),
        (b"IPRD", &tags.album),
        (b"ICRD", &tags.date),
        (b"IGNR", &tags.genre),
        (b"ICMT", &tags.comment),
    ];
    for (id, v) in fields {
        if let Some(v) = v {
            put(id, v);
        }
    }
    if let Some(n) = tags.track_number {
        put(b"ITRK", &n.to_string());
    }
    for (k, v) in &tags.extra {
        if is_info_id(k) {
            put(k.as_bytes(), v);
        }
    }
    if items.is_empty() {
        return items;
    }
    let mut out = b"LIST".to_vec();
    out.extend(((items.len() + 4) as u32).to_le_bytes());
    out.extend_from_slice(b"INFO");
    out.extend(items);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_round_trip() {
        let t = Tags {
            title: Some("Odd".into()),
            artist: Some("Even".into()),
            album: Some("Älbum".into()),
            track_number: Some(4),
            date: Some("2026".into()),
            genre: Some("Speech".into()),
            comment: Some("c".into()),
            extra: vec![("ISFT".into(), "lac".into())],
            ..Tags::default()
        };
        let chunk = write_info(&t);
        assert_eq!(&chunk[..4], b"LIST");
        assert_eq!(chunk.len() % 2, 0);
        let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as usize;
        assert_eq!(size + 8, chunk.len());
        assert_eq!(parse_info(&chunk[12..]), t);
    }

    #[test]
    fn empty_tags_write_nothing() {
        assert!(write_info(&Tags::default()).is_empty());
        // Extras that are not INFO IDs go elsewhere.
        let t = Tags {
            extra: vec![("MOOD".into(), "x".into())],
            ..Tags::default()
        };
        assert!(write_info(&t).is_empty());
    }

    #[test]
    fn latin1_values_and_damage() {
        let mut b = b"INAM\x04\x00\x00\x00Caf\xE9".to_vec();
        assert_eq!(parse_info(&b).title.as_deref(), Some("Café"));
        b.extend_from_slice(b"IART\xFF\xFF\x00\x00x");
        assert_eq!(parse_info(&b).artist, None);
    }

    #[test]
    fn info_ids() {
        assert!(is_info_id("ISFT"));
        assert!(!is_info_id("isft"));
        assert!(!is_info_id("MOOD"));
        assert!(!is_info_id("INAME"));
    }
}
