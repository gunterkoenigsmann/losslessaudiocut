//! The video preview: which picture belongs to an audio position, stepping
//! from picture to picture, fitting a picture into the preview pane, and
//! decoding pictures in the background.
//!
//! The editor's timeline is the audio's (sample positions, see
//! `docs/architecture.md`, section 13); a [`VideoClock`] maps a position to
//! the video's time and back. A [`VideoLoader`] decodes the picture for the
//! latest requested position on its own thread
//! ([`lac_decode::VideoDecoder`]), so playback and scrolling never wait
//! for the decoder: the pane shows the last picture until the next one
//! arrives.
//!
//! The pane draws with the GPU where it can (OpenGL in `lac-gui`): the
//! loader then hands over the decoded planes ([`Image::Planes`]) and the
//! GPU converts and scales them. Otherwise, or for pixel formats a GPU
//! cannot read plane by plane, it converts to RGB itself
//! ([`Image::Rgb`]). [`Output`] chooses.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};

use lac_core::{MediaTime, Rounding, SamplePos, VideoIndex, rescale};
pub use lac_decode::gpu::{Channel, GpuLayout, GpuTexture};
pub use lac_decode::{Frame, Picture};

use crate::Source;

/// Maps audio sample positions to video times and back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoClock {
    /// Audio samples per second.
    rate: u32,
    /// Video ticks per second.
    timescale: u32,
    /// Video time at audio sample 0
    /// ([`lac_container::Container::video_offset`]).
    offset: MediaTime,
}

impl VideoClock {
    /// A clock for audio at `rate` samples per second and video at
    /// `timescale` ticks per second whose time `offset` plays with audio
    /// sample 0.
    pub fn new(rate: u32, timescale: u32, offset: MediaTime) -> Self {
        Self {
            rate: rate.max(1),
            timescale: timescale.max(1),
            offset,
        }
    }

    /// The video time at audio position `pos` (rounded down, so it lies
    /// in the picture shown at that sample).
    pub fn time_at(&self, pos: SamplePos) -> MediaTime {
        let t = MediaTime::try_from(pos).unwrap_or(MediaTime::MAX);
        rescale(t, self.rate, self.timescale, Rounding::Down).saturating_add(self.offset)
    }

    /// The first audio position whose video time is `time` or later (so
    /// that [`VideoClock::time_at`] of it lies in the picture starting at
    /// `time`); 0 for times before the audio.
    pub fn position_of(&self, time: MediaTime) -> SamplePos {
        let s = rescale(
            time.saturating_sub(self.offset),
            self.timescale,
            self.rate,
            Rounding::Up,
        );
        SamplePos::try_from(s).unwrap_or(0)
    }
}

/// Presentation times of the frames, in display order.
fn display_times(index: &VideoIndex) -> Vec<MediaTime> {
    index
        .display_order()
        .iter()
        .map(|&k| index.presentation_time(k))
        .collect()
}

/// The presentation time of the picture shown at video time `t`: the
/// first picture before the track, the last one after it; `None` for a
/// track without frames.
pub fn picture_time(index: &VideoIndex, t: MediaTime) -> Option<MediaTime> {
    let times = display_times(index);
    let n = times.partition_point(|&p| p <= t);
    times.get(n.saturating_sub(1)).copied()
}

/// The audio position of the next picture after the one shown at `pos`
/// (`forward`), or of the start of the picture shown at `pos` if `pos`
/// lies inside it, else of the previous picture (backwards). `None` when
/// there is no picture in that direction within `0..=total`.
pub fn step(
    index: &VideoIndex,
    clock: &VideoClock,
    pos: SamplePos,
    total: SamplePos,
    forward: bool,
) -> Option<SamplePos> {
    let times = display_times(index);
    let t = clock.time_at(pos);
    // Pictures whose start lies at or before t; the last of them is shown.
    let shown = times.partition_point(|&p| p <= t);
    let candidates: Box<dyn Iterator<Item = SamplePos>> = if forward {
        Box::new(times[shown..].iter().map(|&p| clock.position_of(p)))
    } else {
        Box::new(times[..shown].iter().rev().map(|&p| clock.position_of(p)))
    };
    // Rounding can map two pictures to one sample; take the first that
    // moves.
    candidates
        .take_while(|&p| p <= total)
        .find(|&p| if forward { p > pos } else { p < pos })
}

/// Where a picture of `width` x `height` pixels, turned clockwise by
/// `rotation` degrees (0, 90, 180 or 270; other values count as 0), goes
/// in a pane of `pane_width` x `pane_height`: the largest size that keeps
/// its aspect ratio, centred, as (x, y, width, height); `None` for an
/// empty pane or picture.
pub fn fit_rect(
    width: u32,
    height: u32,
    rotation: u16,
    pane_width: u32,
    pane_height: u32,
) -> Option<(u32, u32, u32, u32)> {
    let (sw, sh) = (u64::from(width), u64::from(height));
    if sw == 0 || sh == 0 || pane_width == 0 || pane_height == 0 {
        return None;
    }
    let turned = matches!(rotation, 90 | 270);
    // Size as displayed, before scaling.
    let (rw, rh) = if turned { (sh, sw) } else { (sw, sh) };
    let (pw, ph) = (u64::from(pane_width), u64::from(pane_height));
    // The largest size of rw:rh that fits: limited by width or height.
    let (w, h) = if pw * rh <= ph * rw {
        (pw, (pw * rh / rw).max(1))
    } else {
        ((ph * rw / rh).max(1), ph)
    };
    Some((
        ((pw - w) / 2) as u32,
        ((ph - h) / 2) as u32,
        w as u32,
        h as u32,
    ))
}

/// The point of the source picture shown at point (`u`, `v`) of the
/// displayed (turned) picture, both in 0.0..=1.0 from the top left: how
/// a GPU renderer maps the corners of its quad to texture coordinates.
/// Rotation as for [`fit_rect`].
pub fn source_point(rotation: u16, u: f32, v: f32) -> (f32, f32) {
    match rotation {
        90 => (v, 1.0 - u),
        180 => (1.0 - u, 1.0 - v),
        270 => (1.0 - v, u),
        _ => (u, v),
    }
}

/// A picture scaled and rotated to fit a pane, centred in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fitted {
    /// Left edge in the pane.
    pub x: u32,
    /// Top edge in the pane.
    pub y: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height` pixels of red, green, blue and alpha bytes.
    pub rgba: Vec<u8>,
}

/// `picture` turned clockwise by `rotation` degrees (0, 90, 180 or 270;
/// other values count as 0) and scaled, keeping its aspect ratio, to the
/// largest size that fits `width` x `height`; `None` for an empty pane or
/// picture. Each pixel is taken from the nearest source pixel, which is
/// enough for a preview.
pub fn fit(picture: &Picture, rotation: u16, width: u32, height: u32) -> Option<Fitted> {
    let (x, y, w, h) = fit_rect(picture.width, picture.height, rotation, width, height)?;
    let (sw, sh) = (u64::from(picture.width), u64::from(picture.height));
    let turned = matches!(rotation, 90 | 270);
    // Size as displayed, before scaling.
    let (rw, rh) = if turned { (sh, sw) } else { (sw, sh) };
    let (w, h) = (u64::from(w), u64::from(h));
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for oy in 0..h {
        let ry = oy * rh / h;
        for ox in 0..w {
            let rx = ox * rw / w;
            // The source pixel shown at (rx, ry) of the turned picture.
            let (sx, sy) = match rotation {
                90 => (ry, sh - 1 - rx),
                180 => (sw - 1 - rx, sh - 1 - ry),
                270 => (sw - 1 - ry, rx),
                _ => (rx, ry),
            };
            let at = ((sy * sw + sx) * 3) as usize;
            let px = picture.rgb.get(at..at + 3).unwrap_or(&[0, 0, 0]);
            rgba.extend_from_slice(&[px[0], px[1], px[2], 0xff]);
        }
    }
    Some(Fitted {
        x,
        y,
        width: w as u32,
        height: h as u32,
        rgba,
    })
}

/// What a [`VideoLoader`] delivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Output {
    /// Pictures converted to RGB ([`Image::Rgb`]), for drawing as bitmaps.
    #[default]
    Rgb,
    /// Decoded planes where a GPU can read them ([`Image::Planes`], see
    /// [`lac_decode::gpu`]), RGB for other pixel formats.
    Planes,
}

/// A decoded picture as a [`VideoLoader`] delivers it.
#[derive(Debug, Clone, PartialEq)]
pub enum Image {
    /// Converted to RGB.
    Rgb(Picture),
    /// As decoded; [`Frame::gpu_layout`] says how to show it.
    Planes(Frame),
}

impl Image {
    /// Width and height in pixels, before rotation.
    pub fn size(&self) -> (u32, u32) {
        match self {
            Image::Rgb(p) => (p.width, p.height),
            Image::Planes(f) => (f.width, f.height),
        }
    }

    /// Presentation time (ticks of the video timescale).
    pub fn time(&self) -> MediaTime {
        match self {
            Image::Rgb(p) => p.time,
            Image::Planes(f) => f.time,
        }
    }

    /// The picture in RGB, converting decoded planes.
    pub fn to_rgb(&self) -> std::borrow::Cow<'_, Picture> {
        match self {
            Image::Rgb(p) => std::borrow::Cow::Borrowed(p),
            Image::Planes(f) => std::borrow::Cow::Owned(f.to_rgb()),
        }
    }
}

/// What a [`VideoLoader`] knows about the file's video track once it has
/// opened it.
#[derive(Debug, Clone)]
pub struct VideoTrack {
    /// Maps audio positions to video times.
    pub clock: VideoClock,
    /// The frames.
    pub index: VideoIndex,
    /// Clockwise rotation for display, in degrees.
    pub rotation: u16,
}

/// State shared between a [`VideoLoader`] and its thread.
#[derive(Debug, Default)]
struct Shared {
    /// The file has been opened (successfully or not).
    opened: bool,
    /// The video track, if the file has one.
    track: Option<Arc<VideoTrack>>,
    /// Increases with every new picture or error.
    generation: u64,
    picture: Option<Arc<Image>>,
    /// What to deliver.
    output: Option<Output>,
    error: Option<String>,
}

/// A background thread decoding the pictures of one file's video track
/// on request. Only the latest request matters: requests that pile up
/// while the thread is busy are dropped in favour of the newest one.
#[derive(Debug)]
pub struct VideoLoader {
    requests: Sender<MediaTime>,
    shared: Arc<Mutex<Shared>>,
    /// The picture (presentation time) last requested, to ignore repeats.
    requested: Mutex<Option<MediaTime>>,
}

impl VideoLoader {
    /// Starts a thread that opens `path` (independently of any other
    /// [`Source`] on it) and waits for requests, delivering pictures as
    /// `output` says. Dropping the loader ends the thread after the
    /// picture it is working on.
    pub fn start(path: impl Into<PathBuf>, output: Output) -> Self {
        let path = path.into();
        let (tx, rx) = channel();
        let shared = Arc::new(Mutex::new(Shared {
            output: Some(output),
            ..Shared::default()
        }));
        {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || run(&path, &rx, &shared));
        }
        VideoLoader {
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

    /// Asks for the picture shown at audio position `pos`, unless it is
    /// the one asked for last. Does nothing before the track is known.
    pub fn request(&self, pos: SamplePos) {
        let Some(track) = self.track() else {
            return;
        };
        let Some(t) = picture_time(&track.index, track.clock.time_at(pos)) else {
            return;
        };
        let mut last = self.requested.lock().unwrap_or_else(|e| e.into_inner());
        if *last == Some(t) {
            return;
        }
        *last = Some(t);
        // The thread only ends when the loader is dropped (or the file has
        // no video, when a request is never sent).
        let _ = self.requests.send(t);
    }

    /// Delivers later pictures as `output` says (for example RGB after
    /// the GPU renderer failed). The current picture stays as it is.
    pub fn set_output(&self, output: Output) {
        lock(&self.shared).output = Some(output);
    }

    /// The latest picture if its generation differs from `seen`, with
    /// that generation; errors are reported as text for the status line.
    pub fn poll(&self, seen: Option<u64>) -> Option<(u64, Result<Arc<Image>, String>)> {
        let s = lock(&self.shared);
        if seen == Some(s.generation) || s.generation == 0 {
            return None;
        }
        let r = match (&s.error, &s.picture) {
            (Some(e), _) => Err(e.clone()),
            (None, Some(p)) => Ok(Arc::clone(p)),
            (None, None) => return None,
        };
        Some((s.generation, r))
    }
}

/// Locks `m`, recovering from a poisoned lock (fields are replaced as a
/// whole, so they are never half written).
fn lock(m: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Body of the loader thread.
fn run(path: &Path, rx: &Receiver<MediaTime>, shared: &Mutex<Shared>) {
    let opened = Source::open(path).ok().and_then(|source| {
        let clock = source.video_clock()?;
        let decoder = source.video_decoder()?;
        Some((source, clock, decoder))
    });
    let Some((mut source, clock, mut decoder)) = opened else {
        lock(shared).opened = true;
        return;
    };
    {
        let index = decoder.index().clone();
        let mut s = lock(shared);
        s.track = Some(Arc::new(VideoTrack {
            clock,
            rotation: index.info().rotation,
            index,
        }));
        s.opened = true;
    }
    // `recv` fails once the loader is dropped, which ends the thread.
    while let Ok(mut t) = rx.recv() {
        // Skip to the newest request.
        while let Ok(newer) = rx.try_recv() {
            t = newer;
        }
        let output = lock(shared).output.unwrap_or(Output::Rgb);
        let result = source.video_frame(&mut decoder, t).map(|f| match output {
            Output::Planes if f.gpu_layout().is_some() => Image::Planes(f),
            _ => Image::Rgb(f.to_rgb()),
        });
        let mut s = lock(shared);
        s.generation += 1;
        match result {
            Ok(p) => {
                s.picture = Some(Arc::new(p));
                s.error = None;
            }
            Err(e) => s.error = Some(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lac_core::{VideoCodec, VideoFrame, VideoStreamInfo};
    use std::time::{Duration, Instant};

    /// 10 frames of 3000 ticks at 90 kHz (30 fps), in display order.
    fn index() -> VideoIndex {
        let frames = (0..10)
            .map(|i| VideoFrame {
                byte_offset: i * 10,
                byte_len: 10,
                duration: 3000,
                composition_offset: 0,
                sync: i == 0,
            })
            .collect();
        VideoIndex::new(
            VideoStreamInfo {
                codec: VideoCodec::H264,
                width: 4,
                height: 2,
                timescale: 90_000,
                rotation: 0,
            },
            frames,
        )
        .expect("valid")
    }

    #[test]
    fn clock_maps_positions_into_the_picture_and_back() {
        // 48 kHz audio; the video starts 1500 ticks (half a frame) into
        // its media.
        let c = VideoClock::new(48_000, 90_000, 1500);
        assert_eq!(c.time_at(0), 1500);
        assert_eq!(c.time_at(1600), 4500);
        // Picture 1 starts at 3000 ticks: 800 samples in.
        assert_eq!(c.position_of(3000), 800);
        assert_eq!(c.time_at(c.position_of(3000)), 3000);
        // Rounding up keeps the position inside the picture.
        let c = VideoClock::new(44_100, 90_000, 0);
        for t in [3000, 3003, 6006, 90_000] {
            let p = c.position_of(t);
            assert!(c.time_at(p) >= t && c.time_at(p) < t + 3000, "{t}");
            assert!(c.time_at(p - 1) < t, "{t}: the first such position");
        }
        assert_eq!(c.position_of(-5000), 0);
    }

    #[test]
    fn picture_times_clamp_to_the_track() {
        let i = index();
        assert_eq!(picture_time(&i, -1), Some(0));
        assert_eq!(picture_time(&i, 2999), Some(0));
        assert_eq!(picture_time(&i, 3000), Some(3000));
        assert_eq!(picture_time(&i, 1_000_000), Some(27_000));
    }

    #[test]
    fn stepping_goes_from_picture_to_picture() {
        let i = index();
        let c = VideoClock::new(48_000, 90_000, 0);
        // Pictures start every 1600 samples.
        assert_eq!(step(&i, &c, 0, 100_000, true), Some(1600));
        assert_eq!(step(&i, &c, 1000, 100_000, true), Some(1600));
        assert_eq!(step(&i, &c, 1600, 100_000, true), Some(3200));
        // Back: to the start of the picture, then to the one before.
        assert_eq!(step(&i, &c, 2000, 100_000, false), Some(1600));
        assert_eq!(step(&i, &c, 1600, 100_000, false), Some(0));
        assert_eq!(step(&i, &c, 0, 100_000, false), None);
        // The last picture starts at 14400; after it there is none.
        assert_eq!(step(&i, &c, 14_400, 100_000, true), None);
        // Nor beyond the audio's end.
        assert_eq!(step(&i, &c, 1600, 2000, true), None);
        // From after the video, back to its last picture.
        assert_eq!(step(&i, &c, 50_000, 100_000, false), Some(14_400));
    }

    /// A 2x1 picture: red, then green.
    fn red_green() -> Picture {
        Picture {
            time: 0,
            width: 2,
            height: 1,
            rgb: vec![255, 0, 0, 0, 255, 0],
        }
    }

    fn pixels(f: &Fitted) -> Vec<[u8; 3]> {
        f.rgba.chunks(4).map(|p| [p[0], p[1], p[2]]).collect()
    }

    const RED: [u8; 3] = [255, 0, 0];
    const GREEN: [u8; 3] = [0, 255, 0];

    #[test]
    fn fitting_keeps_the_aspect_and_centres() {
        let f = fit(&red_green(), 0, 10, 10).expect("fits");
        assert_eq!((f.x, f.y, f.width, f.height), (0, 2, 10, 5));
        assert_eq!(pixels(&f)[0], RED);
        assert_eq!(pixels(&f)[9], GREEN);
        let f = fit(&red_green(), 0, 100, 10).expect("fits");
        assert_eq!((f.x, f.y, f.width, f.height), (40, 0, 20, 10));
        assert!(fit(&red_green(), 0, 0, 10).is_none());
    }

    #[test]
    fn rotation_turns_clockwise() {
        // Turned by 90 degrees clockwise, the left (red) pixel is on top.
        let f = fit(&red_green(), 90, 1, 2).expect("fits");
        assert_eq!((f.width, f.height), (1, 2));
        assert_eq!(pixels(&f), [RED, GREEN]);
        let f = fit(&red_green(), 270, 1, 2).expect("fits");
        assert_eq!(pixels(&f), [GREEN, RED]);
        let f = fit(&red_green(), 180, 2, 1).expect("fits");
        assert_eq!(pixels(&f), [GREEN, RED]);
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
    fn loader_delivers_the_picture_at_a_position() {
        let l = VideoLoader::start(fixture("video_h264_opengop_rot90.mp4"), Output::Rgb);
        let track = wait(|| l.track());
        assert_eq!(track.rotation, 270);
        assert!(!l.has_no_video());
        assert!(l.poll(None).is_none(), "nothing before a request");
        // 0.5 s into the 48 kHz audio.
        l.request(24_000);
        let (g, p) = wait(|| l.poll(None));
        let p = p.expect("decodes");
        let want = picture_time(&track.index, track.clock.time_at(24_000));
        assert!(matches!(*p, Image::Rgb(_)));
        assert_eq!(Some(p.time()), want);
        assert_eq!(p.size(), (64, 64));
        // The same picture again starts nothing new.
        l.request(24_001);
        std::thread::sleep(Duration::from_millis(50));
        assert!(l.poll(Some(g)).is_none());
    }

    #[test]
    fn audio_files_have_no_video() {
        let l = VideoLoader::start(fixture("mp3_vbr_44k.mp3"), Output::Planes);
        wait(|| l.has_no_video().then_some(()));
        assert!(l.track().is_none());
        l.request(1000);
        assert!(l.poll(None).is_none());
    }

    #[test]
    fn loader_delivers_planes_for_the_gpu_and_switches_back() {
        let l = VideoLoader::start(fixture("video_h264_opengop_rot90.mp4"), Output::Planes);
        wait(|| l.track());
        l.request(0);
        let (g, p) = wait(|| l.poll(None));
        let p = p.expect("decodes");
        let Image::Planes(f) = &*p else {
            panic!("expected planes");
        };
        assert!(f.gpu_layout().is_some());
        let rgb = p.to_rgb();
        assert_eq!((rgb.width, rgb.height, rgb.time), (64, 64, p.time()));
        // After the GPU renderer gave up: RGB again.
        l.set_output(Output::Rgb);
        l.request(24_000);
        let (_, p) = wait(|| l.poll(Some(g)));
        assert!(matches!(*p.expect("decodes"), Image::Rgb(_)));
    }

    #[test]
    fn fit_rect_matches_fit() {
        // 3 x 2 picture in panes of different shapes and every rotation.
        let pic = Picture {
            time: 0,
            width: 3,
            height: 2,
            rgb: vec![0; 18],
        };
        for rotation in [0, 90, 180, 270] {
            for (w, h) in [(3, 2), (2, 3), (30, 7), (7, 30), (1, 1)] {
                let f = fit(&pic, rotation, w, h).expect("fits");
                assert_eq!(
                    fit_rect(3, 2, rotation, w, h),
                    Some((f.x, f.y, f.width, f.height))
                );
            }
        }
        assert_eq!(fit_rect(3, 2, 0, 0, 5), None);
        assert_eq!(fit_rect(0, 2, 0, 5, 5), None);
    }

    #[test]
    fn source_point_matches_the_rotation_of_fit() {
        // A 3 x 2 picture with a different value in every pixel, shown
        // unscaled: the centre of each displayed pixel must map to the
        // centre of the source pixel `fit` takes.
        let pic = Picture {
            time: 0,
            width: 3,
            height: 2,
            rgb: (0..6).flat_map(|i| [i * 40, 0, 0]).collect(),
        };
        for rotation in [0u16, 90, 180, 270] {
            let (dw, dh) = if rotation % 180 == 90 { (2, 3) } else { (3, 2) };
            let f = fit(&pic, rotation, dw, dh).expect("fits");
            for y in 0..dh {
                for x in 0..dw {
                    let u = (x as f32 + 0.5) / dw as f32;
                    let v = (y as f32 + 0.5) / dh as f32;
                    let (su, sv) = source_point(rotation, u, v);
                    let sx = (su * 3.0) as usize;
                    let sy = (sv * 2.0) as usize;
                    let want = pic.rgb[(sy * 3 + sx) * 3];
                    let got = f.rgba[((y * dw + x) * 4) as usize];
                    assert_eq!(got, want, "rotation {rotation} at {x},{y}");
                }
            }
        }
    }
}
