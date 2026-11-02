//! # lac-container
//!
//! Demuxers and muxers for lossless, packet-level cutting.
//!
//! Every format module reads a source file into a [`lac_core::PacketIndex`]
//! without decoding it, reports its [`lac_core::CutCapabilities`], and writes
//! new files by copying packets verbatim according to
//! [`lac_core::CutPlan`]s. Lossy audio is never decoded or re-encoded here.
//!
//! Supported so far: [`wav`].

pub mod wav;

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
