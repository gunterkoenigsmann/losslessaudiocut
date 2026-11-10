//! Apple Lossless (ALAC) frames: a decoder, and an encoder for the
//! format's uncompressed ("escape") frames.
//!
//! ALAC is lossless, so a frame cut by a cut point may be rewritten, as
//! FLAC's boundary frames are: it is decoded and the kept samples are
//! written back as an uncompressed frame, which every ALAC decoder reads.
//! That makes joins inside a file sample-exact (issue #75).
//!
//! The bitstream is described by Apple's open-source reference
//! implementation (`ALACDecoder.cpp`, `ag_dec.c`, `dp_dec.c` of the ALAC
//! project, Apache 2.0). Where Apple's and libavcodec's decoders differ in
//! an edge case (the zero-run parameter after a history of exactly 0),
//! this follows libavcodec, which is the decoder this program uses.
//!
//! ## Frame layout
//!
//! A frame is a sequence of syntax elements, each starting with a 3-bit
//! tag, and ends with the tag `END` and byte alignment. A single channel
//! element (`SCE`, also `LFE`) holds one channel, a channel pair element
//! (`CPE`) two. Each element's header:
//!
//! ```text
//! tag 3 | instance 4 | unused 12 | has_size 1 | bytes_shifted 2 | escape 1
//! [samples 32, if has_size]
//! ```
//!
//! A compressed element then holds the stereo mix parameters, per channel
//! an adaptive LPC predictor, the low bytes of each sample ("shifted"
//! bytes, for 24 and 32 bits), and the prediction residuals, coded with an
//! adaptive Golomb-Rice code. An escaped element holds each sample with
//! the stream's bit depth, interleaved.

use crate::bits::{BitReader, BitWriter};

/// Syntax element tags (`ALACAudioTypes.h`).
mod tag {
    /// Single channel element.
    pub const SCE: u32 = 0;
    /// Channel pair element.
    pub const CPE: u32 = 1;
    /// Low-frequency effects element (decoded like `SCE`).
    pub const LFE: u32 = 3;
    /// End of the frame.
    pub const END: u32 = 7;
}

/// The rice code's escape: a prefix of this many 1 bits is followed by the
/// value itself (`MAX_PREFIX_32` in `ag_dec.c`).
const RICE_THRESHOLD: u32 = 9;

/// Most samples (all channels together) of a frame that is decoded. Apple's
/// encoder writes 4096 per channel; a damaged configuration or frame
/// header could otherwise make a few bytes allocate gigabytes, since runs
/// of zeros take almost no bits.
const MAX_FRAME_VALUES: usize = 1 << 22;

/// Why a frame could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AlacError {
    /// The configuration (`ALACSpecificConfig`) is too short or invalid.
    #[error("invalid ALAC configuration")]
    Config,
    /// The frame is damaged or uses a feature ALAC decoders reject.
    #[error("invalid ALAC frame: {0}")]
    Frame(&'static str),
}

impl From<crate::bits::EndOfData> for AlacError {
    fn from(_: crate::bits::EndOfData) -> Self {
        AlacError::Frame("unexpected end of the frame")
    }
}

/// The parts of the `ALACSpecificConfig` (Apple's "magic cookie") that
/// decoding needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlacConfig {
    /// Samples per channel of a full frame.
    pub frame_length: u32,
    /// Bits per sample (16, 20, 24 or 32).
    pub bit_depth: u8,
    /// Rice history multiplier (`pb`, normally 40).
    pub history_mult: u8,
    /// Initial rice history (`mb`, normally 10).
    pub initial_history: u8,
    /// Largest rice parameter (`kb`, normally 14).
    pub rice_limit: u8,
    /// Channels.
    pub channels: u8,
    /// Largest frame in bytes, or 0 if unknown.
    pub max_frame_bytes: u32,
}

/// Offset of `maxFrameBytes` in the `alac` box as [`super::Mp4Codec::Alac`]
/// holds it: 8 bytes box header, 4 bytes version and flags, then 12 bytes
/// of the config before it.
pub const MAX_FRAME_BYTES_AT: usize = 24;

impl AlacConfig {
    /// Parses the `alac` box (header included) of a sample entry.
    pub fn parse(cookie: &[u8]) -> Result<Self, AlacError> {
        // ALACSpecificConfig, 24 bytes after the box header and the full
        // box's version and flags: frameLength 32, compatibleVersion 8,
        // bitDepth 8, pb 8, mb 8, kb 8, numChannels 8, maxRun 16,
        // maxFrameBytes 32, avgBitRate 32, sampleRate 32.
        let c = cookie.get(12..36).ok_or(AlacError::Config)?;
        let u32_at = |i: usize| u32::from_be_bytes([c[i], c[i + 1], c[i + 2], c[i + 3]]);
        let cfg = AlacConfig {
            frame_length: u32_at(0),
            bit_depth: c[5],
            history_mult: c[6],
            initial_history: c[7],
            rice_limit: c[8],
            channels: c[9],
            max_frame_bytes: u32_at(12),
        };
        let valid = cfg.frame_length > 0
            && (1..=32).contains(&cfg.bit_depth)
            && cfg.rice_limit > 0
            && cfg.channels > 0;
        valid.then_some(cfg).ok_or(AlacError::Config)
    }
}

/// One decoded syntax element: its tag and instance, and its channels'
/// samples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    /// [`tag::SCE`], [`tag::CPE`] or [`tag::LFE`].
    tag: u32,
    /// The element instance tag, copied to rewritten frames.
    instance: u32,
    /// One sample vector per channel (two for a pair), each value in the
    /// stream's bit depth.
    pub channels: Vec<Vec<i32>>,
}

/// A decoded frame: its elements in stream order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The syntax elements.
    pub elements: Vec<Element>,
}

impl Frame {
    /// Samples per channel.
    pub fn len(&self) -> usize {
        self.elements
            .first()
            .and_then(|e| e.channels.first())
            .map_or(0, Vec::len)
    }

    /// Whether the frame holds no samples.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Encodes samples `range` of this frame as an uncompressed frame with
    /// the same elements. A frame shorter than `cfg.frame_length` states
    /// its length.
    pub fn encode_escaped(&self, cfg: &AlacConfig, range: std::ops::Range<usize>) -> Vec<u8> {
        let n = range.len();
        let depth = u32::from(cfg.bit_depth);
        let mut w = BitWriter::new();
        for e in &self.elements {
            w.write(u64::from(e.tag), 3);
            w.write(u64::from(e.instance), 4);
            w.write(0, 12);
            let has_size = n as u64 != u64::from(cfg.frame_length);
            w.write(u64::from(has_size), 1);
            w.write(0, 2); // no shifted bytes
            w.write(1, 1); // escape: uncompressed
            if has_size {
                w.write(n as u64, 32);
            }
            for i in range.clone() {
                for ch in &e.channels {
                    w.write_signed(i64::from(ch[i]), depth);
                }
            }
        }
        w.write(u64::from(tag::END), 3);
        w.align();
        w.into_bytes()
    }
}

/// Decodes one frame.
pub fn decode(cfg: &AlacConfig, data: &[u8]) -> Result<Frame, AlacError> {
    let mut r = BitReader::new(data);
    let mut elements = Vec::new();
    let mut len = None;
    let mut channels = 0usize;
    loop {
        let t = r.read_u32(3)?;
        let n_ch = match t {
            tag::END => break,
            tag::SCE | tag::LFE => 1,
            tag::CPE => 2,
            // Coupling channels, data stream, program config and fill
            // elements are not written by ALAC encoders, and libavcodec
            // refuses them too.
            _ => return Err(AlacError::Frame("unsupported syntax element")),
        };
        let e = decode_element(cfg, &mut r, t, n_ch)?;
        let n = e.channels[0].len();
        if *len.get_or_insert(n) != n {
            return Err(AlacError::Frame("elements of different lengths"));
        }
        channels += n_ch;
        if channels > usize::from(cfg.channels) {
            return Err(AlacError::Frame("more channels than configured"));
        }
        elements.push(e);
    }
    if channels != usize::from(cfg.channels) {
        return Err(AlacError::Frame("fewer channels than configured"));
    }
    Ok(Frame { elements })
}

/// The parameters of one channel's predictor.
#[derive(Debug, Default, Clone)]
struct Predictor {
    /// 0: one LPC pass; 15: a first-order pass before it.
    mode: u32,
    /// Right shift of the prediction (`denShift`, libavcodec's `lpc_quant`).
    quant: u32,
    /// Factor of the rice history multiplier, in quarters.
    history_factor: u32,
    /// Coefficients, the last read first (as libavcodec stores them).
    coefs: Vec<i32>,
}

fn decode_element(
    cfg: &AlacConfig,
    r: &mut BitReader,
    t: u32,
    n_ch: usize,
) -> Result<Element, AlacError> {
    let instance = r.read_u32(4)?;
    r.read_u32(12)?; // unused, zero
    let has_size = r.read_u32(1)? == 1;
    let shift = r.read_u32(2)? * 8;
    let escape = r.read_u32(1)? == 1;
    let n = if has_size {
        r.read_u32(32)?
    } else {
        cfg.frame_length
    };
    if n > cfg.frame_length {
        return Err(AlacError::Frame("more samples than a frame holds"));
    }
    if n as usize * usize::from(cfg.channels) > MAX_FRAME_VALUES {
        return Err(AlacError::Frame("implausibly long frame"));
    }
    let n = n as usize;
    let depth = u32::from(cfg.bit_depth);
    let mut out = vec![vec![0i32; n]; n_ch];

    if escape {
        for i in 0..n {
            for ch in out.iter_mut() {
                ch[i] = r.read_i64(depth)? as i32;
            }
        }
        return Ok(Element {
            tag: t,
            instance,
            channels: out,
        });
    }

    // Bits per residual: the depth without the shifted bytes, plus one
    // for the side channel of a pair.
    let bps = (depth + n_ch as u32 - 1)
        .checked_sub(shift)
        .filter(|b| (1..=32).contains(b))
        .ok_or(AlacError::Frame("invalid sample size"))?;
    let mix_shift = r.read_u32(8)?;
    let mix_weight = r.read_u32(8)? as i32;
    if n_ch == 2 && mix_weight != 0 && mix_shift > 31 {
        return Err(AlacError::Frame("invalid stereo mix"));
    }
    let mut preds = Vec::with_capacity(n_ch);
    for _ in 0..n_ch {
        let mode = r.read_u32(4)?;
        let quant = r.read_u32(4)?;
        let history_factor = r.read_u32(3)?;
        let order = r.read_u32(5)? as usize;
        if quant == 0 || order >= cfg.frame_length as usize {
            return Err(AlacError::Frame("invalid predictor"));
        }
        let mut coefs = vec![0i32; order];
        for c in coefs.iter_mut().rev() {
            *c = r.read_i64(16)? as i32;
        }
        preds.push(Predictor {
            mode,
            quant,
            history_factor,
            coefs,
        });
    }
    // The shifted low bytes, interleaved, before the residuals.
    let mut low = vec![vec![0i32; n]; n_ch];
    if shift > 0 {
        for i in 0..n {
            for ch in low.iter_mut() {
                ch[i] = r.read_u32(shift)? as i32;
            }
        }
    }
    for (ch, p) in out.iter_mut().zip(&preds) {
        let mult = p.history_factor * u32::from(cfg.history_mult) / 4;
        let mut residual = rice_decode(cfg, r, n, bps, mult)?;
        if p.mode == 15 {
            // Mode 15 runs a first-order predictor over the residuals
            // first (libavcodec: "lpc_order 31").
            first_order(&mut residual, bps);
        } else if p.mode != 0 {
            return Err(AlacError::Frame("unknown prediction mode"));
        }
        lpc(&residual, ch, bps, p.coefs.clone(), p.quant);
    }

    if n_ch == 2 && mix_weight != 0 {
        // dp_dec.c unmix: left = u + v - (weight * v >> shift), right =
        // left - v, in 32-bit two's complement arithmetic.
        let (u, v) = out.split_at_mut(1);
        for (a, b) in u[0].iter_mut().zip(v[0].iter_mut()) {
            let right = a.wrapping_sub(b.wrapping_mul(mix_weight) >> mix_shift);
            let left = right.wrapping_add(*b);
            *a = left;
            *b = right;
        }
    }
    if shift > 0 {
        for (ch, l) in out.iter_mut().zip(&low) {
            for (s, &lo) in ch.iter_mut().zip(l) {
                *s = (*s << shift) | lo;
            }
        }
    }
    Ok(Element {
        tag: t,
        instance,
        channels: out,
    })
}

/// Sign-extends the low `bits` bits of `v`.
fn sign_extend(v: i32, bits: u32) -> i32 {
    let s = 32 - bits;
    (v << s) >> s
}

/// Reads `n` bits like libavcodec's `show_bits`, as zeros past the end.
fn peek(r: &BitReader, n: u32) -> u32 {
    let mut p = r.clone();
    let mut v = 0;
    for _ in 0..n {
        v = (v << 1) | p.read_u32(1).unwrap_or(0);
    }
    v
}

/// One value of the adaptive Golomb code (`dyn_get_32bit` in `ag_dec.c`,
/// `decode_scalar` in libavcodec): a unary prefix `x` of 1 bits, ended by
/// a 0 bit; from [`RICE_THRESHOLD`] on, the value follows in `bps` bits.
/// Otherwise the value is `x * (2^k - 1)` plus the next `k` bits minus 1,
/// where a suffix of 0 or 1 takes only `k - 1` bits.
fn rice_value(r: &mut BitReader, k: u32, bps: u32) -> Result<u32, AlacError> {
    let mut x = 0;
    while x < RICE_THRESHOLD && r.read_u32(1)? == 1 {
        x += 1;
    }
    if x >= RICE_THRESHOLD {
        return Ok(r.read_u32(bps)?);
    }
    if k == 1 {
        return Ok(x);
    }
    let extra = peek(r, k);
    let mut v = (x << k) - x;
    if extra > 1 {
        v += extra - 1;
        r.read_u32(k)?;
    } else {
        r.read_u32(k - 1)?;
    }
    Ok(v)
}

/// The prediction residuals of one channel (`dyn_decomp` in `ag_dec.c`,
/// `rice_decompress` in libavcodec). The rice parameter follows a running
/// mean (`history`); when it falls low, a run of zeros may follow.
fn rice_decode(
    cfg: &AlacConfig,
    r: &mut BitReader,
    n: usize,
    bps: u32,
    mult: u32,
) -> Result<Vec<i32>, AlacError> {
    let limit = u32::from(cfg.rice_limit);
    let mut out = vec![0i32; n];
    let mut history = u32::from(cfg.initial_history);
    let mut sign_modifier = 0;
    let mut i = 0;
    while i < n {
        let k = ilog2((history >> 9) + 3).min(limit);
        let x = rice_value(r, k, bps)?.wrapping_add(sign_modifier);
        sign_modifier = 0;
        // The lowest bit is the sign.
        out[i] = ((x >> 1) as i32) ^ -((x & 1) as i32);
        if x > 0xFFFF {
            history = 0xFFFF;
        } else {
            history = history
                .wrapping_add(x.wrapping_mul(mult))
                .wrapping_sub(history.wrapping_mul(mult) >> 9);
        }
        if history < 128 && i + 1 < n {
            // A run of zero residuals.
            let k = (7 - ilog2(history) + ((history + 16) >> 6)).min(limit);
            let run = rice_value(r, k, 16)? as usize;
            if run >= n - i {
                return Err(AlacError::Frame("zero run beyond the frame"));
            }
            i += run;
            if run <= 0xFFFF {
                sign_modifier = 1;
            }
            history = 0;
        }
        i += 1;
    }
    Ok(out)
}

/// `floor(log2(v))`, 0 for 0 (libavcodec's `av_log2`).
fn ilog2(v: u32) -> u32 {
    31 - (v | 1).leading_zeros()
}

/// Mode 15's first-order prediction, in place.
fn first_order(buf: &mut [i32], bps: u32) {
    for i in 1..buf.len() {
        buf[i] = sign_extend(buf[i - 1].wrapping_add(buf[i]), bps);
    }
}

/// The adaptive LPC predictor (`unpc_block` in `dp_dec.c`,
/// `lpc_prediction` in libavcodec): predicts each sample from the
/// `coefs.len()` before it and adapts the coefficients by the sign of the
/// residual, in 32-bit two's complement arithmetic like the reference.
fn lpc(residual: &[i32], out: &mut [i32], bps: u32, mut coefs: Vec<i32>, quant: u32) {
    let n = residual.len();
    if n == 0 {
        return;
    }
    out[0] = residual[0];
    let order = coefs.len();
    if order == 0 {
        out[1..].copy_from_slice(&residual[1..]);
        return;
    }
    // Warm-up: first-order prediction.
    let mut i = 1;
    while i <= order && i < n {
        out[i] = sign_extend(out[i - 1].wrapping_add(residual[i]), bps);
        i += 1;
    }
    while i < n {
        // `d` is the sample before the predictor's window, `win` the
        // window of `order` samples in front of sample i.
        let base = i - order - 1;
        let d = out[base];
        let mut val = 0i32;
        for (j, c) in coefs.iter().enumerate() {
            val = val.wrapping_add(out[base + 1 + j].wrapping_sub(d).wrapping_mul(*c));
        }
        let rounded = (i64::from(val) + (1i64 << (quant - 1))) >> quant;
        let mut err = residual[i];
        out[i] = sign_extend((rounded as i32).wrapping_add(d).wrapping_add(err), bps);
        let sign = err.signum();
        if sign != 0 {
            // Adapt while the residual still has the same sign.
            for (j, c) in coefs.iter_mut().enumerate() {
                if err.wrapping_mul(sign) <= 0 {
                    break;
                }
                let v = d.wrapping_sub(out[base + 1 + j]);
                let s = v.signum() * sign;
                *c -= s;
                let v = v.wrapping_mul(s);
                err = err.wrapping_sub((v >> quant).wrapping_mul(j as i32 + 1));
            }
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(frame_length: u32, depth: u8, channels: u8) -> Vec<u8> {
        let mut c = vec![0, 0, 0, 36];
        c.extend(b"alac");
        c.extend([0; 4]);
        c.extend(frame_length.to_be_bytes());
        c.extend([0, depth, 40, 10, 14, channels, 0, 255]);
        c.extend([0; 12]);
        c
    }

    #[test]
    fn config_is_parsed_and_checked() {
        let cfg = AlacConfig::parse(&cookie(4096, 16, 2)).expect("valid");
        assert_eq!(
            cfg,
            AlacConfig {
                frame_length: 4096,
                bit_depth: 16,
                history_mult: 40,
                initial_history: 10,
                rice_limit: 14,
                channels: 2,
                max_frame_bytes: 0,
            }
        );
        assert_eq!(
            AlacConfig::parse(&cookie(4096, 16, 2)[..35]),
            Err(AlacError::Config)
        );
        assert_eq!(AlacConfig::parse(&cookie(0, 16, 2)), Err(AlacError::Config));
        assert_eq!(
            AlacConfig::parse(&cookie(4096, 0, 2)),
            Err(AlacError::Config)
        );
    }

    /// Escaped frames decode to exactly what was encoded, at every depth,
    /// as mono, a pair and a pair plus a mono element; full frames carry
    /// no size, shorter ones do.
    #[test]
    fn escaped_frames_round_trip() {
        for depth in [16u8, 20, 24, 32] {
            for layout in [&[tag::SCE][..], &[tag::CPE], &[tag::SCE, tag::CPE]] {
                let channels: u8 = layout
                    .iter()
                    .map(|&t| if t == tag::CPE { 2 } else { 1 })
                    .sum();
                let cfg = AlacConfig::parse(&cookie(8, depth, channels)).expect("valid");
                let max = if depth == 32 {
                    i32::MAX
                } else {
                    (1 << (depth - 1)) - 1
                };
                let min = if depth == 32 {
                    i32::MIN
                } else {
                    -(1 << (depth - 1))
                };
                let elements = layout
                    .iter()
                    .enumerate()
                    .map(|(k, &t)| Element {
                        tag: t,
                        instance: k as u32,
                        channels: (0..if t == tag::CPE { 2 } else { 1 })
                            .map(|c| vec![min, max, 0, -1, 1, 7 * c, -300, max - c])
                            .collect(),
                    })
                    .collect();
                let f = Frame { elements };
                let full = f.encode_escaped(&cfg, 0..8);
                assert_eq!(decode(&cfg, &full), Ok(f.clone()), "{depth} {layout:?}");
                let part = f.encode_escaped(&cfg, 2..5);
                assert!(part.len() < full.len());
                let d = decode(&cfg, &part).expect("decodes");
                assert_eq!(d.len(), 3);
                for (a, b) in d.elements.iter().zip(&f.elements) {
                    for (x, y) in a.channels.iter().zip(&b.channels) {
                        assert_eq!(x[..], y[2..5]);
                    }
                }
            }
        }
    }

    /// Damaged frames are errors, never panics.
    #[test]
    fn damaged_frames_are_errors() {
        let cfg = AlacConfig::parse(&cookie(8, 16, 2)).expect("valid");
        let f = Frame {
            elements: vec![Element {
                tag: tag::CPE,
                instance: 0,
                channels: vec![vec![1; 8], vec![2; 8]],
            }],
        };
        let good = f.encode_escaped(&cfg, 0..8);
        for n in 0..good.len() {
            assert!(decode(&cfg, &good[..n]).is_err(), "{n}");
        }
        // A mono frame where a pair is configured.
        let mono = AlacConfig::parse(&cookie(8, 16, 1)).expect("valid");
        assert!(decode(&mono, &good).is_err());
        assert!(decode(&cfg, &[0xFF; 64]).is_err());
        // A huge frame length is refused before anything is allocated.
        let huge = AlacConfig::parse(&cookie(u32::MAX, 16, 2)).expect("valid");
        let mut w = BitWriter::new();
        w.write(u64::from(tag::CPE), 3);
        w.write(0, 16);
        w.write(0b1001, 4); // has size, not shifted, escaped
        w.write(u64::from(u32::MAX), 32);
        assert_eq!(
            decode(&huge, &w.into_bytes()),
            Err(AlacError::Frame("implausibly long frame"))
        );
        for seed in 0u32..200 {
            let noise: Vec<u8> = (0..64u32)
                .map(|i| (i.wrapping_mul(2_654_435_761) ^ seed.wrapping_mul(40_503)) as u8)
                .collect();
            let _ = decode(&cfg, &noise);
        }
    }

    #[test]
    fn rice_values() {
        // k = 3: prefix 2 (110), suffix 101 = 5: 2 * 7 + 5 - 1 = 18.
        let b = [0b1101_0100];
        assert_eq!(rice_value(&mut BitReader::new(&b), 3, 16), Ok(18));
        // Suffix 001 (< 2): only 2 bits are taken, value 2 * 7.
        let b = [0b1100_0100];
        let mut r = BitReader::new(&b);
        assert_eq!(rice_value(&mut r, 3, 16), Ok(14));
        assert_eq!(r.read_u32(3), Ok(0b100));
        // Nine 1 bits: the value follows in bps bits.
        let mut w = BitWriter::new();
        w.write(0x1FF, 9);
        w.write(0x1234, 16);
        let b = w.into_bytes();
        let mut r = BitReader::new(&b);
        assert_eq!(rice_value(&mut r, 5, 16), Ok(0x1234));
        assert_eq!(ilog2(0), 0);
        assert_eq!(ilog2(3), 1);
        assert_eq!(ilog2(1 << 20), 20);
    }
}
