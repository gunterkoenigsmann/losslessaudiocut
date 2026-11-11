//! # lac-container
//!
//! Demuxers and muxers for lossless, packet-level cutting.
//!
//! Every format module reads a source file into a [`lac_core::PacketIndex`]
//! without decoding it, reports its [`lac_core::CutCapabilities`], and writes
//! new files by copying packets verbatim according to
//! [`lac_core::CutPlan`]s. Lossy audio is never decoded or re-encoded here.
//!
//! Supported so far: [`wav`], [`aiff`], [`flac`], [`ogg`] (Vorbis, Opus, Speex),
//! [`mp3`], [`mkv`] (Matroska/WebM with Opus), [`mp4`] (MP4/M4A with
//! AAC or ALAC), [`adts`] (raw AAC) and [`ac3`] (raw AC-3). [`open`] detects the format
//! from the file's content and returns it behind the common [`Container`]
//! interface. Each format also reads and writes its tags; the shared tag
//! formats live in [`tags`].

pub mod ac3;
pub mod adts;
pub mod aiff;
mod bits;
pub mod decode;
pub mod flac;
pub mod mkv;
pub mod mp3;
pub mod mp4;
pub mod ogg;
pub mod tags;
pub mod wav;
mod window;

use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;

use lac_core::{
    CutCapabilities, CutPlan, MediaTime, PacketIndex, PlanError, SamplePos, Tags, VideoIndex,
    plan_cuts,
};

pub use decode::{Codec, DecodeSection};

/// Anything that can be read and seeked; used for trait objects.
pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek + ?Sized> ReadSeek for T {}

/// Anything that can be written and seeked; used for trait objects.
pub trait WriteSeek: Write + Seek {}
impl<T: Write + Seek + ?Sized> WriteSeek for T {}

/// A part of the source that the file itself marks as a track of its own,
/// with that part's tags: a link of a chained Ogg file (stream rippers
/// start one per song). Matroska chapters or ID3 `CHAP` frames could be
/// offered the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chapter {
    /// Where the part starts (index position, i.e. output sample frames).
    pub start: SamplePos,
    /// The part's own tags.
    pub tags: Tags,
}

/// The operations every supported container offers.
///
/// `Send`, so that an opened file can move to a worker thread (decoding
/// for playback, exporting); containers are plain parsed data.
pub trait Container: Send {
    /// Short format name, e.g. `"FLAC"`.
    fn format_name(&self) -> &'static str;
    /// Human-readable details such as the sample encoding.
    fn details(&self) -> String;
    /// The packet index of the audio stream.
    fn index(&self) -> &PacketIndex;
    /// What the format can do when cutting.
    fn capabilities(&self) -> CutCapabilities;
    /// Problems that were tolerated while opening the file.
    fn warnings(&self) -> &[String];
    /// The source's tags, as far as [`Tags`] can hold them.
    fn tags(&self) -> &Tags;
    /// The parts the file marks as tracks of their own, sorted by start,
    /// the first starting at 0. Empty when the file is one part, which is
    /// the case for every format except chained Ogg so far.
    fn chapters(&self) -> &[Chapter] {
        &[]
    }
    /// The frames of the file's video track, for files that have one (MP4,
    /// QuickTime, 3GPP).
    fn video(&self) -> Option<&VideoIndex> {
        None
    }
    /// The video decoder's configuration (FFmpeg's extradata, e.g. the
    /// body of an `avcC` box), empty without video or for codecs whose
    /// frames describe themselves.
    fn video_codec_config(&self) -> &[u8] {
        &[]
    }
    /// The video media time (ticks of the video timescale) shown with
    /// audio sample 0: video time = audio time in ticks + this offset.
    /// 0 without video.
    fn video_offset(&self) -> MediaTime {
        0
    }
    /// Plans the joined output of the kept `ranges` (sorted, not
    /// overlapping) for [`Container::write_cut`]. Audio-only formats plan
    /// with [`plan_cuts`]; files with video plan both tracks together
    /// ([`lac_core::plan_av_cuts`]), which moves joins to keyframes and may
    /// merge parts, so there can be fewer plans than ranges.
    fn plan(&self, ranges: &[Range<SamplePos>]) -> std::result::Result<Vec<CutPlan>, PlanError> {
        plan_cuts(self.index(), self.capabilities(), ranges)
    }
    /// Notes that apply to every cut of this file, for the export report:
    /// where a cut decodes differently in some players although every
    /// packet is copied verbatim (codec noise from a random generator, see
    /// [`mp4::aac`] and [`ac3`]). Reads from `r` where that needs the
    /// packets.
    fn cut_notes(&self, r: &mut dyn ReadSeek) -> Result<Vec<String>> {
        let _ = r;
        Ok(Vec::new())
    }
    /// Writes a new file containing the `actual` ranges of `plans`, joined.
    ///
    /// With `tags`, the new file carries them in the format's native tag
    /// format; without, it keeps the source's tags (copied verbatim where
    /// the format allows). Either way the source's pictures and other
    /// binary tag data are kept; see [`tags`] and each format module.
    fn write_cut(
        &self,
        r: &mut dyn ReadSeek,
        plans: &[CutPlan],
        tags: Option<&Tags>,
        w: &mut dyn WriteSeek,
    ) -> Result<()>;
    /// How to decode the source, see [`decode`]. Reads codec headers from
    /// `r` where the format keeps them in the file.
    fn decode_sections(&self, r: &mut dyn ReadSeek) -> Result<Vec<DecodeSection>>;
    /// Packet `packet` of section `section`, as the decoder wants it.
    fn read_decode_packet(
        &self,
        r: &mut dyn ReadSeek,
        section: usize,
        packet: usize,
    ) -> Result<Vec<u8>>;
}

impl Container for wav::WavFile {
    fn format_name(&self) -> &'static str {
        "WAV"
    }
    fn details(&self) -> String {
        format!("{:?}, {} bit", self.encoding(), self.bits_per_sample())
    }
    fn index(&self) -> &PacketIndex {
        wav::WavFile::index(self)
    }
    fn capabilities(&self) -> CutCapabilities {
        wav::WavFile::capabilities(self)
    }
    fn warnings(&self) -> &[String] {
        wav::WavFile::warnings(self)
    }
    fn tags(&self) -> &Tags {
        wav::WavFile::tags(self)
    }
    fn write_cut(
        &self,
        r: &mut dyn ReadSeek,
        plans: &[CutPlan],
        tags: Option<&Tags>,
        w: &mut dyn WriteSeek,
    ) -> Result<()> {
        wav::WavFile::write_cut(self, r, plans, tags, w)
    }
    fn decode_sections(&self, r: &mut dyn ReadSeek) -> Result<Vec<DecodeSection>> {
        wav::WavFile::decode_sections(self, r)
    }
    fn read_decode_packet(
        &self,
        r: &mut dyn ReadSeek,
        section: usize,
        packet: usize,
    ) -> Result<Vec<u8>> {
        wav::WavFile::read_decode_packet(self, r, section, packet)
    }
}

impl Container for aiff::AiffFile {
    fn format_name(&self) -> &'static str {
        if self.is_aifc() { "AIFF-C" } else { "AIFF" }
    }
    fn details(&self) -> String {
        let order = match (self.block_align() / self.info().channels, self.big_endian()) {
            (1, _) => "",
            (_, true) => ", big-endian",
            (_, false) => ", little-endian",
        };
        format!("{:?}, {} bit{order}", self.encoding(), self.sample_size())
    }
    fn index(&self) -> &PacketIndex {
        aiff::AiffFile::index(self)
    }
    fn capabilities(&self) -> CutCapabilities {
        aiff::AiffFile::capabilities(self)
    }
    fn warnings(&self) -> &[String] {
        aiff::AiffFile::warnings(self)
    }
    fn tags(&self) -> &Tags {
        aiff::AiffFile::tags(self)
    }
    fn write_cut(
        &self,
        r: &mut dyn ReadSeek,
        plans: &[CutPlan],
        tags: Option<&Tags>,
        w: &mut dyn WriteSeek,
    ) -> Result<()> {
        aiff::AiffFile::write_cut(self, r, plans, tags, w)
    }
    fn decode_sections(&self, r: &mut dyn ReadSeek) -> Result<Vec<DecodeSection>> {
        aiff::AiffFile::decode_sections(self, r)
    }
    fn read_decode_packet(
        &self,
        r: &mut dyn ReadSeek,
        section: usize,
        packet: usize,
    ) -> Result<Vec<u8>> {
        aiff::AiffFile::read_decode_packet(self, r, section, packet)
    }
}

impl Container for flac::FlacFile {
    fn format_name(&self) -> &'static str {
        "FLAC"
    }
    fn details(&self) -> String {
        format!("{} bit", self.stream_info().bits_per_sample)
    }
    fn index(&self) -> &PacketIndex {
        flac::FlacFile::index(self)
    }
    fn capabilities(&self) -> CutCapabilities {
        flac::FlacFile::capabilities(self)
    }
    fn warnings(&self) -> &[String] {
        flac::FlacFile::warnings(self)
    }
    fn tags(&self) -> &Tags {
        flac::FlacFile::tags(self)
    }
    fn write_cut(
        &self,
        r: &mut dyn ReadSeek,
        plans: &[CutPlan],
        tags: Option<&Tags>,
        w: &mut dyn WriteSeek,
    ) -> Result<()> {
        flac::FlacFile::write_cut(self, r, plans, tags, w)
    }
    fn decode_sections(&self, r: &mut dyn ReadSeek) -> Result<Vec<DecodeSection>> {
        flac::FlacFile::decode_sections(self, r)
    }
    fn read_decode_packet(
        &self,
        r: &mut dyn ReadSeek,
        section: usize,
        packet: usize,
    ) -> Result<Vec<u8>> {
        flac::FlacFile::read_decode_packet(self, r, section, packet)
    }
}

impl Container for ogg::OggFile {
    fn format_name(&self) -> &'static str {
        "Ogg"
    }
    fn details(&self) -> String {
        let n = self.links();
        let chain = if n > 1 {
            format!(", {n} chained streams")
        } else {
            String::new()
        };
        format!("{}{chain}", self.codec().name())
    }
    fn index(&self) -> &PacketIndex {
        ogg::OggFile::index(self)
    }
    fn capabilities(&self) -> CutCapabilities {
        ogg::OggFile::capabilities(self)
    }
    fn warnings(&self) -> &[String] {
        ogg::OggFile::warnings(self)
    }
    fn tags(&self) -> &Tags {
        ogg::OggFile::tags(self)
    }
    fn chapters(&self) -> &[Chapter] {
        ogg::OggFile::chapters(self)
    }
    fn write_cut(
        &self,
        r: &mut dyn ReadSeek,
        plans: &[CutPlan],
        tags: Option<&Tags>,
        w: &mut dyn WriteSeek,
    ) -> Result<()> {
        ogg::OggFile::write_cut(self, r, plans, tags, w)
    }
    fn decode_sections(&self, r: &mut dyn ReadSeek) -> Result<Vec<DecodeSection>> {
        ogg::OggFile::decode_sections(self, r)
    }
    fn read_decode_packet(
        &self,
        r: &mut dyn ReadSeek,
        section: usize,
        packet: usize,
    ) -> Result<Vec<u8>> {
        ogg::OggFile::read_decode_packet(self, r, section, packet)
    }
}

impl Container for mp3::Mp3File {
    fn format_name(&self) -> &'static str {
        "MP3"
    }
    fn details(&self) -> String {
        mp3::Mp3File::details(self)
    }
    fn index(&self) -> &PacketIndex {
        mp3::Mp3File::index(self)
    }
    fn capabilities(&self) -> CutCapabilities {
        mp3::Mp3File::capabilities(self)
    }
    fn warnings(&self) -> &[String] {
        mp3::Mp3File::warnings(self)
    }
    fn tags(&self) -> &Tags {
        mp3::Mp3File::tags(self)
    }
    fn write_cut(
        &self,
        r: &mut dyn ReadSeek,
        plans: &[CutPlan],
        tags: Option<&Tags>,
        w: &mut dyn WriteSeek,
    ) -> Result<()> {
        mp3::Mp3File::write_cut(self, r, plans, tags, w)
    }
    fn decode_sections(&self, r: &mut dyn ReadSeek) -> Result<Vec<DecodeSection>> {
        mp3::Mp3File::decode_sections(self, r)
    }
    fn read_decode_packet(
        &self,
        r: &mut dyn ReadSeek,
        section: usize,
        packet: usize,
    ) -> Result<Vec<u8>> {
        mp3::Mp3File::read_decode_packet(self, r, section, packet)
    }
}

impl Container for mkv::MkvFile {
    fn format_name(&self) -> &'static str {
        if self.doc_type() == "webm" {
            "WebM"
        } else {
            "Matroska"
        }
    }
    fn details(&self) -> String {
        match self.other_tracks() {
            0 => "Opus".into(),
            n => format!("Opus, {n} other track(s) ignored"),
        }
    }
    fn index(&self) -> &PacketIndex {
        mkv::MkvFile::index(self)
    }
    fn capabilities(&self) -> CutCapabilities {
        mkv::MkvFile::capabilities(self)
    }
    fn warnings(&self) -> &[String] {
        mkv::MkvFile::warnings(self)
    }
    fn tags(&self) -> &Tags {
        mkv::MkvFile::tags(self)
    }
    fn write_cut(
        &self,
        r: &mut dyn ReadSeek,
        plans: &[CutPlan],
        tags: Option<&Tags>,
        w: &mut dyn WriteSeek,
    ) -> Result<()> {
        mkv::MkvFile::write_cut(self, r, plans, tags, w)
    }
    fn decode_sections(&self, r: &mut dyn ReadSeek) -> Result<Vec<DecodeSection>> {
        mkv::MkvFile::decode_sections(self, r)
    }
    fn read_decode_packet(
        &self,
        r: &mut dyn ReadSeek,
        section: usize,
        packet: usize,
    ) -> Result<Vec<u8>> {
        mkv::MkvFile::read_decode_packet(self, r, section, packet)
    }
}

impl Container for mp4::Mp4File {
    fn format_name(&self) -> &'static str {
        "MP4"
    }
    fn details(&self) -> String {
        let video = self
            .video()
            .map(|v| format!(" with {} video", v.index().info().codec.name()))
            .unwrap_or_default();
        match self.other_tracks() {
            0 => format!("{}{video}", self.codec().name()),
            n => format!("{}{video}, {n} other track(s) ignored", self.codec().name()),
        }
    }
    fn index(&self) -> &PacketIndex {
        mp4::Mp4File::index(self)
    }
    fn capabilities(&self) -> CutCapabilities {
        mp4::Mp4File::capabilities(self)
    }
    fn warnings(&self) -> &[String] {
        mp4::Mp4File::warnings(self)
    }
    fn tags(&self) -> &Tags {
        mp4::Mp4File::tags(self)
    }
    fn video(&self) -> Option<&VideoIndex> {
        mp4::Mp4File::video(self).map(mp4::video::Mp4Video::index)
    }
    fn video_codec_config(&self) -> &[u8] {
        mp4::Mp4File::video(self).map_or(&[], mp4::video::Mp4Video::codec_config)
    }
    fn video_offset(&self) -> MediaTime {
        self.video_timeline_offset().unwrap_or(0)
    }
    fn plan(&self, ranges: &[Range<SamplePos>]) -> std::result::Result<Vec<CutPlan>, PlanError> {
        mp4::Mp4File::plan(self, ranges)
    }
    fn cut_notes(&self, r: &mut dyn ReadSeek) -> Result<Vec<String>> {
        Ok(if self.uses_noise_substitution(r)? {
            vec![mp4::aac::NOISE_NOTE.to_owned()]
        } else {
            Vec::new()
        })
    }
    fn write_cut(
        &self,
        r: &mut dyn ReadSeek,
        plans: &[CutPlan],
        tags: Option<&Tags>,
        w: &mut dyn WriteSeek,
    ) -> Result<()> {
        mp4::Mp4File::write_cut(self, r, plans, tags, w)
    }
    fn decode_sections(&self, r: &mut dyn ReadSeek) -> Result<Vec<DecodeSection>> {
        mp4::Mp4File::decode_sections(self, r)
    }
    fn read_decode_packet(
        &self,
        r: &mut dyn ReadSeek,
        section: usize,
        packet: usize,
    ) -> Result<Vec<u8>> {
        mp4::Mp4File::read_decode_packet(self, r, section, packet)
    }
}

impl Container for ac3::Ac3File {
    fn format_name(&self) -> &'static str {
        "AC-3"
    }
    fn details(&self) -> String {
        self.params().layout()
    }
    fn index(&self) -> &PacketIndex {
        ac3::Ac3File::index(self)
    }
    fn capabilities(&self) -> CutCapabilities {
        ac3::Ac3File::capabilities(self)
    }
    fn warnings(&self) -> &[String] {
        ac3::Ac3File::warnings(self)
    }
    fn tags(&self) -> &Tags {
        ac3::Ac3File::tags(self)
    }
    fn cut_notes(&self, r: &mut dyn ReadSeek) -> Result<Vec<String>> {
        Ok(if self.uses_dither(r)? {
            vec![ac3::DITHER_NOTE.to_owned()]
        } else {
            Vec::new()
        })
    }
    fn write_cut(
        &self,
        r: &mut dyn ReadSeek,
        plans: &[CutPlan],
        tags: Option<&Tags>,
        w: &mut dyn WriteSeek,
    ) -> Result<()> {
        ac3::Ac3File::write_cut(self, r, plans, tags, w)
    }
    fn decode_sections(&self, r: &mut dyn ReadSeek) -> Result<Vec<DecodeSection>> {
        ac3::Ac3File::decode_sections(self, r)
    }
    fn read_decode_packet(
        &self,
        r: &mut dyn ReadSeek,
        section: usize,
        packet: usize,
    ) -> Result<Vec<u8>> {
        ac3::Ac3File::read_decode_packet(self, r, section, packet)
    }
}

impl Container for adts::AdtsFile {
    fn format_name(&self) -> &'static str {
        "ADTS"
    }
    fn details(&self) -> String {
        self.params().profile_name().into()
    }
    fn index(&self) -> &PacketIndex {
        adts::AdtsFile::index(self)
    }
    fn capabilities(&self) -> CutCapabilities {
        adts::AdtsFile::capabilities(self)
    }
    fn warnings(&self) -> &[String] {
        adts::AdtsFile::warnings(self)
    }
    fn tags(&self) -> &Tags {
        adts::AdtsFile::tags(self)
    }
    fn cut_notes(&self, r: &mut dyn ReadSeek) -> Result<Vec<String>> {
        Ok(if self.uses_noise_substitution(r)? {
            vec![mp4::aac::NOISE_NOTE.to_owned()]
        } else {
            Vec::new()
        })
    }
    fn write_cut(
        &self,
        r: &mut dyn ReadSeek,
        plans: &[CutPlan],
        tags: Option<&Tags>,
        w: &mut dyn WriteSeek,
    ) -> Result<()> {
        adts::AdtsFile::write_cut(self, r, plans, tags, w)
    }
    fn decode_sections(&self, r: &mut dyn ReadSeek) -> Result<Vec<DecodeSection>> {
        adts::AdtsFile::decode_sections(self, r)
    }
    fn read_decode_packet(
        &self,
        r: &mut dyn ReadSeek,
        section: usize,
        packet: usize,
    ) -> Result<Vec<u8>> {
        adts::AdtsFile::read_decode_packet(self, r, section, packet)
    }
}

/// Detects the container format from the first bytes of `r` and opens it.
pub fn open(r: &mut dyn ReadSeek) -> Result<Box<dyn Container>> {
    let mut head = [0u8; 12];
    r.seek(SeekFrom::Start(0))?;
    let n = std::io::Read::read(r, &mut head)?;
    let head = &head[..n];
    if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WAVE" {
        return Ok(Box::new(wav::WavFile::open(r)?));
    }
    // EA IFF 85: FORM, size, form type.
    if head.len() >= 12 && &head[0..4] == b"FORM" && matches!(&head[8..12], b"AIFF" | b"AIFC") {
        return Ok(Box::new(aiff::AiffFile::open(r)?));
    }
    if head.starts_with(b"fLaC") {
        return Ok(Box::new(flac::FlacFile::open(r)?));
    }
    // An ID3v2 tag precedes MP3 and sometimes FLAC: look behind it.
    if head.starts_with(b"ID3") {
        let len = r.seek(SeekFrom::End(0))?;
        let after = flac::skip_id3v2(r, len)?;
        let mut magic = [0u8; 4];
        r.seek(SeekFrom::Start(after))?;
        let n = std::io::Read::read(r, &mut magic)?;
        if &magic[..n] == b"fLaC" {
            return Ok(Box::new(flac::FlacFile::open(r)?));
        }
        if magic[..n].starts_with(&[0x0B, 0x77]) {
            return Ok(Box::new(ac3::Ac3File::open(r)?));
        }
        if adts::looks_like_adts(&magic[..n]) {
            return Ok(Box::new(adts::AdtsFile::open(r)?));
        }
        return Ok(Box::new(mp3::Mp3File::open(r)?));
    }
    // ADTS sync word (12 set bits) and layer 0, which MPEG audio does not
    // use (ISO/IEC 11172-3 2.4.2.3: "reserved"); before the MPEG test,
    // whose 11-bit sync matches ADTS too.
    if adts::looks_like_adts(head) {
        return Ok(Box::new(adts::AdtsFile::open(r)?));
    }
    // MPEG audio frame sync: 11 set bits.
    if head.len() >= 2 && head[0] == 0xFF && head[1] & 0xE0 == 0xE0 {
        return Ok(Box::new(mp3::Mp3File::open(r)?));
    }
    // A/52 5.3.1: the AC-3 sync word.
    if head.starts_with(&[0x0B, 0x77]) {
        return Ok(Box::new(ac3::Ac3File::open(r)?));
    }
    if head.starts_with(b"OggS") {
        return Ok(Box::new(ogg::OggFile::open(r)?));
    }
    // ISO/IEC 14496-12 4.3: ftyp comes first; old QuickTime files may
    // start with moov, or with mdat, wide or free before it.
    if head.len() >= 8 && matches!(&head[4..8], b"ftyp" | b"moov" | b"mdat" | b"wide" | b"free") {
        return Ok(Box::new(mp4::Mp4File::open(r)?));
    }
    // RFC 8794 8: the EBML header ID.
    if head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return Ok(Box::new(mkv::MkvFile::open(r)?));
    }
    Err(ContainerError::Unsupported(
        "unknown file format (supported: WAV, AIFF, FLAC, Ogg, MP3, Matroska/WebM, MP4/M4A, AAC, AC-3)"
            .into(),
    ))
}

/// Errors raised while reading or writing a container.
#[derive(Debug, thiserror::Error)]
pub enum ContainerError {
    /// The underlying reader or writer failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The file violates the container specification.
    #[error("malformed file at byte {offset}: {reason}")]
    Malformed {
        /// Byte offset where the problem was detected.
        offset: u64,
        /// Human-readable description.
        reason: String,
    },
    /// The file is valid but uses a feature this program does not support.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The output would exceed the size limits of the container.
    #[error("output too large: {0}")]
    TooLarge(String),
    /// A cut plan does not fit the source file it is applied to.
    #[error("invalid cut plan: {0}")]
    InvalidPlan(String),
}

/// Convenience alias for results of this crate.
pub type Result<T> = std::result::Result<T, ContainerError>;
