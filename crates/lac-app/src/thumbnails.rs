//! The strip of keyframe pictures under the waveform of a video file.
//!
//! A cut video can only join at a clean keyframe (`docs/architecture.md`,
//! section 13), so the strip shows the picture of each clean start at its
//! place on the timeline: the user sees both what is where in the video and
//! where a join can go. [`strip_layout`] picks the keyframes that get a
//! picture without overlapping, and a [`ThumbnailLoader`] decodes and
//! scales their pictures on its own thread, like the
//! [`VideoLoader`](crate::video::VideoLoader) does for the preview pane.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};

use lac_core::MediaTime;

use crate::Source;
use crate::video::{Fitted, VideoTrack, fit};

/// How many pictures a loader keeps before it forgets those that are no
/// longer wanted: a few screens' worth, so scrolling back is instant.
const CACHE_LIMIT: usize = 500;

/// Width of a keyframe picture `height` pixels high, from the track's
/// picture size as displayed (rotation included); at least 1.
pub fn thumbnail_width(track: &VideoTrack, height: u32) -> u32 {
    let info = track.index.info();
    let (w, h) = if matches!(track.rotation, 90 | 270) {
        (info.height, info.width)
    } else {
        (info.width, info.height)
    };
    if h == 0 {
        return height.max(1);
    }
    let w = (u64::from(height) * u64::from(w) + u64::from(h) / 2) / u64::from(h);
    u32::try_from(w).unwrap_or(u32::MAX).max(1)
}

/// The keyframes that get a picture in a strip `width` pixels wide:
/// `starts` are all clean starts of the track with their x position, in
/// timeline order; a picture is `thumb_width` wide and starts at its
/// keyframe's x. Going through all keyframes from the start of the file,
/// each one gets a picture if it does not overlap the previous picture
/// (plus `gap`), so the choice does not change while the view scrolls.
/// Returns only those visible in `0..width`.
pub fn strip_layout(
    starts: impl IntoIterator<Item = (MediaTime, f64)>,
    thumb_width: f64,
    gap: f64,
    width: f64,
) -> Vec<(MediaTime, f64)> {
    let mut free_from = f64::NEG_INFINITY;
    let mut out = Vec::new();
    for (t, x) in starts {
        if x < free_from {
            continue;
        }
        free_from = x + thumb_width + gap;
        if x >= width {
            break;
        }
        if x + thumb_width > 0.0 {
            out.push((t, x));
        }
    }
    out
}

/// One request: the pictures at `times` (presentation times of
/// keyframes), `height` pixels high.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Request {
    height: u32,
    times: Vec<MediaTime>,
}

/// State shared between a [`ThumbnailLoader`] and its thread.
#[derive(Debug, Default)]
struct Shared {
    /// The file has been opened (successfully or not).
    opened: bool,
    /// The video track, if the file has one.
    track: Option<Arc<VideoTrack>>,
    /// Height of the pictures in `pictures`.
    height: u32,
    /// Decoded pictures by presentation time; `None` for a keyframe that
    /// does not decode, so it is not tried again.
    pictures: HashMap<MediaTime, Option<Arc<Fitted>>>,
    /// Increases with every new picture.
    generation: u64,
}

/// A background thread decoding the pictures at keyframes of one file's
/// video track, scaled to the strip's height. Only the latest request
/// matters: when the view moves on, the thread turns to the new pictures.
#[derive(Debug)]
pub struct ThumbnailLoader {
    requests: Sender<Request>,
    shared: Arc<Mutex<Shared>>,
    /// The request sent last, to send each one only once.
    requested: Mutex<Option<Request>>,
}

impl ThumbnailLoader {
    /// Starts a thread that opens `path` (independently of any other
    /// [`Source`] on it) and waits for requests. Dropping the loader ends
    /// the thread after the picture it is working on.
    pub fn start(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let (tx, rx) = channel();
        let shared = Arc::new(Mutex::new(Shared::default()));
        {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || run(&path, &rx, &shared));
        }
        ThumbnailLoader {
            requests: tx,
            shared,
            requested: Mutex::new(None),
        }
    }

    /// The video track, once the file is open and if it has one.
    pub fn track(&self) -> Option<Arc<VideoTrack>> {
        lock(&self.shared).track.clone()
    }

    /// The file has been opened and has no (readable) video track.
    pub fn has_no_video(&self) -> bool {
        let s = lock(&self.shared);
        s.opened && s.track.is_none()
    }

    /// Changes whenever a new picture is ready: the strip needs redrawing.
    pub fn generation(&self) -> u64 {
        lock(&self.shared).generation
    }

    /// Asks for the pictures at `times` (presentation times of keyframes),
    /// `height` pixels high, and returns those already decoded, in the
    /// same order (`None` for the others and for keyframes that do not
    /// decode). The missing ones come in the background; a repeated
    /// request starts nothing new.
    pub fn request(&self, height: u32, times: &[MediaTime]) -> Vec<Option<Arc<Fitted>>> {
        if self.track().is_none() || height == 0 {
            return vec![None; times.len()];
        }
        let request = Request {
            height,
            times: times.to_vec(),
        };
        {
            let mut last = self.requested.lock().unwrap_or_else(|e| e.into_inner());
            if last.as_ref() != Some(&request) {
                *last = Some(request.clone());
                // The thread only ends when the loader is dropped.
                let _ = self.requests.send(request);
            }
        }
        let s = lock(&self.shared);
        times
            .iter()
            .map(|t| {
                s.pictures
                    .get(t)
                    .filter(|_| s.height == height)
                    .cloned()
                    .flatten()
            })
            .collect()
    }
}

/// Locks `m`, recovering from a poisoned lock (fields are replaced as a
/// whole, so they are never half written).
fn lock(m: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Body of the loader thread.
fn run(path: &Path, rx: &Receiver<Request>, shared: &Mutex<Shared>) {
    let opened = Source::open(path).ok().and_then(|source| {
        let clock = source.video_clock()?;
        let decoder = source.video_decoder()?;
        Some((source, clock, decoder))
    });
    let Some((mut source, clock, mut decoder)) = opened else {
        lock(shared).opened = true;
        return;
    };
    let index = decoder.index().clone();
    let track = Arc::new(VideoTrack {
        clock,
        rotation: index.info().rotation,
        index,
    });
    {
        let mut s = lock(shared);
        s.track = Some(Arc::clone(&track));
        s.opened = true;
    }
    // `recv` fails once the loader is dropped, which ends the thread.
    let mut next = rx.recv().ok();
    while let Some(mut request) = next.take() {
        // Skip to the newest request.
        while let Ok(newer) = rx.try_recv() {
            request = newer;
        }
        {
            let mut s = lock(shared);
            if s.height != request.height {
                s.pictures.clear();
                s.height = request.height;
            }
        }
        let width = thumbnail_width(&track, request.height);
        for &t in &request.times {
            // A newer request (the view moved) wins over the rest of this one.
            if let Ok(newer) = rx.try_recv() {
                next = Some(newer);
                break;
            }
            if lock(shared).pictures.contains_key(&t) {
                continue;
            }
            let picture = source
                .video_picture(&mut decoder, t)
                .ok()
                .and_then(|p| fit(&p, track.rotation, width, request.height))
                .map(Arc::new);
            let mut s = lock(shared);
            s.pictures.insert(t, picture);
            s.generation += 1;
        }
        {
            let mut s = lock(shared);
            if s.pictures.len() > CACHE_LIMIT {
                s.pictures.retain(|t, _| request.times.contains(t));
            }
        }
        if next.is_none() {
            next = rx.recv().ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::VideoClock;
    use lac_core::{VideoCodec, VideoFrame, VideoIndex, VideoStreamInfo};
    use std::time::{Duration, Instant};

    fn track(width: u32, height: u32, rotation: u16) -> VideoTrack {
        let frame = VideoFrame {
            byte_offset: 0,
            byte_len: 1,
            duration: 3000,
            composition_offset: 0,
            sync: true,
        };
        VideoTrack {
            clock: VideoClock::new(48_000, 90_000, 0),
            index: VideoIndex::new(
                VideoStreamInfo {
                    codec: VideoCodec::H264,
                    width,
                    height,
                    timescale: 90_000,
                    rotation,
                },
                vec![frame],
            )
            .expect("valid"),
            rotation,
        }
    }

    #[test]
    fn pictures_keep_the_displayed_aspect() {
        assert_eq!(thumbnail_width(&track(1920, 1080, 0), 48), 85);
        // A portrait phone video: landscape frames turned by 90 degrees.
        assert_eq!(thumbnail_width(&track(1920, 1080, 90), 48), 27);
        assert_eq!(thumbnail_width(&track(1920, 1080, 270), 96), 54);
        assert_eq!(thumbnail_width(&track(64, 64, 0), 48), 48);
        // Nonsense sizes never give an empty picture.
        assert_eq!(thumbnail_width(&track(0, 1080, 0), 48), 1);
    }

    /// Keyframes every 30 pixels, `n` of them, starting at `x0`.
    fn every_30(x0: f64, n: i64) -> Vec<(MediaTime, f64)> {
        (0..n).map(|i| (i, x0 + 30.0 * i as f64)).collect()
    }

    #[test]
    fn pictures_never_overlap() {
        // 50 wide plus 4 gap: every second keyframe gets a picture.
        let l = strip_layout(every_30(0.0, 10), 50.0, 4.0, 1000.0);
        let times: Vec<_> = l.iter().map(|&(t, _)| t).collect();
        assert_eq!(times, [0, 2, 4, 6, 8]);
        assert!(l.windows(2).all(|w| w[1].1 - w[0].1 >= 54.0));
        // Keyframes far apart all get one, at their own position.
        let l = strip_layout(every_30(5.0, 3), 20.0, 4.0, 1000.0);
        assert_eq!(l, [(0, 5.0), (1, 35.0), (2, 65.0)]);
    }

    #[test]
    fn scrolling_keeps_the_choice_and_drops_the_invisible() {
        let all = strip_layout(every_30(0.0, 40), 50.0, 4.0, 2000.0);
        // Scrolled by 200 pixels and 160 wide: the same keyframes, shifted;
        // a picture cut by the left edge is still drawn.
        let l = strip_layout(every_30(-200.0, 40), 50.0, 4.0, 160.0);
        let want: Vec<_> = all
            .iter()
            .map(|&(t, x)| (t, x - 200.0))
            .filter(|&(_, x)| x + 50.0 > 0.0 && x < 160.0)
            .collect();
        assert_eq!(l, want);
        assert_eq!(l.first().map(|p| p.0), Some(6), "starts at -20");
        assert_eq!(l.last().map(|p| p.0), Some(10), "12 at 160 is not visible");
        assert!(strip_layout(Vec::new(), 50.0, 4.0, 100.0).is_empty());
    }

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../lac-container/tests/fixtures")
            .join(name)
    }

    /// Polls until `cond` returns something, for at most 20 s.
    fn wait<T>(mut cond: impl FnMut() -> Option<T>) -> T {
        let t = Instant::now();
        loop {
            if let Some(r) = cond() {
                return r;
            }
            assert!(t.elapsed() < Duration::from_secs(20), "no result");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn loader_decodes_the_keyframes() {
        let l = ThumbnailLoader::start(fixture("video_h264_fragmented.mp4"));
        let track = wait(|| l.track());
        let times: Vec<MediaTime> = track.index.clean_start_times().collect();
        assert!(times.len() > 1, "fixture has several keyframes");
        let g = l.generation();
        let first = l.request(16, &times);
        assert_eq!(first.len(), times.len());
        let got = wait(|| {
            let p = l.request(16, &times);
            p.iter().all(Option::is_some).then_some(p)
        });
        assert!(l.generation() > g);
        for p in got.iter().flatten() {
            // The 64x64 test pattern: square.
            assert_eq!((p.width, p.height), (16, 16));
            assert_eq!(p.rgba.len(), 16 * 16 * 4);
        }
        // The picture is the one the preview shows at that keyframe.
        let mut source = Source::open(fixture("video_h264_fragmented.mp4")).expect("opens");
        let mut decoder = source.video_decoder().expect("video");
        let p = source
            .video_picture(&mut decoder, times[1])
            .expect("decodes");
        let want = fit(&p, track.rotation, 16, 16).expect("fits");
        assert_eq!(got[1].as_deref(), Some(&want));
        // Another height decodes again.
        let g = l.generation();
        // Never a picture of the old height; the thread may already have
        // the new one by the time `request` reads the cache, so `None` is
        // not guaranteed here (that race failed a CI-like loaded run).
        let now = l.request(8, &times[..1]);
        assert!(now[0].as_ref().is_none_or(|p| p.height == 8));
        let p = wait(|| l.request(8, &times[..1])[0].clone());
        assert_eq!(p.height, 8);
        assert!(l.generation() > g);
    }

    #[test]
    fn audio_files_have_no_pictures() {
        let l = ThumbnailLoader::start(fixture("mp3_vbr_44k.mp3"));
        wait(|| l.has_no_video().then_some(()));
        assert_eq!(l.request(16, &[0, 1]), [None, None]);
        assert_eq!(l.generation(), 0);
    }
}
