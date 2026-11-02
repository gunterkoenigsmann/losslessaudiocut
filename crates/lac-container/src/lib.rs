//! # lac-container
//!
//! Demuxers and muxers for lossless, packet-level cutting.
//!
//! Every format module reads a source file into a [`lac_core::PacketIndex`]
//! without decoding it, reports its [`lac_core::CutCapabilities`], and writes
//! new files by copying packets verbatim according to
//! [`lac_core::CutPlan`]s. Lossy audio is never decoded or re-encoded here.
//!
//! Supported so far: [`wav`] and [`flac`]. [`open`] detects the format
//! from the file's content and returns it behind the common [`Container`]
//! interface.

mod bits;
pub mod flac;
pub mod wav;

use std::io::{Read, Seek, SeekFrom, Write};

use lac_core::{CutCapabilities, CutPlan, PacketIndex};

/// Anything that can be read and seeked; used for trait objects.
pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek + ?Sized> ReadSeek for T {}

/// Anything that can be written and seeked; used for trait objects.
pub trait WriteSeek: Write + Seek {}
impl<T: Write + Seek + ?Sized> WriteSeek for T {}

/// The operations every supported container offers.
pub trait Container {
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
    /// Writes a new file containing the `actual` ranges of `plans`, joined.
    fn write_cut(
        &self,
        r: &mut dyn ReadSeek,
        plans: &[CutPlan],
        w: &mut dyn WriteSeek,
    ) -> Result<()>;
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
    fn write_cut(
        &self,
        r: &mut dyn ReadSeek,
        plans: &[CutPlan],
        w: &mut dyn WriteSeek,
    ) -> Result<()> {
        wav::WavFile::write_cut(self, r, plans, w)
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
    fn write_cut(
        &self,
        r: &mut dyn ReadSeek,
        plans: &[CutPlan],
        w: &mut dyn WriteSeek,
    ) -> Result<()> {
        flac::FlacFile::write_cut(self, r, plans, w)
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
    // FLAC may be preceded by an ID3v2 tag; the FLAC parser skips it and
    // fails cleanly if no FLAC stream follows.
    if head.starts_with(b"fLaC") || head.starts_with(b"ID3") {
        return Ok(Box::new(flac::FlacFile::open(r)?));
    }
    Err(ContainerError::Unsupported(
        "unknown file format (supported: WAV, FLAC)".into(),
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
