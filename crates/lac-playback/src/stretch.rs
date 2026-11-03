//! Time-stretching that keeps the pitch: WSOLA (waveform-similarity
//! overlap-add), for playback faster or slower than normal
//! (`docs/architecture.md`, section "Playback speed").
//!
//! The output is built from overlapping windows of the input, `HOP` apart
//! (half a window, so that two Hann windows add up to exactly 1). For a
//! speed `s` the windows are taken from the input `s * HOP` apart, so the
//! output is `1/s` as long. Taken exactly there, consecutive windows would
//! not be in phase and the result would warble; WSOLA moves each window by
//! up to a tolerance to where it best continues the previous one: it
//! picks the offset whose start correlates most with the audio that
//! *followed* the previous window in the input (its "natural
//! continuation"). This is cheap, has no phase artifacts on speech, and
//! works on any number of channels, all moved together so the stereo
//! image stays.
//!
//! Reference: W. Verhelst, M. Roelands, "An overlap-add technique based on
//! waveform similarity (WSOLA) for high quality time-scale modification of
//! speech", ICASSP 1993.
//!
//! This is for listening only. Normal speed never passes through here (the
//! engine bypasses it), so normal playback stays bit-exact.

/// Window length in milliseconds. 20-40 ms suits speech (a few pitch
/// periods) and is still short enough not to smear transients audibly.
const WINDOW_MS: usize = 30;

/// Step of the coarse search in [`Stretcher::best_match`], in frames.
const COARSE: usize = 4;

/// A streaming WSOLA time-stretcher for interleaved audio.
#[derive(Debug, Clone)]
pub struct Stretcher {
    channels: usize,
    /// Speed in percent: input frames per 100 output frames.
    percent: u64,
    /// Output hop = half a window, in frames.
    hop: usize,
    /// How far a window may move from its nominal position, in frames.
    tolerance: usize,
    /// Periodic Hann window of `2 * hop` frames: `w[i] + w[i + hop] == 1`.
    window: Vec<f32>,
    /// Buffered input, interleaved; index 0 is the oldest frame kept.
    input: Vec<f32>,
    /// Nominal start of the next window in the input, in 1/100 frames
    /// (exact, so long playback does not drift).
    nominal: u64,
    /// Where the input continues after the first half of the last window
    /// taken (its start plus one hop), in input frames: the audio the next
    /// window should resemble. `None` before the first window.
    natural: Option<usize>,
    /// Second half of the last window, already windowed: added to the
    /// first half of the next one.
    tail: Vec<f32>,
}

impl Stretcher {
    /// A stretcher for audio of `rate` frames per second and `channels`
    /// channels, playing at `percent` of normal speed.
    pub fn new(rate: u32, channels: usize, percent: u32) -> Self {
        let hop = (rate as usize * WINDOW_MS / 2000).max(1);
        let n = 2 * hop;
        let window = (0..n)
            .map(|i| {
                let x = std::f64::consts::TAU * i as f64 / n as f64;
                (0.5 - 0.5 * x.cos()) as f32
            })
            .collect();
        Stretcher {
            channels: channels.max(1),
            percent: u64::from(percent.max(1)),
            hop,
            tolerance: hop / 2,
            window,
            input: Vec::new(),
            nominal: 0,
            natural: None,
            tail: Vec::new(),
        }
    }

    /// Forgets all buffered audio, e.g. after a seek.
    pub fn reset(&mut self) {
        self.input.clear();
        self.nominal = 0;
        self.natural = None;
        self.tail.clear();
    }

    /// Stretches the interleaved `input` (whole frames) and appends the
    /// result to `out`. Output lags the input by about one window.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.input.extend_from_slice(input);
        let (ch, hop, tol) = (self.channels, self.hop, self.tolerance);
        loop {
            let frames = self.input.len() / ch;
            let nominal = usize::try_from(self.nominal / 100).unwrap_or(usize::MAX);
            let lo = nominal.saturating_sub(tol);
            let hi = nominal.saturating_add(tol);
            let natural = self.natural;
            // The candidates need a whole window, the natural continuation
            // one hop for the comparison.
            let needed = (hi + 2 * hop).max(natural.map_or(0, |p| p + hop));
            if frames < needed {
                break;
            }
            let start = match natural {
                None => nominal,
                Some(nat) => self.best_match(lo..=hi, nat),
            };
            let x = |i: usize| &self.input[(start + i) * ch..(start + i + 1) * ch];
            if natural.is_none() {
                // The very first window: its rising half would fade the
                // start in, so emit that part unwindowed.
                for i in 0..hop {
                    out.extend_from_slice(x(i));
                }
            } else {
                for i in 0..hop {
                    let w = self.window[i];
                    let t = &self.tail[i * ch..(i + 1) * ch];
                    out.extend(x(i).iter().zip(t).map(|(v, t)| t + w * v));
                }
            }
            let mut tail = Vec::with_capacity(hop * ch);
            for i in hop..2 * hop {
                let w = self.window[i];
                tail.extend(x(i).iter().map(|v| w * v));
            }
            self.tail = tail;
            self.nominal += hop as u64 * self.percent;
            // Drop input that no later window or comparison can use.
            let next = usize::try_from(self.nominal / 100).unwrap_or(usize::MAX);
            let keep_from = next.saturating_sub(tol).min(start + hop);
            self.input.drain(..keep_from * ch);
            self.nominal -= keep_from as u64 * 100;
            self.natural = Some(start + hop - keep_from);
        }
    }

    /// The input has ended: appends the rest of the last window (it fades
    /// out) and starts afresh.
    pub fn finish(&mut self, out: &mut Vec<f32>) {
        out.extend_from_slice(&self.tail);
        self.reset();
    }

    /// The window start within `candidates` whose first hop correlates
    /// most with the hop starting at `natural`.
    ///
    /// Coarse to fine, which is about seven times cheaper than trying
    /// every offset on every frame: every [`COARSE`]th offset compared on
    /// every [`COARSE`]th frame, then every offset around the best one on
    /// every second frame. Audio below a few kHz, which carries what the
    /// match is about (the pitch periods), survives the sparse sampling.
    fn best_match(&self, candidates: std::ops::RangeInclusive<usize>, natural: usize) -> usize {
        let (lo, hi) = (*candidates.start(), *candidates.end());
        let coarse = self.argmax((lo..=hi).step_by(COARSE), natural, COARSE);
        let fine_lo = coarse.saturating_sub(COARSE - 1).max(lo);
        let fine_hi = (coarse + COARSE - 1).min(hi);
        self.argmax(fine_lo..=fine_hi, natural, 2)
    }

    /// The candidate with the largest correlation with the hop at
    /// `natural`, comparing every `stride`th frame.
    fn argmax(
        &self,
        candidates: impl Iterator<Item = usize>,
        natural: usize,
        stride: usize,
    ) -> usize {
        let (ch, hop) = (self.channels, self.hop);
        let target = &self.input[natural * ch..(natural + hop) * ch];
        let mut best = (f32::NEG_INFINITY, natural);
        for c in candidates {
            let cand = &self.input[c * ch..(c + hop) * ch];
            let score: f32 = cand
                .chunks_exact(ch)
                .zip(target.chunks_exact(ch))
                .step_by(stride)
                .map(|(a, b)| a.iter().zip(b).map(|(a, b)| a * b).sum::<f32>())
                .sum();
            if score > best.0 {
                best = (score, c);
            }
        }
        best.1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 44_100;

    fn sine(freq: f64, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| (std::f64::consts::TAU * freq * i as f64 / f64::from(RATE)).sin() as f32 * 0.5)
            .collect()
    }

    /// Runs `input` through a stretcher in uneven blocks, as the engine
    /// does.
    fn stretch(input: &[f32], channels: usize, percent: u32) -> Vec<f32> {
        let mut s = Stretcher::new(RATE, channels, percent);
        let mut out = Vec::new();
        for block in input.chunks(channels * 1234) {
            s.push(block, &mut out);
        }
        s.finish(&mut out);
        out
    }

    /// Frequency of a mono signal from its upward zero crossings.
    fn frequency(x: &[f32]) -> f64 {
        let ups = x.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        ups as f64 * f64::from(RATE) / x.len() as f64
    }

    #[test]
    fn duration_scales_with_the_speed() {
        let input = sine(220.0, 2 * RATE as usize);
        for percent in [50, 75, 125, 150, 200, 300] {
            let out = stretch(&input, 1, percent);
            let expected = input.len() as f64 * 100.0 / f64::from(percent);
            // Off by at most the windows held back at the end.
            let slack = 3.0 * f64::from(RATE) * WINDOW_MS as f64 / 1000.0;
            assert!(
                (out.len() as f64 - expected).abs() < slack,
                "{percent}%: {} frames, expected about {expected}",
                out.len()
            );
        }
    }

    #[test]
    fn pitch_stays_the_same() {
        let input = sine(440.0, 2 * RATE as usize);
        for percent in [50, 150, 300] {
            let out = stretch(&input, 1, percent);
            // Leave out the start and the fading end.
            let mid = &out[2000..out.len() - 4000];
            let f = frequency(mid);
            assert!((f - 440.0).abs() < 440.0 * 0.02, "{percent}%: {f} Hz");
        }
    }

    #[test]
    fn steady_signal_keeps_its_level_and_channels_stay_apart() {
        // Left: constant 0.25; right: silence.
        let input: Vec<f32> = (0..RATE as usize).flat_map(|_| [0.25, 0.0]).collect();
        let out = stretch(&input, 2, 150);
        let frames = out.len() / 2;
        // The windows add up to 1 everywhere but at the fading end.
        for f in 0..frames - 2000 {
            assert!(
                (out[2 * f] - 0.25).abs() < 1e-5,
                "frame {f}: {}",
                out[2 * f]
            );
            assert_eq!(out[2 * f + 1], 0.0, "frame {f}");
        }
    }

    #[test]
    fn reset_forgets_buffered_audio() {
        let mut s = Stretcher::new(RATE, 1, 200);
        let mut out = Vec::new();
        s.push(&vec![1.0; 5000], &mut out);
        s.reset();
        out.clear();
        s.push(&vec![0.0; 5000], &mut out);
        s.finish(&mut out);
        assert!(out.iter().all(|&v| v == 0.0), "old audio leaked");
    }
}
