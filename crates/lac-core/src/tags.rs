//! Format-neutral track metadata.

use std::fmt;

/// Metadata of one output track.
///
/// Muxers map these fields onto the container's native tag format (ID3v2,
/// Vorbis comments, Matroska `Tags`, RIFF `INFO`, ...). Fields that a
/// container cannot represent are dropped by that muxer, never by the model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tags {
    /// Track title.
    pub title: Option<String>,
    /// Performing artist or speaker.
    pub artist: Option<String>,
    /// Album or recording session.
    pub album: Option<String>,
    /// Artist of the whole album, if different from the track artist.
    pub album_artist: Option<String>,
    /// 1-based position of the track in the album.
    pub track_number: Option<u32>,
    /// Number of tracks in the album.
    pub track_total: Option<u32>,
    /// Recording or release date, free-form (ISO 8601 recommended).
    pub date: Option<String>,
    /// Genre, free-form.
    pub genre: Option<String>,
    /// Comment.
    pub comment: Option<String>,
    /// Any further key/value pairs found in the source, kept verbatim.
    /// Keys are in the container's own naming (Vorbis comment field
    /// names, ID3 `TXXX` descriptions, ...), since tags never move between
    /// container formats: an export keeps the source's format.
    pub extra: Vec<(String, String)>,
}

/// The fields of [`Tags`] that a user edits, in the order a track list
/// shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TagField {
    /// [`Tags::track_number`].
    TrackNumber,
    /// [`Tags::title`].
    Title,
    /// [`Tags::artist`].
    Artist,
    /// [`Tags::album`].
    Album,
    /// [`Tags::album_artist`].
    AlbumArtist,
    /// [`Tags::track_total`].
    TrackTotal,
    /// [`Tags::date`].
    Date,
    /// [`Tags::genre`].
    Genre,
    /// [`Tags::comment`].
    Comment,
}

impl TagField {
    /// Every field, in display order.
    pub const ALL: [TagField; 9] = [
        TagField::TrackNumber,
        TagField::Title,
        TagField::Artist,
        TagField::Album,
        TagField::AlbumArtist,
        TagField::TrackTotal,
        TagField::Date,
        TagField::Genre,
        TagField::Comment,
    ];

    /// A short English label for column headers.
    pub fn label(self) -> &'static str {
        match self {
            TagField::TrackNumber => "#",
            TagField::Title => "Title",
            TagField::Artist => "Artist",
            TagField::Album => "Album",
            TagField::AlbumArtist => "Album artist",
            TagField::TrackTotal => "Tracks",
            TagField::Date => "Date",
            TagField::Genre => "Genre",
            TagField::Comment => "Comment",
        }
    }

    /// The lower-case key used on command lines, e.g. `album_artist`.
    pub fn key(self) -> &'static str {
        match self {
            TagField::TrackNumber => "track",
            TagField::Title => "title",
            TagField::Artist => "artist",
            TagField::Album => "album",
            TagField::AlbumArtist => "album_artist",
            TagField::TrackTotal => "tracktotal",
            TagField::Date => "date",
            TagField::Genre => "genre",
            TagField::Comment => "comment",
        }
    }

    /// The field whose [`TagField::key`] is `key` (ignoring case).
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|f| f.key().eq_ignore_ascii_case(key))
    }

    /// Whether the field holds a number.
    pub fn is_numeric(self) -> bool {
        matches!(self, TagField::TrackNumber | TagField::TrackTotal)
    }
}

impl fmt::Display for TagField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A value that does not fit a [`TagField`]: the text of a numeric field
/// that is not a non-negative integer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{field} must be a whole number, not {value:?}")]
pub struct TagValueError {
    /// The field.
    pub field: TagField,
    /// The rejected text.
    pub value: String,
}

impl Tags {
    /// Whether no field is set.
    pub fn is_empty(&self) -> bool {
        *self == Tags::default()
    }

    /// The value of `field` as text, `None` if unset.
    pub fn get(&self, field: TagField) -> Option<String> {
        match field {
            TagField::TrackNumber => self.track_number.map(|n| n.to_string()),
            TagField::TrackTotal => self.track_total.map(|n| n.to_string()),
            _ => self.text(field).clone(),
        }
    }

    /// Sets `field` from text. Leading and trailing white space is
    /// removed; an empty value unsets the field. Numeric fields take a
    /// non-negative integer.
    pub fn set(&mut self, field: TagField, value: &str) -> Result<(), TagValueError> {
        let value = value.trim();
        let number = |v: &str| -> Result<Option<u32>, TagValueError> {
            if v.is_empty() {
                return Ok(None);
            }
            v.parse().map(Some).map_err(|_| TagValueError {
                field,
                value: v.to_string(),
            })
        };
        match field {
            TagField::TrackNumber => self.track_number = number(value)?,
            TagField::TrackTotal => self.track_total = number(value)?,
            _ => {
                if let Some(slot) = self.text_mut(field) {
                    *slot = (!value.is_empty()).then(|| value.to_string());
                }
            }
        }
        Ok(())
    }

    /// The text field behind `field`; the numeric fields map to an
    /// always-`None` placeholder.
    fn text(&self, field: TagField) -> &Option<String> {
        const NONE: &Option<String> = &None;
        match field {
            TagField::Title => &self.title,
            TagField::Artist => &self.artist,
            TagField::Album => &self.album,
            TagField::AlbumArtist => &self.album_artist,
            TagField::Date => &self.date,
            TagField::Genre => &self.genre,
            TagField::Comment => &self.comment,
            TagField::TrackNumber | TagField::TrackTotal => NONE,
        }
    }

    /// Mutable access to a text field, `None` for the numeric ones.
    fn text_mut(&mut self, field: TagField) -> Option<&mut Option<String>> {
        Some(match field {
            TagField::Title => &mut self.title,
            TagField::Artist => &mut self.artist,
            TagField::Album => &mut self.album,
            TagField::AlbumArtist => &mut self.album_artist,
            TagField::Date => &mut self.date,
            TagField::Genre => &mut self.genre,
            TagField::Comment => &mut self.comment,
            TagField::TrackNumber | TagField::TrackTotal => return None,
        })
    }
}

/// Parses a track number the way taggers write it: `"3"`, `"03"` or
/// `"3/12"`. Returns (number, total); either may be missing.
pub fn parse_track_number(s: &str) -> (Option<u32>, Option<u32>) {
    let mut parts = s.splitn(2, '/');
    let num = parts.next().and_then(|p| p.trim().parse().ok());
    let total = parts.next().and_then(|p| p.trim().parse().ok());
    (num, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_get_every_field() {
        let mut t = Tags::default();
        for (i, f) in TagField::ALL.into_iter().enumerate() {
            t.set(f, &format!(" {i} ")).expect("valid");
            assert_eq!(t.get(f), Some(i.to_string()), "{f}");
        }
        assert_eq!(t.track_number, Some(0));
        assert_eq!(t.title.as_deref(), Some("1"));
        assert_eq!(t.comment.as_deref(), Some("8"));
        for f in TagField::ALL {
            t.set(f, "  ").expect("empty is fine");
            assert_eq!(t.get(f), None, "{f}");
        }
        assert!(t.is_empty());
    }

    #[test]
    fn numeric_fields_reject_text() {
        let mut t = Tags::default();
        let e = t
            .set(TagField::TrackNumber, "two")
            .expect_err("not a number");
        assert_eq!(e.field, TagField::TrackNumber);
        assert!(t.set(TagField::TrackTotal, "-1").is_err());
        assert!(t.is_empty());
    }

    #[test]
    fn keys_round_trip() {
        for f in TagField::ALL {
            assert_eq!(TagField::from_key(f.key()), Some(f));
            assert_eq!(TagField::from_key(&f.key().to_uppercase()), Some(f));
        }
        assert_eq!(TagField::from_key("nonsense"), None);
    }

    #[test]
    fn track_numbers_as_taggers_write_them() {
        assert_eq!(parse_track_number("3"), (Some(3), None));
        assert_eq!(parse_track_number("03/12"), (Some(3), Some(12)));
        assert_eq!(parse_track_number(" 7 / 9 "), (Some(7), Some(9)));
        assert_eq!(parse_track_number("x/9"), (None, Some(9)));
        assert_eq!(parse_track_number(""), (None, None));
    }
}
