//! Per-frame features of a decoded recording.
//!
//! The timeline is divided into frames of [`hop_for_rate`] sample frames.
//! Each frame is described by
//!
//! - its **level**: RMS of the frame's own samples in dBFS (time domain,
//!   unwindowed, so a sine of amplitude 1 has −3 dB),
//! - its **spectral flatness**: geometric over arithmetic mean of the
//!   power spectrum between [`FLATNESS_LOW_HZ`] and [`FLATNESS_HIGH_HZ`].
//!   White noise gives about 0.56 (the expected value of a periodogram of
//!   Gaussian noise is `exp(-γ)`), harmonic sounds such as voiced speech
//!   and music give well below 0.1, a single sine almost 0,
//! - its **band spectrum**: power in [`BANDS`] log-spaced bands, for
//!   telling one piece of music from the next.
//!
//! The spectrum is taken over a Hann window of two hops centred on the
//! frame (50 % overlap), of the mean of all channels.
//!
//! Why flatness and not "the RMS of the FFT": by Parseval's theorem the
//! RMS of the spectrum equals the RMS of the signal, so their ratio is
//! constant. What tells noise from content is how *unevenly* the energy is
//! distributed, which the ratio of geometric to arithmetic mean measures
//! (Wiener entropy; also called tonality coefficient in MPEG-7).
//!
//! Flatness is measured in a band that every codec we support keeps
//! (lossy encoders low-pass at 3.4 to 20 kHz): the empty bins above the
//! low-pass would otherwise make everything look tonal.

use std::sync::Arc;

use lac_core::SamplePos;
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

/// Number of bands of [`Frame::band_db`].
pub const BANDS: usize = 16;

/// Lower edge of the band in which [`Frame::flatness`] is measured: below,
/// mains hum and rumble dominate.
pub const FLATNESS_LOW_HZ: f32 = 150.0;
/// Upper edge of the flatness band (or 90 % of the Nyquist frequency, if
/// lower), below the low-pass of narrow-band speech codecs.
pub const FLATNESS_HIGH_HZ: f32 = 6000.0;
/// Lower edge of the lowest of the [`BANDS`].
const BANDS_LOW_HZ: f32 = 80.0;
/// Upper edge of the highest of the [`BANDS`] (or 90 % of Nyquist).
const BANDS_HIGH_HZ: f32 = 12000.0;

/// Level reported for digital silence; also the lower limit of every dB
/// value here.
pub(crate) const FLOOR_DB: f32 = -120.0;
/// Power added to every spectral bin before taking logarithms (−120 dB per
/// bin), so that digital silence has a defined flatness (1).
const BIN_EPS: f64 = 1e-12;

/// Analysis hop in sample frames for `sample_rate`: the power of two at or
/// above 20 ms (1024 at 44.1 and 48 kHz, 256 at 8 kHz), so that the FFT
/// of two hops has a power-of-two size.
pub fn hop_for_rate(sample_rate: u32) -> usize {
    ((sample_rate / 50).max(16) as usize).next_power_of_two()
}

/// Features of one analysis frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    /// RMS of the frame's samples (mean of the channels) in dBFS; at
    /// least −120.
    pub level_db: f32,
    /// Spectral flatness, 0 (one pure tone) to 1 (perfectly flat).
    pub flatness: f32,
    /// Band powers, see [`Frame::band_db`]; stored in half-dB steps from
    /// −128 dB to keep a multi-hour recording at a few megabytes.
    bands: [u8; BANDS],
}

impl Frame {
    /// Power in band `b` (`< BANDS`, log-spaced from 80 Hz to 12 kHz or
    /// 90 % of Nyquist) as its contribution to the frame's mean square, in
    /// dB (−128 to −0.5, in half-dB steps).
    pub fn band_db(&self, b: usize) -> f32 {
        f32::from(self.bands[b]) / 2.0 - 128.0
    }
}

/// Quantises a band power in dB to [`Frame::bands`]' storage.
fn quantise_db(db: f64) -> u8 {
    // `as` saturates: anything below −128 dB becomes 0, above −0.5 dB 255.
    ((db + 128.0) * 2.0).round() as u8
}

/// The features of a whole recording, one [`Frame`] per hop.
#[derive(Debug, Clone, PartialEq)]
pub struct Features {
    sample_rate: u32,
    hop: usize,
    total: SamplePos,
    frames: Vec<Frame>,
}

impl Features {
    /// Sample rate of the analysed recording.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Sample frames per analysis frame.
    pub fn hop(&self) -> usize {
        self.hop
    }

    /// Length of the analysed recording in sample frames.
    pub fn total_samples(&self) -> SamplePos {
        self.total
    }

    /// The frames; frame `i` covers [`Features::frame_range`]`(i)`.
    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    /// First sample frame of analysis frame `i` (clamped to the length,
    /// so `frame_start(frames().len())` is the end of the recording).
    pub fn frame_start(&self, i: usize) -> SamplePos {
        (i as SamplePos)
            .saturating_mul(self.hop as SamplePos)
            .min(self.total)
    }

    /// Sample frames covered by analysis frame `i`.
    pub fn frame_range(&self, i: usize) -> std::ops::Range<SamplePos> {
        self.frame_start(i)..self.frame_start(i + 1)
    }

    /// Converts seconds into a number of analysis frames (rounded, at
    /// least 1).
    pub fn frames_for_secs(&self, secs: f32) -> usize {
        let f = f64::from(secs.max(0.0)) * f64::from(self.sample_rate) / self.hop as f64;
        (f.round() as usize).max(1)
    }
}

/// Computes [`Features`] from decoded samples in timeline order.
pub struct FeatureExtractor {
    sample_rate: u32,
    channels: usize,
    hop: usize,
    fft: Arc<dyn Fft<f32>>,
    /// Hann window of two hops.
    window: Vec<f32>,
    /// `Σ w²`, for normalising band powers.
    window_power: f64,
    /// Mono samples from the start of the next frame's window on.
    pending: Vec<f32>,
    /// FFT buffer and scratch, kept to avoid allocating per frame.
    buf: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    flat_bins: std::ops::Range<usize>,
    band_bins: [std::ops::Range<usize>; BANDS],
    total: SamplePos,
    frames: Vec<Frame>,
}

impl std::fmt::Debug for FeatureExtractor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeatureExtractor")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("hop", &self.hop)
            .field("total", &self.total)
            .field("frames", &self.frames.len())
            .finish_non_exhaustive()
    }
}

impl FeatureExtractor {
    /// Starts an analysis of a recording with `channels` interleaved
    /// channels (0 is treated as 1) at `sample_rate` (0 is treated as 1).
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        let sample_rate = sample_rate.max(1);
        let hop = hop_for_rate(sample_rate);
        let n = 2 * hop;
        let fft = FftPlanner::new().plan_fft_forward(n);
        // Periodic Hann window: overlapping copies at 50 % sum to 1.
        let window: Vec<f32> = (0..n)
            .map(|k| {
                let x = std::f64::consts::PI * k as f64 / n as f64;
                (x.sin() * x.sin()) as f32
            })
            .collect();
        let window_power = window.iter().map(|&w| f64::from(w) * f64::from(w)).sum();
        let bin_hz = sample_rate as f32 / n as f32;
        let nyquist_bin = n / 2;
        let bin = |hz: f32| ((hz / bin_hz).round() as usize).clamp(1, nyquist_bin);
        let top = 0.9 * sample_rate as f32 / 2.0;
        let flat_hi = bin(FLATNESS_HIGH_HZ.min(top));
        let flat_bins = bin(FLATNESS_LOW_HZ)..flat_hi.max(bin(FLATNESS_LOW_HZ) + 1);
        // Log-spaced band edges; every band gets at least one bin, so the
        // lowest bands of a low sample rate may be wider than planned.
        let (lo, hi) = (BANDS_LOW_HZ, BANDS_HIGH_HZ.min(top).max(BANDS_LOW_HZ * 2.0));
        let mut band_bins: [std::ops::Range<usize>; BANDS] = Default::default();
        let mut start = bin(lo);
        for (b, r) in band_bins.iter_mut().enumerate() {
            let edge = lo * (hi / lo).powf((b + 1) as f32 / BANDS as f32);
            let end = bin(edge).max(start + 1).min(nyquist_bin + 1);
            *r = start.min(end - 1)..end;
            start = end;
        }
        FeatureExtractor {
            sample_rate,
            channels: channels.max(1),
            hop,
            fft,
            window,
            window_power,
            // The first frame's window starts half a hop before 0.
            pending: vec![0.0; hop / 2],
            buf: vec![Complex::default(); n],
            scratch: vec![Complex::default(); n],
            flat_bins,
            band_bins,
            total: 0,
            frames: Vec::new(),
        }
    }

    /// Adds the next decoded sample frames, interleaved by channel, in
    /// timeline order. An incomplete trailing sample frame is ignored.
    pub fn push(&mut self, interleaved: &[f32]) {
        let c = self.channels;
        let scale = 1.0 / c as f32;
        for frame in interleaved.chunks_exact(c) {
            self.pending.push(frame.iter().sum::<f32>() * scale);
            self.total += 1;
            if self.pending.len() >= 2 * self.hop {
                self.analyse(self.hop);
            }
        }
    }

    /// Analyses the frames still pending (the end of the recording is
    /// padded with silence for the spectrum, but not counted in the
    /// level) and returns the features.
    pub fn finish(mut self) -> Features {
        let needed = self.total.div_ceil(self.hop as SamplePos) as usize;
        while self.frames.len() < needed {
            let start = self.frames.len() as SamplePos * self.hop as SamplePos;
            let valid = (self.total - start).min(self.hop as SamplePos) as usize;
            self.pending
                .resize(self.pending.len().max(2 * self.hop), 0.0);
            self.analyse(valid);
        }
        Features {
            sample_rate: self.sample_rate,
            hop: self.hop,
            total: self.total,
            frames: self.frames,
        }
    }

    /// Analyses the frame whose window is `pending[..2 * hop]`, of which
    /// the frame's own samples are `pending[hop/2..][..valid]`, then drops
    /// one hop of `pending`.
    fn analyse(&mut self, valid: usize) {
        let hop = self.hop;
        let n = 2 * hop;
        let own = &self.pending[hop / 2..hop / 2 + valid];
        let ms = own
            .iter()
            .map(|&x| f64::from(x) * f64::from(x))
            .sum::<f64>()
            / valid.max(1) as f64;
        let level_db = to_db(ms);

        for ((b, &x), &w) in self.buf.iter_mut().zip(&self.pending).zip(&self.window) {
            *b = Complex::new(x * w, 0.0);
        }
        self.fft
            .process_with_scratch(&mut self.buf, &mut self.scratch);
        // Bin power scaled so that the bins 1..n/2 sum to the mean square
        // of the windowed signal relative to the window's own (Parseval):
        // a band's value is its share of the frame's mean square.
        let scale = 2.0 / (n as f64 * self.window_power);
        let power = |k: usize| f64::from(self.buf[k].norm_sqr()) * scale;

        let (mut log_sum, mut sum) = (0f64, 0f64);
        for k in self.flat_bins.clone() {
            let p = power(k) + BIN_EPS;
            log_sum += p.ln();
            sum += p;
        }
        let m = self.flat_bins.len() as f64;
        let flatness = ((log_sum / m).exp() / (sum / m)).clamp(0.0, 1.0) as f32;

        let mut bands = [0u8; BANDS];
        for (q, r) in bands.iter_mut().zip(&self.band_bins) {
            let p: f64 = r.clone().map(power).sum();
            *q = quantise_db(10.0 * (p + BIN_EPS).log10());
        }

        self.frames.push(Frame {
            level_db,
            flatness,
            bands,
        });
        self.pending.drain(..hop);
    }
}

/// Mean square to dBFS, at least [`FLOOR_DB`].
fn to_db(ms: f64) -> f32 {
    (10.0 * ms.max(1e-30).log10()).max(f64::from(FLOOR_DB)) as f32
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Deterministic white noise in `-1..1` (xorshift; tests must not be
    /// flaky).
    pub(crate) fn noise(n: usize, amplitude: f32, seed: u64) -> Vec<f32> {
        let mut s = seed.max(1);
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                ((s >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) as f32 * amplitude
            })
            .collect()
    }

    /// A harmonic tone (fundamental `f0`, `harmonics` partials falling off
    /// as 1/k), like a sung or played note, with peak amplitude about
    /// `amplitude`.
    pub(crate) fn tone(n: usize, rate: u32, f0: f32, harmonics: usize, amplitude: f32) -> Vec<f32> {
        let norm: f32 = (1..=harmonics).map(|k| 1.0 / k as f32).sum();
        // The phase in f64: in f32 its rounding noise grows with time
        // and would itself change the spectrum.
        (0..n)
            .map(|i| {
                let t = i as f64 / f64::from(rate);
                let s: f64 = (1..=harmonics)
                    .map(|k| {
                        (std::f64::consts::TAU * f64::from(f0) * k as f64 * t).sin() / k as f64
                    })
                    .sum();
                (s * f64::from(amplitude / norm)) as f32
            })
            .collect()
    }

    fn features_of(samples: &[f32], rate: u32) -> Features {
        let mut x = FeatureExtractor::new(rate, 1);
        x.push(samples);
        x.finish()
    }

    #[test]
    fn hop_is_a_power_of_two_of_about_20_ms() {
        assert_eq!(hop_for_rate(48000), 1024);
        assert_eq!(hop_for_rate(44100), 1024);
        assert_eq!(hop_for_rate(16000), 512);
        assert_eq!(hop_for_rate(8000), 256);
        assert_eq!(hop_for_rate(0), 16);
    }

    #[test]
    fn one_frame_per_started_hop() {
        for len in [0, 1, 511, 512, 513, 5000] {
            let f = features_of(&vec![0.1; len], 16000);
            assert_eq!(f.frames().len(), len.div_ceil(512), "len {len}");
            assert_eq!(f.total_samples(), len as SamplePos);
            assert_eq!(f.frame_start(f.frames().len()), len as SamplePos);
        }
    }

    #[test]
    fn level_is_rms_in_dbfs() {
        let rate = 16000;
        let sine: Vec<f32> = (0..rate as usize)
            .map(|i| 0.5 * (std::f32::consts::TAU * 1000.0 * i as f32 / rate as f32).sin())
            .collect();
        let f = features_of(&sine, rate);
        for fr in f.frames() {
            // 0.5 / √2 → −9.03 dB.
            assert!((fr.level_db + 9.03).abs() < 0.05, "{}", fr.level_db);
        }
        let silent = features_of(&vec![0.0; 2000], rate);
        assert!(silent.frames().iter().all(|fr| fr.level_db == FLOOR_DB));
    }

    #[test]
    fn last_partial_frame_level_counts_only_real_samples() {
        // 600 samples at 16 kHz: frame 1 holds 88 samples of 0.5.
        let f = features_of(&vec![0.5; 600], 16000);
        assert!((f.frames()[1].level_db + 6.02).abs() < 0.05);
    }

    #[test]
    fn flatness_separates_noise_from_harmonic_sounds() {
        let rate = 16000;
        let n = rate as usize * 2;
        let flat = |s: &[f32]| {
            let f = features_of(s, rate);
            // Skip the frames touching the start and end (zero padding).
            let fr = &f.frames()[2..f.frames().len() - 2];
            fr.iter().map(|x| x.flatness).sum::<f32>() / fr.len() as f32
        };
        let white = flat(&noise(n, 0.3, 1));
        let note = flat(&tone(n, rate, 220.0, 12, 0.5));
        let sine = flat(&tone(n, rate, 1000.0, 1, 0.5));
        assert!(white > 0.45 && white < 0.7, "white noise {white}");
        assert!(note < 0.1, "harmonic tone {note}");
        assert!(sine < 0.01, "sine {sine}");
    }

    #[test]
    fn bands_follow_the_spectrum() {
        let rate = 16000;
        let low = features_of(&tone(rate as usize, rate, 150.0, 1, 0.5), rate);
        let high = features_of(&tone(rate as usize, rate, 3000.0, 1, 0.5), rate);
        let (l, h) = (&low.frames()[5], &high.frames()[5]);
        let loudest = |f: &Frame| (0..BANDS).max_by(|&a, &b| f.band_db(a).total_cmp(&f.band_db(b)));
        // 80 Hz .. 7.2 kHz in 16 log-spaced bands: 150 Hz lies in band 2,
        // 3 kHz in band 12.
        assert_eq!(loudest(l), Some(2));
        assert_eq!(loudest(h), Some(12));
        assert!(l.band_db(2) > l.band_db(12) + 40.0);
        assert!(h.band_db(12) > h.band_db(2) + 40.0);
        // A sine's energy sits in one or two bands, at about its level.
        let max = (0..BANDS).map(|b| l.band_db(b)).fold(f32::MIN, f32::max);
        assert!((max - l.level_db).abs() < 3.0, "{max} vs {}", l.level_db);
    }

    #[test]
    fn chunking_does_not_change_the_result() {
        let rate = 8000;
        let s = noise(3000, 0.5, 7);
        let whole = features_of(&s, rate);
        let mut x = FeatureExtractor::new(rate, 1);
        for c in s.chunks(37) {
            x.push(c);
        }
        assert_eq!(x.finish(), whole);
    }

    #[test]
    fn channels_are_mixed_to_mono() {
        let rate = 8000;
        let s = noise(3000, 0.5, 3);
        let stereo: Vec<f32> = s.iter().flat_map(|&x| [x, x]).collect();
        let mut x = FeatureExtractor::new(rate, 2);
        x.push(&stereo);
        assert_eq!(x.finish(), features_of(&s, rate));
    }
}
