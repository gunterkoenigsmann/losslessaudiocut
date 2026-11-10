//! Control surfaces (issue #95): DAW controllers such as the Behringer
//! X-Touch, iCON Platform or PreSonus FaderPort, in their Mackie Control
//! (MCU) mode.
//!
//! The Mackie Control protocol fixes the MIDI message of each control, so
//! one table serves every device that speaks it, without a list per
//! model:
//!
//! - **Buttons** send Note On on channel 1 (`0x90`, note, velocity `0x7F`)
//!   when pressed and the same note with velocity 0 (or a Note Off) when
//!   released. Only presses count here.
//! - **The jog wheel** sends Control Change 60 on channel 1 (`0xB0 0x3C`)
//!   with a relative value: 1..63 steps clockwise, 65..127 is 1..63 steps
//!   counter-clockwise (bit 6 is the direction).
//!
//! Only the transport section, the cursor keys, the jog wheel, *Marker* and
//! *Undo* are used; faders, encoders and the many mixer buttons have
//! nothing to do in an editor without a mixer. What each control does is
//! [`Command`]; the GUI runs the menu command of the same name where there
//! is one, so a control surface does nothing a menu could not.

use std::ops::Range;

use lac_core::SamplePos;

/// What a control surface asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// *Play*: start playing (nothing while playing).
    Play,
    /// *Stop*: stop playing (nothing while stopped).
    Stop,
    /// *Rewind*: the cursor [`SKIP_SECONDS`] back.
    Rewind,
    /// *Fast forward*: the cursor [`SKIP_SECONDS`] ahead.
    FastForward,
    /// Cursor *left*: the cursor to the start of the track it is in, or
    /// of the previous track if it is at a start already.
    PreviousTrack,
    /// Cursor *right*: the cursor to the start of the next track.
    NextTrack,
    /// Cursor *up*: View > Zoom in.
    ZoomIn,
    /// Cursor *down*: View > Zoom out.
    ZoomOut,
    /// *Marker*: Edit > Split track here.
    Split,
    /// *Undo*: Edit > Undo.
    Undo,
    /// The jog wheel turned this many steps (negative: counter-clockwise),
    /// each [`JOG_PIXELS`] of the waveform at its current zoom.
    Jog(i32),
}

/// How far *Rewind* and *Fast forward* move the cursor, in seconds.
pub const SKIP_SECONDS: u32 = 5;

/// How far one step of the jog wheel moves the cursor, in pixels of the
/// waveform: the wheel works at any zoom, finer the further one zooms in.
pub const JOG_PIXELS: f64 = 4.0;

/// Note On and Control Change on MIDI channel 1, where the Mackie Control
/// protocol sends. Note Off (`0x80`) is a release and ignored like other
/// messages.
const NOTE_ON: u8 = 0x90;
const CONTROL_CHANGE: u8 = 0xB0;

/// The jog wheel's controller number.
const JOG: u8 = 0x3C;

/// Note numbers of the buttons used (Mackie Control protocol, as listed
/// in the manuals of MCU-compatible devices).
mod note {
    pub const UNDO: u8 = 0x51;
    pub const MARKER: u8 = 0x54;
    pub const REWIND: u8 = 0x5B;
    pub const FAST_FORWARD: u8 = 0x5C;
    pub const STOP: u8 = 0x5D;
    pub const PLAY: u8 = 0x5E;
    pub const UP: u8 = 0x60;
    pub const DOWN: u8 = 0x61;
    pub const LEFT: u8 = 0x62;
    pub const RIGHT: u8 = 0x63;
}

/// The command of one MIDI message, or `None` for anything else: releases,
/// other buttons and controls, other channels, system messages.
pub fn parse(msg: &[u8]) -> Option<Command> {
    match *msg {
        [NOTE_ON, n, velocity] if velocity > 0 => Some(match n {
            note::PLAY => Command::Play,
            note::STOP => Command::Stop,
            note::REWIND => Command::Rewind,
            note::FAST_FORWARD => Command::FastForward,
            note::LEFT => Command::PreviousTrack,
            note::RIGHT => Command::NextTrack,
            note::UP => Command::ZoomIn,
            note::DOWN => Command::ZoomOut,
            note::MARKER => Command::Split,
            note::UNDO => Command::Undo,
            _ => return None,
        }),
        [CONTROL_CHANGE, JOG, v] if v & 0x3F != 0 => {
            let steps = i32::from(v & 0x3F);
            Some(Command::Jog(if v & 0x40 != 0 { -steps } else { steps }))
        }
        _ => None,
    }
}

/// Where a cursor command puts the cursor, or `None` if `cmd` does not
/// move it (or there is nowhere to go).
///
/// `cursor` is the cursor and `total` the recording's length, both source
/// samples at `rate`; `samples_per_pixel` is the waveform's zoom and
/// `starts` the start of each track, in ascending order.
pub fn cursor_target(
    cmd: Command,
    cursor: SamplePos,
    total: SamplePos,
    rate: u32,
    samples_per_pixel: f64,
    starts: &[SamplePos],
) -> Option<SamplePos> {
    let last = total.saturating_sub(1);
    let skip = u64::from(SKIP_SECONDS) * u64::from(rate);
    let target = match cmd {
        Command::Rewind => cursor.saturating_sub(skip),
        Command::FastForward => cursor.saturating_add(skip).min(last),
        Command::Jog(steps) => {
            // At least one sample per step, so a zoom past single samples
            // still moves.
            let per_step = (JOG_PIXELS * samples_per_pixel).round().max(1.0) as u64;
            let delta = per_step.saturating_mul(u64::from(steps.unsigned_abs()));
            if steps < 0 {
                cursor.saturating_sub(delta)
            } else {
                cursor.saturating_add(delta).min(last)
            }
        }
        Command::PreviousTrack => starts.iter().rev().copied().find(|&s| s < cursor)?,
        Command::NextTrack => starts.iter().copied().find(|&s| s > cursor)?,
        _ => return None,
    };
    (target != cursor).then_some(target)
}

/// The starts of `spans` (the tracks), in ascending order.
pub fn track_starts(spans: impl IntoIterator<Item = Range<SamplePos>>) -> Vec<SamplePos> {
    let mut starts: Vec<SamplePos> = spans.into_iter().map(|s| s.start).collect();
    starts.sort_unstable();
    starts.dedup();
    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_buttons_are_mapped_on_press_only() {
        assert_eq!(parse(&[0x90, 0x5E, 0x7F]), Some(Command::Play));
        assert_eq!(parse(&[0x90, 0x5D, 0x7F]), Some(Command::Stop));
        assert_eq!(parse(&[0x90, 0x5B, 0x7F]), Some(Command::Rewind));
        assert_eq!(parse(&[0x90, 0x5C, 0x7F]), Some(Command::FastForward));
        assert_eq!(parse(&[0x90, 0x62, 0x7F]), Some(Command::PreviousTrack));
        assert_eq!(parse(&[0x90, 0x63, 0x7F]), Some(Command::NextTrack));
        assert_eq!(parse(&[0x90, 0x60, 0x7F]), Some(Command::ZoomIn));
        assert_eq!(parse(&[0x90, 0x61, 0x7F]), Some(Command::ZoomOut));
        assert_eq!(parse(&[0x90, 0x54, 0x7F]), Some(Command::Split));
        assert_eq!(parse(&[0x90, 0x51, 0x7F]), Some(Command::Undo));
        // Releases: velocity 0 or Note Off.
        assert_eq!(parse(&[0x90, 0x5E, 0x00]), None);
        assert_eq!(parse(&[0x80, 0x5E, 0x00]), None);
        // Other buttons, other channels, other messages.
        assert_eq!(parse(&[0x90, 0x00, 0x7F]), None);
        assert_eq!(parse(&[0x91, 0x5E, 0x7F]), None);
        assert_eq!(parse(&[0xF0, 0x00, 0x00, 0x66, 0x14, 0xF7]), None);
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&[0x90]), None);
    }

    #[test]
    fn jog_wheel_is_relative() {
        assert_eq!(parse(&[0xB0, 0x3C, 0x01]), Some(Command::Jog(1)));
        assert_eq!(parse(&[0xB0, 0x3C, 0x05]), Some(Command::Jog(5)));
        assert_eq!(parse(&[0xB0, 0x3C, 0x41]), Some(Command::Jog(-1)));
        assert_eq!(parse(&[0xB0, 0x3C, 0x7F]), Some(Command::Jog(-63)));
        // No movement, and other controllers (the V-Pots, 0x10..0x17).
        assert_eq!(parse(&[0xB0, 0x3C, 0x00]), None);
        assert_eq!(parse(&[0xB0, 0x3C, 0x40]), None);
        assert_eq!(parse(&[0xB0, 0x10, 0x01]), None);
    }

    #[test]
    fn cursor_moves() {
        let (rate, total) = (1000, 60_000);
        let starts = [0, 10_000, 30_000];
        let at = |cmd, cursor| cursor_target(cmd, cursor, total, rate, 10.0, &starts);
        assert_eq!(at(Command::Rewind, 20_000), Some(15_000));
        assert_eq!(at(Command::Rewind, 2_000), Some(0));
        assert_eq!(at(Command::Rewind, 0), None);
        assert_eq!(at(Command::FastForward, 20_000), Some(25_000));
        assert_eq!(at(Command::FastForward, 58_000), Some(59_999));
        // 4 pixels of 10 samples per step.
        assert_eq!(at(Command::Jog(3), 100), Some(220));
        assert_eq!(at(Command::Jog(-3), 100), Some(0));
        assert_eq!(at(Command::PreviousTrack, 20_000), Some(10_000));
        assert_eq!(at(Command::PreviousTrack, 10_000), Some(0));
        assert_eq!(at(Command::PreviousTrack, 0), None);
        assert_eq!(at(Command::NextTrack, 10_000), Some(30_000));
        assert_eq!(at(Command::NextTrack, 30_000), None);
        assert_eq!(at(Command::Play, 5), None);
        // Zoomed in past single samples, a step still moves one sample.
        assert_eq!(
            cursor_target(Command::Jog(1), 5, total, rate, 0.01, &starts),
            Some(6)
        );
        assert_eq!(track_starts([30..40, 0..10, 10..30, 10..30]), [0, 10, 30]);
    }
}
