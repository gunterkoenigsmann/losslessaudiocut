//! A sliding read buffer for scanning large files byte by byte.

use std::io::{self, Read, Seek, SeekFrom};

/// A sliding read buffer over a seekable source, so that hours-long files
/// can be scanned without loading them into memory.
pub(crate) struct Window<'a, R: Read + Seek + ?Sized> {
    r: &'a mut R,
    /// File offset of `buf[0]`.
    base: u64,
    buf: Vec<u8>,
    /// File length.
    pub len: u64,
    /// Bytes from this offset on must stay buffered.
    pub keep: u64,
}

impl<'a, R: Read + Seek + ?Sized> Window<'a, R> {
    const CHUNK: usize = 1 << 20;

    pub fn new(r: &'a mut R, len: u64) -> Self {
        Self {
            r,
            base: 0,
            buf: Vec::new(),
            len,
            keep: 0,
        }
    }

    /// Makes `[pos, end)` (clamped to the file) available in `buf`.
    fn ensure(&mut self, pos: u64, end: u64) -> io::Result<()> {
        let end = end.min(self.len);
        let keep = self.keep.min(pos);
        if keep < self.base || keep > self.base + self.buf.len() as u64 {
            self.base = keep;
            self.buf.clear();
        } else if keep - self.base > 4 * Self::CHUNK as u64 {
            self.buf.drain(..(keep - self.base) as usize);
            self.base = keep;
        }
        while self.base + (self.buf.len() as u64) < end {
            let at = self.base + self.buf.len() as u64;
            self.r.seek(SeekFrom::Start(at))?;
            let want = ((end - at) as usize).max(Self::CHUNK);
            let old = self.buf.len();
            self.buf.resize(old + want, 0);
            let got = read_up_to(self.r, &mut self.buf[old..])?;
            self.buf.truncate(old + got);
            if got == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "file shrank"));
            }
        }
        Ok(())
    }

    pub fn byte(&mut self, pos: u64) -> io::Result<Option<u8>> {
        if pos >= self.len {
            return Ok(None);
        }
        if pos < self.base || pos >= self.base + self.buf.len() as u64 {
            self.ensure(pos, pos + 1)?;
        }
        Ok(Some(self.buf[(pos - self.base) as usize]))
    }

    /// Up to `n` bytes from `pos` (fewer at the end of the file).
    pub fn bytes(&mut self, pos: u64, n: u64) -> io::Result<&[u8]> {
        if pos >= self.len {
            return Ok(&[]);
        }
        let end = (pos + n).min(self.len);
        if pos < self.base || end > self.base + self.buf.len() as u64 {
            self.ensure(pos, end)?;
        }
        Ok(&self.buf[(pos - self.base) as usize..(end - self.base) as usize])
    }
}

/// Reads until `buf` is full or the end of the input is reached.
fn read_up_to<R: Read + ?Sized>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}
