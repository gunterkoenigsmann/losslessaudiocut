//! Format-neutral track metadata.

/// Metadata of one output track.
///
/// Muxers map these fields onto the container's native tag format (ID3v2,
/// Vorbis comments, MP4 `ilst`, RIFF `INFO`, ...). Fields that a container
/// cannot represent are dropped by that muxer, never by the model.
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
    pub extra: Vec<(String, String)>,
}
