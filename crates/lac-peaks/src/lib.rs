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
//!
//! A finished summary can be stored and loaded ([`PeakSummary::to_bytes`],
//! [`PeakSummary::from_bytes`]) so that a recording is decoded only once;
//! where the bytes live is up to the caller (`lac-app`'s peak cache).

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

    /// Min, max and RMS of decoded samples (`-1.0..=1.0`), quantised like
    /// the summary. `None` for no samples. Used where decoded samples are
    /// at hand anyway, e.g. a zoomed-in display.
    pub fn of_samples(samples: impl IntoIterator<Item = f32>) -> Option<Peak> {
        let (mut min, mut max, mut sq, mut n) = (f32::INFINITY, f32::NEG_INFINITY, 0f64, 0u64);
        for x in samples {
            min = min.min(x);
            max = max.max(x);
            sq += f64::from(x).powi(2);
            n += 1;
        }
        (n > 0).then(|| Peak {
            min: to_i16(min),
            max: to_i16(max),
            rms: ((sq / n as f64).sqrt() * 65535.0).round().min(65535.0) as u16,
        })
    }
}

/// Converts a decoded sample (`-1.0..=1.0`) to the stored scale.
fn to_i16(x: f32) -> i16 {
    // `as` saturates, and NaN becomes 0.
    (x * 32767.0).round() as i16
}

/// Why stored summary bytes were rejected. Callers treat every variant the
/// same way: rebuild the summary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FormatError {
    /// The bytes do not start with [`PeakSummary::MAGIC`].
    #[error("not a stored waveform summary")]
    Magic,
    /// Written by another version of the format.
    #[error("waveform summary format {0}, expected {expected}", expected = PeakSummary::FORMAT_VERSION)]
    Version(u32),
    /// Cut short, inconsistent or with trailing bytes.
    #[error("damaged waveform summary: {0}")]
    Damaged(&'static str),
}

/// The waveform summary of one source.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// First bytes of [`PeakSummary::to_bytes`].
    pub const MAGIC: [u8; 8] = *b"LACPEAKS";
    /// Version of the byte layout; bumped on every change, and an old
    /// version is never read (it is rebuilt instead).
    pub const FORMAT_VERSION: u32 = 1;

    /// Whether every packet is summarised and the pyramid is complete,
    /// i.e. this came from [`PeakBuilder::finish`] after all samples.
    pub fn is_complete(&self) -> bool {
        self.packets_done() == self.packets() && self.levels_consistent()
    }

    /// Whether the summary was built for `index` (same packet layout), so
    /// that a stored one may be used for it.
    pub fn matches_index(&self, index: &PacketIndex) -> bool {
        usize::from(index.info().channels).max(1) == self.channels
            && self.packets() == index.packets().len()
            && self.total_samples() == index.total_samples()
            && index
                .packets()
                .iter()
                .zip(&self.starts)
                .all(|(p, &s)| p.first_sample == s)
    }

    /// The level sizes [`PeakBuilder::finish`] produces: each level has
    /// ceil(n / FAN_OUT) entries of the n below, up to a single entry.
    fn levels_consistent(&self) -> bool {
        let c = self.channels;
        let mut entries = self.packets();
        for (k, level) in self.levels.iter().enumerate() {
            if level.len() != entries * c {
                return false;
            }
            let last = k + 1 == self.levels.len();
            if last != (entries <= 1) {
                return false;
            }
            entries = entries.div_ceil(FAN_OUT);
        }
        !self.levels.is_empty()
    }

    /// The summary as bytes (little-endian): [`PeakSummary::MAGIC`], format
    /// version, channel count, packet starts plus total length, then every
    /// level as its entry count and (min, max, rms) per channel and entry.
    /// Meant for complete summaries; [`PeakSummary::from_bytes`] rejects
    /// others.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            32 + self.starts.len() * 8 + self.levels.iter().map(|l| 8 + l.len() * 6).sum::<usize>(),
        );
        out.extend(Self::MAGIC);
        out.extend(Self::FORMAT_VERSION.to_le_bytes());
        out.extend((self.channels as u32).to_le_bytes());
        out.extend((self.starts.len() as u64).to_le_bytes());
        for s in &self.starts {
            out.extend(s.to_le_bytes());
        }
        out.extend((self.levels.len() as u32).to_le_bytes());
        for level in &self.levels {
            out.extend((level.len() as u64).to_le_bytes());
            for p in level {
                out.extend(p.min.to_le_bytes());
                out.extend(p.max.to_le_bytes());
                out.extend(p.rms.to_le_bytes());
            }
        }
        out
    }

    /// Reads bytes written by [`PeakSummary::to_bytes`]. Anything else
    /// (another version, truncation, inconsistent sizes, an incomplete
    /// summary) is an error and never a panic; the caller rebuilds then.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut r = Reader(bytes);
        if r.take(8)? != Self::MAGIC {
            return Err(FormatError::Magic);
        }
        let version = r.u32()?;
        if version != Self::FORMAT_VERSION {
            return Err(FormatError::Version(version));
        }
        let channels = r.u32()? as usize;
        if channels == 0 {
            return Err(FormatError::Damaged("no channels"));
        }
        let n_starts = r.len(8)?;
        let starts = (0..n_starts)
            .map(|_| r.u64())
            .collect::<Result<Vec<_>, _>>()?;
        // Packets are never empty (PacketIndex), so starts increase.
        if starts.is_empty() || starts[0] != 0 || starts.windows(2).any(|w| w[0] >= w[1]) {
            return Err(FormatError::Damaged("packet starts"));
        }
        let n_levels = r.u32()? as usize;
        let mut levels = Vec::new();
        for _ in 0..n_levels {
            let n = r.len(6)?;
            let level = (0..n)
                .map(|_| {
                    Ok(Peak {
                        min: r.u16()? as i16,
                        max: r.u16()? as i16,
                        rms: r.u16()?,
                    })
                })
                .collect::<Result<Vec<_>, FormatError>>()?;
            levels.push(level);
        }
        if !r.0.is_empty() {
            return Err(FormatError::Damaged("trailing bytes"));
        }
        let s = PeakSummary {
            channels,
            starts,
            levels,
        };
        if !s.is_complete() {
            return Err(FormatError::Damaged("level sizes"));
        }
        Ok(s)
    }

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

/// Reads little-endian numbers from a byte slice, failing on truncation.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], FormatError> {
        if self.0.len() < n {
            return Err(FormatError::Damaged("cut short"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }

    fn u16(&mut self) -> Result<u16, FormatError> {
        Ok(u16::from_le_bytes(
            self.take(2)?
                .try_into()
                .map_err(|_| FormatError::Damaged("cut short"))?,
        ))
    }

    fn u32(&mut self) -> Result<u32, FormatError> {
        Ok(u32::from_le_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| FormatError::Damaged("cut short"))?,
        ))
    }

    fn u64(&mut self) -> Result<u64, FormatError> {
        Ok(u64::from_le_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| FormatError::Damaged("cut short"))?,
        ))
    }

    /// An element count followed by `count * elem_size` bytes; checked
    /// against the bytes left so that a damaged count cannot make us
    /// allocate gigabytes.
    fn len(&mut self, elem_size: usize) -> Result<usize, FormatError> {
        let n = usize::try_from(self.u64()?).map_err(|_| FormatError::Damaged("count"))?;
        if n.checked_mul(elem_size).is_none_or(|b| b > self.0.len()) {
            return Err(FormatError::Damaged("count"));
        }
        Ok(n)
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

    #[test]
    fn peak_of_samples_matches_the_summary_scale() {
        assert_eq!(Peak::of_samples([]), None);
        let p = Peak::of_samples([0.5, -0.25, 0.5, -0.25]).expect("samples");
        assert_eq!((p.min, p.max), (to_i16(-0.25), to_i16(0.5)));
        // RMS of ±(0.5, 0.25): sqrt((0.25 + 0.0625) / 2).
        assert!((p.rms_f32() - 0.395_284_7).abs() < 1e-4, "{}", p.rms_f32());
        let clipped = Peak::of_samples([2.0, f32::NEG_INFINITY]).expect("samples");
        assert_eq!((clipped.min, clipped.max), (i16::MIN, i16::MAX));
    }

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

    /// A finished summary of `lengths` packets with `channels` channels.
    fn finished(lengths: &[u32], channels: u16, seed: u64) -> (PacketIndex, PeakSummary) {
        let idx = index(lengths, channels);
        let mut b = PeakBuilder::new(&idx);
        b.push(&samples(
            idx.total_samples() as usize * usize::from(channels),
            seed,
        ));
        (idx, b.finish())
    }

    #[test]
    fn stored_bytes_are_rejected_when_damaged() {
        let (_, s) = finished(&[100; 40], 2, 7);
        let bytes = s.to_bytes();
        assert_eq!(PeakSummary::from_bytes(&bytes).as_ref(), Ok(&s));
        // Every truncation, and trailing bytes.
        for n in 0..bytes.len() {
            assert!(PeakSummary::from_bytes(&bytes[..n]).is_err(), "cut at {n}");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(PeakSummary::from_bytes(&longer).is_err());
        // Another version is rebuilt, not read.
        let mut v2 = bytes.clone();
        v2[8] = 2;
        assert_eq!(PeakSummary::from_bytes(&v2), Err(FormatError::Version(2)));
        assert_eq!(
            PeakSummary::from_bytes(b"RIFFxxxxxxxx"),
            Err(FormatError::Magic)
        );
        // A huge count must not allocate; it fails as damaged.
        let mut huge = bytes.clone();
        huge[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(PeakSummary::from_bytes(&huge).is_err());
        // A partial summary is never stored as if complete.
        let idx = index(&[100; 40], 2);
        let mut b = PeakBuilder::new(&idx);
        b.push(&samples(1000 * 2, 1));
        assert!(!b.summary().is_complete());
        assert!(PeakSummary::from_bytes(&b.summary().to_bytes()).is_err());
    }

    #[test]
    fn matches_only_its_own_index() {
        let (idx, s) = finished(&[10, 20, 30], 1, 3);
        assert!(s.matches_index(&idx));
        assert!(!s.matches_index(&index(&[10, 20, 31], 1)), "length");
        assert!(!s.matches_index(&index(&[20, 10, 30], 1)), "layout");
        assert!(!s.matches_index(&index(&[10, 20, 30], 2)), "channels");
    }

    proptest! {
        /// Stored and loaded summaries are identical, for any layout, and
        /// flipping any single byte never panics.
        #[test]
        fn bytes_round_trip(
            lengths in proptest::collection::vec(1u32..40, 1..400),
            channels in 1u16..4,
            seed in any::<u64>(),
            flip in any::<prop::sample::Index>(),
        ) {
            let (_, s) = finished(&lengths, channels, seed);
            prop_assert!(s.is_complete());
            let bytes = s.to_bytes();
            prop_assert_eq!(PeakSummary::from_bytes(&bytes), Ok(s));
            let mut bad = bytes.clone();
            bad[flip.index(bytes.len())] ^= 0x5a;
            let _ = PeakSummary::from_bytes(&bad);
        }

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
