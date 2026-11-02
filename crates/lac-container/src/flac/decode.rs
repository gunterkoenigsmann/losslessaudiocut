//! FLAC frame decoder (RFC 9639, sections 9.2 and 9.3).
//!
//! `lac-container` normally never decodes audio. FLAC is the exception: it is
//! a lossless codec, so the two frames at a cut point can be decoded and
//! re-encoded without any loss, which makes FLAC cuts sample-exact. The same
//! decoder computes the STREAMINFO MD5 of the output.

use super::frame::{ChannelAssignment, FrameHeader};
use crate::bits::{BitReader, EndOfData};

/// Decoded samples of one frame, one vector per channel.
pub type Channels = Vec<Vec<i32>>;

/// Errors while decoding a frame body.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The frame ended early.
    #[error("frame data ends early")]
    Truncated,
    /// A reserved or impossible value was found.
    #[error("invalid frame data: {0}")]
    Invalid(&'static str),
}

impl From<EndOfData> for DecodeError {
    fn from(_: EndOfData) -> Self {
        DecodeError::Truncated
    }
}

/// Decodes the frame in `frame` (starting with its header) whose parsed
/// header is `h`. `bps` is the stream's bits per sample, used when the
/// header refers to STREAMINFO.
pub fn decode_frame(frame: &[u8], h: &FrameHeader, bps: u32) -> Result<Channels, DecodeError> {
    let bps = h.bits_per_sample.unwrap_or(bps);
    let n = h.block_size as usize;
    let body = frame.get(h.len..).ok_or(DecodeError::Truncated)?;
    let mut r = BitReader::new(body);

    let nch = h.channels.channels() as usize;
    let mut chans: Vec<Vec<i64>> = Vec::with_capacity(nch);
    for ch in 0..nch {
        // The side channel needs one extra bit (RFC 9639 9.2).
        let side = match h.channels {
            ChannelAssignment::LeftSide | ChannelAssignment::MidSide => ch == 1,
            ChannelAssignment::SideRight => ch == 0,
            ChannelAssignment::Independent(_) => false,
        };
        chans.push(decode_subframe(&mut r, n, bps + u32::from(side))?);
    }

    // Undo inter-channel decorrelation (RFC 9639 9.2.2).
    match h.channels {
        ChannelAssignment::Independent(_) => {}
        ChannelAssignment::LeftSide => {
            let (l, s) = chans.split_at_mut(1);
            for (left, side) in l[0].iter().zip(s[0].iter_mut()) {
                *side = left - *side; // side -> right
            }
        }
        ChannelAssignment::SideRight => {
            let (s, r) = chans.split_at_mut(1);
            for (side, right) in s[0].iter_mut().zip(r[0].iter()) {
                *side += right; // side -> left
            }
        }
        ChannelAssignment::MidSide => {
            let (m, s) = chans.split_at_mut(1);
            for (mid, side) in m[0].iter_mut().zip(s[0].iter_mut()) {
                // The lost LSB of mid equals the LSB of side.
                let m2 = (*mid << 1) | (*side & 1);
                let (left, right) = ((m2 + *side) >> 1, (m2 - *side) >> 1);
                *mid = left;
                *side = right;
            }
        }
    }

    // Samples that do not fit the declared bit depth mean a corrupt frame.
    let lo = -(1i64 << (bps - 1));
    let hi = (1i64 << (bps - 1)) - 1;
    chans
        .into_iter()
        .map(|c| {
            c.into_iter()
                .map(|s| {
                    if (lo..=hi).contains(&s) {
                        Ok(s as i32)
                    } else {
                        Err(DecodeError::Invalid("sample exceeds bit depth"))
                    }
                })
                .collect()
        })
        .collect()
}

/// Decodes one subframe of `n` samples with `bps` bits per sample.
fn decode_subframe(r: &mut BitReader, n: usize, bps: u32) -> Result<Vec<i64>, DecodeError> {
    if r.read_u32(1)? != 0 {
        return Err(DecodeError::Invalid("subframe padding bit set"));
    }
    let kind = r.read_u32(6)?;
    // Wasted bits: a 1 flag followed by (k - 1) in unary.
    let wasted = if r.read_u32(1)? == 1 {
        r.read_unary()? + 1
    } else {
        0
    };
    if wasted >= bps {
        return Err(DecodeError::Invalid("wasted bits exceed bit depth"));
    }
    let bps = bps - wasted;

    let mut s = match kind {
        0 => vec![r.read_i64(bps)?; n],
        1 => (0..n).map(|_| r.read_i64(bps)).collect::<Result<_, _>>()?,
        8..=12 => {
            let order = (kind - 8) as usize;
            let mut s = warm_up(r, n, order, bps)?;
            decode_residual(r, n, order, &mut s)?;
            restore_fixed(&mut s, order);
            s
        }
        32..=63 => {
            let order = (kind - 31) as usize;
            let mut s = warm_up(r, n, order, bps)?;
            let precision = r.read_u32(4)? + 1;
            if precision == 16 {
                return Err(DecodeError::Invalid("LPC precision 0b1111"));
            }
            let shift = r.read_i64(5)?;
            if shift < 0 {
                return Err(DecodeError::Invalid("negative LPC shift"));
            }
            let coefs = (0..order)
                .map(|_| r.read_i64(precision))
                .collect::<Result<Vec<_>, _>>()?;
            decode_residual(r, n, order, &mut s)?;
            restore_lpc(&mut s, &coefs, shift as u32);
            s
        }
        _ => return Err(DecodeError::Invalid("reserved subframe type")),
    };
    if wasted > 0 {
        for x in &mut s {
            *x <<= wasted;
        }
    }
    Ok(s)
}

/// Reads the `order` unencoded warm-up samples.
fn warm_up(r: &mut BitReader, n: usize, order: usize, bps: u32) -> Result<Vec<i64>, DecodeError> {
    if order > n {
        return Err(DecodeError::Invalid("predictor order exceeds block size"));
    }
    let mut s = Vec::with_capacity(n);
    for _ in 0..order {
        s.push(r.read_i64(bps)?);
    }
    Ok(s)
}

/// Appends the `n - order` residuals to `s` (RFC 9639 9.2.7).
fn decode_residual(
    r: &mut BitReader,
    n: usize,
    order: usize,
    s: &mut Vec<i64>,
) -> Result<(), DecodeError> {
    let param_bits = match r.read_u32(2)? {
        0 => 4,
        1 => 5,
        _ => return Err(DecodeError::Invalid("reserved residual coding method")),
    };
    let escape = (1 << param_bits) - 1;
    let partition_order = r.read_u32(4)?;
    let partitions = 1usize << partition_order;
    if !n.is_multiple_of(partitions) || n / partitions < order {
        return Err(DecodeError::Invalid("bad partition order"));
    }
    for p in 0..partitions {
        let count = n / partitions - if p == 0 { order } else { 0 };
        let k = r.read_u32(param_bits)?;
        if k == escape {
            // Escaped partition: fixed-width signed samples.
            let width = r.read_u32(5)?;
            for _ in 0..count {
                s.push(r.read_i64(width)?);
            }
        } else {
            for _ in 0..count {
                let q = u64::from(r.read_unary()?);
                let u = (q << k) | r.read_u64(k)?;
                // Zig-zag decoding: 0, -1, 1, -2, 2, ...
                s.push((u >> 1) as i64 ^ -((u & 1) as i64));
            }
        }
    }
    Ok(())
}

/// Turns residuals into samples for the fixed predictors (RFC 9639 9.2.5).
fn restore_fixed(s: &mut [i64], order: usize) {
    for i in order..s.len() {
        let p = match order {
            0 => 0,
            1 => s[i - 1],
            2 => 2 * s[i - 1] - s[i - 2],
            3 => 3 * s[i - 1] - 3 * s[i - 2] + s[i - 3],
            _ => 4 * s[i - 1] - 6 * s[i - 2] + 4 * s[i - 3] - s[i - 4],
        };
        s[i] += p;
    }
}

/// Turns residuals into samples for an LPC predictor (RFC 9639 9.2.6).
fn restore_lpc(s: &mut [i64], coefs: &[i64], shift: u32) {
    let order = coefs.len();
    for i in order..s.len() {
        // coefs[0] applies to the most recent sample.
        let sum: i64 = coefs
            .iter()
            .enumerate()
            .map(|(j, c)| c * s[i - 1 - j])
            .sum();
        s[i] += sum >> shift;
    }
}
