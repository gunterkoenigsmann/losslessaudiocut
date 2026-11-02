//! Cut planning: mapping a wanted sample range onto whole packets.
//!
//! A lossless cut copies whole packets. Where a cut point falls inside a
//! packet, there are two possibilities:
//!
//! - The format can make the cut sample-exact anyway, either by trimming
//!   metadata (MP3 LAME tag, MP4 edit list, Ogg granule positions) or because
//!   boundary packets can be rewritten losslessly (PCM, FLAC). The plan then
//!   records how many samples to drop at either end ([`CutPlan::trim_start`],
//!   [`CutPlan::trim_end`]).
//! - Otherwise the cut is widened outwards to the packet boundary, so that no
//!   wanted audio is ever lost. [`CutPlan::actual`] reports the result.
//!
//! Some codecs additionally need *pre-roll*: packets before the first wanted
//! one that the decoder must see to build up its state (MDCT overlap, MP3 bit
//! reservoir). They are included in the plan and, where possible, trimmed.

use std::ops::Range;

use crate::{PacketIndex, SamplePos};

/// What a format can do when cutting. Determined once per opened file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CutCapabilities {
    /// Packets that must be kept in front of the first wanted packet so the
    /// decoder produces correct output for it.
    pub preroll_packets: u32,
    /// The container can tell the decoder to drop leading samples.
    pub can_trim_start: bool,
    /// The container can tell the decoder to drop trailing samples.
    pub can_trim_end: bool,
    /// Boundary packets may be rewritten without any loss (PCM, FLAC), which
    /// makes both cut points sample-exact.
    pub lossless_reencode: bool,
}

impl CutCapabilities {
    /// Capabilities of uncompressed PCM: every sample is addressable.
    pub const PCM: Self = Self {
        preroll_packets: 0,
        can_trim_start: false,
        can_trim_end: false,
        lossless_reencode: true,
    };

    fn exact_start(&self) -> bool {
        self.can_trim_start || self.lossless_reencode
    }

    fn exact_end(&self) -> bool {
        self.can_trim_end || self.lossless_reencode
    }
}

/// The result of planning one kept range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CutPlan {
    /// Packets to copy, as indexes into the [`PacketIndex`], including
    /// pre-roll packets. Empty for an empty range.
    pub packets: Range<usize>,
    /// Sample frames to drop from the start of the decoded packets.
    pub trim_start: SamplePos,
    /// Sample frames to drop from the end of the decoded packets.
    pub trim_end: SamplePos,
    /// The source range the listener will actually get. Contains the
    /// requested range, and equals it when the format is sample-exact.
    pub actual: Range<SamplePos>,
}

/// Errors of [`plan_cut`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    /// The requested range is reversed or reaches beyond the stream.
    #[error("range {start}..{end} is outside the stream (length {len})")]
    OutOfRange {
        /// Start of the requested range.
        start: SamplePos,
        /// End of the requested range.
        end: SamplePos,
        /// Length of the stream.
        len: SamplePos,
    },
}

/// Plans the lossless extraction of the source range `range`.
///
/// Invariant of the result, relied upon by muxers and checked by property
/// tests: the decoded length of `packets` minus `trim_start` and `trim_end`
/// equals the length of `actual`.
pub fn plan_cut(
    index: &PacketIndex,
    caps: CutCapabilities,
    range: Range<SamplePos>,
) -> Result<CutPlan, PlanError> {
    let len = index.total_samples();
    if range.start > range.end || range.end > len {
        return Err(PlanError::OutOfRange {
            start: range.start,
            end: range.end,
            len,
        });
    }
    if range.is_empty() {
        return Ok(CutPlan {
            packets: 0..0,
            trim_start: 0,
            trim_end: 0,
            actual: range.start..range.start,
        });
    }
    let packets = index.packets();
    // Both lookups succeed: range is non-empty and within the stream.
    let first = index.packet_containing(range.start).unwrap_or(0);
    let last = index
        .packet_containing(range.end - 1)
        .unwrap_or(packets.len() - 1);
    let preroll_first = first.saturating_sub(caps.preroll_packets as usize);
    let preroll_len = packets[first].first_sample - packets[preroll_first].first_sample;

    let (trim_start, actual_start) = if caps.exact_start() {
        // Drop the pre-roll output plus the head of the first wanted packet.
        (
            preroll_len + (range.start - packets[first].first_sample),
            range.start,
        )
    } else {
        // Without trimming the pre-roll is audible, so it belongs to the
        // actual range.
        (0, packets[preroll_first].first_sample)
    };
    let last_end = packets[last].sample_range().end;
    let (trim_end, actual_end) = if caps.exact_end() {
        (last_end - range.end, range.end)
    } else {
        (0, last_end)
    };
    Ok(CutPlan {
        packets: preroll_first..last + 1,
        trim_start,
        trim_end,
        actual: actual_start..actual_end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioStreamInfo, PacketFlags, PacketInfo};
    use proptest::prelude::*;

    fn index(lengths: &[u32]) -> PacketIndex {
        let mut pos = 0;
        let packets = lengths
            .iter()
            .map(|&n| {
                let p = PacketInfo {
                    byte_offset: pos,
                    byte_len: n,
                    first_sample: pos,
                    n_samples: n,
                    flags: PacketFlags::default(),
                };
                pos += u64::from(n);
                p
            })
            .collect();
        PacketIndex::new(
            AudioStreamInfo {
                sample_rate: 44_100,
                channels: 1,
            },
            packets,
        )
        .expect("contiguous")
    }

    const MP3_LIKE: CutCapabilities = CutCapabilities {
        preroll_packets: 1,
        can_trim_start: false,
        can_trim_end: false,
        lossless_reencode: false,
    };

    #[test]
    fn packet_granular_cut_widens_outwards() {
        let idx = index(&[1152; 10]);
        let plan = plan_cut(&idx, MP3_LIKE, 2000..5000).expect("in range");
        // 2000 is in packet 1, so packet 0 is pre-roll; 4999 is in packet 4.
        assert_eq!(plan.packets, 0..5);
        assert_eq!(plan.actual, 0..5760);
        assert_eq!((plan.trim_start, plan.trim_end), (0, 0));
    }

    #[test]
    fn pcm_cut_is_sample_exact() {
        let idx = index(&[4096; 4]);
        let plan = plan_cut(&idx, CutCapabilities::PCM, 100..5000).expect("in range");
        assert_eq!(plan.packets, 0..2);
        assert_eq!(plan.actual, 100..5000);
        assert_eq!((plan.trim_start, plan.trim_end), (100, 8192 - 5000));
    }

    #[test]
    fn out_of_range_is_error() {
        let idx = index(&[10; 3]);
        assert!(plan_cut(&idx, CutCapabilities::PCM, 0..31).is_err());
    }

    fn caps() -> impl Strategy<Value = CutCapabilities> {
        (0u32..3, any::<bool>(), any::<bool>(), any::<bool>()).prop_map(|(p, s, e, r)| {
            CutCapabilities {
                preroll_packets: p,
                can_trim_start: s,
                can_trim_end: e,
                lossless_reencode: r,
            }
        })
    }

    proptest! {
        #[test]
        fn plan_invariants(
            lengths in proptest::collection::vec(1u32..300, 1..20),
            a in 0u64..6000, b in 0u64..6000,
            caps in caps(),
        ) {
            let idx = index(&lengths);
            let len = idx.total_samples();
            let (a, b) = (a.min(len), b.min(len));
            let range = a.min(b)..a.max(b);
            let plan = plan_cut(&idx, caps, range.clone()).expect("in range");

            // No wanted audio is lost.
            prop_assert!(plan.actual.start <= range.start && plan.actual.end >= range.end);
            // Sample-exact capabilities give exactly what was asked.
            if caps.exact_start() { prop_assert_eq!(plan.actual.start, range.start); }
            if caps.exact_end() { prop_assert_eq!(plan.actual.end, range.end); }
            // The plan stays within the index.
            prop_assert!(plan.packets.end <= idx.packets().len());
            // Decoded length minus trims equals the actual length.
            let decoded: u64 = idx.packets()[plan.packets.clone()]
                .iter().map(|p| u64::from(p.n_samples)).sum();
            prop_assert_eq!(decoded - plan.trim_start - plan.trim_end,
                            plan.actual.end - plan.actual.start);
        }
    }
}
