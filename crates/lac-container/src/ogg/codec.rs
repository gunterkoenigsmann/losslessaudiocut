//! Codec-specific knowledge needed to cut Ogg streams without decoding:
//! how to recognise the codec, how many header packets it has, and how
//! many samples each audio packet decodes to.
//!
//! Packet durations are computed from the packets themselves, never from
//! granule positions, because many recordings (radio captures in
//! particular) carry random or inconsistent granule positions.

use crate::bits::{EndOfData, LsbBitReader};

/// The codecs that can be cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecKind {
    /// Vorbis I.
    Vorbis,
    /// Opus (RFC 7845 encapsulation).
    Opus,
    /// Speex.
    Speex,
}

impl CodecKind {
    /// Recognises a codec from the first packet of a logical bitstream.
    pub fn detect(first_packet: &[u8]) -> Option<Self> {
        if first_packet.starts_with(b"\x01vorbis") {
            Some(Self::Vorbis)
        } else if first_packet.starts_with(b"OpusHead") {
            Some(Self::Opus)
        } else if first_packet.starts_with(b"Speex   ") {
            Some(Self::Speex)
        } else {
            None
        }
    }

    /// Display name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Vorbis => "Vorbis",
            Self::Opus => "Opus",
            Self::Speex => "Speex",
        }
    }
}

/// Errors in codec headers or packets.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CodecError(pub String);

impl From<EndOfData> for CodecError {
    fn from(_: EndOfData) -> Self {
        CodecError("header ends early".into())
    }
}

fn err<T>(msg: &str) -> Result<T, CodecError> {
    Err(CodecError(msg.into()))
}

/// Everything learned from a stream's header packets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecInfo {
    /// The codec.
    pub kind: CodecKind,
    /// Rate of the timeline in Hz (always 48 000 for Opus).
    pub sample_rate: u32,
    /// Number of channels.
    pub channels: u16,
    /// Number of header packets before the first audio packet.
    pub header_packets: usize,
    /// Opus: samples to discard at the start of the stream.
    pub pre_skip: u32,
    /// Vorbis: the two block sizes.
    vorbis_blocksizes: [u32; 2],
    /// Vorbis: block flag of every mode (filled from the setup header).
    vorbis_mode_flags: Vec<bool>,
    /// Speex: samples per packet.
    speex_packet_samples: u32,
}

impl CodecInfo {
    /// Parses the identification header (the first packet).
    pub fn from_id_header(kind: CodecKind, p: &[u8]) -> Result<Self, CodecError> {
        let le16 = |i: usize| p.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
        let le32 = |i: usize| {
            p.get(i..i + 4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        let mut info = Self {
            kind,
            sample_rate: 0,
            channels: 0,
            header_packets: 0,
            pre_skip: 0,
            vorbis_blocksizes: [0; 2],
            vorbis_mode_flags: Vec::new(),
            speex_packet_samples: 0,
        };
        match kind {
            CodecKind::Vorbis => {
                // Vorbis I 4.2.2: version, channels, rate, 3 bitrates,
                // blocksizes (two 4-bit exponents), framing bit.
                if p.len() < 30 || le32(7) != Some(0) {
                    return err("unsupported Vorbis identification header");
                }
                info.channels = u16::from(p[11]);
                info.sample_rate = le32(12).unwrap_or(0);
                let (b0, b1) = (u32::from(p[28] & 0x0F), u32::from(p[28] >> 4));
                if !(6..=13).contains(&b0) || !(6..=13).contains(&b1) || b0 > b1 {
                    return err("invalid Vorbis block sizes");
                }
                info.vorbis_blocksizes = [1 << b0, 1 << b1];
                info.header_packets = 3;
            }
            CodecKind::Opus => {
                // RFC 7845 5.1: version, channels, pre-skip, input rate, ...
                if p.len() < 19 || p[8] & 0xF0 != 0 {
                    return err("unsupported Opus header version");
                }
                info.channels = u16::from(p[9]);
                info.pre_skip = u32::from(le16(10).unwrap_or(0));
                info.sample_rate = 48_000;
                info.header_packets = 2;
            }
            CodecKind::Speex => {
                // speex_header.h: rate at 36, channels at 48, frame size at
                // 56, frames per packet at 64, extra headers at 68.
                if p.len() < 72 {
                    return err("Speex header too short");
                }
                info.sample_rate = le32(36).unwrap_or(0);
                info.channels = le32(48).unwrap_or(0) as u16;
                let frame_size = le32(56).unwrap_or(0);
                let frames = le32(64).unwrap_or(1).max(1);
                info.speex_packet_samples = frame_size.saturating_mul(frames);
                let extra = le32(68).unwrap_or(0) as usize;
                if extra > 16 {
                    return err("implausible number of extra Speex headers");
                }
                info.header_packets = 2 + extra;
                if info.speex_packet_samples == 0 || info.speex_packet_samples > 1 << 16 {
                    return err("invalid Speex frame size");
                }
            }
        }
        if info.sample_rate == 0 || info.channels == 0 {
            return err("sample rate or channel count is zero");
        }
        Ok(info)
    }

    /// Handles a further header packet (`index` counts from 0 = id header).
    pub fn add_header(&mut self, index: usize, p: &[u8]) -> Result<(), CodecError> {
        match (self.kind, index) {
            (CodecKind::Vorbis, 1) if !p.starts_with(b"\x03vorbis") => {
                err("missing Vorbis comment header")
            }
            (CodecKind::Vorbis, 2) => {
                if !p.starts_with(b"\x05vorbis") {
                    return err("missing Vorbis setup header");
                }
                self.vorbis_mode_flags = vorbis_mode_flags(&p[7..], self.channels)?;
                Ok(())
            }
            (CodecKind::Opus, 1) if !p.starts_with(b"OpusTags") => err("missing OpusTags header"),
            _ => Ok(()),
        }
    }

    /// Block size of a Vorbis audio packet, from its mode number.
    fn vorbis_blocksize(&self, p: &[u8]) -> Result<u32, CodecError> {
        let mut r = LsbBitReader::new(p);
        if r.flag()? {
            return err("Vorbis audio packet with header flag set");
        }
        let bits = ilog(self.vorbis_mode_flags.len() as u32 - 1);
        let mode = r.read(bits)? as usize;
        let long = *self
            .vorbis_mode_flags
            .get(mode)
            .ok_or_else(|| CodecError("invalid Vorbis mode number".into()))?;
        Ok(self.vorbis_blocksizes[usize::from(long)])
    }

    /// Number of samples (per channel, at [`CodecInfo::sample_rate`]) that
    /// audio packet `p` adds to the decoded output.
    ///
    /// `prev` carries per-stream state between calls (the previous Vorbis
    /// block size); start each logical stream with `None`.
    pub fn packet_samples(&self, p: &[u8], prev: &mut Option<u32>) -> Result<u32, CodecError> {
        match self.kind {
            CodecKind::Vorbis => {
                if p.is_empty() {
                    // Zero-length packets are legal and decode to nothing.
                    return Ok(0);
                }
                let bs = self.vorbis_blocksize(p)?;
                // Vorbis I 1.3.2: the first packet only primes the overlap;
                // afterwards each packet completes prev/4 + cur/4 samples.
                let n = prev.map_or(0, |pb| pb / 4 + bs / 4);
                *prev = Some(bs);
                Ok(n)
            }
            CodecKind::Opus => opus_packet_samples(p),
            CodecKind::Speex => Ok(self.speex_packet_samples),
        }
    }
}

/// Duration of an Opus packet in 48 kHz samples (RFC 6716 3.1). Needs
/// only the first two bytes (TOC and, for code 3, the frame count).
pub fn opus_packet_samples(p: &[u8]) -> Result<u32, CodecError> {
    let Some(&toc) = p.first() else {
        return err("empty Opus packet");
    };
    let config = u32::from(toc >> 3);
    // Frame duration in units of 2.5 ms (= 120 samples at 48 kHz).
    let units = match config {
        0..=11 => [4, 8, 16, 24][(config % 4) as usize], // SILK: 10/20/40/60 ms
        12..=15 => [4, 8][(config % 2) as usize],        // hybrid: 10/20 ms
        _ => [1, 2, 4, 8][(config % 4) as usize],        // CELT: 2.5/5/10/20 ms
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u32::from(
            *p.get(1)
                .ok_or_else(|| CodecError("Opus code 3 packet without frame count".into()))?
                & 0x3F,
        ),
    };
    let n = units * 120 * frames;
    // RFC 6716 3.2.5: at most 120 ms per packet.
    if n == 0 || n > 5760 {
        return err("invalid Opus frame count");
    }
    Ok(n)
}

/// Number of bits needed to represent `v` (Vorbis I 9.2.1); `ilog(0) = 0`.
fn ilog(v: u32) -> u32 {
    32 - v.leading_zeros()
}

/// Walks the Vorbis setup header (after its 7-byte packet type and magic)
/// far enough to read the block flag of every mode (Vorbis I 4.2.4).
///
/// Codebooks, floors, residues and mappings must be parsed only to skip
/// over them; their contents are not kept.
fn vorbis_mode_flags(p: &[u8], channels: u16) -> Result<Vec<bool>, CodecError> {
    let mut r = LsbBitReader::new(p);

    // Codebooks (3.2.1).
    let books = r.read(8)? + 1;
    for _ in 0..books {
        if r.read(24)? != 0x56_4342 {
            return err("bad Vorbis codebook sync pattern");
        }
        let dims = r.read(16)?;
        let entries = r.read(24)?;
        if r.flag()? {
            // Ordered: runs of entries with increasing codeword lengths.
            let mut current = 0;
            let mut _len = r.read(5)? + 1;
            while current < entries {
                let n = r.read(ilog(entries - current))?;
                current += n;
                _len += 1;
                if current > entries || _len > 32 {
                    return err("bad ordered Vorbis codebook");
                }
            }
        } else {
            let sparse = r.flag()?;
            for _ in 0..entries {
                if !sparse || r.flag()? {
                    r.read(5)?;
                }
            }
        }
        match r.read(4)? {
            0 => {}
            t @ (1 | 2) => {
                r.read(32)?; // minimum value
                r.read(32)?; // delta value
                let value_bits = r.read(4)? + 1;
                r.read(1)?; // sequence_p
                let values = if t == 1 {
                    lookup1_values(entries, dims)
                } else {
                    u64::from(entries) * u64::from(dims)
                };
                for _ in 0..values {
                    r.read(value_bits)?;
                }
            }
            _ => return err("reserved Vorbis codebook lookup type"),
        }
    }

    // Time domain transforms: placeholders, all zero (4.2.4 step 2).
    let times = r.read(6)? + 1;
    for _ in 0..times {
        if r.read(16)? != 0 {
            return err("nonzero Vorbis time domain transform");
        }
    }

    // Floors (6.2.1 and 7.2.2).
    let floors = r.read(6)? + 1;
    for _ in 0..floors {
        match r.read(16)? {
            0 => {
                r.read(8)?; // order
                r.read(16)?; // rate
                r.read(16)?; // bark map size
                r.read(6)?; // amplitude bits
                r.read(8)?; // amplitude offset
                let n = r.read(4)? + 1;
                for _ in 0..n {
                    r.read(8)?;
                }
            }
            1 => {
                let partitions = r.read(5)?;
                let classes: Vec<u32> = (0..partitions)
                    .map(|_| r.read(4))
                    .collect::<Result<_, _>>()?;
                let max_class = classes.iter().copied().max().map_or(0, |m| m + 1);
                let mut dims = Vec::with_capacity(max_class as usize);
                for _ in 0..max_class {
                    dims.push(r.read(3)? + 1);
                    let sub = r.read(2)?;
                    if sub > 0 {
                        r.read(8)?; // master book
                    }
                    for _ in 0..(1u32 << sub) {
                        r.read(8)?; // subclass book + 1
                    }
                }
                r.read(2)?; // multiplier - 1
                let range_bits = r.read(4)?;
                for c in &classes {
                    for _ in 0..dims[*c as usize] {
                        r.read(range_bits)?;
                    }
                }
            }
            _ => return err("reserved Vorbis floor type"),
        }
    }

    // Residues (8.6.1).
    let residues = r.read(6)? + 1;
    for _ in 0..residues {
        if r.read(16)? > 2 {
            return err("reserved Vorbis residue type");
        }
        r.read(24)?; // begin
        r.read(24)?; // end
        r.read(24)?; // partition size - 1
        let classifications = r.read(6)? + 1;
        r.read(8)?; // classbook
        let mut cascades = Vec::with_capacity(classifications as usize);
        for _ in 0..classifications {
            let low = r.read(3)?;
            let high = if r.flag()? { r.read(5)? } else { 0 };
            cascades.push(high * 8 + low);
        }
        for c in cascades {
            for bit in 0..8 {
                if c & (1 << bit) != 0 {
                    r.read(8)?;
                }
            }
        }
    }

    // Mappings (4.2.4 step 5).
    let mappings = r.read(6)? + 1;
    let ch_bits = ilog(u32::from(channels).saturating_sub(1));
    for _ in 0..mappings {
        if r.read(16)? != 0 {
            return err("reserved Vorbis mapping type");
        }
        let submaps = if r.flag()? { r.read(4)? + 1 } else { 1 };
        if r.flag()? {
            let steps = r.read(8)? + 1;
            for _ in 0..steps {
                r.read(ch_bits)?; // magnitude channel
                r.read(ch_bits)?; // angle channel
            }
        }
        if r.read(2)? != 0 {
            return err("reserved bits in Vorbis mapping");
        }
        if submaps > 1 {
            for _ in 0..channels {
                r.read(4)?;
            }
        }
        for _ in 0..submaps {
            r.read(8)?; // unused time configuration
            r.read(8)?; // floor
            r.read(8)?; // residue
        }
    }

    // Modes (4.2.4 step 6): the part we actually need.
    let modes = r.read(6)? + 1;
    let mut flags = Vec::with_capacity(modes as usize);
    for _ in 0..modes {
        flags.push(r.flag()?);
        r.read(16)?; // window type
        r.read(16)?; // transform type
        if r.read(8)? >= mappings {
            return err("Vorbis mode refers to a missing mapping");
        }
    }
    if !r.flag()? {
        return err("Vorbis setup header framing bit not set");
    }
    Ok(flags)
}

/// The largest `r` with `r^dims <= entries` (Vorbis I 9.2.3).
fn lookup1_values(entries: u32, dims: u32) -> u64 {
    if dims == 0 {
        return 0;
    }
    let mut r = (f64::from(entries).powf(1.0 / f64::from(dims))) as u64;
    // Correct floating-point rounding in both directions.
    while (r + 1)
        .checked_pow(dims)
        .is_some_and(|v| v <= u64::from(entries))
    {
        r += 1;
    }
    while r > 0 && r.checked_pow(dims).is_none_or(|v| v > u64::from(entries)) {
        r -= 1;
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_durations() {
        // config 31 (CELT 20 ms), code 0: one 960-sample frame.
        assert_eq!(opus_packet_samples(&[31 << 3]), Ok(960));
        // config 1 (SILK 20 ms), code 1: two frames.
        assert_eq!(opus_packet_samples(&[(1 << 3) | 1]), Ok(1920));
        // config 16 (CELT 2.5 ms), code 3 with 6 frames.
        assert_eq!(opus_packet_samples(&[(16 << 3) | 3, 6]), Ok(720));
        assert!(opus_packet_samples(&[]).is_err());
        // 3 frames of 60 ms exceed the 120 ms limit.
        assert!(opus_packet_samples(&[(3 << 3) | 3, 3]).is_err());
    }

    #[test]
    fn lookup1() {
        assert_eq!(lookup1_values(81, 4), 3);
        assert_eq!(lookup1_values(80, 4), 2);
        assert_eq!(lookup1_values(1, 1), 1);
        assert_eq!(lookup1_values(4096, 2), 64);
    }

    #[test]
    fn ilog_values() {
        assert_eq!([0, 1, 2, 3, 4, 7, 8].map(ilog), [0, 1, 2, 2, 3, 3, 4]);
    }
}
