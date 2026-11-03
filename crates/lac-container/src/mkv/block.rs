//! Matroska Block and SimpleBlock bodies (RFC 9559 10), including lacing.
//!
//! Layout: track number (a size-style VINT), a 16-bit signed timestamp
//! relative to the cluster, one flag byte, then the frame data. Bits 1-2
//! of the flags select the lacing, which packs several frames into one
//! block:
//!
//! - `00` no lacing: one frame.
//! - `01` Xiph lacing: frame count minus one, then every size but the
//!   last as runs of 255 plus a remainder.
//! - `11` EBML lacing: frame count minus one, the first size as a VINT,
//!   the others as signed VINT differences to the previous size.
//! - `10` fixed-size lacing: frame count minus one, all frames equal.
//!
//! The last frame's size is whatever remains.

use super::ebml::{read_signed_vint, read_size};

/// A parsed block body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// Track the block belongs to.
    pub track: u64,
    /// Frames as (offset, length) relative to the start of the body.
    pub frames: Vec<(usize, usize)>,
}

/// Parses a Block or SimpleBlock body.
pub fn parse_block(b: &[u8]) -> Result<Block, &'static str> {
    let (track, n) = read_size(b).map_err(|_| "invalid track number")?;
    let track = track.ok_or("invalid track number")?;
    // Timestamp (2 bytes) and flags.
    let flags = *b.get(n + 2).ok_or("block too short")?;
    let mut p = n + 3;
    let lacing = (flags >> 1) & 3;
    if lacing == 0 {
        return Ok(Block {
            track,
            frames: vec![(p, b.len() - p)],
        });
    }
    let count = usize::from(*b.get(p).ok_or("block too short")?) + 1;
    p += 1;
    let mut sizes = Vec::with_capacity(count);
    match lacing {
        1 => {
            for _ in 1..count {
                let mut size = 0usize;
                loop {
                    let v = *b.get(p).ok_or("Xiph lacing runs past the block")?;
                    p += 1;
                    size += usize::from(v);
                    if v < 255 {
                        break;
                    }
                }
                sizes.push(size);
            }
        }
        3 => {
            let (first, m) = read_size(&b[p..]).map_err(|_| "invalid EBML lacing")?;
            let mut size = i64::try_from(first.ok_or("invalid EBML lacing")?)
                .map_err(|_| "invalid EBML lacing")?;
            p += m;
            sizes.push(usize::try_from(size).map_err(|_| "invalid EBML lacing")?);
            for _ in 2..count {
                let (d, m) = read_signed_vint(&b[p..]).map_err(|_| "invalid EBML lacing")?;
                p += m;
                size += d;
                sizes.push(usize::try_from(size).map_err(|_| "negative EBML lace size")?);
            }
        }
        _ => {
            let rest = b.len() - p;
            if !rest.is_multiple_of(count) {
                return Err("fixed-size lacing does not divide the block");
            }
            sizes.resize(count - 1, rest / count);
        }
    }
    let used: usize = sizes.iter().sum();
    let last = (b.len() - p)
        .checked_sub(used)
        .ok_or("laced frames run past the block")?;
    sizes.push(last);
    let frames = sizes
        .into_iter()
        .map(|s| {
            let f = (p, s);
            p += s;
            f
        })
        .collect();
    Ok(Block { track, frames })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Header for track 1, timestamp 0, with the given flags.
    fn head(flags: u8) -> Vec<u8> {
        vec![0x81, 0, 0, flags]
    }

    fn frames(b: &[u8]) -> Vec<Vec<u8>> {
        let blk = parse_block(b).expect("block");
        assert_eq!(blk.track, 1);
        blk.frames
            .iter()
            .map(|&(o, l)| b[o..o + l].to_vec())
            .collect()
    }

    #[test]
    fn unlaced() {
        let b = [head(0x80), vec![1, 2, 3]].concat();
        assert_eq!(frames(&b), vec![vec![1, 2, 3]]);
    }

    #[test]
    fn xiph_lacing() {
        // Three frames of 300, 2 and 1 bytes: 300 = 255 + 45.
        let mut b = head(0x02);
        b.extend([2, 255, 45, 2]);
        b.extend(vec![7; 300]);
        b.extend([8, 8, 9]);
        assert_eq!(frames(&b), vec![vec![7; 300], vec![8, 8], vec![9]]);
    }

    #[test]
    fn ebml_lacing() {
        // Sizes 10, 8 (difference -2: 0xBF - 2 = 0xBD), 5 (the rest).
        let mut b = head(0x06);
        b.extend([2, 0x8A, 0xBD]);
        b.extend([1; 10]);
        b.extend([2; 8]);
        b.extend([3; 5]);
        assert_eq!(frames(&b), vec![vec![1; 10], vec![2; 8], vec![3; 5]]);
    }

    #[test]
    fn fixed_lacing() {
        let mut b = head(0x04);
        b.extend([1, 4, 4, 5, 5]);
        assert_eq!(frames(&b), vec![vec![4, 4], vec![5, 5]]);
        let mut odd = head(0x04);
        odd.extend([1, 4, 4, 5]);
        assert!(parse_block(&odd).is_err());
    }

    #[test]
    fn malformed_blocks_are_errors() {
        assert!(parse_block(&[]).is_err());
        assert!(parse_block(&[0x81, 0]).is_err());
        // Xiph sizes larger than the block.
        assert!(parse_block(&[0x81, 0, 0, 0x02, 1, 200, 1]).is_err());
        // EBML lacing whose second size would be negative.
        assert!(parse_block(&[0x81, 0, 0, 0x06, 2, 0x81, 0x80, 1]).is_err());
        // Lacing count byte missing.
        assert!(parse_block(&[0x81, 0, 0, 0x06]).is_err());
    }
}
