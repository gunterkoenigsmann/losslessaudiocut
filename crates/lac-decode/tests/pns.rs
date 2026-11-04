//! What a cut changes in an AAC stream with perceptual noise substitution
//! (PNS, ISO/IEC 14496-3 4.6.13), checked in the decoder's own transform
//! domain (issue #72).
//!
//! A PNS band carries only its energy; the decoder fills it with noise
//! from a random generator and scales that noise to the energy. libavcodec
//! runs the generator from the start of the stream, so after a cut the
//! noise differs. The decision in #72: tell the user, and test everything
//! else as strictly as possible. That is what this test does:
//!
//! The decoder's output is the overlap-add of windowed inverse MDCTs
//! (4.6.11). The forward MDCT with the same window recovers each frame's
//! spectrum from the output, because the transforms of neighbouring
//! frames are orthogonal to it (time-domain aliasing cancellation). So for
//! every long-window frame of a cut, its spectrum must equal the source's
//! frame spectrum:
//!
//! - exactly (to `f32` rounding) in every band that is not noise, and
//! - in each noise band, with the same energy (but other values).
//!
//! The fixture is mono and encoded without temporal noise shaping, whose
//! filter would spread each noise band's difference across the spectrum.

use std::f64::consts::PI;
use std::io::Cursor;
use std::path::Path;

use lac_container::mp4::Mp4File;
use lac_container::mp4::aac::{self, ChannelInfo, NOISE_HCB, window};
use lac_core::{CutPlan, plan_cuts};
use lac_decode::decode_to_vec;

/// Spectral lines of a long window (half the window length).
const LINES: usize = 1024;

/// Scale factor band offsets of long windows at 44.1 and 48 kHz (ISO/IEC
/// 14496-3 Table 4.129): band `b` covers lines `SWB[b]..SWB[b + 1]`.
const SWB_LONG_44K: [usize; 50] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 48, 56, 64, 72, 80, 88, 96, 108, 120, 132, 144, 160,
    176, 196, 216, 240, 264, 292, 320, 352, 384, 416, 448, 480, 512, 544, 576, 608, 640, 672, 704,
    736, 768, 800, 832, 864, 896, 928, 1024,
];

fn fixture(name: &str) -> Vec<u8> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../lac-container/tests/fixtures")
        .join(name);
    std::fs::read(p).expect("fixture")
}

/// The modified Bessel function of the first kind, order 0 (power series).
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..50 {
        term *= (x / 2.0) * (x / 2.0) / f64::from(k * k);
        sum += term;
    }
    sum
}

/// One half (rising, `LINES` values) of a long window: sine (shape 0) or
/// Kaiser-Bessel derived with alpha 4 (shape 1), ISO/IEC 14496-3 4.6.11.3.2.
fn rising_half(shape: u8) -> Vec<f64> {
    let n = 2 * LINES;
    if shape == 0 {
        return (0..LINES)
            .map(|i| (PI / n as f64 * (i as f64 + 0.5)).sin())
            .collect();
    }
    let alpha = 4.0;
    let kernel: Vec<f64> = (0..=LINES)
        .map(|i| {
            let t = (i as f64 - n as f64 / 4.0) / (n as f64 / 4.0);
            bessel_i0(PI * alpha * (1.0 - t * t).max(0.0).sqrt())
        })
        .collect();
    let total: f64 = kernel.iter().sum();
    let mut acc = 0.0;
    (0..LINES)
        .map(|i| {
            acc += kernel[i];
            (acc / total).sqrt()
        })
        .collect()
}

/// The forward MDCT of the `2 * LINES` samples `x`, windowed with the
/// left half of shape `left` and the right half of shape `right`.
fn mdct(x: &[f64], left: u8, right: u8) -> Vec<f64> {
    let n = 2 * LINES;
    let (l, r) = (rising_half(left), rising_half(right));
    let w: Vec<f64> = (0..n)
        .map(|i| {
            if i < LINES {
                l[i] * x[i]
            } else {
                r[n - 1 - i] * x[i]
            }
        })
        .collect();
    // 4.6.11.3.1: n0 = (N / 2 + 1) / 2.
    let n0 = (n as f64 / 2.0 + 1.0) / 2.0;
    (0..LINES)
        .map(|k| {
            w.iter()
                .enumerate()
                .map(|(i, v)| v * (2.0 * PI / n as f64 * (i as f64 + n0) * (k as f64 + 0.5)).cos())
                .sum()
        })
        .collect()
}

/// A decoded file with what the test needs per index entry.
struct Decoded {
    pcm: Vec<f64>,
    /// Per entry: payload, timeline position of its decoder output, and
    /// the first channel's layout.
    frames: Vec<(Vec<u8>, i64, Option<ChannelInfo>)>,
}

fn decode(bytes: &[u8]) -> Decoded {
    let f = Mp4File::open(&mut Cursor::new(bytes)).expect("valid MP4");
    assert_eq!(f.index().info().channels, 1, "the test needs a mono stream");
    let frames = f
        .index()
        .packets()
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let payload = f.read_packet(&mut Cursor::new(bytes), i).expect("packet");
            let info = aac::first_channel(&payload);
            let start = p.first_sample as i64 - i64::from(p.hidden_before);
            (payload, start, info)
        })
        .collect();
    let c = lac_container::open(&mut Cursor::new(bytes)).expect("opens");
    let (pcm, report) = decode_to_vec(c.as_ref(), &mut Cursor::new(bytes)).expect("decodes");
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    Decoded {
        pcm: pcm.into_iter().map(f64::from).collect(),
        frames,
    }
}

/// The spectrum of entry `i` recovered from the decoded output, or `None`
/// where that is not possible: not a long window, no predecessor whose
/// window shape is known, or its window reaches beyond the timeline.
fn spectrum(d: &Decoded, i: usize) -> Option<(Vec<f64>, &ChannelInfo)> {
    let (_, start, Some(info)) = d.frames.get(i)? else {
        return None;
    };
    let (_, prev_start, Some(prev)) = d.frames.get(i.checked_sub(1)?)? else {
        return None;
    };
    if info.window_sequence != window::ONLY_LONG || *prev_start + LINES as i64 != *start {
        return None;
    }
    // Frame i's window spans its own output and the next frame's.
    let from = usize::try_from(*start).ok()?;
    let x = d.pcm.get(from..from + 2 * LINES)?;
    Some((mdct(x, prev.window_shape, info.window_shape), info))
}

#[test]
fn a_cut_changes_only_the_noise_and_keeps_its_energy() {
    let name = "mp4_aac_pns_mono_44k.m4a";
    let src = fixture(name);
    let f = Mp4File::open(&mut Cursor::new(&src)).expect("valid");
    assert_eq!(f.index().info().sample_rate, 44_100, "SWB_LONG_44K");
    let len = f.index().total_samples();
    let kept = len / 5..len * 4 / 5;
    let plans: Vec<CutPlan> =
        plan_cuts(f.index(), f.capabilities(), std::slice::from_ref(&kept)).expect("plan");
    let mut out = Cursor::new(Vec::new());
    f.write_cut(&mut Cursor::new(&src), &plans, None, &mut out)
        .expect("write");
    let out = out.into_inner();

    let s = decode(&src);
    let c = decode(&out);
    // The cut's frames are a run of the source's: find where it starts.
    let payloads = |d: &Decoded| d.frames.iter().map(|f| f.0.clone()).collect::<Vec<_>>();
    let (sp, cp) = (payloads(&s), payloads(&c));
    let offset = (0..=sp.len() - cp.len())
        .find(|&o| sp[o..o + cp.len()] == cp[..])
        .expect("the cut's frames are the source's");

    let mut checked = 0;
    let mut noise_bands = 0;
    let mut noise_differs = false;
    for j in 1..cp.len() {
        let (Some((xc, info)), Some((xs, _))) = (spectrum(&c, j), spectrum(&s, offset + j)) else {
            continue;
        };
        checked += 1;
        let peak = xs.iter().fold(0f64, |m, v| m.max(v.abs())).max(1e-9);
        let bands = &info.codebooks[0];
        for b in 0..SWB_LONG_44K.len() - 1 {
            let lines = SWB_LONG_44K[b]..SWB_LONG_44K[b + 1];
            if bands.get(b) == Some(&NOISE_HCB) {
                noise_bands += 1;
                let energy = |x: &[f64]| lines.clone().map(|k| x[k] * x[k]).sum::<f64>();
                let (ec, es) = (energy(&xc), energy(&xs));
                assert!(
                    (ec - es).abs() <= 1e-3 * es + 1e-9 * peak * peak,
                    "{name}: frame {j}, noise band {b}: energy {ec} vs {es}"
                );
                noise_differs |= lines.clone().any(|k| (xc[k] - xs[k]).abs() > 1e-3 * peak);
            } else {
                for k in lines {
                    assert!(
                        (xc[k] - xs[k]).abs() <= 1e-5 * peak,
                        "{name}: frame {j}, band {b}, line {k}: {} vs {}",
                        xc[k],
                        xs[k]
                    );
                }
            }
        }
    }
    // The check must have something to check.
    assert!(checked >= 10, "{name}: only {checked} long frames checked");
    assert!(noise_bands >= 10, "{name}: only {noise_bands} noise bands");
    assert!(
        noise_differs,
        "{name}: the noise did not change; is PNS used?"
    );
}
