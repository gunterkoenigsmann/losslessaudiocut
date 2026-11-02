//! The two CRCs of FLAC frames (RFC 9639, sections 9.1.8 and 9.3).
//!
//! Both are MSB-first, non-reflected, with initial value 0 and no final XOR.

/// Builds a byte-wise lookup table for an MSB-first CRC of `width` bits.
const fn table(poly: u16, width: u32) -> [u16; 256] {
    let top = 1u32 << (width - 1);
    let mask = ((1u32 << width) - 1) as u16;
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u32) << (width - 8);
        let mut b = 0;
        while b < 8 {
            c = if c & top != 0 {
                (c << 1) ^ poly as u32
            } else {
                c << 1
            };
            b += 1;
        }
        t[i] = (c as u16) & mask;
        i += 1;
    }
    t
}

/// CRC-8, polynomial x^8 + x^2 + x + 1 (0x07); protects the frame header.
const CRC8: [u16; 256] = table(0x07, 8);
/// CRC-16, polynomial x^16 + x^15 + x^2 + 1 (0x8005); protects the frame.
const CRC16: [u16; 256] = table(0x8005, 16);

/// CRC-8 of `data`.
pub fn crc8(data: &[u8]) -> u8 {
    data.iter()
        .fold(0u8, |c, &b| CRC8[usize::from(c ^ b)] as u8)
}

/// Continues a CRC-16 computation with one more byte.
pub fn crc16_update(crc: u16, byte: u8) -> u16 {
    (crc << 8) ^ CRC16[usize::from((crc >> 8) as u8 ^ byte)]
}

/// CRC-16 of `data`.
pub fn crc16(data: &[u8]) -> u16 {
    data.iter().fold(0, |c, &b| crc16_update(c, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Check values from the CRC catalogue ("123456789").
    #[test]
    fn catalogue_check_values() {
        assert_eq!(crc8(b"123456789"), 0xF4); // CRC-8/SMBUS
        assert_eq!(crc16(b"123456789"), 0xFEE8); // CRC-16/UMTS (BUYPASS)
    }
}
