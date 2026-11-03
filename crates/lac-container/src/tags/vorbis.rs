//! Vorbis comments ("Vorbis I specification", section 5; field names from
//! the Xiph recommendations at <https://xiph.org/vorbis/doc/v-comment.html>).
//!
//! ```text
//! u32 le  vendor length, vendor string (UTF-8)
//! u32 le  number of fields
//!         per field: u32 le length, "KEY=value" (UTF-8)
//! ```
//!
//! The same structure is FLAC's `VORBIS_COMMENT` block (RFC 9639 8.6), the
//! body of Ogg Vorbis' comment header (after `\x03vorbis`, followed by a
//! framing bit), of Opus' `OpusTags` header (RFC 7845 5.2) and the whole
//! Speex comment header.

use lac_core::{Tags, parse_track_number};

use super::non_empty;

/// A parsed Vorbis comment block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VorbisComment {
    /// The encoder that wrote the block; kept on export.
    pub vendor: String,
    /// The fields in file order, keys as found.
    pub fields: Vec<(String, String)>,
}

/// What a Vorbis comment key means for [`Tags`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Title,
    Artist,
    Album,
    AlbumArtist,
    TrackNumber,
    TrackTotal,
    Date,
    Genre,
    Comment,
}

/// The [`Key`] of a field name; names are case-insensitive (spec 5.2.2).
fn key(name: &str) -> Option<Key> {
    Some(match name.to_ascii_uppercase().as_str() {
        "TITLE" => Key::Title,
        "ARTIST" => Key::Artist,
        "ALBUM" => Key::Album,
        // Both spellings are common (foobar2000 and others).
        "ALBUMARTIST" | "ALBUM ARTIST" => Key::AlbumArtist,
        "TRACKNUMBER" => Key::TrackNumber,
        "TRACKTOTAL" | "TOTALTRACKS" => Key::TrackTotal,
        "DATE" => Key::Date,
        "GENRE" => Key::Genre,
        "COMMENT" => Key::Comment,
        _ => return None,
    })
}

impl VorbisComment {
    /// Parses a block from the start of `b`. Returns the block and the
    /// number of bytes it took, so callers can keep what follows (Ogg
    /// Vorbis' framing bit, Opus' binary extension data). Invalid UTF-8 is
    /// replaced, fields without `=` are ignored.
    pub fn parse(b: &[u8]) -> Result<(Self, usize), String> {
        let mut pos = 0usize;
        let mut chunk = |len: usize| -> Result<&[u8], String> {
            let end = pos
                .checked_add(len)
                .filter(|&e| e <= b.len())
                .ok_or_else(|| "the Vorbis comment block ends early".to_string())?;
            let s = &b[pos..end];
            pos = end;
            Ok(s)
        };
        let u32_at = |s: &[u8]| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize;
        let vendor_len = u32_at(chunk(4)?);
        let vendor = String::from_utf8_lossy(chunk(vendor_len)?).into_owned();
        let count = u32_at(chunk(4)?);
        // Every field needs at least its 4-byte length, which bounds a
        // garbage count before anything is allocated for it.
        let mut fields = Vec::with_capacity(count.min(b.len() / 4));
        for _ in 0..count {
            let len = u32_at(chunk(4)?);
            let field = String::from_utf8_lossy(chunk(len)?).into_owned();
            if let Some((k, v)) = field.split_once('=') {
                fields.push((k.to_string(), v.to_string()));
            }
        }
        Ok((VorbisComment { vendor, fields }, pos))
    }

    /// The block's bytes, without any framing bit.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut put = |s: &[u8]| {
            // Fields above 4 GiB cannot occur: tags are built from text
            // the user typed or that was read from a 32-bit length.
            out.extend((s.len() as u32).to_le_bytes());
            out.extend_from_slice(s);
        };
        put(self.vendor.as_bytes());
        out.extend((self.fields.len() as u32).to_le_bytes());
        for (k, v) in &self.fields {
            out.extend(((k.len() + 1 + v.len()) as u32).to_le_bytes());
            out.extend_from_slice(k.as_bytes());
            out.push(b'=');
            out.extend_from_slice(v.as_bytes());
        }
        out
    }

    /// The fields as [`Tags`]. The first value of a known field fills it;
    /// further values (multi-valued fields such as several `ARTIST`s) and
    /// unknown fields go to [`Tags::extra`] with their original key, so
    /// that [`VorbisComment::with_tags`] writes them back.
    pub fn tags(&self) -> Tags {
        let mut t = Tags::default();
        for (k, v) in &self.fields {
            let slot = match key(k) {
                Some(Key::Title) => &mut t.title,
                Some(Key::Artist) => &mut t.artist,
                Some(Key::Album) => &mut t.album,
                Some(Key::AlbumArtist) => &mut t.album_artist,
                Some(Key::Date) => &mut t.date,
                Some(Key::Genre) => &mut t.genre,
                Some(Key::Comment) => &mut t.comment,
                Some(Key::TrackNumber) if t.track_number.is_none() => {
                    let (n, total) = parse_track_number(v);
                    t.track_number = n;
                    t.track_total = t.track_total.or(total);
                    if n.is_some() {
                        continue;
                    }
                    t.extra.push((k.clone(), v.clone()));
                    continue;
                }
                Some(Key::TrackTotal) if t.track_total.is_none() => {
                    if let Ok(n) = v.trim().parse() {
                        t.track_total = Some(n);
                        continue;
                    }
                    t.extra.push((k.clone(), v.clone()));
                    continue;
                }
                Some(Key::TrackNumber | Key::TrackTotal) | None => {
                    t.extra.push((k.clone(), v.clone()));
                    continue;
                }
            };
            match slot {
                None if non_empty(v).is_some() => *slot = non_empty(v),
                _ => t.extra.push((k.clone(), v.clone())),
            }
        }
        t
    }

    /// A block with this block's vendor and the fields of `tags`: the
    /// known fields under their recommended names, then the extras.
    pub fn with_tags(&self, tags: &Tags) -> VorbisComment {
        let mut fields = Vec::new();
        let mut text = |k: &str, v: &Option<String>| {
            if let Some(v) = v {
                fields.push((k.to_string(), v.clone()));
            }
        };
        text("TITLE", &tags.title);
        text("ARTIST", &tags.artist);
        text("ALBUM", &tags.album);
        text("ALBUMARTIST", &tags.album_artist);
        text("TRACKNUMBER", &tags.track_number.map(|n| n.to_string()));
        text("TRACKTOTAL", &tags.track_total.map(|n| n.to_string()));
        text("DATE", &tags.date);
        text("GENRE", &tags.genre);
        text("COMMENT", &tags.comment);
        fields.extend(tags.extra.iter().cloned());
        VorbisComment {
            vendor: self.vendor.clone(),
            fields,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(fields: &[&str]) -> Vec<u8> {
        let vc = VorbisComment {
            vendor: "test vendor".into(),
            fields: fields
                .iter()
                .map(|f| {
                    let (k, v) = f.split_once('=').expect("key=value");
                    (k.to_string(), v.to_string())
                })
                .collect(),
        };
        vc.to_bytes()
    }

    #[test]
    fn known_fields_fill_tags() {
        let b = block(&[
            "title=Song",
            "ARTIST=A",
            "Artist=B",
            "ALBUM=Live",
            "ALBUM ARTIST=Band",
            "TRACKNUMBER=3/12",
            "DATE=2026-11-03",
            "GENRE=Speech",
            "COMMENT=hi",
            "METADATA_BLOCK_PICTURE=AAAA",
        ]);
        let (vc, used) = VorbisComment::parse(&b).expect("valid");
        assert_eq!(used, b.len());
        assert_eq!(vc.vendor, "test vendor");
        let t = vc.tags();
        assert_eq!(t.title.as_deref(), Some("Song"));
        assert_eq!(t.artist.as_deref(), Some("A"));
        assert_eq!(t.album.as_deref(), Some("Live"));
        assert_eq!(t.album_artist.as_deref(), Some("Band"));
        assert_eq!((t.track_number, t.track_total), (Some(3), Some(12)));
        assert_eq!(t.date.as_deref(), Some("2026-11-03"));
        assert_eq!(t.genre.as_deref(), Some("Speech"));
        assert_eq!(t.comment.as_deref(), Some("hi"));
        assert_eq!(
            t.extra,
            vec![
                ("Artist".into(), "B".into()),
                ("METADATA_BLOCK_PICTURE".into(), "AAAA".into())
            ]
        );
    }

    #[test]
    fn tags_round_trip() {
        let b = block(&[
            "TITLE=x",
            "TOTALTRACKS=9",
            "FOO=bar",
            "ARTIST=a",
            "ARTIST=b",
        ]);
        let (vc, _) = VorbisComment::parse(&b).expect("valid");
        let t = vc.tags();
        assert_eq!(t.track_total, Some(9));
        let again = VorbisComment::parse(&vc.with_tags(&t).to_bytes())
            .expect("valid")
            .0;
        assert_eq!(again.vendor, "test vendor");
        assert_eq!(again.tags(), t);
    }

    #[test]
    fn trailing_bytes_are_not_consumed() {
        let mut b = block(&["TITLE=x"]);
        let len = b.len();
        b.push(1); // Vorbis framing bit
        assert_eq!(VorbisComment::parse(&b).expect("valid").1, len);
    }

    #[test]
    fn truncated_blocks_are_errors_not_panics() {
        let b = block(&["TITLE=something long"]);
        for n in 0..b.len() {
            assert!(VorbisComment::parse(&b[..n]).is_err(), "{n}");
        }
        // A huge field count must not allocate or panic.
        let mut huge = block(&[]);
        let at = huge.len() - 4;
        huge[at..].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(VorbisComment::parse(&huge).is_err());
    }

    proptest::proptest! {
        #[test]
        fn any_tags_round_trip(t in crate::tags::arb_tags()) {
            // Extra keys must not be one of the known names.
            let t = Tags {
                extra: t.extra.into_iter().map(|(k, v)| (format!("X_{k}"), v)).collect(),
                ..t
            };
            let vc = VorbisComment::default().with_tags(&t);
            let (back, _) = VorbisComment::parse(&vc.to_bytes()).expect("valid");
            proptest::prop_assert_eq!(back.tags(), t);
        }
    }

    #[test]
    fn unparsable_numbers_are_kept_as_extras() {
        let b = block(&["TRACKNUMBER=A1", "TRACKTOTAL=many"]);
        let t = VorbisComment::parse(&b).expect("valid").0.tags();
        assert_eq!(t.track_number, None);
        assert_eq!(t.extra.len(), 2);
    }
}
