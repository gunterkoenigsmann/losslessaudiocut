//! Decoded samples for a zoomed-in waveform, loaded in the background.
//!
//! The waveform summary ([`lac_peaks`]) stores one min/max/RMS per packet.
//! When the view shows less than a packet per pixel column, it needs the
//! samples themselves ([`crate::waveform::DrawMode`]). A [`DetailLoader`]
//! decodes them on its own thread with random access
//! ([`crate::Source::decode_range`]), so zooming never waits for the
//! decoder: the display shows the summary until the samples arrive.
//!
//! Only the latest request matters. Requests that pile up while the thread
//! is busy are dropped in favour of the newest one.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};

use lac_core::SamplePos;

use crate::Source;

/// Decoded sample frames of one stretch of the timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Detail {
    /// The timeline range the samples cover.
    pub range: Range<SamplePos>,
    /// Channels per frame.
    pub channels: usize,
    /// Interleaved samples, `range.end - range.start` frames.
    pub samples: Vec<f32>,
}

impl Detail {
    /// Whether the samples cover all of `r`.
    pub fn covers(&self, r: &Range<SamplePos>) -> bool {
        self.range.start <= r.start && r.end <= self.range.end
    }

    /// The samples of channel `ch` for the frames `r` (clipped to the
    /// covered range).
    pub fn channel(&self, ch: usize, r: Range<SamplePos>) -> impl Iterator<Item = f32> + '_ {
        let a = r.start.clamp(self.range.start, self.range.end) - self.range.start;
        let b = r.end.clamp(self.range.start, self.range.end) - self.range.start;
        let c = self.channels;
        (a..b.max(a)).filter_map(move |i| self.samples.get(i as usize * c + ch).copied())
    }
}

/// State shared between a [`DetailLoader`] and its thread.
#[derive(Debug, Default)]
struct Shared {
    /// Increases with every new result or error.
    generation: u64,
    detail: Option<Arc<Detail>>,
    error: Option<String>,
}

/// A background thread decoding [`Detail`]s of one file on request.
#[derive(Debug)]
pub struct DetailLoader {
    requests: Sender<Range<SamplePos>>,
    shared: Arc<Mutex<Shared>>,
    /// The last range requested, to ignore repeated requests.
    requested: Option<Range<SamplePos>>,
}

impl DetailLoader {
    /// Starts a thread that opens `path` (independently of any other
    /// [`Source`] on it) and waits for requests. Dropping the loader ends
    /// the thread after the request it is working on.
    pub fn start(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let (tx, rx) = channel();
        let shared = Arc::new(Mutex::new(Shared::default()));
        {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || run(&path, &rx, &shared));
        }
        DetailLoader {
            requests: tx,
            shared,
            requested: None,
        }
    }

    /// Asks for the samples of `range`, unless the latest request or result
    /// already covers it.
    pub fn request(&mut self, range: Range<SamplePos>) {
        let covered = |r: &Range<SamplePos>| r.start <= range.start && range.end <= r.end;
        if self.requested.as_ref().is_some_and(covered) {
            return;
        }
        if lock(&self.shared)
            .detail
            .as_ref()
            .is_some_and(|d| d.covers(&range))
        {
            return;
        }
        self.requested = Some(range.clone());
        // The thread only ends when the loader is dropped, so a failed
        // send cannot happen while `self` exists.
        let _ = self.requests.send(range);
    }

    /// The latest result if its generation differs from `seen`, with that
    /// generation; errors are reported as text for the status line.
    pub fn poll(&self, seen: Option<u64>) -> Option<(u64, Result<Arc<Detail>, String>)> {
        let s = lock(&self.shared);
        if seen == Some(s.generation) || s.generation == 0 {
            return None;
        }
        let r = match (&s.error, &s.detail) {
            (Some(e), _) => Err(e.clone()),
            (None, Some(d)) => Ok(Arc::clone(d)),
            (None, None) => return None,
        };
        Some((s.generation, r))
    }
}

/// Locks `m`, recovering from a poisoned lock (the state is replaced as a
/// whole, so it is never half written).
fn lock(m: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Body of the loader thread.
fn run(path: &std::path::Path, rx: &Receiver<Range<SamplePos>>, shared: &Mutex<Shared>) {
    let publish = |detail: Option<Detail>, error: Option<String>| {
        let mut s = lock(shared);
        s.generation += 1;
        if let Some(d) = detail {
            s.detail = Some(Arc::new(d));
        }
        s.error = error;
    };
    let mut source = match Source::open(path) {
        Ok(s) => s,
        Err(e) => return publish(None, Some(e.to_string())),
    };
    let channels = usize::from(source.info().channels);
    let total = source.total_samples();
    // `recv` fails once the loader is dropped, which ends the thread.
    while let Ok(mut range) = rx.recv() {
        // Skip to the newest request.
        while let Ok(r) = rx.try_recv() {
            range = r;
        }
        let range = range.start.min(total)..range.end.min(total).max(range.start.min(total));
        match source.decode_range(range.clone()) {
            Ok(samples) => publish(
                Some(Detail {
                    range,
                    channels,
                    samples,
                }),
                None,
            ),
            Err(e) => publish(None, Some(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../lac-container/tests/fixtures")
            .join(name)
    }

    /// Polls until a result newer than `seen` arrives.
    fn wait(l: &DetailLoader, seen: Option<u64>) -> (u64, Result<Arc<Detail>, String>) {
        let t = Instant::now();
        loop {
            if let Some(r) = l.poll(seen) {
                return r;
            }
            assert!(t.elapsed() < Duration::from_secs(20), "no result");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn delivers_what_decode_range_decodes() {
        let path = fixture("mp3_vbr_44k.mp3");
        let mut l = DetailLoader::start(&path);
        assert!(l.poll(None).is_none(), "nothing before a request");
        l.request(10_000..12_000);
        let (g, d) = wait(&l, None);
        let d = d.expect("decodes");
        let mut s = Source::open(&path).expect("opens");
        assert_eq!(d.range, 10_000..12_000);
        assert_eq!(d.samples, s.decode_range(10_000..12_000).expect("decodes"));
        assert!(l.poll(Some(g)).is_none());
        // A covered request starts nothing new.
        l.request(10_500..11_000);
        std::thread::sleep(Duration::from_millis(50));
        assert!(l.poll(Some(g)).is_none());
    }

    #[test]
    fn clamps_to_the_timeline_and_reads_channels() {
        let path = fixture("flac_stereo16_44k.flac");
        let total = Source::open(&path).expect("opens").total_samples();
        let mut l = DetailLoader::start(&path);
        l.request(total - 10..total + 50);
        let d = wait(&l, None).1.expect("decodes");
        assert_eq!(d.range, total - 10..total);
        assert_eq!(d.channels, 2);
        assert!(d.covers(&(total - 5..total)));
        assert!(!d.covers(&(total - 11..total)));
        let left: Vec<f32> = d.channel(0, total - 12..total - 8).collect();
        assert_eq!(
            left,
            vec![d.samples[0], d.samples[2]],
            "clipped to the range"
        );
    }

    #[test]
    fn a_missing_file_reports_an_error() {
        let mut l = DetailLoader::start("/nonexistent/x.mp3");
        l.request(0..10);
        let e = wait(&l, None).1.expect_err("fails");
        assert!(e.contains("x.mp3"), "{e}");
    }
}
