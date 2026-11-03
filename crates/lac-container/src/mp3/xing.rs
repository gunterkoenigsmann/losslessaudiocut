//! The Xing/Info header and the LAME tag, carried in the first frame of an
//! MP3 file instead of audio.
//!
//! ```text
//! offset from "Xing"/"Info" (X)          (all multi-byte fields big-endian)
//! X+0    4   "Xing" (VBR) or "Info" (CBR)
//! X+4    4   flags: 1 frames, 2 bytes, 4 TOC, 8 quality
//! X+8    4   number of audio frames (without this one)
//! X+12   4   bytes of the stream, this frame included
//! X+16 100   TOC: byte position of i % of the duration, in 1/256 of bytes
//! X+116  4   quality
//! LAME tag (offsets for all four flags set)
//! X+120  9   encoder version, e.g. "LAME3.100" (ffmpeg writes "Lavc...")
//! X+129  1   tag revision << 4 | VBR method
//! X+130  1   lowpass / 100 Hz
//! X+131  8   replay gain: peak, radio, audiophile
//! X+139  1   encoding flags << 4 | ATH type
//! X+140  1   bitrate
//! X+141  3   encoder delay (12 bits) and padding (12 bits)
//! X+144  1   misc, X+145 MP3 gain, X+146 preset and surround (2)
//! X+148  4   music length: bytes from this frame to the last frame
//! X+152  2   CRC-16 of the music (all following frames)
//! X+154  2   CRC-16 of this frame up to here
//! ```
//!
//! Gapless decoders (ffmpeg, mpg123, iTunes) skip `delay + 529` samples at
//! the start (529 is the decoder delay of the hybrid filterbank) and stop
//! `padding - 529` samples before the end of the last frame.

/// Decoder delay assumed by the LAME delay/padding convention.
pub const DECODER_DELAY: u32 = 529;

/// Offset of the LAME tag from "Xing" with all flags set.
const LAME_OFFSET: usize = 120;
/// Length of the LAME tag.
const LAME_LEN: usize = 36;

/// What the first frame says about the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XingInfo {
    /// Number of audio frames, if stated.
    pub frames: Option<u32>,
    /// The LAME tag, if present and from a known encoder.
    pub lame: Option<LameTag>,
}

/// Fields of the LAME tag that are kept or rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LameTag {
    /// The 21 bytes from the encoder version to just before the delay:
    /// version, revision/VBR method, lowpass, replay gain, flags, bitrate.
    pub info: [u8; 21],
    /// Encoder delay in samples.
    pub delay: u32,
    /// Padding in samples.
    pub padding: u32,
    /// Misc, MP3 gain, preset and surround.
    pub tail: [u8; 4],
}

/// Recognises a Xing/Info or VBRI header in `frame` whose side information
/// ends at `side_end`. Returns `None` for an ordinary audio frame.
pub fn parse(frame: &[u8], side_end: usize) -> Option<XingInfo> {
    // Fraunhofer's VBRI header sits at a fixed offset of 32 after the
    // header; it carries no gapless information we use.
    if frame.get(36..40) == Some(b"VBRI") {
        return Some(XingInfo {
            frames: None,
            lame: None,
        });
    }
    let x = frame.get(side_end..)?;
    if !(x.starts_with(b"Xing") || x.starts_with(b"Info")) {
        return None;
    }
    let be32 = |i: usize| {
        x.get(i..i + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    let flags = be32(4)?;
    let mut pos = 8;
    let mut frames = None;
    if flags & 1 != 0 {
        frames = be32(pos);
        pos += 4;
    }
    for (bit, len) in [(2, 4), (4, 100), (8, 4)] {
        if flags & bit != 0 {
            pos += len;
        }
    }
    let lame = x.get(pos..pos + LAME_LEN).and_then(|t| {
        // Only encoders known to write the delay/padding fields; ffmpeg
        // checks the same prefixes.
        let known = [b"LAME", b"Lavf", b"Lavc", b"L3.9"]
            .iter()
            .any(|p| t.starts_with(*p));
        known.then(|| {
            let mut info = [0; 21];
            info.copy_from_slice(&t[..21]);
            let mut tail = [0; 4];
            tail.copy_from_slice(&t[24..28]);
            LameTag {
                info,
                delay: (u32::from(t[21]) << 4) | u32::from(t[22] >> 4),
                padding: (u32::from(t[22] & 0x0F) << 8) | u32::from(t[23]),
                tail,
            }
        })
    });
    Some(XingInfo { frames, lame })
}

/// Bytes needed after the side information for [`write()`].
pub const LEN: usize = LAME_OFFSET + LAME_LEN;

/// Everything [`write()`] needs.
#[derive(Debug, Clone)]
pub struct XingWrite<'a> {
    /// "Xing" for variable, "Info" for constant bitrate.
    pub vbr: bool,
    /// Audio frames after the Xing frame.
    pub frames: u32,
    /// Bytes of the stream including the Xing frame.
    pub bytes: u32,
    /// Byte offset (from the Xing frame) of every audio frame.
    pub frame_offsets: &'a [u64],
    /// LAME tag fields to keep (encoder, replay gain, ...).
    pub lame: &'a LameTag,
    /// CRC-16 of all audio frames.
    pub music_crc: u16,
}

/// Fills `frame` (an empty frame whose side information ends at
/// `side_end`) with a Xing/Info header and LAME tag.
pub fn write(frame: &mut [u8], side_end: usize, x: &XingWrite) {
    let mut t = Vec::with_capacity(LEN);
    t.extend(if x.vbr { b"Xing" } else { b"Info" });
    t.extend(15u32.to_be_bytes());
    t.extend(x.frames.to_be_bytes());
    t.extend(x.bytes.to_be_bytes());
    // TOC: for each percent of the duration, the position of the frame
    // starting it as a fraction of the stream length, times 256.
    let n = x.frame_offsets.len();
    for i in 0..100 {
        let off = x.frame_offsets.get(i * n / 100).copied().unwrap_or(0);
        t.push((off * 256 / u64::from(x.bytes.max(1))).min(255) as u8);
    }
    t.extend(0u32.to_be_bytes()); // quality: unknown
    t.extend(x.lame.info);
    let (d, p) = (x.lame.delay.min(4095), x.lame.padding.min(4095));
    t.extend([(d >> 4) as u8, ((d & 0xF) << 4 | p >> 8) as u8, p as u8]);
    t.extend(x.lame.tail);
    t.extend(x.bytes.to_be_bytes()); // music length
    t.extend(x.music_crc.to_be_bytes());
    frame[side_end..side_end + t.len()].copy_from_slice(&t);
    let crc = crc16_arc(&frame[..side_end + t.len()]);
    frame[side_end + t.len()..side_end + t.len() + 2].copy_from_slice(&crc.to_be_bytes());
}

/// CRC-16/ARC (reflected polynomial 0xA001, initial value 0), used by the
/// LAME tag for both of its checksums.
pub fn crc16_arc(data: &[u8]) -> u16 {
    crc16_arc_update(0, data)
}

/// Continues a CRC-16/ARC over more data.
pub fn crc16_arc_update(crc: u16, data: &[u8]) -> u16 {
    data.iter().fold(crc, |mut crc, &b| {
        crc ^= u16::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
        crc
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_arc_check_value() {
        assert_eq!(crc16_arc(b"123456789"), 0xBB3D);
    }

    #[test]
    fn write_then_parse() {
        let lame = LameTag {
            info: *b"LAME3.100\0\0\0\0\0\0\0\0\0\0\0\0",
            delay: 576,
            padding: 1234,
            tail: [0; 4],
        };
        let mut frame = vec![0u8; 417];
        let offsets: Vec<u64> = (0..10).map(|i| 417 + i * 418).collect();
        write(
            &mut frame,
            36,
            &XingWrite {
                vbr: false,
                frames: 10,
                bytes: 417 + 10 * 418,
                frame_offsets: &offsets,
                lame: &lame,
                music_crc: 0x1234,
            },
        );
        let x = parse(&frame, 36).expect("Info frame");
        assert_eq!(x.frames, Some(10));
        assert_eq!(x.lame, Some(lame));
        // The tag CRC covers everything before it.
        let end = 36 + LEN;
        assert_eq!(
            crc16_arc(&frame[..end - 2]).to_be_bytes(),
            [frame[end - 2], frame[end - 1]]
        );
    }
}
