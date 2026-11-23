//! Measures how fast the video preview can show a file, to tell which
//! part is slow when playback stutters:
//!
//! ```sh
//! cargo run --release -p lac-app --example video_speed -- phone.mp4
//! ```
//!
//! It prints how many pictures a second the decoder delivers (as the
//! preview's playback decoder, on all cores), how long the conversion to
//! RGB takes (only the bitmap way of drawing needs it; OpenGL and Metal
//! convert on the GPU), and how many pictures the background loader
//! delivers while a simulated playhead runs in real time for five
//! seconds, for both ways of drawing. A debug build is much slower.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use lac_app::Source;
use lac_app::video::{Output, VideoLoader};

fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .context("usage: video_speed <video file>")?;
    let mut source = Source::open(&path)?;
    let mut decoder = source
        .video_decoder()
        .context("the file has no video track")?
        .for_playback();
    let index = decoder.index().clone();
    let times: Vec<_> = index
        .display_order()
        .iter()
        .map(|&k| index.presentation_time(k))
        .collect();
    let info = index.info();
    println!(
        "{} frames of {}x{} {}",
        times.len(),
        info.width,
        info.height,
        info.codec.name()
    );

    let (mut decoding, mut converting) = (Duration::ZERO, Duration::ZERO);
    let mut format = String::new();
    for &t in &times {
        let start = Instant::now();
        let frame = source.video_frame(&mut decoder, t)?;
        decoding += start.elapsed();
        let start = Instant::now();
        std::hint::black_box(frame.to_rgb());
        converting += start.elapsed();
        format.clone_from(&frame.layout.name);
    }
    let n = times.len().max(1) as f64;
    println!(
        "decoding: {:.1} pictures/s ({format}); RGB conversion: {:.1} ms per picture",
        n / decoding.as_secs_f64(),
        converting.as_secs_f64() * 1000.0 / n
    );

    let rate = f64::from(source.info().sample_rate);
    for (output, name) in [(Output::Planes, "GPU"), (Output::Rgb, "bitmaps")] {
        let delivered = play(&path, rate, output, Duration::from_secs(5));
        println!("playback drawing with {name}: {delivered:.1} pictures/s");
    }
    Ok(())
}

/// Runs a playhead (`rate` audio samples a second) through the start of
/// `path` in real time for
/// `length`, asking a [`VideoLoader`] for the picture at it as the
/// editor's timer does, and returns the pictures delivered per second.
fn play(path: &std::ffi::OsStr, rate: f64, output: Output, length: Duration) -> f64 {
    let loader = VideoLoader::start(path, output);
    while loader.track().is_none() && !loader.has_no_video() {
        std::thread::sleep(Duration::from_millis(1));
    }
    let start = Instant::now();
    let (mut seen, mut delivered) = (None, 0u32);
    while start.elapsed() < length {
        loader.request((start.elapsed().as_secs_f64() * rate) as u64);
        if let Some((generation, _)) = loader.poll(seen) {
            seen = Some(generation);
            delivered += 1;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    f64::from(delivered) / length.as_secs_f64()
}
