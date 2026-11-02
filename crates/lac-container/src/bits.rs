//! Big-endian bit-level reading and writing, as used by FLAC (and later MP3).
//!
//! Both work MSB-first: the first bit of a byte is its most significant bit.

/// Raised when a read runs past the end of the data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unexpected end of bitstream")]
pub struct EndOfData;

/// Reads bits MSB-first from a byte slice.
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Position in bits from the start of `data`.
    pos: usize,
}

impl<'a> BitReader<'a> {
    /// Starts reading at the first bit of `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Reads `n` bits (0..=64) as an unsigned value.
    pub fn read_u64(&mut self, n: u32) -> Result<u64, EndOfData> {
        debug_assert!(n <= 64);
        if self.pos + n as usize > self.data.len() * 8 {
            return Err(EndOfData);
        }
        let mut v: u64 = 0;
        let mut left = n;
        while left > 0 {
            let byte = self.data[self.pos / 8];
            let bit_in_byte = (self.pos % 8) as u32;
            let avail = 8 - bit_in_byte;
            let take = avail.min(left);
            // Bits `bit_in_byte .. bit_in_byte + take` of `byte`, MSB first.
            let chunk = (u32::from(byte) >> (avail - take)) & ((1 << take) - 1);
            v = (v << take) | u64::from(chunk);
            left -= take;
            self.pos += take as usize;
        }
        Ok(v)
    }

    /// Reads `n` bits (0..=32) as an unsigned value.
    pub fn read_u32(&mut self, n: u32) -> Result<u32, EndOfData> {
        debug_assert!(n <= 32);
        // Cannot truncate: at most 32 bits were read.
        self.read_u64(n).map(|v| v as u32)
    }

    /// Reads `n` bits (0..=64) as a two's complement signed value.
    pub fn read_i64(&mut self, n: u32) -> Result<i64, EndOfData> {
        if n == 0 {
            return Ok(0);
        }
        let v = self.read_u64(n)?;
        // Sign-extend by shifting the value's top bit into bit 63.
        let shift = 64 - n;
        Ok(((v << shift) as i64) >> shift)
    }

    /// Reads a unary number: the count of 0 bits before the next 1 bit.
    pub fn read_unary(&mut self) -> Result<u32, EndOfData> {
        let mut n = 0;
        loop {
            let byte_idx = self.pos / 8;
            let Some(&byte) = self.data.get(byte_idx) else {
                return Err(EndOfData);
            };
            let bit_in_byte = self.pos % 8;
            // Fast path: the rest of this byte is zero.
            let rest = byte << bit_in_byte;
            if rest == 0 {
                n += 8 - bit_in_byte as u32;
                self.pos += 8 - bit_in_byte;
                continue;
            }
            let zeros = rest.leading_zeros();
            n += zeros;
            self.pos += zeros as usize + 1;
            return Ok(n);
        }
    }
}

/// Writes bits MSB-first into a growing byte vector.
#[derive(Debug, Default, Clone)]
pub struct BitWriter {
    bytes: Vec<u8>,
    /// Number of bits already used in the last byte (0 = byte-aligned).
    used: u32,
}

impl BitWriter {
    /// An empty writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends the low `n` bits (0..=64) of `v`, most significant first.
    pub fn write(&mut self, v: u64, n: u32) {
        debug_assert!(n <= 64);
        for i in (0..n).rev() {
            let bit = ((v >> i) & 1) as u8;
            if self.used == 0 {
                self.bytes.push(0);
            }
            if let Some(last) = self.bytes.last_mut() {
                *last |= bit << (7 - self.used);
            }
            self.used = (self.used + 1) % 8;
        }
    }

    /// Appends `n` bits of a signed value in two's complement.
    pub fn write_signed(&mut self, v: i64, n: u32) {
        self.write(v as u64, n);
    }

    /// Pads with zero bits to the next byte boundary.
    pub fn align(&mut self) {
        self.used = 0;
    }

    /// The written bytes; a partial last byte is zero-padded.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn reads_across_byte_boundaries() {
        let mut r = BitReader::new(&[0b1010_1100, 0b0101_0011]);
        assert_eq!(r.read_u32(3), Ok(0b101));
        assert_eq!(r.read_u32(7), Ok(0b011_0001));
        assert_eq!(r.read_i64(6), Ok(0b01_0011));
        assert_eq!(r.read_u32(1), Err(EndOfData));
    }

    #[test]
    fn sign_extension() {
        let mut r = BitReader::new(&[0b1110_0000]);
        assert_eq!(r.read_i64(3), Ok(-1));
    }

    #[test]
    fn unary_spans_bytes() {
        let mut r = BitReader::new(&[0, 0, 0b0010_0000, 0xFF]);
        assert_eq!(r.read_unary(), Ok(18));
        // Five zero bits remain in byte 2, the next 1 is the MSB of byte 3.
        assert_eq!(r.read_unary(), Ok(5));
        assert_eq!(r.read_u32(7), Ok(0x7F));
        assert_eq!(r.read_unary(), Err(EndOfData));
    }

    proptest! {
        /// Whatever is written can be read back.
        #[test]
        fn round_trip(fields in proptest::collection::vec((any::<i64>(), 1u32..=64), 0..50)) {
            let mut w = BitWriter::new();
            for &(v, n) in &fields {
                w.write_signed(v, n);
            }
            let bytes = w.into_bytes();
            let mut r = BitReader::new(&bytes);
            for &(v, n) in &fields {
                let shift = 64 - n;
                let expected = (v << shift) >> shift;
                prop_assert_eq!(r.read_i64(n), Ok(expected));
            }
        }
    }
}
