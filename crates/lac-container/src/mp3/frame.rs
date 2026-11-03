//! MPEG audio Layer III frame headers and side information
//! (ISO/IEC 11172-3 2.4.1, ISO/IEC 13818-3 2.4.1; MPEG 2.5 is the
//! unofficial Fraunhofer extension to 8, 11.025 and 12 kHz).
//!
//! ```text
//! header (32 bits, MSB first)
//!   11  sync (all ones)
//!    2  version: 00 MPEG 2.5, 01 reserved, 10 MPEG-2, 11 MPEG-1
//!    2  layer: 01 Layer III (10 = II, 11 = I)
//!    1  protection_bit: 0 = a CRC-16 follows the header
//!    4  bitrate_index (0 = free format, 15 = forbidden)
//!    2  sampling_frequency (11 reserved)
//!    1  padding_bit: frame is one byte longer
//!    1  private_bit
//!    2  mode: 00 stereo, 01 joint stereo, 10 dual channel, 11 mono
//!    2  mode_extension
//!    1  copyright, 1 original, 2 emphasis
//! [CRC-16]
//! side information (17/32 bytes MPEG-1, 9/17 bytes MPEG-2/2.5)
//! main data slot (rest of the frame)
//! ```

/// MPEG version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    /// MPEG-1 (32, 44.1, 48 kHz).
    Mpeg1,
    /// MPEG-2 low sampling frequencies (16, 22.05, 24 kHz).
    Mpeg2,
    /// MPEG 2.5 (8, 11.025, 12 kHz).
    Mpeg25,
}

/// Why four bytes are not a usable Layer III header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// No sync word or a reserved/forbidden value.
    Invalid,
    /// A valid MPEG audio header of Layer I or II.
    OtherLayer,
    /// Free-format bitrate (frame length is not in the header).
    FreeFormat,
}

/// Layer III bitrates in kbit/s, by version group and index.
const BITRATES: [[u32; 15]; 2] = [
    [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ],
    [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
];

/// Highest bitrate index.
pub const MAX_BITRATE_INDEX: u8 = 14;

/// A parsed Layer III frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// The four header bytes as stored.
    pub raw: [u8; 4],
    /// MPEG version.
    pub version: Version,
    /// A CRC-16 follows the header.
    pub protected: bool,
    /// Bitrate index (1..=14).
    pub bitrate_index: u8,
    /// Sampling frequency index (0..=2).
    pub sr_index: u8,
    /// One padding byte.
    pub padding: bool,
    /// Channel mode 3 is mono.
    pub mode: u8,
}

impl FrameHeader {
    /// Parses the four bytes at the start of `b`.
    pub fn parse(b: &[u8]) -> Result<Self, HeaderError> {
        let raw: [u8; 4] = b
            .get(..4)
            .and_then(|s| s.try_into().ok())
            .ok_or(HeaderError::Invalid)?;
        if raw[0] != 0xFF || raw[1] & 0xE0 != 0xE0 {
            return Err(HeaderError::Invalid);
        }
        let version = match (raw[1] >> 3) & 3 {
            0 => Version::Mpeg25,
            2 => Version::Mpeg2,
            3 => Version::Mpeg1,
            _ => return Err(HeaderError::Invalid),
        };
        let layer = (raw[1] >> 1) & 3;
        let bitrate_index = raw[2] >> 4;
        let sr_index = (raw[2] >> 2) & 3;
        if layer == 0 || bitrate_index == 15 || sr_index == 3 || raw[3] & 3 == 2 {
            return Err(HeaderError::Invalid);
        }
        if layer != 1 {
            return Err(HeaderError::OtherLayer);
        }
        if bitrate_index == 0 {
            return Err(HeaderError::FreeFormat);
        }
        Ok(Self {
            raw,
            version,
            protected: raw[1] & 1 == 0,
            bitrate_index,
            sr_index,
            padding: raw[2] & 2 != 0,
            mode: raw[3] >> 6,
        })
    }

    /// Sampling rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        let base = [44_100, 48_000, 32_000][usize::from(self.sr_index)];
        match self.version {
            Version::Mpeg1 => base,
            Version::Mpeg2 => base / 2,
            Version::Mpeg25 => base / 4,
        }
    }

    /// Number of channels (1 or 2).
    pub fn channels(&self) -> u16 {
        if self.mode == 3 { 1 } else { 2 }
    }

    /// Samples per channel in one frame.
    pub fn samples(&self) -> u32 {
        match self.version {
            Version::Mpeg1 => 1152,
            Version::Mpeg2 | Version::Mpeg25 => 576,
        }
    }

    /// Granules per frame (each 576 samples).
    pub fn granules(&self) -> u32 {
        self.samples() / 576
    }

    /// Bitrate in kbit/s.
    pub fn bitrate(&self) -> u32 {
        let group = usize::from(self.version != Version::Mpeg1);
        BITRATES[group][usize::from(self.bitrate_index)]
    }

    /// Frame length in bytes (ISO/IEC 11172-3 2.4.3.1: 144 * bitrate /
    /// sampling_frequency for Layer III, half that for one granule).
    pub fn frame_len(&self) -> usize {
        let coeff = 72 * self.granules();
        (coeff * self.bitrate() * 1000 / self.sample_rate()) as usize + usize::from(self.padding)
    }

    /// Length of the side information.
    pub fn side_info_len(&self) -> usize {
        match (self.version, self.channels()) {
            (Version::Mpeg1, 1) => 17,
            (Version::Mpeg1, _) => 32,
            (_, 1) => 9,
            _ => 17,
        }
    }

    /// Offset of the side information within the frame.
    pub fn side_info_offset(&self) -> usize {
        4 + if self.protected { 2 } else { 0 }
    }

    /// Offset of the main data slot within the frame.
    pub fn slot_offset(&self) -> usize {
        self.side_info_offset() + self.side_info_len()
    }

    /// Length of the main data slot.
    pub fn slot_len(&self) -> usize {
        self.frame_len().saturating_sub(self.slot_offset())
    }

    /// Largest possible `main_data_begin` (9 bits MPEG-1, 8 bits else).
    pub fn max_main_data_begin(&self) -> usize {
        if self.version == Version::Mpeg1 {
            511
        } else {
            255
        }
    }

    /// The same header with another bitrate and padding.
    pub fn with_bitrate(&self, bitrate_index: u8, padding: bool) -> Self {
        let mut raw = self.raw;
        raw[2] = (raw[2] & 0x0D) | (bitrate_index << 4) | (u8::from(padding) << 1);
        Self {
            raw,
            bitrate_index,
            padding,
            ..*self
        }
    }

    /// The same header without CRC protection.
    pub fn unprotected(&self) -> Self {
        let mut raw = self.raw;
        raw[1] |= 1;
        Self {
            raw,
            protected: false,
            ..*self
        }
    }

    /// Whether `other` belongs to the same stream (version, layer, rate
    /// and channel count must not change between frames).
    pub fn compatible(&self, other: &Self) -> bool {
        self.version == other.version
            && self.sr_index == other.sr_index
            && self.channels() == other.channels()
    }
}

/// The two side-information fields the cutter needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SideInfo {
    /// Bytes of main data that lie before this frame's slot, in the slots
    /// of preceding frames (the bit reservoir).
    pub main_data_begin: usize,
    /// Length of this frame's main data in bytes (sum of `part2_3_length`
    /// over all granules and channels, rounded up to whole bytes).
    pub main_data_len: usize,
}

/// Reads [`SideInfo`] from the side information `si` of a frame with
/// header `h`.
pub fn parse_side_info(h: &FrameHeader, si: &[u8]) -> Option<SideInfo> {
    let si = si.get(..h.side_info_len())?;
    let nch = usize::from(h.channels());
    // Bit positions (MSB first) of the fields, see ISO/IEC 11172-3 2.4.1.7
    // and ISO/IEC 13818-3 2.4.1.7.
    let (mdb_bits, first_gr, stride) = match h.version {
        // main_data_begin 9, private_bits 5 (mono) / 3, scfsi 4 per channel;
        // per granule and channel 59 bits starting with part2_3_length.
        Version::Mpeg1 => (9, 9 + if nch == 1 { 5 } else { 3 } + 4 * nch, 59),
        // main_data_begin 8, private_bits 1 / 2; one granule of 63 bits per
        // channel (scalefac_compress is 9 bits, no preflag).
        _ => (8, 8 + nch, 63),
    };
    let bits = |pos: usize, n: usize| -> usize {
        (pos..pos + n).fold(0, |v, p| {
            (v << 1) | usize::from((si[p / 8] >> (7 - p % 8)) & 1)
        })
    };
    let main_data_begin = bits(0, mdb_bits);
    let mut total = 0;
    for i in 0..h.granules() as usize * nch {
        total += bits(first_gr + i * stride, 12);
    }
    Some(SideInfo {
        main_data_begin,
        main_data_len: total.div_ceil(8),
    })
}

/// Overwrites `main_data_begin` in side information `si`.
pub fn set_main_data_begin(h: &FrameHeader, si: &mut [u8], mdb: usize) {
    match h.version {
        Version::Mpeg1 => {
            si[0] = (mdb >> 1) as u8;
            si[1] = (si[1] & 0x7F) | (((mdb & 1) as u8) << 7);
        }
        _ => si[0] = mdb as u8,
    }
}

/// CRC-16 of a protected frame (ISO/IEC 11172-3 2.4.3.1: polynomial
/// 0x8005, initial value 0xFFFF, over the last two header bytes and the
/// side information).
pub fn frame_crc(header: &[u8; 4], side_info: &[u8]) -> u16 {
    header[2..]
        .iter()
        .chain(side_info)
        .fold(0xFFFF_u16, |mut crc, &b| {
            crc ^= u16::from(b) << 8;
            for _ in 0..8 {
                crc = if crc & 0x8000 != 0 {
                    (crc << 1) ^ 0x8005
                } else {
                    crc << 1
                };
            }
            crc
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpeg1_header_fields() {
        // 128 kbit/s, 44.1 kHz, joint stereo, no CRC, padding.
        let h = FrameHeader::parse(&[0xFF, 0xFB, 0x92, 0x64]).expect("valid");
        assert_eq!(h.version, Version::Mpeg1);
        assert_eq!(
            (h.bitrate(), h.sample_rate(), h.channels()),
            (128, 44_100, 2)
        );
        assert!(h.padding && !h.protected);
        assert_eq!(h.frame_len(), 418);
        assert_eq!(h.slot_offset(), 36);
        let h2 = h.with_bitrate(14, false);
        assert_eq!(h2.frame_len(), 1044);
        assert_eq!(FrameHeader::parse(&h2.raw), Ok(h2));
    }

    #[test]
    fn mpeg2_mono_header() {
        // MPEG-2, 64 kbit/s, 22.05 kHz, mono, CRC.
        let h = FrameHeader::parse(&[0xFF, 0xF2, 0x80, 0xC0]).expect("valid");
        assert_eq!(h.version, Version::Mpeg2);
        assert_eq!(
            (h.bitrate(), h.sample_rate(), h.channels()),
            (64, 22_050, 1)
        );
        assert_eq!(h.frame_len(), 208);
        assert_eq!(h.slot_offset(), 4 + 2 + 9);
    }

    #[test]
    fn rejects_other_layers_and_free_format() {
        assert_eq!(
            FrameHeader::parse(&[0xFF, 0xFD, 0x90, 0x00]),
            Err(HeaderError::OtherLayer)
        );
        assert_eq!(
            FrameHeader::parse(&[0xFF, 0xFB, 0x00, 0x00]),
            Err(HeaderError::FreeFormat)
        );
        assert_eq!(
            FrameHeader::parse(&[0xFF, 0xFB, 0xF0, 0x00]),
            Err(HeaderError::Invalid)
        );
        assert_eq!(
            FrameHeader::parse(&[0xFF, 0xEB, 0x90, 0x00]),
            Err(HeaderError::Invalid)
        );
    }

    #[test]
    fn side_info_round_trip() {
        let h = FrameHeader::parse(&[0xFF, 0xFB, 0x90, 0x64]).expect("valid");
        let mut si = vec![0u8; 32];
        set_main_data_begin(&h, &mut si, 300);
        // part2_3_length of granule 0 channel 0 starts at bit 9+3+8 = 20.
        si[2] = 0x0F; // bits 20..23 = 1111 -> part2_3_length 0b1111_0000_0000
        let s = parse_side_info(&h, &si).expect("long enough");
        assert_eq!(s.main_data_begin, 300);
        assert_eq!(s.main_data_len, 0xF00 / 8);
    }

    #[test]
    fn crc_check_value() {
        // CRC-16/CMS-like parameters over "123456789" split into a fake
        // header tail and side info: polynomial 0x8005, init 0xFFFF.
        let crc = frame_crc(&[0, 0, b'1', b'2'], b"3456789");
        assert_eq!(crc, 0xAEE7);
    }
}
