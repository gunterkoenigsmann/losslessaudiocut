//! Just enough of the AAC-LC bitstream (ISO/IEC 14496-3 4.4.2) to tell
//! which scale factor bands of a frame are coded with perceptual noise
//! substitution (PNS, 4.6.13).
//!
//! A PNS band carries no waveform, only its energy; the decoder fills it
//! with noise from its own random generator. libavcodec runs that
//! generator from the start of the stream, so after a cut the band decodes
//! to different noise of the same energy. The packets themselves are still
//! copied verbatim; see "Join points inside a file" in
//! `docs/architecture.md` and issue #72.
//!
//! Only the first channel of a frame's first syntax element is read: its
//! section data follows the element header and `ics_info` directly, while
//! anything after it is behind Huffman-coded scale factors and spectral
//! data. Encoders use PNS on all channels alike, so this is enough to tell
//! whether a stream uses it.

use crate::bits::BitReader;

/// `window_sequence` values (ISO/IEC 14496-3 Table 4.85).
pub mod window {
    /// One long window.
    pub const ONLY_LONG: u8 = 0;
    /// Long window that hands over to short windows.
    pub const LONG_START: u8 = 1;
    /// Eight short windows.
    pub const EIGHT_SHORT: u8 = 2;
    /// Long window that takes over from short windows.
    pub const LONG_STOP: u8 = 3;
}

/// The section codebook that marks a PNS band (ISO/IEC 14496-3 4.6.13.1:
/// `NOISE_HCB`).
pub const NOISE_HCB: u8 = 13;

/// What the export report says about a stream that uses PNS.
pub const NOISE_NOTE: &str = "this AAC stream fills some frequency bands with \
     generated noise (perceptual noise substitution); after a cut, players \
     generate different noise of the same loudness there, everything else \
     is unchanged";

/// The first channel's window and section layout of one AAC frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelInfo {
    /// `window_sequence`, see [`window`].
    pub window_sequence: u8,
    /// `window_shape`: 0 sine, 1 Kaiser-Bessel derived (4.6.11.3.2).
    pub window_shape: u8,
    /// Number of transmitted scale factor bands per window.
    pub max_sfb: u8,
    /// The section codebook of each band below `max_sfb`, per window group
    /// (one group for long windows).
    pub codebooks: Vec<Vec<u8>>,
}

impl ChannelInfo {
    /// Whether any band is noise-substituted.
    pub fn uses_noise_substitution(&self) -> bool {
        self.codebooks.iter().flatten().any(|&c| c == NOISE_HCB)
    }
}

/// Parses the first channel of `frame`, a raw AAC-LC `raw_data_block`.
/// `None` if the frame starts with another element (or is not AAC-LC in
/// a way this parser notices) or is malformed.
pub fn first_channel(frame: &[u8]) -> Option<ChannelInfo> {
    let mut b = BitReader::new(frame);
    let mut read = |n| b.read_u32(n).ok();
    // 4.4.2.1 raw_data_block: id_syn_ele (3) and element_instance_tag (4).
    let id = read(3)?;
    read(4)?;
    let info = match id {
        // single_channel_element, lfe_channel_element: one
        // individual_channel_stream with global_gain (8) and ics_info.
        0 | 3 => {
            read(8)?;
            ics_info(&mut read)?
        }
        // channel_pair_element (4.4.2.1 Table 4.5).
        1 => {
            let common_window = read(1)? == 1;
            if common_window {
                let info = ics_info(&mut read)?;
                // ms_mask_present (2); 1: one ms_used bit per band and group.
                if read(2)? == 1 {
                    for _ in 0..info.groups {
                        for _ in 0..info.max_sfb {
                            read(1)?;
                        }
                    }
                }
                read(8)?; // global_gain of the first channel
                info
            } else {
                read(8)?;
                ics_info(&mut read)?
            }
        }
        _ => return None,
    };
    // 4.4.2.7 section_data.
    let short = info.window_sequence == window::EIGHT_SHORT;
    let sect_bits = if short { 3 } else { 5 };
    let esc = (1 << sect_bits) - 1;
    let mut codebooks = Vec::with_capacity(usize::from(info.groups));
    for _ in 0..info.groups {
        let mut cbs = Vec::with_capacity(usize::from(info.max_sfb));
        while cbs.len() < usize::from(info.max_sfb) {
            let cb = read(4)? as u8;
            // Reserved codebook 12 (Table 4.112) means this is no AAC-LC
            // frame as we understand it.
            if cb == 12 {
                return None;
            }
            let mut len = 0usize;
            loop {
                let incr = read(sect_bits)?;
                len += incr as usize;
                if incr != esc {
                    break;
                }
            }
            if len == 0 || cbs.len() + len > usize::from(info.max_sfb) {
                return None;
            }
            cbs.extend(std::iter::repeat_n(cb, len));
        }
        codebooks.push(cbs);
    }
    Some(ChannelInfo {
        window_sequence: info.window_sequence,
        window_shape: info.window_shape,
        max_sfb: info.max_sfb,
        codebooks,
    })
}

/// What section data needs from `ics_info`.
struct IcsInfo {
    window_sequence: u8,
    window_shape: u8,
    max_sfb: u8,
    /// Number of window groups (1 for long windows).
    groups: u8,
}

/// 4.4.2.1 ics_info, for AAC-LC.
fn ics_info(read: &mut impl FnMut(u32) -> Option<u32>) -> Option<IcsInfo> {
    if read(1)? != 0 {
        return None; // ics_reserved_bit
    }
    let window_sequence = read(2)? as u8;
    let window_shape = read(1)? as u8;
    if window_sequence == window::EIGHT_SHORT {
        let max_sfb = read(4)? as u8;
        // scale_factor_grouping: a 0 bit starts a new group (4.5.2.3.4).
        let grouping = read(7)?;
        let groups = 1 + (0..7).filter(|i| grouping & (0x40 >> i) == 0).count() as u8;
        // 15 short-window bands at most (Table 4.130 and others).
        (max_sfb <= 15).then_some(IcsInfo {
            window_sequence,
            window_shape,
            max_sfb,
            groups,
        })
    } else {
        let max_sfb = read(6)? as u8;
        // predictor_data_present must be 0 in AAC-LC.
        if read(1)? != 0 || max_sfb > 51 {
            return None;
        }
        Some(IcsInfo {
            window_sequence,
            window_shape,
            max_sfb,
            groups: 1,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BitWriter;

    /// A single channel element with a long window and the given sections
    /// (codebook, length).
    fn sce_long(shape: u8, sections: &[(u8, u32)]) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.write(0, 3); // SCE
        w.write(0, 4);
        w.write(100, 8); // global_gain
        w.write(0, 1);
        w.write(u64::from(window::ONLY_LONG), 2);
        w.write(u64::from(shape), 1);
        let max_sfb: u32 = sections.iter().map(|s| s.1).sum();
        w.write(u64::from(max_sfb), 6);
        w.write(0, 1);
        for &(cb, mut len) in sections {
            w.write(u64::from(cb), 4);
            while len >= 31 {
                w.write(31, 5);
                len -= 31;
            }
            w.write(u64::from(len), 5);
        }
        w.write(0, 32); // scale factors etc. that are not read
        w.into_bytes()
    }

    #[test]
    fn sections_and_noise_bands() {
        let f = sce_long(1, &[(1, 10), (NOISE_HCB, 3), (0, 35)]);
        let c = first_channel(&f).expect("valid");
        assert_eq!((c.window_sequence, c.window_shape, c.max_sfb), (0, 1, 48));
        let mut want = vec![1u8; 10];
        want.extend([NOISE_HCB; 3]);
        want.extend([0; 35]);
        assert_eq!(c.codebooks, vec![want]);
        assert!(c.uses_noise_substitution());
        let plain = first_channel(&sce_long(0, &[(5, 49)])).expect("valid");
        assert!(!plain.uses_noise_substitution());
    }

    #[test]
    fn short_windows_have_groups() {
        let mut w = BitWriter::new();
        w.write(0, 3);
        w.write(0, 4);
        w.write(100, 8);
        w.write(0, 1);
        w.write(u64::from(window::EIGHT_SHORT), 2);
        w.write(0, 1);
        w.write(2, 4); // max_sfb
        w.write(0b110_1111, 7); // groups of 3, 5 windows
        for _ in 0..2 {
            w.write(u64::from(NOISE_HCB), 4);
            w.write(2, 3);
        }
        w.write(0, 16);
        let c = first_channel(&w.into_bytes()).expect("valid");
        assert_eq!(c.codebooks, vec![vec![NOISE_HCB; 2]; 2]);
    }

    #[test]
    fn other_elements_and_garbage() {
        // A fill element first; sections beyond max_sfb; too short.
        assert_eq!(first_channel(&[0xC0, 0, 0, 0]), None);
        let mut bad = sce_long(0, &[(1, 49)]);
        bad[3] |= 0x40; // predictor_data_present (bit 25)
        assert_eq!(first_channel(&bad), None);
        assert_eq!(first_channel(&[0]), None);
        for n in 0..64u8 {
            let _ = first_channel(&[n, n.wrapping_mul(37), 0xFF, n]);
        }
    }
}
