//! Converting decoded audio to the output device's format: sample rate and
//! channel count.
//!
//! This is for listening only (exports never pass through here), so a
//! linear-interpolation resampler is good enough; it is cheap and has no
//! latency. When the rates are equal the samples pass through unchanged,
//! which the tests check bit for bit.

/// Maps one interleaved input frame of `inp.len()` channels to `out`:
///
/// - mono is copied to every output channel;
/// - more input than output channels: output channel `c` is the average of
///   the input channels `c, c + n, c + 2n, …` (`n` output channels), e.g.
///   quad to stereo mixes front and rear of each side;
/// - fewer: the input channels go to the first outputs, the rest is silent.
pub fn map_channels(inp: &[f32], out: &mut [f32]) {
    let (ni, no) = (inp.len(), out.len());
    if ni == 0 {
        out.fill(0.0);
    } else if ni == 1 {
        out.fill(inp[0]);
    } else if ni > no {
        for (c, o) in out.iter_mut().enumerate() {
            let (sum, n) = inp
                .iter()
                .skip(c)
                .step_by(no)
                .fold((0.0, 0u32), |(s, n), v| (s + v, n + 1));
            *o = sum / n.max(1) as f32;
        }
    } else {
        out[..ni].copy_from_slice(inp);
        out[ni..].fill(0.0);
    }
}

/// A streaming sample rate and channel converter.
#[derive(Debug, Clone)]
pub struct Converter {
    in_channels: usize,
    out_channels: usize,
    /// Input frames per output frame is `step / den`: the position is
    /// kept as an exact fraction, so long playback never drifts and equal
    /// rates never interpolate.
    step: u64,
    den: u64,
    /// Position of the next output frame, in `1/den` input frames counted
    /// from `prev` (index 0) when there is one, else from the first new
    /// frame.
    t: u64,
    /// The last input frame of the previous block, already channel-mapped;
    /// interpolation across block boundaries needs it.
    prev: Option<Vec<f32>>,
}

impl Converter {
    /// A converter from `in_rate`/`in_channels` to `out_rate`/`out_channels`.
    pub fn new(in_rate: u32, in_channels: usize, out_rate: u32, out_channels: usize) -> Self {
        Converter {
            in_channels: in_channels.max(1),
            out_channels: out_channels.max(1),
            step: u64::from(in_rate.max(1)),
            den: u64::from(out_rate.max(1)),
            t: 0,
            prev: None,
        }
    }

    /// Forgets the previous block, e.g. after a seek.
    pub fn reset(&mut self) {
        self.t = 0;
        self.prev = None;
    }

    /// Converts the interleaved `input` (whole frames) and appends the
    /// result to `out`.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let (ci, co) = (self.in_channels, self.out_channels);
        // Channel-map first: interpolating fewer channels is cheaper only
        // when downmixing, and mapping first keeps `prev` in one format.
        let mut frames: Vec<f32> = Vec::with_capacity((input.len() / ci + 1) * co);
        if let Some(p) = self.prev.take() {
            frames.extend_from_slice(&p);
        }
        let mut mapped = vec![0.0; co];
        for f in input.chunks_exact(ci) {
            map_channels(f, &mut mapped);
            frames.extend_from_slice(&mapped);
        }
        let n = frames.len() / co;
        if n == 0 {
            return;
        }
        let frame = |i: usize| &frames[i * co..(i + 1) * co];
        loop {
            let i = usize::try_from(self.t / self.den).unwrap_or(usize::MAX);
            let rem = self.t % self.den;
            if i >= n {
                break;
            }
            if rem == 0 {
                // Exactly on an input frame: copy, so equal rates pass
                // through bit for bit.
                out.extend_from_slice(frame(i));
            } else if i + 1 < n {
                let frac = (rem as f64 / self.den as f64) as f32;
                let (a, b) = (frame(i), frame(i + 1));
                out.extend(a.iter().zip(b).map(|(a, b)| a + (b - a) * frac));
            } else {
                // Needs the next block's first frame.
                break;
            }
            self.t += self.step;
        }
        // Keep the last frame; positions become relative to it.
        self.prev = Some(frame(n - 1).to_vec());
        self.t -= (n as u64 - 1) * self.den;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_mapping() {
        let mut out = [0.0; 2];
        map_channels(&[0.5], &mut out);
        assert_eq!(out, [0.5, 0.5]);
        map_channels(&[0.1, 0.2, 0.3, 0.5], &mut out);
        assert!((out[0] - 0.2).abs() < 1e-6 && (out[1] - 0.35).abs() < 1e-6);
        let mut out6 = [9.0; 6];
        map_channels(&[0.1, 0.2], &mut out6);
        assert_eq!(out6, [0.1, 0.2, 0.0, 0.0, 0.0, 0.0]);
        let mut mono = [0.0; 1];
        map_channels(&[0.2, 0.4], &mut mono);
        assert!((mono[0] - 0.3).abs() < 1e-6);
    }

    #[test]
    fn equal_rates_pass_through_bit_for_bit_across_blocks() {
        let input: Vec<f32> = (0..2000)
            .map(|i| ((i * 7919) % 1000) as f32 / 1000.0)
            .collect();
        let mut c = Converter::new(44_100, 2, 44_100, 2);
        let mut out = Vec::new();
        for block in input.chunks(2 * 137) {
            c.push(block, &mut out);
        }
        // The very last frame waits for a successor only when the
        // position is fractional, which it never is here.
        assert_eq!(out, input);
    }

    #[test]
    fn resampling_keeps_the_duration_and_interpolates() {
        // A ramp at 8 kHz to 48 kHz: 6 output frames per input frame.
        let input: Vec<f32> = (0..800).map(|i| i as f32).collect();
        let mut c = Converter::new(8_000, 1, 48_000, 1);
        let mut out = Vec::new();
        for block in input.chunks(97) {
            c.push(block, &mut out);
        }
        // Everything up to the last input frame: 799 * 6 + 1 frames.
        assert_eq!(out.len(), 799 * 6 + 1);
        for (k, v) in out.iter().enumerate() {
            assert!((v - k as f32 / 6.0).abs() < 1e-3, "frame {k}: {v}");
        }
        // Downsampling 48 kHz to 44.1 kHz keeps the length within a frame.
        let mut c = Converter::new(48_000, 1, 44_100, 1);
        let mut out = Vec::new();
        c.push(&vec![0.25; 48_000], &mut out);
        assert!((out.len() as i64 - 44_100).abs() <= 1, "{}", out.len());
        assert!(out.iter().all(|&v| v == 0.25));
    }

    #[test]
    fn reset_starts_fresh() {
        let mut c = Converter::new(1, 1, 2, 1);
        let mut out = Vec::new();
        c.push(&[0.0, 1.0], &mut out);
        assert_eq!(out, [0.0, 0.5, 1.0]);
        c.reset();
        out.clear();
        c.push(&[4.0], &mut out);
        assert_eq!(out, [4.0], "no interpolation from before the reset");
    }
}
