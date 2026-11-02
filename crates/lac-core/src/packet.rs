//! Packets: the smallest units a lossless cut can keep or drop.

use std::ops::Range;

use crate::SamplePos;

/// Basic parameters of an audio stream, independent of its codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioStreamInfo {
    /// Sample frames per second.
    pub sample_rate: u32,
    /// Number of channels (1 = mono). No upper limit is assumed.
    pub channels: u16,
}

/// Properties of a single packet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PacketFlags {
    /// The packet can be decoded without any preceding packet.
    ///
    /// True for every PCM block and FLAC frame. For lossy codecs it depends
    /// on the codec; for video it would mark keyframes.
    pub independent: bool,
}

/// One compressed (or, for PCM, uncompressed) unit as found in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketInfo {
    /// Position of the packet payload in the source file, in bytes.
    pub byte_offset: u64,
    /// Length of the payload in bytes.
    pub byte_len: u32,
    /// Presentation position of the packet's first sample frame.
    pub first_sample: SamplePos,
    /// Number of sample frames this packet decodes to.
    pub n_samples: u32,
    /// Additional properties.
    pub flags: PacketFlags,
}

impl PacketInfo {
    /// The half-open range of sample frames this packet covers.
    pub fn sample_range(&self) -> Range<SamplePos> {
        self.first_sample..self.first_sample + SamplePos::from(self.n_samples)
    }
}

/// Error raised when a packet list does not form a valid timeline.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IndexError {
    /// Packets must follow each other without gaps or overlaps, starting at 0.
    #[error("packet {index} starts at sample {found}, expected {expected}")]
    NotContiguous {
        /// Index of the offending packet.
        index: usize,
        /// Sample position the packet should start at.
        expected: SamplePos,
        /// Sample position it actually starts at.
        found: SamplePos,
    },
    /// Every packet must contain at least one sample frame.
    #[error("packet {index} contains no samples")]
    EmptyPacket {
        /// Index of the offending packet.
        index: usize,
    },
}

/// The immutable, gap-free sequence of packets of one audio stream.
///
/// Built once by a demuxer when a file is opened. Invariant (checked by
/// [`PacketIndex::new`]): packet *n+1* starts exactly where packet *n* ends,
/// and the first packet starts at sample 0. This makes lookups by sample
/// position a simple binary search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketIndex {
    info: AudioStreamInfo,
    packets: Vec<PacketInfo>,
}

impl PacketIndex {
    /// Creates an index after checking the contiguity invariant.
    pub fn new(info: AudioStreamInfo, packets: Vec<PacketInfo>) -> Result<Self, IndexError> {
        let mut expected = 0;
        for (index, p) in packets.iter().enumerate() {
            if p.n_samples == 0 {
                return Err(IndexError::EmptyPacket { index });
            }
            if p.first_sample != expected {
                return Err(IndexError::NotContiguous {
                    index,
                    expected,
                    found: p.first_sample,
                });
            }
            expected = p.sample_range().end;
        }
        Ok(Self { info, packets })
    }

    /// Stream parameters.
    pub fn info(&self) -> AudioStreamInfo {
        self.info
    }

    /// All packets in presentation order.
    pub fn packets(&self) -> &[PacketInfo] {
        &self.packets
    }

    /// Total length of the stream in sample frames.
    pub fn total_samples(&self) -> SamplePos {
        self.packets.last().map_or(0, |p| p.sample_range().end)
    }

    /// Index of the packet containing sample frame `pos`, or `None` if `pos`
    /// is at or beyond the end of the stream.
    pub fn packet_containing(&self, pos: SamplePos) -> Option<usize> {
        if pos >= self.total_samples() {
            return None;
        }
        // The first packet whose end is beyond `pos` contains it.
        Some(
            self.packets
                .partition_point(|p| p.sample_range().end <= pos),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: AudioStreamInfo = AudioStreamInfo {
        sample_rate: 48_000,
        channels: 2,
    };

    fn packets(lengths: &[u32]) -> Vec<PacketInfo> {
        let mut pos = 0;
        lengths
            .iter()
            .map(|&n| {
                let p = PacketInfo {
                    byte_offset: pos * 4,
                    byte_len: n * 4,
                    first_sample: pos,
                    n_samples: n,
                    flags: PacketFlags { independent: true },
                };
                pos += u64::from(n);
                p
            })
            .collect()
    }

    #[test]
    fn lookup_finds_containing_packet() {
        let idx = PacketIndex::new(INFO, packets(&[10, 5, 20])).expect("valid");
        assert_eq!(idx.total_samples(), 35);
        assert_eq!(idx.packet_containing(0), Some(0));
        assert_eq!(idx.packet_containing(9), Some(0));
        assert_eq!(idx.packet_containing(10), Some(1));
        assert_eq!(idx.packet_containing(14), Some(1));
        assert_eq!(idx.packet_containing(15), Some(2));
        assert_eq!(idx.packet_containing(34), Some(2));
        assert_eq!(idx.packet_containing(35), None);
    }

    #[test]
    fn empty_index_is_valid() {
        let idx = PacketIndex::new(INFO, Vec::new()).expect("valid");
        assert_eq!(idx.total_samples(), 0);
        assert_eq!(idx.packet_containing(0), None);
    }

    #[test]
    fn gaps_are_rejected() {
        let mut p = packets(&[10, 10]);
        p[1].first_sample = 11;
        assert_eq!(
            PacketIndex::new(INFO, p),
            Err(IndexError::NotContiguous {
                index: 1,
                expected: 10,
                found: 11
            })
        );
    }

    #[test]
    fn empty_packets_are_rejected() {
        let mut p = packets(&[10, 10]);
        p[1].n_samples = 0;
        assert_eq!(
            PacketIndex::new(INFO, p),
            Err(IndexError::EmptyPacket { index: 1 })
        );
    }
}
