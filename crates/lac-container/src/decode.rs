//! What a decoder needs to turn a source back into PCM.
//!
//! The containers never decode lossy audio themselves (see the crate
//! documentation); they describe the stream so that `lac-decode` can feed
//! a codec library with *our* packets. That keeps the packet index the
//! single source of truth: the decoded timeline is exactly the one the cut
//! planner works on.
//!
//! A source consists of one or more [`DecodeSection`]s, each decoded by a
//! fresh decoder: one per chained Ogg stream, a single one for every other
//! format. Of each section's decoder output only [`DecodeSection::keep`]
//! belongs to the timeline; the rest is encoder delay, Opus pre-skip or end
//! padding. The kept parts of all sections, concatenated, are the timeline
//! of [`crate::Container::index`].

use std::ops::Range;

use crate::wav::SampleEncoding;

/// The codec of a section and its fixed parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// Uncompressed samples as stored in a WAV file.
    Pcm {
        /// How each sample is encoded.
        encoding: SampleEncoding,
        /// Bits per sample as stored (8, 16, 24, 32 or 64).
        bits_per_sample: u16,
        /// Bytes per sample frame (all channels).
        block_align: u16,
    },
    /// FLAC; the extradata is the 34-byte STREAMINFO block (RFC 9639 8.2).
    Flac,
    /// Vorbis; the extradata holds the three header packets in Xiph lacing
    /// (the layout libavcodec expects for Vorbis extradata).
    Vorbis,
    /// Opus; the extradata is the `OpusHead` packet (RFC 7845 5.1).
    Opus,
    /// Speex; the extradata is the 80-byte Speex header packet.
    Speex,
    /// MPEG-1/2/2.5 audio layer III; no extradata. Packets are whole
    /// frames, fed in file order so that the decoder can follow the bit
    /// reservoir itself.
    Mp3,
}

/// One part of a source that one decoder instance decodes from start to
/// end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeSection {
    /// The codec.
    pub codec: Codec,
    /// Sample rate of the decoder output in Hz.
    pub sample_rate: u32,
    /// Number of channels.
    pub channels: u16,
    /// Codec-specific setup data, see [`Codec`].
    pub extradata: Vec<u8>,
    /// One entry per packet to feed (read with
    /// [`crate::Container::read_decode_packet`]): how many sample frames
    /// of decoder output, counted like [`DecodeSection::keep`], exist once
    /// that packet and all before it have been decoded. Non-decreasing.
    ///
    /// Decoders emit a packet's output right after it, so this also places
    /// the output of a fresh decoder that starts in the middle of the
    /// section: its last output sample after packet *j* lies just before
    /// `packet_ends[j]`. That is what random access (`lac-decode`'s
    /// `decode_range`) relies on.
    pub packet_ends: Vec<u64>,
    /// The sample frames of the decoder output (counted from the first
    /// output sample of this section) that belong to the timeline.
    pub keep: Range<u64>,
}

impl DecodeSection {
    /// Number of packets to feed.
    pub fn packets(&self) -> usize {
        self.packet_ends.len()
    }
}

/// [`DecodeSection::packet_ends`] of packets that output `lens` sample
/// frames each.
pub(crate) fn running_ends(lens: impl IntoIterator<Item = u64>) -> Vec<u64> {
    lens.into_iter()
        .scan(0u64, |end, n| {
            *end += n;
            Some(*end)
        })
        .collect()
}

/// Reads index entry `entry` of `index` as one contiguous byte range: the
/// decode packet of formats whose packets are the index entries.
pub(crate) fn read_index_packet<R: std::io::Read + std::io::Seek + ?Sized>(
    index: &lac_core::PacketIndex,
    r: &mut R,
    entry: usize,
) -> crate::Result<Vec<u8>> {
    let p = index
        .packets()
        .get(entry)
        .ok_or_else(|| crate::ContainerError::InvalidPlan(format!("no packet {entry}")))?;
    r.seek(std::io::SeekFrom::Start(p.byte_offset))?;
    let mut out = vec![0u8; p.byte_len as usize];
    r.read_exact(&mut out)?;
    Ok(out)
}

/// The error for a section number a single-section format does not have.
pub(crate) fn no_section(section: usize) -> crate::ContainerError {
    crate::ContainerError::InvalidPlan(format!("no decode section {section}"))
}

/// Packs header packets in Xiph lacing: the number of packets minus one,
/// the lengths of all but the last as runs of 255-valued bytes plus a
/// remainder, then the packets themselves.
pub(crate) fn xiph_lace(packets: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![packets.len().saturating_sub(1) as u8];
    for p in &packets[..packets.len().saturating_sub(1)] {
        out.extend(std::iter::repeat_n(255u8, p.len() / 255));
        out.push((p.len() % 255) as u8);
    }
    for p in packets {
        out.extend_from_slice(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xiph_lacing_layout() {
        let a = vec![1u8; 30];
        let b = vec![2u8; 300];
        let c = vec![3u8; 7];
        let laced = xiph_lace(&[a.clone(), b.clone(), c.clone()]);
        // 2 = three packets; 30; 300 = 255 + 45; the last length is implied.
        assert_eq!(&laced[..4], &[2, 30, 255, 45]);
        assert_eq!(laced.len(), 4 + 30 + 300 + 7);
        assert_eq!(&laced[4..34], &a[..]);
        assert_eq!(&laced[334..], &c[..]);
    }
}
