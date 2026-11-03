//! # lac-peaks
//!
//! The waveform summary of a source: per packet and channel the minimum,
//! maximum and RMS of the decoded samples (*level 0*), plus a pyramid of
//! coarser levels in which every entry merges [`FAN_OUT`] entries of the
//! level below. A zoomed-out view then costs O(pixels · log packets),
//! independent of the recording's length.
//!
//! Levels are indexed by packet number, not by time, because packet
//! lengths differ between codecs and vary within Vorbis; a pixel column is
//! mapped to a packet range by binary search on the packets' first sample.
//! See `docs/architecture.md`, section "Waveform display".
//!
//! The summary is built front to back from decoded samples
//! ([`PeakBuilder::push`]) and can be queried while it grows, so the
//! display fills in progressively. This crate does no decoding itself.

use std::ops::Range;

use lac_core::{PacketIndex, SamplePos};

/// Entries of a level merged into one entry of the level above.
pub const FAN_OUT: usize = 16;

/// Summary of one channel over a stretch of samples.
///
/// Sample values are stored as `i16` (full scale = ±32767), plenty for
/// display; RMS as a fraction of full scale times 65535.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peak {
    /// Smallest sample value.
    pub min: i16,
    /// Largest sample value.
    pub max: i16,
    /// Root mean square, 65535 = full scale.
    pub rms: u16,
}

impl Peak {
    /// The identity of [`Peak::merge`] for min/max; used before any sample
    /// was seen.
    const EMPTY: Peak = Peak {
        min: i16::MAX,
        max: i16::MIN,
        rms: 0,
    };

    /// Combines min and max of two peaks (RMS is merged separately, as it
    /// needs the sample counts).
    fn merge_min_max(self, o: Peak) -> Peak {
        Peak {
            min: self.min.min(o.min),
            max: self.max.max(o.max),
            rms: 0,
        }
    }

    /// The RMS as a fraction of full scale.
    pub fn rms_f32(self) -> f32 {
        f32::from(self.rms) / 65535.0
    }
}

/// Converts a decoded sample (`-1.0..=1.0`) to the stored scale.
fn to_i16(x: f32) -> i16 {
    // `as` saturates, and NaN becomes 0.
    (x * 32767.0).round() as i16
}

/// The waveform summary of one source.
#[derive(Debug, Clone)]
pub struct PeakSummary {
    channels: usize,
    /// First sample of every packet, plus the total length at the end.
    starts: Vec<SamplePos>,
    /// `levels[k]` has one entry per [`FAN_OUT`]`^k` packets; each entry
    /// holds one [`Peak`] per channel, channel-interleaved. Only complete
    /// entries are stored (the last one of each level becomes complete
    /// when the build finishes).
    levels: Vec<Vec<Peak>>,
}

impl PeakSummary {
    /// Number of channels.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Number of packets (level-0 entries) of the source.
    pub fn packets(&self) -> usize {
        self.starts.len() - 1
    }

    /// Number of packets summarised so far.
    pub fn packets_done(&self) -> usize {
        self.levels[0].len() / self.channels.max(1)
    }

    /// Length of the source in sample frames.
    pub fn total_samples(&self) -> SamplePos {
        self.starts[self.starts.len() - 1]
    }

    /// The summary of packet `packet`, one [`Peak`] per channel, or `None`
    /// if it is not summarised yet.
    pub fn packet(&self, packet: usize) -> Option<&[Peak]> {
        let c = self.channels;
        self.levels[0].get(packet * c..(packet + 1) * c)
    }

    /// Packets overlapping the sample range `r` (empty for an empty range).
    pub fn packets_overlapping(&self, r: Range<SamplePos>) -> Range<usize> {
        if r.start >= r.end {
            return 0..0;
        }
        // First packet whose end lies after r.start, first whose start is
        // at or after r.end.
        let a = self.starts[1..].partition_point(|&e| e <= r.start);
        let b = self.starts[..self.packets()].partition_point(|&s| s < r.end);
        a..b.max(a)
    }

    /// Min, max and RMS per channel over the packets `p`, using the
    /// coarsest pyramid levels that fit (O(log) entries). `None` if the
    /// range is empty or not summarised yet.
    pub fn summarize(&self, p: Range<usize>) -> Option<Vec<Peak>> {
        if p.is_empty() || p.end > self.packets_done() {
            return None;
        }
        let c = self.channels;
        let mut acc = vec![Peak::EMPTY; c];
        // Sum over the covered entries of rms² · samples, per channel.
        let mut sq = vec![0f64; c];
        let mut take = |level: usize, i: usize| {
            let e = &self.levels[level][i * c..(i + 1) * c];
            let span = FAN_OUT.pow(level as u32);
            let n = self.span_samples(i * span..((i + 1) * span).min(self.packets()));
            for ch in 0..c {
                acc[ch] = acc[ch].merge_min_max(e[ch]);
                sq[ch] += (f64::from(e[ch].rms) / 65535.0).powi(2) * n as f64;
            }
        };
        // Cover [a, b) left to right with the biggest aligned entry that
        // lies inside it and exists. An upper-level entry covers FAN_OUT^k
        // packets, the last one of a level possibly fewer.
        let packets = self.packets();
        let (mut a, b) = (p.start, p.end);
        while a < b {
            let mut level = 0;
            for k in (1..self.levels.len()).rev() {
                let span = FAN_OUT.pow(k as u32);
                let end = (a + span).min(packets);
                if a.is_multiple_of(span) && end <= b && (a / span + 1) * c <= self.levels[k].len()
                {
                    level = k;
                    break;
                }
            }
            let span = FAN_OUT.pow(level as u32);
            take(level, a / span);
            a = (a + span).min(packets);
        }
        let n = self.span_samples(p).max(1) as f64;
        for ch in 0..c {
            acc[ch].rms = ((sq[ch] / n).sqrt() * 65535.0).round().min(65535.0) as u16;
        }
        Some(acc)
    }

    /// Splits `range` into `columns` equal pixel columns and summarises
    /// each over the packets it touches. A column is `None` while its
    /// packets are not summarised yet.
    ///
    /// Columns narrower than a packet show that whole packet; the caller
    /// switches to decoded samples there (see "Data sources per zoom
    /// range" in the architecture).
    pub fn columns(&self, range: Range<SamplePos>, columns: usize) -> Vec<Option<Vec<Peak>>> {
        let len = range.end.saturating_sub(range.start);
        (0..columns as u64)
            .map(|i| {
                let n = columns as u64;
                // u128 so that multi-hour ranges times wide screens cannot
                // overflow.
                let at =
                    |k: u64| range.start + (u128::from(len) * u128::from(k) / u128::from(n)) as u64;
                let (a, b) = (at(i), at(i + 1).max(at(i) + 1));
                self.summarize(self.packets_overlapping(a..b))
            })
            .collect()
    }

    /// Sample frames covered by the packets `p`.
    fn span_samples(&self, p: Range<usize>) -> SamplePos {
        self.starts[p.end] - self.starts[p.start]
    }
}

/// Builds a [`PeakSummary`] from decoded samples in timeline order.
#[derive(Debug, Clone)]
pub struct PeakBuilder {
    summary: PeakSummary,
    /// Packet currently being filled, and how many of its samples are in.
    packet: usize,
    done_in_packet: SamplePos,
    /// Running min/max and sum of squares of the current packet.
    cur: Vec<Peak>,
    cur_sq: Vec<f64>,
}

impl PeakBuilder {
    /// Starts an empty summary for the packets of `index`.
    pub fn new(index: &PacketIndex) -> Self {
        let channels = usize::from(index.info().channels).max(1);
        let mut starts: Vec<SamplePos> = index.packets().iter().map(|p| p.first_sample).collect();
        starts.push(index.total_samples());
        PeakBuilder {
            summary: PeakSummary {
                channels,
                starts,
                levels: vec![Vec::new()],
            },
            packet: 0,
            done_in_packet: 0,
            cur: vec![Peak::EMPTY; channels],
            cur_sq: vec![0.0; channels],
        }
    }

    /// The summary as far as it is built.
    pub fn summary(&self) -> &PeakSummary {
        &self.summary
    }

    /// Adds the next decoded sample frames, interleaved by channel, in
    /// timeline order. Samples beyond the end of the index are ignored.
    pub fn push(&mut self, interleaved: &[f32]) {
        let c = self.summary.channels;
        for frame in interleaved.chunks_exact(c) {
            if self.packet >= self.summary.packets() {
                return;
            }
            for (ch, &x) in frame.iter().enumerate() {
                let v = to_i16(x);
                let p = &mut self.cur[ch];
                p.min = p.min.min(v);
                p.max = p.max.max(v);
                self.cur_sq[ch] += f64::from(x) * f64::from(x);
            }
            self.done_in_packet += 1;
            let len = self.summary.span_samples(self.packet..self.packet + 1);
            if self.done_in_packet >= len {
                self.finish_packet(len);
            }
        }
    }

    /// Completes the summary: the last, partial entries of the upper
    /// levels are added. Packets the decoder never reached stay missing.
    pub fn finish(mut self) -> PeakSummary {
        let c = self.summary.channels;
        for level in 0.. {
            if level + 1 >= self.summary.levels.len() {
                // Add levels until one entry covers everything summarised.
                if self.summary.levels[level].len() <= c {
                    break;
                }
                self.summary.levels.push(Vec::new());
            }
            let entries = self.summary.levels[level].len() / c;
            let have = self.summary.levels[level + 1].len() / c;
            if have * FAN_OUT < entries {
                self.merge_into(level + 1, have);
            }
        }
        self.summary
    }

    /// Stores the current packet and starts the next one.
    fn finish_packet(&mut self, len: SamplePos) {
        let c = self.summary.channels;
        for ch in 0..c {
            let mut p = self.cur[ch];
            let rms = (self.cur_sq[ch] / len.max(1) as f64).sqrt();
            p.rms = (rms * 65535.0).round().min(65535.0) as u16;
            self.summary.levels[0].push(p);
        }
        self.cur.fill(Peak::EMPTY);
        self.cur_sq.fill(0.0);
        // (PacketIndex guarantees that no packet is empty.)
        self.packet += 1;
        self.done_in_packet = 0;
        // Complete upper-level entries whose 16 children are now done.
        let mut level = 0;
        while (self.summary.levels[level].len() / c).is_multiple_of(FAN_OUT) {
            if self.summary.levels.len() == level + 1 {
                self.summary.levels.push(Vec::new());
            }
            let i = self.summary.levels[level].len() / c / FAN_OUT - 1;
            self.merge_into(level + 1, i);
            level += 1;
        }
    }

    /// Computes entry `i` of `level` from its (up to [`FAN_OUT`]) children.
    fn merge_into(&mut self, level: usize, i: usize) {
        let c = self.summary.channels;
        let below = &self.summary.levels[level - 1];
        let children = i * FAN_OUT..((i + 1) * FAN_OUT).min(below.len() / c);
        let child_span = FAN_OUT.pow(level as u32 - 1);
        let packets = self.summary.packets();
        let mut merged = vec![Peak::EMPTY; c];
        let mut sq = vec![0f64; c];
        let mut n_total = 0u64;
        for k in children {
            let p = k * child_span..((k + 1) * child_span).min(packets);
            let n = self.summary.span_samples(p);
            n_total += n;
            for ch in 0..c {
                let e = below[k * c + ch];
                merged[ch] = merged[ch].merge_min_max(e);
                sq[ch] += (f64::from(e.rms) / 65535.0).powi(2) * n as f64;
            }
        }
        for ch in 0..c {
            let rms = (sq[ch] / n_total.max(1) as f64).sqrt();
            merged[ch].rms = (rms * 65535.0).round().min(65535.0) as u16;
        }
        self.summary.levels[level].extend(merged);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lac_core::{AudioStreamInfo, PacketFlags, PacketInfo};
    use proptest::prelude::*;

    fn index(lengths: &[u32], channels: u16) -> PacketIndex {
        let mut pos = 0;
        let packets = lengths
            .iter()
            .map(|&n| {
                let p = PacketInfo {
                    byte_offset: pos,
                    byte_len: 1,
                    first_sample: pos,
                    n_samples: n,
                    hidden_before: 0,
                    hidden_after: 0,
                    flags: PacketFlags::default(),
                };
                pos += u64::from(n);
                p
            })
            .collect();
        PacketIndex::new(
            AudioStreamInfo {
                sample_rate: 8000,
                channels,
            },
            packets,
        )
        .expect("contiguous")
    }

    /// Deterministic samples in -1..1 (xorshift), interleaved.
    fn samples(n: usize, seed: u64) -> Vec<f32> {
        let mut x = seed | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                ((x >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
            })
            .collect()
    }

    /// Min, max and RMS of channel `ch` over sample frames `r`, directly.
    fn brute(s: &[f32], c: usize, ch: usize, r: Range<u64>) -> (i16, i16, f64) {
        let vals: Vec<f32> = (r.start..r.end).map(|i| s[i as usize * c + ch]).collect();
        let min = vals.iter().map(|&x| to_i16(x)).min().expect("non-empty");
        let max = vals.iter().map(|&x| to_i16(x)).max().expect("non-empty");
        let rms =
            (vals.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / vals.len() as f64).sqrt();
        (min, max, rms)
    }

    #[test]
    fn single_packet_summary() {
        let idx = index(&[4], 1);
        let mut b = PeakBuilder::new(&idx);
        b.push(&[0.5, -0.25, 1.0, 0.0]);
        let s = b.finish();
        let p = s.packet(0).expect("done")[0];
        assert_eq!((p.min, p.max), (-8192, 32767));
        // sqrt((0.25 + 0.0625 + 1) / 4) = 0.5728...
        assert!((p.rms_f32() - 0.5728).abs() < 1e-3, "{}", p.rms_f32());
    }

    #[test]
    fn columns_map_to_packets() {
        let idx = index(&[100; 10], 1);
        let mut b = PeakBuilder::new(&idx);
        // Packet k holds the constant value k/10.
        for k in 0..10 {
            b.push(&vec![k as f32 / 10.0; 100]);
        }
        let s = b.finish();
        let cols = s.columns(0..1000, 5);
        let maxima: Vec<i16> = cols
            .iter()
            .map(|c| c.as_ref().expect("done")[0].max)
            .collect();
        // Column i covers packets 2i and 2i+1; the maximum is the latter's.
        let want: Vec<i16> = (0..5).map(|i| to_i16((2 * i + 1) as f32 / 10.0)).collect();
        assert_eq!(maxima, want);
        // A column inside one packet shows that packet.
        let one = s.columns(450..460, 1);
        assert_eq!(one[0].as_ref().expect("done")[0].max, to_i16(0.4));
    }

    #[test]
    fn partial_build_reports_missing_columns() {
        let idx = index(&[10; 40], 2);
        let mut b = PeakBuilder::new(&idx);
        b.push(&samples(17 * 10 * 2 + 5, 1)); // 17 packets and a half
        let s = b.summary();
        assert_eq!(s.packets_done(), 17);
        let cols = s.columns(0..400, 4);
        assert!(cols[0].is_some());
        assert!(cols[1].is_none(), "packets 10..20 are not all done");
        assert!(cols[2].is_none() && cols[3].is_none());
    }

    proptest! {
        /// Any packet range summarises like the samples themselves, for
        /// finished and for partial builds: min and max exactly, RMS
        /// within the quantisation of the stored values.
        #[test]
        fn summarize_matches_samples(
            lengths in proptest::collection::vec(1u32..40, 1..700),
            channels in 1u16..4,
            seed in any::<u64>(),
            done_frac in 0.0f64..1.0,
            finish in any::<bool>(),
            a in 0.0f64..1.0,
            b in 0.0f64..1.0,
        ) {
            let idx = index(&lengths, channels);
            let c = usize::from(channels);
            let total = idx.total_samples() as usize;
            let s = samples(total * c, seed);
            let mut builder = PeakBuilder::new(&idx);
            let fed = if finish { total } else { (total as f64 * done_frac) as usize };
            // Feed in odd-sized chunks to cross packet boundaries anywhere.
            for chunk in s[..fed * c].chunks(37 * c) {
                builder.push(chunk);
            }
            let summary = if finish { builder.finish() } else { builder.summary().clone() };
            let done = summary.packets_done();
            let n = lengths.len();
            let (pa, pb) = {
                let (x, y) = ((a * n as f64) as usize, (b * n as f64) as usize);
                (x.min(y), x.max(y) + 1)
            };
            let pb = pb.min(n);
            let got = summary.summarize(pa..pb);
            if pb > done {
                prop_assert!(got.is_none());
            } else {
                let got = got.expect("summarised");
                let r = idx.packets()[pa].first_sample..idx.packets()[pb - 1].sample_range().end;
                for (ch, g) in got.iter().enumerate() {
                    let (min, max, rms) = brute(&s, c, ch, r.clone());
                    prop_assert_eq!((g.min, g.max), (min, max));
                    prop_assert!((f64::from(g.rms_f32()) - rms).abs() < 1e-3,
                        "rms {} vs {}", g.rms_f32(), rms);
                }
            }
        }
    }
}
