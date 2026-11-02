//! FLAC frame headers (RFC 9639, section 9.1).
//!
//! ```text
//! sync 0b1111_1111_1111_10 | reserved 0 | blocking strategy (1 bit)
//! block size code (4) | sample rate code (4)
//! channel assignment (4) | sample size code (3) | reserved 0 (1)
//! coded frame or sample number (UTF-8-like, 1..7 bytes)
//! [block size - 1, 8 or 16 bit]   if block size code is 0b0110 / 0b0111
//! [sample rate, 8 or 16 bit]      if sample rate code is 0b1100..0b1110
//! CRC-8 of all header bytes above
//! ```

use super::crc::crc8;

/// How the channels of a frame are coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelAssignment {
    /// `n` channels coded separately.
    Independent(u8),
    /// Left channel and side (left - right).
    LeftSide,
    /// Side (left - right) and right channel.
    SideRight,
    /// Mid ((left + right) / 2) and side.
    MidSide,
}

impl ChannelAssignment {
    /// Number of audio channels.
    pub fn channels(self) -> u8 {
        match self {
            ChannelAssignment::Independent(n) => n,
            _ => 2,
        }
    }
}

/// A parsed frame header plus the raw codes needed to write it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameHeader {
    /// Variable block size stream: the coded number is a sample number;
    /// otherwise it is a frame number.
    pub variable_blocksize: bool,
    /// Samples per channel in this frame.
    pub block_size: u32,
    /// Sample rate in Hz, or `None` for "see STREAMINFO".
    pub sample_rate: Option<u32>,
    /// Channel coding.
    pub channels: ChannelAssignment,
    /// Bits per sample, or `None` for "see STREAMINFO".
    pub bits_per_sample: Option<u32>,
    /// The coded frame or sample number.
    pub number: u64,
    /// Header length in bytes, including the CRC-8.
    pub len: usize,
    /// Byte 2 of the header (block size and sample rate codes), verbatim.
    raw_codes: u8,
    /// Byte 3 of the header (channels, sample size), verbatim.
    raw_format: u8,
    /// Extra block size bytes, verbatim.
    raw_bs_extra: Vec<u8>,
    /// Extra sample rate bytes, verbatim.
    raw_sr_extra: Vec<u8>,
}

/// Why a candidate position is not a frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// Not enough bytes to decide.
    Truncated,
    /// Not a header (bad sync, reserved value or CRC).
    Invalid,
}

impl FrameHeader {
    /// Parses a header at the start of `b`.
    pub fn parse(b: &[u8]) -> Result<Self, HeaderError> {
        if b.len() < 6 {
            return Err(HeaderError::Truncated);
        }
        if b[0] != 0xFF || b[1] & 0xFE != 0xF8 {
            return Err(HeaderError::Invalid);
        }
        let variable_blocksize = b[1] & 1 == 1;
        let bs_code = b[2] >> 4;
        let sr_code = b[2] & 0x0F;
        let ch_code = b[3] >> 4;
        let ss_code = (b[3] >> 1) & 0x07;
        if b[3] & 1 != 0 || bs_code == 0 || sr_code == 0x0F || ch_code > 10 || ss_code == 3 {
            return Err(HeaderError::Invalid);
        }

        let (number, mut pos) = decode_coded_number(&b[4..]).map(|(n, l)| (n, 4 + l))?;
        let extra_start = pos;
        // Reads an `n`-byte big-endian extra field at `pos` and advances it.
        let take = |pos: &mut usize, n: usize| -> Result<u32, HeaderError> {
            let s = b.get(*pos..*pos + n).ok_or(HeaderError::Truncated)?;
            *pos += n;
            Ok(s.iter().fold(0, |a, &x| (a << 8) | u32::from(x)))
        };
        let block_size = match bs_code {
            1 => 192,
            2..=5 => 576 << (bs_code - 2),
            6 => take(&mut pos, 1)? + 1,
            7 => take(&mut pos, 2)? + 1,
            _ => 256 << (bs_code - 8),
        };
        let sr_extra_start = pos;
        let sample_rate = match sr_code {
            0 => None,
            1 => Some(88_200),
            2 => Some(176_400),
            3 => Some(192_000),
            4 => Some(8_000),
            5 => Some(16_000),
            6 => Some(22_050),
            7 => Some(24_000),
            8 => Some(32_000),
            9 => Some(44_100),
            10 => Some(48_000),
            11 => Some(96_000),
            12 => Some(take(&mut pos, 1)? * 1000),
            13 => Some(take(&mut pos, 2)?),
            _ => Some(take(&mut pos, 2)? * 10), // 14; 15 was rejected above
        };
        let raw_bs_extra = b[extra_start..sr_extra_start].to_vec();
        let raw_sr_extra = b[sr_extra_start..pos].to_vec();
        let crc = *b.get(pos).ok_or(HeaderError::Truncated)?;
        if crc8(&b[..pos]) != crc {
            return Err(HeaderError::Invalid);
        }
        let channels = match ch_code {
            0..=7 => ChannelAssignment::Independent(ch_code + 1),
            8 => ChannelAssignment::LeftSide,
            9 => ChannelAssignment::SideRight,
            _ => ChannelAssignment::MidSide,
        };
        let bits_per_sample = match ss_code {
            0 => None,
            1 => Some(8),
            2 => Some(12),
            4 => Some(16),
            5 => Some(20),
            6 => Some(24),
            _ => Some(32), // 7; 3 was rejected above
        };
        Ok(Self {
            variable_blocksize,
            block_size,
            sample_rate,
            channels,
            bits_per_sample,
            number,
            len: pos + 1,
            raw_codes: b[2],
            raw_format: b[3],
            raw_bs_extra,
            raw_sr_extra,
        })
    }

    /// Serialises this header as a variable-block-size header whose coded
    /// number is `first_sample`; all other fields are kept verbatim.
    pub fn to_variable_bytes(&self, first_sample: u64) -> Vec<u8> {
        let mut h = vec![0xFF, 0xF9, self.raw_codes, self.raw_format];
        h.extend(encode_coded_number(first_sample));
        h.extend(&self.raw_bs_extra);
        h.extend(&self.raw_sr_extra);
        h.push(crc8(&h));
        h
    }

    /// Builds a variable-block-size header for a newly encoded frame of
    /// `block_size` samples with independent channels, starting at sample
    /// `first_sample`.
    ///
    /// Sample rate and sample size are coded exactly as in `self`, a header
    /// of the source stream. Writing "see STREAMINFO" instead would be
    /// equally valid, but ffmpeg's FLAC parser treats a change of these
    /// codes between adjacent frames as a hint of a false sync and can then
    /// lose track of frames.
    pub fn new_variable_like(&self, first_sample: u64, block_size: u32) -> Vec<u8> {
        debug_assert!((1..=65_536).contains(&block_size));
        let channels = self.channels.channels();
        // Block size code 0b0111: 16-bit (block size - 1) follows.
        let codes = 0x70 | (self.raw_codes & 0x0F);
        let format = ((channels - 1) << 4) | (self.raw_format & 0x0E);
        let mut h = vec![0xFF, 0xF9, codes, format];
        h.extend(encode_coded_number(first_sample));
        h.extend(((block_size - 1) as u16).to_be_bytes());
        h.extend(&self.raw_sr_extra);
        h.push(crc8(&h));
        h
    }
}

/// Decodes the UTF-8-like coded number; returns value and length in bytes.
fn decode_coded_number(b: &[u8]) -> Result<(u64, usize), HeaderError> {
    let first = *b.first().ok_or(HeaderError::Truncated)?;
    // Number of leading 1 bits gives the total length (0 means 1 byte).
    let len = match first.leading_ones() {
        0 => return Ok((u64::from(first), 1)),
        1 => return Err(HeaderError::Invalid), // a continuation byte
        n @ 2..=7 => n as usize,               // 0xFE has 7 leading ones: 7 bytes, 36 bits
        _ => return Err(HeaderError::Invalid), // 0xFF
    };
    let mut v = u64::from(first) & (0x7F >> len);
    for i in 1..len {
        let c = *b.get(i).ok_or(HeaderError::Truncated)?;
        if c & 0xC0 != 0x80 {
            return Err(HeaderError::Invalid);
        }
        v = (v << 6) | u64::from(c & 0x3F);
    }
    Ok((v, len))
}

/// Encodes `v` (at most 36 bits) in the UTF-8-like coding.
fn encode_coded_number(v: u64) -> Vec<u8> {
    debug_assert!(v < 1 << 36);
    if v < 0x80 {
        return vec![v as u8];
    }
    // Smallest length whose payload fits: an `l`-byte coding carries
    // 6 bits per continuation byte plus 7 - l bits in the first byte, i.e.
    // 5 * l + 1 bits; the 7-byte form carries 36 bits.
    let len = (2..=7)
        .find(|&l| {
            let bits = if l == 7 { 36 } else { 5 * l + 1 };
            v < 1u64 << bits
        })
        .unwrap_or(7);
    let mut out = vec![0u8; len];
    let mut rest = v;
    for i in (1..len).rev() {
        out[i] = 0x80 | (rest & 0x3F) as u8;
        rest >>= 6;
    }
    // Leading `len` ones, then a zero, then the remaining value bits.
    let prefix = if len == 7 { 0xFE } else { !(0xFFu8 >> len) };
    out[0] = prefix | rest as u8;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn parses_typical_header() {
        // Fixed block size, block size code 0b1100 (4096), 44.1 kHz,
        // mid/side, 16 bit, frame number 0.
        let mut h = vec![0xFF, 0xF8, 0xC9, 0xA8, 0x00];
        h.push(crc8(&h));
        let f = FrameHeader::parse(&h).expect("valid");
        assert!(!f.variable_blocksize);
        assert_eq!(f.block_size, 4096);
        assert_eq!(f.sample_rate, Some(44_100));
        assert_eq!(f.channels, ChannelAssignment::MidSide);
        assert_eq!(f.bits_per_sample, Some(16));
        assert_eq!(f.len, 6);
    }

    #[test]
    fn rejects_bad_crc_and_reserved_values() {
        let mut h = vec![0xFF, 0xF8, 0xC9, 0xA8, 0x00];
        h.push(crc8(&h) ^ 1);
        assert_eq!(FrameHeader::parse(&h), Err(HeaderError::Invalid));
        let mut h = vec![0xFF, 0xF8, 0x09, 0xA8, 0x00]; // block size code 0
        h.push(crc8(&h));
        assert_eq!(FrameHeader::parse(&h), Err(HeaderError::Invalid));
        assert_eq!(
            FrameHeader::parse(&[0xFF, 0xF8]),
            Err(HeaderError::Truncated)
        );
    }

    #[test]
    fn rewritten_header_round_trips() {
        let mut h = vec![0xFF, 0xF8, 0x79, 0x18, 0x05, 0x01, 0x23];
        h.push(crc8(&h));
        let f = FrameHeader::parse(&h).expect("valid");
        assert_eq!(f.block_size, 0x124);
        let g = FrameHeader::parse(&f.to_variable_bytes(123_456_789)).expect("valid");
        assert!(g.variable_blocksize);
        assert_eq!(g.number, 123_456_789);
        assert_eq!(g.block_size, f.block_size);
        assert_eq!(g.channels, f.channels);
    }

    #[test]
    fn new_header_copies_rate_and_size_codes() {
        // Template: 4 channels, sample rate 0b1101 (16-bit Hz follows:
        // 37 800 Hz), 24 bit.
        let mut t = vec![0xFF, 0xF8, 0xCD, 0x3C, 0x00, 0x93, 0xA8];
        t.push(crc8(&t));
        let template = FrameHeader::parse(&t).expect("valid template");
        assert_eq!(template.sample_rate, Some(37_800));
        let f = FrameHeader::parse(&template.new_variable_like(1 << 35, 17)).expect("valid");
        assert!(f.variable_blocksize);
        assert_eq!(f.number, 1 << 35);
        assert_eq!(f.block_size, 17);
        assert_eq!(f.channels, ChannelAssignment::Independent(4));
        assert_eq!(f.sample_rate, Some(37_800));
        assert_eq!(f.bits_per_sample, Some(24));
    }

    proptest! {
        #[test]
        fn coded_number_round_trip(v in 0u64..(1 << 36)) {
            let e = encode_coded_number(v);
            prop_assert_eq!(decode_coded_number(&e), Ok((v, e.len())));
        }
    }
}
