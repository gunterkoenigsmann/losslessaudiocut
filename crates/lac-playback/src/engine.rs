//! The playback engine without the audio device: a decoder thread that
//! renders the [`Timeline`] into a ring buffer, and the [`Output`] end that
//! the audio callback reads from.
//!
//! ```text
//! Player (GUI thread) --commands--> decoder thread --ring buffer--> Output::fill (audio callback)
//!        ^------------------------- atomic position -------------------------'
//! ```
//!
//! [`Output::fill`] is real-time safe: it never locks, waits or (after the
//! first call) allocates. A seek must not wait for the buffered old audio
//! to play out, so the decoder, when it takes a seek command, raises a
//! flush request and waits until the callback has discarded the ring's
//! contents and acknowledged it; only then does it write audio for the new
//! position. The request comes from the decoder rather than the player:
//! only the decoder knows that it will write no more audio of the old
//! position, so nothing stale can slip in after the flush. Tests drive
//! [`Output::fill`] directly, so the whole engine runs without a sound
//! card.
//!
//! Jumps are smoothed so they never click: at a seek the callback fades
//! out what it was playing over [`FADE_SECONDS`], stays silent until the
//! decoder has audio for the new position, and fades that in. The first
//! chunk after a seek is short, so that this gap stays small; that keeps
//! scrubbing ([`crate::scrub`]), which seeks every few tens of
//! milliseconds, responsive.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lac_core::SamplePos;

use crate::convert::Converter;
use crate::timeline::Timeline;

/// Decoded audio for the player. Implemented by the application on top of
/// its decoder (`lac_app` uses random-access decoding), and by tests with
/// generated samples.
pub trait PcmSource: Send + 'static {
    /// Channels per frame.
    fn channels(&self) -> usize;

    /// Sample frames per second.
    fn sample_rate(&self) -> u32;

    /// The interleaved samples of the source frames `range`. Returning
    /// fewer frames than asked for is allowed (e.g. at a damaged spot);
    /// playback then continues with the next chunk.
    fn read(&mut self, range: Range<SamplePos>) -> Result<Vec<f32>, String>;
}

/// The format of the audio device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputFormat {
    /// Frames per second.
    pub sample_rate: u32,
    /// Channels per frame.
    pub channels: usize,
}

/// State shared by the player, the decoder thread and the audio callback.
#[derive(Debug, Default)]
struct Shared {
    /// Whether the callback plays (else it outputs silence and keeps the
    /// buffered audio for resuming).
    playing: AtomicBool,
    /// Seek commands sent by the player.
    seeks: AtomicU64,
    /// Seek commands taken by the decoder; the callback then discards the
    /// ring buffer and copies the value to `flush_ack`.
    flush_req: AtomicU64,
    flush_ack: AtomicU64,
    /// Timeline position of the latest seek (the first frame after the
    /// last flush once `flush_ack == seeks`).
    base: AtomicU64,
    /// Output frames played since the last flush.
    played: AtomicU64,
    /// The decoder has written everything up to the end of the timeline:
    /// the flush generation (see `flush_req`) it finished, plus one; 0 if
    /// none. Tagged so that a finish from before a seek is not mistaken
    /// for one after it.
    done: AtomicU64,
    /// The callback found the ring empty after `done`: playback reached
    /// the end.
    drained: AtomicBool,
    /// The first decoding error, for the user.
    error: Mutex<Option<String>>,
}

/// The consuming end, to be called from the audio callback.
#[derive(Debug)]
pub struct Output {
    shared: Arc<Shared>,
    ring: rtrb::Consumer<f32>,
    channels: usize,
    /// Length of a fade in frames.
    fade: usize,
    /// Frames of the current fade-in played so far (`fade` when none).
    fade_in: usize,
    /// Frames of the fade-out for the pending seek played so far.
    fade_out: usize,
}

/// Length of the fades at jumps: long enough to avoid clicks, short
/// enough not to be heard as a fade.
pub const FADE_SECONDS: f64 = 0.005;

impl Output {
    /// Fills `out` (interleaved, in the [`OutputFormat`]) with the next
    /// audio, or silence while paused, flushing or starved. Returns the
    /// number of frames of audio at the start of `out`; the rest is
    /// silence.
    pub fn fill(&mut self, out: &mut [f32]) -> usize {
        let s = &self.shared;
        let req = s.flush_req.load(Ordering::Acquire);
        if req != s.flush_ack.load(Ordering::Relaxed) {
            // Throw away audio of the old position.
            let n = self.ring.slots();
            if let Ok(chunk) = self.ring.read_chunk(n) {
                chunk.commit_all();
            }
            s.played.store(0, Ordering::Relaxed);
            s.drained.store(false, Ordering::Relaxed);
            s.flush_ack.store(req, Ordering::Release);
            self.fade_in = 0;
        }
        if !s.playing.load(Ordering::Relaxed) {
            out.fill(0.0);
            return 0;
        }
        let ch = self.channels;
        if s.seeks.load(Ordering::Acquire) != s.flush_ack.load(Ordering::Relaxed) {
            // A seek waits for the decoder: what is in the ring belongs to
            // the old position. Fade it out, then stay silent. None of it
            // counts as played.
            let frames = out.len() / ch;
            let mut i = 0;
            while i < frames && self.fade_out < self.fade && self.ring.slots() >= ch {
                let gain = 1.0 - (self.fade_out + 1) as f32 / self.fade as f32;
                if let Ok(chunk) = self.ring.read_chunk(ch) {
                    let (a, b) = chunk.as_slices();
                    for (o, v) in out[i * ch..(i + 1) * ch].iter_mut().zip(a.iter().chain(b)) {
                        *o = v * gain;
                    }
                    chunk.commit_all();
                }
                i += 1;
                self.fade_out += 1;
            }
            out[i * ch..].fill(0.0);
            return 0;
        }
        self.fade_out = 0;
        // Whole frames only, so channels never shift.
        let avail = self.ring.slots() / self.channels * self.channels;
        let n = avail.min(out.len() / self.channels * self.channels);
        if let Ok(chunk) = self.ring.read_chunk(n) {
            let (a, b) = chunk.as_slices();
            out[..a.len()].copy_from_slice(a);
            out[a.len()..n].copy_from_slice(b);
            chunk.commit_all();
        }
        // Fade in after a seek.
        for frame in out[..n].chunks_exact_mut(ch) {
            if self.fade_in >= self.fade {
                break;
            }
            let gain = self.fade_in as f32 / self.fade as f32;
            frame.iter_mut().for_each(|v| *v *= gain);
            self.fade_in += 1;
        }
        out[n..].fill(0.0);
        s.played
            .fetch_add((n / self.channels) as u64, Ordering::Relaxed);
        if n == 0 && s.done.load(Ordering::Acquire) == req + 1 {
            s.drained.store(true, Ordering::Release);
        }
        n / self.channels
    }
}

/// Commands from the player to the decoder thread.
#[derive(Debug)]
enum Command {
    /// Continue at this timeline position (and possibly a new timeline).
    Seek(SamplePos, Option<Timeline>),
    /// End the thread.
    Stop,
}

/// Plays a [`Timeline`] of a [`PcmSource`]: the control side of the
/// engine. The audio device, if any, lives in [`crate::Player`].
#[derive(Debug)]
pub struct Engine {
    shared: Arc<Shared>,
    commands: Sender<Command>,
    thread: Option<JoinHandle<()>>,
    in_rate: u32,
    out_rate: u32,
    /// Length of the current timeline.
    len: SamplePos,
}

/// Source frames decoded per step: a quarter second keeps the cost of
/// each random-access decoder start (its pre-roll) small.
const CHUNK_SECONDS_DIV: u32 = 4;
/// The first chunk after a seek is shorter (1/20 s), so that audio for
/// the new position arrives quickly.
const FIRST_CHUNK_SECONDS_DIV: u32 = 20;
/// Ring buffer length in seconds of output.
const RING_SECONDS: usize = 1;
/// How long a seek waits for the callback to acknowledge the flush. The
/// callback runs every few milliseconds; if it does not (device gone),
/// playback continues anyway.
const FLUSH_TIMEOUT: Duration = Duration::from_millis(500);
/// How long the decoder sleeps when the ring is full.
const IDLE: Duration = Duration::from_millis(5);

impl Engine {
    /// Starts the decoder thread for `source`, positioned at the start of
    /// `timeline` and paused. Returns the engine and the end to hand to
    /// the audio callback.
    pub fn start<S: PcmSource>(
        source: S,
        timeline: Timeline,
        format: OutputFormat,
    ) -> (Engine, Output) {
        let shared = Arc::new(Shared::default());
        let channels = format.channels.max(1);
        let capacity = format.sample_rate.max(1) as usize * channels * RING_SECONDS;
        let (producer, consumer) = rtrb::RingBuffer::new(capacity);
        let (tx, rx) = channel();
        let in_rate = source.sample_rate();
        let len = timeline.len();
        let thread = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                Decoder {
                    converter: Converter::new(
                        in_rate,
                        source.channels(),
                        format.sample_rate,
                        channels,
                    ),
                    source,
                    timeline,
                    pos: 0,
                    shared,
                    ring: producer,
                    pending: Vec::new(),
                    generation: 0,
                }
                .run(&rx);
            })
        };
        let engine = Engine {
            shared: Arc::clone(&shared),
            commands: tx,
            thread: Some(thread),
            in_rate,
            out_rate: format.sample_rate.max(1),
            len,
        };
        let fade = ((f64::from(format.sample_rate) * FADE_SECONDS) as usize).max(1);
        let output = Output {
            shared,
            ring: consumer,
            channels,
            fade,
            // No fade-in at the very start: playback starts with a seek
            // anyway when it does not start at 0.
            fade_in: fade,
            fade_out: 0,
        };
        (engine, output)
    }

    /// Starts or resumes playing.
    pub fn play(&self) {
        self.shared.playing.store(true, Ordering::Relaxed);
    }

    /// Pauses; [`Engine::play`] resumes at the same spot.
    pub fn pause(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
    }

    /// Whether playing (also while waiting at the end).
    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    /// Continues at timeline position `t`.
    pub fn seek(&mut self, t: SamplePos) {
        self.send_seek(t.min(self.len), None);
    }

    /// Switches to a new timeline (after an edit) and continues at its
    /// position `t`.
    pub fn set_timeline(&mut self, timeline: Timeline, t: SamplePos) {
        self.len = timeline.len();
        self.send_seek(t.min(self.len), Some(timeline));
    }

    fn send_seek(&self, t: SamplePos, timeline: Option<Timeline>) {
        // Report the new position at once, before the decoder gets to it.
        self.shared.base.store(t, Ordering::Relaxed);
        self.shared.seeks.fetch_add(1, Ordering::AcqRel);
        self.shared.drained.store(false, Ordering::Relaxed);
        let _ = self.commands.send(Command::Seek(t, timeline));
    }

    /// Length of the timeline being played.
    pub fn len(&self) -> SamplePos {
        self.len
    }

    /// Whether the timeline is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The timeline position being heard (up to the device's latency).
    pub fn position(&self) -> SamplePos {
        let s = &self.shared;
        if self.at_end() {
            return self.len;
        }
        let base = s.base.load(Ordering::Relaxed);
        if s.seeks.load(Ordering::Acquire) != s.flush_ack.load(Ordering::Acquire) {
            // The callback still has old audio; its count is stale.
            return base;
        }
        let played = u128::from(s.played.load(Ordering::Relaxed));
        let t = played * u128::from(self.in_rate) / u128::from(self.out_rate);
        (base + SamplePos::try_from(t).unwrap_or(SamplePos::MAX)).min(self.len)
    }

    /// Whether playback reached the end of the timeline.
    pub fn at_end(&self) -> bool {
        let s = &self.shared;
        // An end found before the latest seek does not count.
        s.drained.load(Ordering::Acquire)
            && s.seeks.load(Ordering::Acquire) == s.flush_ack.load(Ordering::Acquire)
    }

    /// The first decoding error, if any. Playback skips the chunk that
    /// failed.
    pub fn error(&self) -> Option<String> {
        self.shared
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        // Do not join: the thread may be decoding a chunk; it ends at its
        // next command check, and closing a file must not wait for that.
        drop(self.thread.take());
    }
}

/// The decoder thread's state.
struct Decoder<S> {
    source: S,
    timeline: Timeline,
    /// Next timeline position to decode.
    pos: SamplePos,
    converter: Converter,
    shared: Arc<Shared>,
    ring: rtrb::Producer<f32>,
    /// Converted samples not yet written because the ring was full.
    pending: Vec<f32>,
    /// Seeks taken so far; the value of `flush_req` for the current
    /// position.
    generation: u64,
}

impl<S: PcmSource> Decoder<S> {
    fn run(mut self, rx: &Receiver<Command>) {
        let rate = self.source.sample_rate();
        let chunk = SamplePos::from((rate / CHUNK_SECONDS_DIV).max(1));
        let first_chunk = SamplePos::from((rate / FIRST_CHUNK_SECONDS_DIV).max(1));
        let mut next_chunk = chunk;
        loop {
            // Wait only when there is nothing to do right now.
            let busy = !self.pending.is_empty() || self.pos < self.timeline.len();
            let cmd = if busy && self.ring.slots() > 0 {
                rx.try_recv().map_err(|e| match e {
                    std::sync::mpsc::TryRecvError::Empty => RecvTimeoutError::Timeout,
                    std::sync::mpsc::TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
                })
            } else {
                rx.recv_timeout(IDLE)
            };
            match cmd {
                Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(Command::Seek(t, timeline)) => {
                    if let Some(tl) = timeline {
                        self.timeline = tl;
                    }
                    self.pos = t;
                    self.generation += 1;
                    self.pending.clear();
                    self.converter.reset();
                    self.shared
                        .flush_req
                        .store(self.generation, Ordering::Release);
                    self.wait_for_flush();
                    next_chunk = first_chunk;
                    continue;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            if !self.pending.is_empty() {
                self.write_pending();
                continue;
            }
            match self.timeline.chunk(self.pos, next_chunk) {
                Some(range) => {
                    next_chunk = chunk;
                    self.pos += range.end - range.start;
                    match self.source.read(range) {
                        Ok(pcm) => self.converter.push(&pcm, &mut self.pending),
                        Err(e) => {
                            let mut err =
                                self.shared.error.lock().unwrap_or_else(|e| e.into_inner());
                            err.get_or_insert(e);
                        }
                    }
                    self.write_pending();
                }
                None => self
                    .shared
                    .done
                    .store(self.generation + 1, Ordering::Release),
            }
        }
    }

    /// Waits until the callback has dropped the audio of the old position.
    fn wait_for_flush(&self) {
        let start = Instant::now();
        let s = &self.shared;
        while s.flush_ack.load(Ordering::Acquire) != s.flush_req.load(Ordering::Acquire)
            && start.elapsed() < FLUSH_TIMEOUT
        {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Writes as much of `pending` as fits into the ring.
    fn write_pending(&mut self) {
        let n = self.ring.slots().min(self.pending.len());
        if let Ok(mut chunk) = self.ring.write_chunk(n) {
            let (a, b) = chunk.as_mut_slices();
            let la = a.len();
            a.copy_from_slice(&self.pending[..la]);
            b.copy_from_slice(&self.pending[la..n]);
            chunk.commit_all();
            self.pending.drain(..n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mono source whose sample at frame `i` is `i` (exact in f32 up to
    /// 2^24), so the output says which frames were played.
    struct Ramp {
        rate: u32,
        total: SamplePos,
    }

    impl PcmSource for Ramp {
        fn channels(&self) -> usize {
            1
        }
        fn sample_rate(&self) -> u32 {
            self.rate
        }
        fn read(&mut self, r: Range<SamplePos>) -> Result<Vec<f32>, String> {
            if r.end > self.total {
                return Err(format!("read past the end: {r:?}"));
            }
            Ok(r.map(|i| i as f32).collect())
        }
    }

    const FMT: OutputFormat = OutputFormat {
        sample_rate: 1000,
        channels: 1,
    };

    /// Calls the callback until `want` frames came out or a second passed.
    fn pull(out: &mut Output, want: usize) -> Vec<f32> {
        let mut got = Vec::new();
        let start = Instant::now();
        let mut buf = [0.0; 64];
        while got.len() < want && start.elapsed() < Duration::from_secs(5) {
            // Never more than asked for, so positions are exact.
            let room = (want - got.len()).min(buf.len());
            let n = out.fill(&mut buf[..room]) * out.channels;
            got.extend_from_slice(&buf[..n]);
            if n == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        got
    }

    #[test]
    fn plays_the_kept_parts_in_order_and_reaches_the_end() {
        let tl = Timeline::new([10..300, 500..520, 900..1000]);
        let (engine, mut out) = Engine::start(
            Ramp {
                rate: 1000,
                total: 1000,
            },
            tl.clone(),
            FMT,
        );
        assert_eq!(engine.position(), 0);
        // Paused: silence and no progress.
        let mut buf = [1.0; 16];
        out.fill(&mut buf);
        assert_eq!(buf, [0.0; 16]);
        engine.play();
        let got = pull(&mut out, 410);
        let want: Vec<f32> = tl
            .segments()
            .iter()
            .cloned()
            .flatten()
            .map(|i| i as f32)
            .collect();
        assert_eq!(got, want, "every kept frame once, bit for bit");
        assert_eq!(engine.position(), 410);
        // Ask until the callback notices the end.
        let start = Instant::now();
        while !engine.at_end() && start.elapsed() < Duration::from_secs(5) {
            out.fill(&mut buf);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(engine.at_end());
        assert_eq!(engine.position(), 410);
        assert_eq!(engine.error(), None);
    }

    #[test]
    fn seeking_drops_buffered_audio() {
        let (mut engine, mut out) = Engine::start(
            Ramp {
                rate: 1000,
                total: 10_000,
            },
            Timeline::whole(10_000),
            FMT,
        );
        engine.play();
        let first = pull(&mut out, 100);
        assert_eq!(first[0], 0.0);
        // Let the decoder fill the ring well ahead.
        std::thread::sleep(Duration::from_millis(50));
        engine.seek(5000);
        assert_eq!(engine.position(), 5000, "reported at once");
        let after = pull(&mut out, 50);
        // FMT is 1 kHz: the fade-in is 5 frames, starting from silence.
        assert_eq!(after[0], 0.0);
        assert_eq!(after[1], 5001.0 * 0.2);
        assert_eq!(
            &after[5..8],
            &[5005.0, 5006.0, 5007.0],
            "no old audio after a seek"
        );
        assert_eq!(after[49], 5049.0);
        assert_eq!(engine.position(), 5050);
    }

    /// A ramp that lets only `allowed` reads through and holds the next
    /// one until more are allowed, to catch the engine mid-seek.
    struct Gated {
        ramp: Ramp,
        allowed: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl PcmSource for Gated {
        fn channels(&self) -> usize {
            1
        }
        fn sample_rate(&self) -> u32 {
            self.ramp.rate
        }
        fn read(&mut self, r: Range<SamplePos>) -> Result<Vec<f32>, String> {
            while self
                .allowed
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                .is_err()
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.ramp.read(r)
        }
    }

    #[test]
    fn a_seek_fades_out_the_old_audio_and_fades_in_the_new() {
        let allowed = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let source = Gated {
            ramp: Ramp {
                rate: 1000,
                total: 10_000,
            },
            allowed: Arc::clone(&allowed),
        };
        let (mut engine, mut out) = Engine::start(source, Timeline::whole(10_000), FMT);
        engine.play();
        assert_eq!(
            pull(&mut out, 10),
            (0..10).map(|i| i as f32).collect::<Vec<_>>()
        );
        // The decoder now hangs in its second read, so the seek stays
        // pending: the callback fades out frames 10..15 (5 frames at
        // 1 kHz) and then is silent; none of it counts as played.
        engine.seek(5000);
        let mut buf = [9.0; 10];
        assert_eq!(out.fill(&mut buf), 0);
        let want = [
            10.0 * 0.8,
            11.0 * 0.6,
            12.0 * 0.4,
            13.0 * 0.2,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
        ];
        for (g, w) in buf.iter().zip(want) {
            assert!((g - w).abs() < 1e-4, "{buf:?}");
        }
        assert_eq!(out.fill(&mut buf), 0);
        assert_eq!(buf, [0.0; 10], "the fade-out happens once");
        assert_eq!(engine.position(), 5000);
        allowed.store(usize::MAX / 2, Ordering::Release);
        let got = pull(&mut out, 8);
        assert_eq!(got[0], 0.0, "fades in from silence");
        assert_eq!(&got[5..], &[5005.0, 5006.0, 5007.0]);
    }

    #[test]
    fn a_new_timeline_continues_at_the_given_position() {
        let (mut engine, mut out) = Engine::start(
            Ramp {
                rate: 1000,
                total: 1000,
            },
            Timeline::whole(1000),
            FMT,
        );
        engine.play();
        pull(&mut out, 10);
        // Delete 100..200; continue at timeline 100, i.e. source 200.
        engine.set_timeline(Timeline::new([0..100, 200..1000]), 100);
        assert_eq!(engine.len(), 900);
        let got = pull(&mut out, 8);
        // After the fade-in (5 frames at 1 kHz).
        assert_eq!(&got[5..], &[205.0, 206.0, 207.0]);
    }

    #[test]
    fn pausing_keeps_the_position() {
        let (engine, mut out) = Engine::start(
            Ramp {
                rate: 1000,
                total: 1000,
            },
            Timeline::whole(1000),
            FMT,
        );
        engine.play();
        pull(&mut out, 64);
        engine.pause();
        let mut buf = [0.0; 64];
        out.fill(&mut buf);
        assert_eq!(engine.position(), 64);
        engine.play();
        assert_eq!(pull(&mut out, 1), [64.0]);
    }

    #[test]
    fn resamples_and_maps_channels_with_a_consistent_position() {
        // 1 kHz mono to 2 kHz stereo: every frame twice in time, on both
        // channels.
        let fmt = OutputFormat {
            sample_rate: 2000,
            channels: 2,
        };
        let (engine, mut out) = Engine::start(
            Ramp {
                rate: 1000,
                total: 1000,
            },
            Timeline::whole(1000),
            fmt,
        );
        engine.play();
        let got = pull(&mut out, 200);
        assert_eq!(&got[..8], &[0.0, 0.0, 0.5, 0.5, 1.0, 1.0, 1.5, 1.5]);
        // 100 output frames are 50 source frames.
        assert_eq!(engine.position(), 50);
    }

    #[test]
    fn errors_are_reported_and_skipped() {
        // The source claims fewer frames than the timeline has.
        let (engine, mut out) = Engine::start(
            Ramp {
                rate: 1000,
                total: 300,
            },
            Timeline::whole(600),
            FMT,
        );
        engine.play();
        // Chunks are a quarter second (250 frames): the first plays, the
        // ones reaching past frame 300 fail and are skipped.
        let got = pull(&mut out, 250);
        assert_eq!(got, (0..250).map(|i| i as f32).collect::<Vec<_>>());
        let start = Instant::now();
        let mut buf = [0.0; 64];
        while !engine.at_end() && start.elapsed() < Duration::from_secs(5) {
            assert_eq!(out.fill(&mut buf), 0, "nothing after the failed chunks");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(engine.at_end());
        assert!(engine.error().expect("error").contains("past the end"));
    }
}
