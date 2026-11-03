//! EBML element headers and values (RFC 8794), reading and writing.
//!
//! An EBML element is an ID, a size and a body. IDs and sizes are
//! variable-length integers ("VINTs", RFC 8794 4): the number of leading
//! zero bits of the first byte plus one is the total length in bytes,
//! followed by a 1 bit (the marker). IDs keep the marker as part of their
//! value; sizes drop it. A size whose value bits are all 1 means "unknown"
//! (RFC 8794 6.2), used by live recordings that cannot seek back.

/// Error while parsing an element header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// The buffer ends inside the header.
    Truncated,
    /// The bytes cannot be an element header (a VINT longer than allowed).
    Invalid,
}

/// The header of one element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Element ID including its length marker, e.g. `0x1A45DFA3`.
    pub id: u32,
    /// Body size in bytes; `None` for an unknown size.
    pub size: Option<u64>,
    /// Length of ID plus size field in bytes.
    pub len: usize,
}

/// Length in bytes of the VINT starting with `first` (1 to 8), or `None`
/// for a zero byte, which would need more than 8 bytes.
fn vint_len(first: u8) -> Option<usize> {
    (first != 0).then(|| first.leading_zeros() as usize + 1)
}

/// Reads an element ID (RFC 8794 5: at most 4 bytes, marker kept).
pub fn read_id(b: &[u8]) -> Result<(u32, usize), HeaderError> {
    let first = *b.first().ok_or(HeaderError::Truncated)?;
    let n = vint_len(first)
        .filter(|&n| n <= 4)
        .ok_or(HeaderError::Invalid)?;
    let bytes = b.get(..n).ok_or(HeaderError::Truncated)?;
    Ok((bytes.iter().fold(0u32, |v, &x| v << 8 | u32::from(x)), n))
}

/// Reads an element size (RFC 8794 6: at most 8 bytes, marker removed);
/// `None` for the reserved "unknown" value.
pub fn read_size(b: &[u8]) -> Result<(Option<u64>, usize), HeaderError> {
    let first = *b.first().ok_or(HeaderError::Truncated)?;
    let n = vint_len(first).ok_or(HeaderError::Invalid)?;
    let bytes = b.get(..n).ok_or(HeaderError::Truncated)?;
    // Drop the marker bit from the first byte.
    let mask = if n == 8 { 0 } else { 0xFFu8 >> n };
    let v = bytes[1..]
        .iter()
        .fold(u64::from(first & mask), |v, &x| v << 8 | u64::from(x));
    let all_ones = (1u64 << (7 * n)) - 1;
    Ok(((v != all_ones).then_some(v), n))
}

/// Parses the element header at the start of `b`.
pub fn parse_header(b: &[u8]) -> Result<Header, HeaderError> {
    let (id, n) = read_id(b)?;
    let (size, m) = read_size(&b[n..])?;
    Ok(Header {
        id,
        size,
        len: n + m,
    })
}

/// An unsigned integer element body (RFC 8794 7.2: 0 to 8 bytes, big
/// endian; empty means 0).
pub fn read_uint(body: &[u8]) -> Option<u64> {
    (body.len() <= 8).then(|| body.iter().fold(0u64, |v, &x| v << 8 | u64::from(x)))
}

/// A signed integer element body (RFC 8794 7.1: two's complement).
pub fn read_int(body: &[u8]) -> Option<i64> {
    let u = read_uint(body)?;
    if body.is_empty() {
        return Some(0);
    }
    let shift = 64 - 8 * body.len() as u32;
    // Sign-extend from the body's width.
    Some(((u << shift) as i64) >> shift)
}

/// A float element body (RFC 8794 7.3: empty, 4 or 8 bytes).
pub fn read_float(body: &[u8]) -> Option<f64> {
    match body.len() {
        0 => Some(0.0),
        4 => Some(f64::from(f32::from_be_bytes(body.try_into().ok()?))),
        8 => Some(f64::from_be_bytes(body.try_into().ok()?)),
        _ => None,
    }
}

/// Reads a signed VINT as used by EBML lacing (RFC 9559 10.3.3): the
/// unsigned value minus half its range.
pub fn read_signed_vint(b: &[u8]) -> Result<(i64, usize), HeaderError> {
    let (v, n) = read_size(b)?;
    // The "unknown" bit pattern is a valid number here.
    let v = v.unwrap_or((1u64 << (7 * n)) - 1);
    Ok((v as i64 - ((1i64 << (7 * n - 1)) - 1), n))
}

// ------------------------------------------------------------ writing

/// Appends an element ID (its bytes, marker included).
pub fn put_id(out: &mut Vec<u8>, id: u32) {
    let n = (4 - id.leading_zeros() as usize / 8).max(1);
    out.extend_from_slice(&id.to_be_bytes()[4 - n..]);
}

/// Appends `size` as a VINT of the shortest length that does not collide
/// with the "unknown" pattern.
pub fn put_size(out: &mut Vec<u8>, size: u64) {
    let n = (1..=8).find(|&n| size < (1u64 << (7 * n)) - 1).unwrap_or(8);
    put_size_n(out, size, n);
}

/// Appends `size` as an `n`-byte VINT (so it can be patched later).
pub fn put_size_n(out: &mut Vec<u8>, size: u64, n: usize) {
    let v = size | 1u64 << (7 * n);
    out.extend_from_slice(&v.to_be_bytes()[8 - n..]);
}

/// Appends a complete element.
pub fn put_element(out: &mut Vec<u8>, id: u32, body: &[u8]) {
    put_id(out, id);
    put_size(out, body.len() as u64);
    out.extend_from_slice(body);
}

/// Appends an unsigned integer element in as few bytes as possible.
pub fn put_uint(out: &mut Vec<u8>, id: u32, v: u64) {
    let n = (8 - v.leading_zeros() as usize / 8).max(1);
    put_element(out, id, &v.to_be_bytes()[8 - n..]);
}

/// Appends an unsigned integer element with an 8-byte body, so that its
/// value can be patched in place later.
pub fn put_uint8(out: &mut Vec<u8>, id: u32, v: u64) {
    put_element(out, id, &v.to_be_bytes());
}

/// Appends a signed integer element in as few bytes as possible.
pub fn put_int(out: &mut Vec<u8>, id: u32, v: i64) {
    let bytes = v.to_be_bytes();
    // Drop leading bytes that only repeat the sign.
    let mut start = 0;
    while start < 7 {
        let (b, next) = (bytes[start], bytes[start + 1]);
        if (b == 0 && next & 0x80 == 0) || (b == 0xFF && next & 0x80 != 0) {
            start += 1;
        } else {
            break;
        }
    }
    put_element(out, id, &bytes[start..]);
}

/// Appends an 8-byte float element.
pub fn put_float(out: &mut Vec<u8>, id: u32, v: f64) {
    put_element(out, id, &v.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_sizes() {
        // Segment ID, 8-byte unknown size.
        let b = [
            0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        ];
        let h = parse_header(&b).expect("header");
        assert_eq!((h.id, h.size, h.len), (0x1853_8067, None, 12));
        // One-byte ID, two-byte size 0x123.
        let h = parse_header(&[0xA3, 0x41, 0x23]).expect("header");
        assert_eq!((h.id, h.size, h.len), (0xA3, Some(0x123), 3));
        // 0xFF as a one-byte size is "unknown" as well.
        assert_eq!(read_size(&[0xFF]), Ok((None, 1)));
        assert_eq!(parse_header(&[0x00, 0x81]), Err(HeaderError::Invalid));
        // A 5-byte ID is not allowed.
        assert_eq!(read_id(&[0x08, 0, 0, 0, 0]), Err(HeaderError::Invalid));
        assert_eq!(parse_header(&[0x1A, 0x45]), Err(HeaderError::Truncated));
        assert_eq!(parse_header(&[0xA3]), Err(HeaderError::Truncated));
    }

    #[test]
    fn values() {
        assert_eq!(read_uint(&[]), Some(0));
        assert_eq!(read_uint(&[0x0F, 0x42, 0x40]), Some(1_000_000));
        assert_eq!(read_uint(&[0; 9]), None);
        assert_eq!(read_int(&[0xFF]), Some(-1));
        assert_eq!(read_int(&[0x00, 0xCD, 0xFE, 0x60]), Some(13_500_000));
        assert_eq!(read_int(&[0x80, 0x00]), Some(-32768));
        assert_eq!(read_float(&48_000f32.to_be_bytes()), Some(48_000.0));
        assert_eq!(read_float(&1.5f64.to_be_bytes()), Some(1.5));
        assert_eq!(read_float(&[1, 2]), None);
        // RFC 9559 10.3.3: one-byte signed VINTs range from -63 to 64.
        assert_eq!(read_signed_vint(&[0x80]), Ok((-63, 1)));
        assert_eq!(read_signed_vint(&[0xBF]), Ok((0, 1)));
        assert_eq!(read_signed_vint(&[0x40, 0x00]), Ok((-8191, 2)));
    }

    #[test]
    fn writing_round_trips() {
        for v in [0u64, 1, 126, 127, 128, 16_382, 16_383, 1 << 40] {
            let mut out = Vec::new();
            put_size(&mut out, v);
            assert_eq!(read_size(&out), Ok((Some(v), out.len())), "{v}");
        }
        for v in [0i64, 1, -1, 127, 128, -128, -129, 13_500_000, i64::MIN] {
            let mut out = Vec::new();
            put_int(&mut out, 0x75A2, v);
            let h = parse_header(&out).expect("header");
            assert_eq!(read_int(&out[h.len..]), Some(v), "{v}");
        }
        for id in [0xA3u32, 0x4286, 0x2A_D7B1, 0x1A45_DFA3] {
            let mut out = Vec::new();
            put_uint(&mut out, id, 300);
            let h = parse_header(&out).expect("header");
            assert_eq!(h.id, id);
            assert_eq!(read_uint(&out[h.len..]), Some(300));
        }
        let mut out = Vec::new();
        put_size_n(&mut out, 5, 8);
        assert_eq!(out, [0x01, 0, 0, 0, 0, 0, 0, 5]);
    }
}
