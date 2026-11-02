//! Small FLAC frame encoder for the boundary frames of a cut.
//!
//! Boundary frames are rare (about two per kept segment), so simplicity
//! beats compression: channels are coded independently, each with the best
//! FIXED predictor (order 0..4, RFC 9639 9.2.5) and a single Rice partition.
//! For bit depths above 24, where residuals could exceed 32 bits, VERBATIM
//! subframes are used instead.
//!
//! Real prediction matters for more than size: VERBATIM data of quiet audio
//! contains many byte-aligned `FF F8` patterns (the 16-bit sample -8), which
//! look like frame sync codes. libFLAC copes, but ffmpeg's FLAC parser can
//! lose track of frames on them. Rice-coded residuals rarely contain such
//! long runs of 1 bits.

use super::crc::crc16;
use super::frame::FrameHeader;
use crate::bits::BitWriter;

/// Largest bit depth for which FIXED prediction is used.
const MAX_PREDICTED_BPS: u32 = 24;

/// Encodes `chans` (one vector per channel, all of equal length between 1
/// and 65536) as a frame starting at sample `first_sample`. `template` is a
/// frame header of the source stream (see [`FrameHeader::new_variable_like`]).
pub fn encode_frame(
    chans: &[Vec<i32>],
    first_sample: u64,
    bps: u32,
    template: &FrameHeader,
) -> Vec<u8> {
    let n = chans.first().map_or(0, Vec::len);
    let mut w = BitWriter::new();
    for &b in &template.new_variable_like(first_sample, n as u32) {
        w.write(u64::from(b), 8);
    }
    for c in chans {
        if bps <= MAX_PREDICTED_BPS {
            write_fixed_subframe(&mut w, c, bps);
        } else {
            write_verbatim_subframe(&mut w, c, bps);
        }
    }
    w.align();
    let mut bytes = w.into_bytes();
    let crc = crc16(&bytes);
    bytes.extend(crc.to_be_bytes());
    bytes
}

fn write_verbatim_subframe(w: &mut BitWriter, c: &[i32], bps: u32) {
    // Subframe header: padding 0, type 0b000001 (VERBATIM), no wasted bits.
    w.write(0b0000_0010, 8);
    for &s in c {
        w.write_signed(i64::from(s), bps);
    }
}

/// Writes the cheapest FIXED subframe for `c`.
fn write_fixed_subframe(w: &mut BitWriter, c: &[i32], bps: u32) {
    let max_order = c.len().min(4);
    let (order, param, _) = (0..=max_order)
        .map(|order| {
            let (param, bits) = best_rice_param(&residuals(c, order));
            (order, param, bits)
        })
        .min_by_key(|&(_, _, bits)| bits)
        .unwrap_or((0, 0, 0));
    let res = residuals(c, order);

    // Subframe header: padding 0, type 0b001xxx (FIXED, order xxx), no
    // wasted bits.
    w.write((0b1000 | order as u64) << 1, 8);
    for &s in &c[..order] {
        w.write_signed(i64::from(s), bps);
    }
    // Residual: method 0b00 (4-bit parameters) or 0b01 (5-bit) and a
    // single partition (partition order 0).
    let five_bit = param > 14;
    w.write(u64::from(five_bit), 2);
    w.write(0, 4);
    w.write(u64::from(param), if five_bit { 5 } else { 4 });
    for &r in &res {
        let u = zigzag(r);
        for _ in 0..(u >> param) {
            w.write(0, 1);
        }
        w.write(1, 1);
        w.write(u & ((1 << param) - 1), param);
    }
}

/// Residuals of the FIXED predictor of `order` (after the warm-up samples).
fn residuals(c: &[i32], order: usize) -> Vec<i64> {
    let s = |i: usize| i64::from(c[i]);
    (order..c.len())
        .map(|i| {
            let p = match order {
                0 => 0,
                1 => s(i - 1),
                2 => 2 * s(i - 1) - s(i - 2),
                3 => 3 * s(i - 1) - 3 * s(i - 2) + s(i - 3),
                _ => 4 * s(i - 1) - 6 * s(i - 2) + 4 * s(i - 3) - s(i - 4),
            };
            s(i) - p
        })
        .collect()
}

/// Maps signed residuals to unsigned: 0, -1, 1, -2, 2, ...
fn zigzag(r: i64) -> u64 {
    ((r << 1) ^ (r >> 63)) as u64
}

/// The Rice parameter (0..=30, never an escape code) with the fewest bits,
/// and that bit count.
fn best_rice_param(res: &[i64]) -> (u32, u64) {
    (0..=30u32)
        .filter(|&p| p != 15) // 15 is the escape code of the 4-bit method
        .map(|p| {
            let bits: u64 = res
                .iter()
                .map(|&r| (zigzag(r) >> p) + 1 + u64::from(p))
                .sum();
            (p, bits)
        })
        .min_by_key(|&(_, bits)| bits)
        .unwrap_or((0, 0))
}

#[cfg(test)]
mod tests {
    use super::super::crc::crc8;
    use super::super::decode::decode_frame;
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Encoding then decoding returns the input for every bit depth
        /// (FIXED subframes up to 24 bit, VERBATIM above).
        #[test]
        fn encode_round_trip(
            bps in 4u32..=32,
            nch in 1usize..=8,
            n in 1usize..300,
            smooth in any::<bool>(),
            seed in any::<u64>(),
        ) {
            let mut x = seed | 1;
            let lo = -(1i64 << (bps - 1));
            let span = 1i64 << bps;
            let chans: Vec<Vec<i32>> = (0..nch)
                .map(|_| (0..n).map(|_| {
                    x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                    (lo + (x % span as u64) as i64) as i32
                }).collect())
                // Smooth signals exercise higher predictor orders.
                .map(|c: Vec<i32>| if smooth {
                    let mut acc = 0i64;
                    c.iter().map(|&v| { acc = (acc * 7 + i64::from(v)) / 8; acc as i32 }).collect()
                } else { c })
                .collect();
            // Template header: `nch` channels, rate and size from STREAMINFO.
            let mut t = vec![0xFF, 0xF8, 0xC0, (nch as u8 - 1) << 4, 0x00];
            t.push(crc8(&t));
            let template = FrameHeader::parse(&t).expect("valid template");
            let f = encode_frame(&chans, 12_345, bps, &template);
            let h = FrameHeader::parse(&f).expect("own header parses");
            prop_assert_eq!(crc16(&f[..f.len() - 2]).to_be_bytes(), [f[f.len() - 2], f[f.len() - 1]]);
            prop_assert_eq!(decode_frame(&f, &h, bps).expect("decodes"), chans);
        }
    }
}
