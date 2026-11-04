//! Reading and writing ISO base media file format boxes (ISO/IEC 14496-12
//! 4.2) held in memory.

/// A box found in a buffer: its type and payload (after the header).
#[derive(Debug, Clone, Copy)]
pub struct BoxRef<'a> {
    /// The four-character type.
    pub kind: [u8; 4],
    /// The payload, without the size/type header.
    pub body: &'a [u8],
    /// Offset of the payload within the buffer that was iterated.
    pub body_offset: usize,
}

/// Iterates over the boxes in `buf`. Stops at the first box whose header
/// is malformed or whose size runs beyond the buffer; [`Boxes::truncated`]
/// tells whether that happened.
pub struct Boxes<'a> {
    buf: &'a [u8],
    pos: usize,
    /// A malformed or truncated box ended the iteration.
    pub truncated: bool,
}

impl<'a> Boxes<'a> {
    /// The boxes of `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            truncated: false,
        }
    }
}

impl<'a> Iterator for Boxes<'a> {
    type Item = BoxRef<'a>;

    fn next(&mut self) -> Option<BoxRef<'a>> {
        let rest = self.buf.get(self.pos..)?;
        if rest.is_empty() {
            return None;
        }
        let Some((kind, header, size)) = parse_header(rest, rest.len() as u64) else {
            self.truncated = true;
            return None;
        };
        let size = usize::try_from(size).ok().filter(|&s| s <= rest.len());
        let Some(size) = size else {
            self.truncated = true;
            return None;
        };
        let start = self.pos + header;
        self.pos += size;
        Some(BoxRef {
            kind,
            body: &self.buf[start..self.pos],
            body_offset: start,
        })
    }
}

/// Parses a box header from `b`: (type, header length, total size).
/// `remaining` is what is left of the enclosing space, for size 0 ("to
/// the end", ISO/IEC 14496-12 4.2). `None` if `b` is too short or the
/// size is smaller than the header.
pub fn parse_header(b: &[u8], remaining: u64) -> Option<([u8; 4], usize, u64)> {
    let size32 = u32::from_be_bytes(b.get(0..4)?.try_into().ok()?);
    let kind: [u8; 4] = b.get(4..8)?.try_into().ok()?;
    let (header, size) = match size32 {
        0 => (8, remaining),
        1 => (16, u64::from_be_bytes(b.get(8..16)?.try_into().ok()?)),
        s => (8, u64::from(s)),
    };
    (size >= header as u64).then_some((kind, header, size))
}

/// The first child box of type `kind` in `buf`.
pub fn find<'a>(buf: &'a [u8], kind: &[u8; 4]) -> Option<BoxRef<'a>> {
    Boxes::new(buf).find(|b| &b.kind == kind)
}

/// Follows a path of box types from `buf`, e.g. `[b"mdia", b"minf"]`.
pub fn path<'a>(buf: &'a [u8], kinds: &[&[u8; 4]]) -> Option<BoxRef<'a>> {
    let (first, rest) = kinds.split_first()?;
    let mut b = find(buf, first)?;
    for k in rest {
        let off = b.body_offset;
        let child = find(b.body, k)?;
        b = BoxRef {
            body_offset: off + child.body_offset,
            ..child
        };
    }
    Some(b)
}

/// A big-endian reader over a byte slice that reports running out of
/// data as `None`, so malformed boxes become errors, never panics.
pub struct Reader<'a> {
    b: &'a [u8],
    /// Current position.
    pub pos: usize,
}

impl<'a> Reader<'a> {
    /// Reads `b` from the start.
    pub fn new(b: &'a [u8]) -> Self {
        Self { b, pos: 0 }
    }

    /// The next `n` bytes.
    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(s)
    }

    /// The next byte.
    pub fn u8(&mut self) -> Option<u8> {
        Some(self.bytes(1)?[0])
    }

    /// The next big-endian `u16`.
    pub fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.bytes(2)?.try_into().ok()?))
    }

    /// The next big-endian `u32`.
    pub fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.bytes(4)?.try_into().ok()?))
    }

    /// The next big-endian `u64`.
    pub fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.bytes(8)?.try_into().ok()?))
    }

    /// The bytes not read yet.
    pub fn rest(&self) -> &'a [u8] {
        self.b.get(self.pos..).unwrap_or_default()
    }
}

/// Appends a box of type `kind` with payload `body`, using a 64-bit size
/// only when needed.
pub fn put(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    let size = body.len() as u64 + 8;
    if let Ok(s) = u32::try_from(size) {
        out.extend(s.to_be_bytes());
        out.extend(kind);
    } else {
        out.extend(1u32.to_be_bytes());
        out.extend(kind);
        out.extend((size + 8).to_be_bytes());
    }
    out.extend_from_slice(body);
}

/// Appends a full box (ISO/IEC 14496-12 4.2): version, 24-bit flags, body.
pub fn put_full(out: &mut Vec<u8>, kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) {
    let mut b = Vec::with_capacity(body.len() + 4);
    b.push(version);
    b.extend(&flags.to_be_bytes()[1..]);
    b.extend_from_slice(body);
    put(out, kind, &b);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iterates_and_stops_at_damage() {
        let mut buf = Vec::new();
        put(&mut buf, b"free", &[1, 2, 3]);
        put(&mut buf, b"skip", &[]);
        let ok: Vec<_> = Boxes::new(&buf)
            .map(|b| (b.kind, b.body.to_vec()))
            .collect();
        assert_eq!(ok, vec![(*b"free", vec![1, 2, 3]), (*b"skip", vec![])]);
        // A size beyond the buffer, and one smaller than the header.
        for bad in [[0, 0, 0, 99], [0, 0, 0, 4]] {
            let mut b = buf.clone();
            b.extend(bad);
            b.extend(b"junk");
            let mut it = Boxes::new(&b);
            assert_eq!(it.by_ref().count(), 2);
            assert!(it.truncated);
        }
    }

    #[test]
    fn large_and_to_end_sizes() {
        // size 1: 64-bit size follows; size 0: to the end.
        let mut b = vec![0, 0, 0, 1];
        b.extend(b"mdat");
        b.extend(20u64.to_be_bytes());
        b.extend([7; 4]);
        b.extend([0, 0, 0, 0]);
        b.extend(b"free");
        b.extend([9; 3]);
        let v: Vec<_> = Boxes::new(&b).map(|x| (x.kind, x.body.len())).collect();
        assert_eq!(v, vec![(*b"mdat", 4), (*b"free", 3)]);
    }

    #[test]
    fn reader_never_panics() {
        let mut r = Reader::new(&[1, 2, 3]);
        assert_eq!(r.u16(), Some(0x0102));
        assert_eq!(r.u16(), None);
        assert_eq!(r.u8(), Some(3));
        assert_eq!(r.u8(), None);
    }
}
