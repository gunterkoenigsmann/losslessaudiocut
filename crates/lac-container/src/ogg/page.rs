//! Ogg pages (RFC 3533).
//!
//! ```text
//! offset size
//!  0     4   capture pattern "OggS"
//!  4     1   version (0)
//!  5     1   header type: 0x01 continued packet, 0x02 BOS, 0x04 EOS
//!  6     8   granule position (i64 le, -1 = no packet ends here)
//! 14     4   bitstream serial number (le)
//! 18     4   page sequence number (le)
//! 22     4   CRC-32 of the whole page with this field zeroed (le)
//! 26     1   number of segments n
//! 27     n   lacing values; a packet ends at the first value < 255
//! 27+n       segment data
//! ```

/// Header type flag: the first packet continues one from the previous page.
pub const FLAG_CONTINUED: u8 = 0x01;
/// Header type flag: first page of a logical bitstream.
pub const FLAG_BOS: u8 = 0x02;
/// Header type flag: last page of a logical bitstream.
pub const FLAG_EOS: u8 = 0x04;

/// Granule position meaning "no packet finishes on this page".
pub const NO_GRANULE: i64 = -1;

/// CRC-32 used by Ogg: polynomial 0x04C11DB7, MSB-first, init 0, no XOR.
const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u32) << 24;
        let mut b = 0;
        while b < 8 {
            c = if c & 0x8000_0000 != 0 {
                (c << 1) ^ 0x04C1_1DB7
            } else {
                c << 1
            };
            b += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// Ogg CRC-32 of `data`.
pub fn crc32(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |c, &b| {
        (c << 8) ^ CRC_TABLE[((c >> 24) as u8 ^ b) as usize]
    })
}

/// The header fields of a page plus its segment layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageHeader {
    /// Header type flags.
    pub flags: u8,
    /// Granule position as stored (often unreliable, see the module docs of
    /// [`super`]).
    pub granule: i64,
    /// Serial number of the logical bitstream.
    pub serial: u32,
    /// Page sequence number within the logical bitstream.
    pub sequence: u32,
    /// Lacing values.
    pub lacing: Vec<u8>,
}

impl PageHeader {
    /// Length of the header including the lacing table.
    pub fn header_len(&self) -> usize {
        27 + self.lacing.len()
    }

    /// Length of the page body.
    pub fn body_len(&self) -> usize {
        self.lacing.iter().map(|&l| usize::from(l)).sum()
    }
}

/// Why bytes at a position are not a valid page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageError {
    /// Not enough bytes.
    Truncated,
    /// Bad capture pattern, version or CRC.
    Invalid,
}

/// Parses and CRC-checks the page at the start of `b`.
///
/// `b` may be longer than the page; the page's total length is
/// `header_len() + body_len()` of the result.
pub fn parse_page(b: &[u8]) -> Result<PageHeader, PageError> {
    if b.len() < 27 {
        return Err(PageError::Truncated);
    }
    if &b[0..4] != b"OggS" || b[4] != 0 {
        return Err(PageError::Invalid);
    }
    let n = usize::from(b[26]);
    let lacing = b.get(27..27 + n).ok_or(PageError::Truncated)?.to_vec();
    let body: usize = lacing.iter().map(|&l| usize::from(l)).sum();
    let total = 27 + n + body;
    let page = b.get(..total).ok_or(PageError::Truncated)?;
    let stored = u32::from_le_bytes([b[22], b[23], b[24], b[25]]);
    let mut copy = page.to_vec();
    copy[22..26].fill(0);
    if crc32(&copy) != stored {
        return Err(PageError::Invalid);
    }
    let le64 = |r: std::ops::Range<usize>| {
        b[r].iter()
            .rev()
            .fold(0u64, |a, &x| (a << 8) | u64::from(x))
    };
    Ok(PageHeader {
        flags: b[5],
        granule: le64(6..14) as i64,
        serial: le64(14..18) as u32,
        sequence: le64(18..22) as u32,
        lacing,
    })
}

/// Serialises a page; the CRC is computed here.
pub fn write_page(
    flags: u8,
    granule: i64,
    serial: u32,
    sequence: u32,
    lacing: &[u8],
    body: &[u8],
) -> Vec<u8> {
    debug_assert!(lacing.len() <= 255);
    let mut p = Vec::with_capacity(27 + lacing.len() + body.len());
    p.extend(b"OggS");
    p.push(0);
    p.push(flags);
    p.extend(granule.to_le_bytes());
    p.extend(serial.to_le_bytes());
    p.extend(sequence.to_le_bytes());
    p.extend([0; 4]);
    p.push(lacing.len() as u8);
    p.extend(lacing);
    p.extend(body);
    let crc = crc32(&p);
    p[22..26].copy_from_slice(&crc.to_le_bytes());
    p
}

/// Packs packets into pages of one logical bitstream.
///
/// Every call to [`Paginator::flush`] ends the current page, so callers can
/// put header packets on pages of their own, as Vorbis, Opus and Speex
/// require.
pub struct Paginator {
    serial: u32,
    sequence: u32,
    /// Lacing values and body of the page being built.
    lacing: Vec<u8>,
    body: Vec<u8>,
    /// The page being built starts with the rest of an unfinished packet.
    continued: bool,
    /// Granule position after the last packet finished on this page.
    granule: i64,
    /// No page has been emitted yet (the next one gets the BOS flag).
    first: bool,
    /// Offset of the most recently emitted page in `out`.
    last_page_start: usize,
    out: Vec<u8>,
}

/// Body size at which a page is closed; any size up to 65 025 is valid.
const TARGET_BODY: usize = 4096;

impl Paginator {
    /// Starts a logical bitstream with the given serial number.
    pub fn new(serial: u32) -> Self {
        Self {
            serial,
            sequence: 0,
            lacing: Vec::new(),
            body: Vec::new(),
            continued: false,
            granule: NO_GRANULE,
            first: true,
            last_page_start: 0,
            out: Vec::new(),
        }
    }

    /// Appends a packet; `granule` is the stream position after it.
    pub fn push(&mut self, packet: &[u8], granule: i64) {
        let mut rest = packet;
        loop {
            if self.lacing.len() == 255 {
                // Page full in the middle of a packet: continue on the next.
                self.emit(false);
                self.continued = true;
            }
            let take = rest.len().min(255);
            self.lacing.push(take as u8);
            self.body.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            // A lacing value below 255 ends the packet; a packet whose
            // length is a multiple of 255 ends with a 0 value.
            if take < 255 {
                break;
            }
        }
        self.granule = granule;
        if self.body.len() >= TARGET_BODY {
            self.flush();
        }
    }

    /// Ends the current page, if it holds anything.
    pub fn flush(&mut self) {
        if !self.lacing.is_empty() {
            self.emit(false);
        }
    }

    /// Removes and returns the pages that are complete for good, so long
    /// streams need not be held in memory. The most recently emitted page is
    /// kept back because [`Paginator::finish`] may still set its EOS flag.
    pub fn take_complete(&mut self) -> Vec<u8> {
        let tail = self.out.split_off(self.last_page_start);
        self.last_page_start = 0;
        std::mem::replace(&mut self.out, tail)
    }

    /// Ends the stream: emits the last page with the EOS flag and returns
    /// all pages.
    pub fn finish(mut self) -> Vec<u8> {
        if self.lacing.is_empty() && !self.out.is_empty() {
            // The last page was already emitted: set EOS on it and fix its
            // CRC. Its header starts at `last_page_start`.
            let start = self.last_page_start;
            self.out[start + 5] |= FLAG_EOS;
            self.out[start + 22..start + 26].fill(0);
            let crc = crc32(&self.out[start..]);
            self.out[start + 22..start + 26].copy_from_slice(&crc.to_le_bytes());
        } else {
            self.emit(true);
        }
        self.out
    }

    fn emit(&mut self, eos: bool) {
        let mut flags = 0;
        if self.continued {
            flags |= FLAG_CONTINUED;
        }
        if self.first {
            flags |= FLAG_BOS;
        }
        if eos {
            flags |= FLAG_EOS;
        }
        // A page on which no packet finishes carries granule -1.
        let finishes = self.lacing.last().is_some_and(|&l| l < 255);
        let granule = if finishes { self.granule } else { NO_GRANULE };
        self.last_page_start = self.out.len();
        self.out.extend(write_page(
            flags,
            granule,
            self.serial,
            self.sequence,
            &self.lacing,
            &self.body,
        ));
        self.sequence += 1;
        self.lacing.clear();
        self.body.clear();
        self.continued = false;
        self.first = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_check_value() {
        // Ogg's CRC is CRC-32/POSIX (check 0x765E7680) without the final
        // XOR with 0xFFFFFFFF.
        assert_eq!(crc32(b"123456789"), 0x89A1_897F);
    }

    #[test]
    fn paginator_round_trips_packets() {
        let packets: Vec<Vec<u8>> = vec![
            vec![1; 10],
            vec![2; 255],
            vec![3; 70_000],
            vec![],
            vec![4; 3],
        ];
        let mut p = Paginator::new(42);
        p.push(&packets[0], 0);
        p.flush();
        for (i, pk) in packets[1..].iter().enumerate() {
            p.push(pk, i as i64 + 1);
        }
        let bytes = p.finish();

        // Re-read: walk pages and reassemble packets.
        let mut pos = 0;
        let mut got: Vec<Vec<u8>> = Vec::new();
        let mut cur = Vec::new();
        let mut flags = Vec::new();
        while pos < bytes.len() {
            let h = parse_page(&bytes[pos..]).expect("valid page");
            flags.push(h.flags);
            let mut b = pos + h.header_len();
            for &l in &h.lacing {
                cur.extend_from_slice(&bytes[b..b + usize::from(l)]);
                b += usize::from(l);
                if l < 255 {
                    got.push(std::mem::take(&mut cur));
                }
            }
            pos += h.header_len() + h.body_len();
        }
        assert_eq!(got, packets);
        assert_eq!(flags[0] & FLAG_BOS, FLAG_BOS);
        assert_eq!(flags.last().map(|f| f & FLAG_EOS), Some(FLAG_EOS));
        assert!(flags[1..].iter().all(|f| f & FLAG_BOS == 0));
    }
}
