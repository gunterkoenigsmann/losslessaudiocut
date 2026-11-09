//! Turning [`Features`] into suggestions: regions without voice or music,
//! and track starts.
//!
//! ## Frame classes
//!
//! Every frame is one of
//!
//! - **quiet**: its level is at most [`Thresholds::quiet_db`],
//! - **noise**: louder, but at most [`DetectParams::noise_band_db`] above
//!   the quiet threshold, and either spectrally flat (smoothed flatness at
//!   least [`DetectParams::noise_flatness`]: hiss, wind, crowd murmur,
//!   traffic) or steady (smoothed spectral flux below
//!   [`DetectParams::steady_flux_db`]: mains hum, a fan, a fridge),
//! - **content**: everything else (voice, music, and loud noise), and noise
//!   frames that rise as a burst above their surroundings (see below).
//!
//! The quiet threshold adapts to the recording: it lies a fraction of the
//! way from the noise floor (a low percentile of the frame levels) to the
//! loud level (a high percentile), at least [`DetectParams::min_margin_db`]
//! above the floor, and never above [`DetectParams::max_quiet_db`] so that
//! a recording without any pause does not get its quiet passages removed.
//! Flatness is what keeps a noisy pause (a live recording between songs,
//! a phone in the wind) from counting as content although it is loud.
//! Hum is the opposite of flat (a few harmonics of 50 or 60 Hz), but it
//! never changes, while voice and music change their spectrum several
//! times a second: the *spectral flux* (how much the band spectrum moves
//! from frame to frame, weighted by where the energy is) tells them apart.
//!
//! Some voice is noise-like too: whispers and unvoiced sounds have a flat
//! spectrum. What gives them away is their rhythm: syllables come in
//! bursts, about four per second, with dips in between. A noise frame is
//! therefore content when its level is at least
//! [`DetectParams::burst_rise_db`] above the lowest level within
//! [`DetectParams::burst_window_secs`] before it *and* the lowest level
//! within that time after it. This is the local signal-to-noise test of
//! energy-based voice activity detectors; requiring a dip on both sides
//! keeps a pause that simply gets louder (applause starting) a pause.
//!
//! ## Regions and splits
//!
//! Short pauses inside content ([`DetectParams::min_pause_secs`], breathing,
//! a beat of rest) are closed, then short content blips
//! ([`DetectParams::min_content_secs`], clicks) are removed. Every remaining
//! run of non-content frames is a [`Region`] that can be deleted.
//!
//! A track start is suggested
//!
//! - after every region inside the recording that lasts at least
//!   [`DetectParams::min_track_gap_secs`], [`DetectParams::lead_in_secs`]
//!   before the content resumes (so a fade-in or an attack is not cut),
//!   provided the [`DetectParams::min_track_secs`] before the pause and after
//!   it are at least [`DetectParams::min_track_content`] content (rain with
//!   a few loud drops is no sequence of tracks),
//! - where the band spectrum averaged over [`DetectParams::novelty_window_secs`]
//!   before a frame differs by at least [`DetectParams::min_novelty_db`] from
//!   the one after it, with both windows nearly all content: the next piece
//!   of a gapless recording (a DJ mix, a live album). This is a simple form
//!   of Foote's checkerboard novelty on the band spectrum. Verse and chorus
//!   of one song differ as much, but they come back: the change only counts
//!   when the window after it also differs from every window in the
//!   [`DetectParams::recurrence_secs`] before it, and the window before it
//!   from every window in that time after it.
//!
//! Of two suggestions closer than [`DetectParams::min_track_secs`] only the
//! more confident one is kept.
//!
//! The defaults were tuned on about 30 h of albums glued together (their
//! track boundaries known) and 15 h of talks, concerts and rain; the
//! method and the numbers are in `docs/architecture.md`, section 8.

use std::ops::Range;

use lac_core::SamplePos;

use crate::features::{BANDS, FLOOR_DB, Features};

/// Threshold for [`DetectParams::min_key_distance`] when the key detector
/// is turned on: the same melody on both sides stays below 0.08, a fifth
/// apart gives about 0.3.
pub const KEY_DISTANCE: f32 = 0.25;

/// Tuning of [`detect`]. The defaults are meant for speech and music
/// recordings; every field is in seconds or dB so it can be shown in a
/// settings dialog.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectParams {
    /// Percentile (0..1) of the frame levels taken as the noise floor.
    /// Frames below −90 dBFS (digital silence) are left out.
    pub floor_percentile: f32,
    /// Percentile (0..1) of the frame levels taken as the loud level.
    pub loud_percentile: f32,
    /// The quiet threshold lies this fraction of the way from the floor to
    /// the loud level ...
    pub relative_margin: f32,
    /// ... but at least this far above the floor (dB) ...
    pub min_margin_db: f32,
    /// ... and never above this level (dBFS). Quiet passages of music
    /// reach −40 dBFS, so a recording without pauses must not get its
    /// threshold there; pauses in a noisy recording louder than this are
    /// found by their flat spectrum instead (see [`FrameClass::Noise`]).
    pub max_quiet_db: f32,
    /// How far above the quiet threshold (dB) a flat frame still counts as
    /// noise rather than content.
    pub noise_band_db: f32,
    /// Smoothed spectral flatness from which a frame is noise-like.
    pub noise_flatness: f32,
    /// Length of the moving average over the flatness (seconds); a single
    /// frame's flatness fluctuates, and fricatives ("s", "f") are briefly
    /// noise-like inside speech.
    pub flatness_smoothing_secs: f32,
    /// Smoothed spectral flux (mean change of the band spectrum per frame
    /// in dB, weighted by band energy) below which a frame is steady, and
    /// noise if not loud.
    pub steady_flux_db: f32,
    /// Length (seconds) of the averages over the spectral flux: a frame is
    /// steady if the flux averaged over this long before it, or after it,
    /// is low. Long enough to span several notes or syllables, so a held
    /// note does not count as steady.
    pub flux_smoothing_secs: f32,
    /// How far (dB) a noise frame must rise above the quietest frame
    /// before and after it to be a burst (a syllable), and thus content.
    /// `f32::INFINITY` turns this off.
    pub burst_rise_db: f32,
    /// How far (seconds) to look before and after a frame for the dips
    /// around a burst.
    pub burst_window_secs: f32,
    /// Pauses shorter than this (seconds) count as content.
    pub min_pause_secs: f32,
    /// Content shorter than this (seconds) between pauses counts as pause.
    pub min_content_secs: f32,
    /// Pauses at least this long (seconds) suggest a new track after them.
    pub min_track_gap_secs: f32,
    /// A suggested track start lies this long (seconds) before the content
    /// after the pause, but never before the pause starts.
    pub lead_in_secs: f32,
    /// Length (seconds) of each of the two windows compared for a change
    /// of timbre; also the minimum distance of two suggested splits.
    pub novelty_window_secs: f32,
    /// Change of the average band spectrum (dB, see [`SplitReason`]) from
    /// which a timbre change suggests a new track. `f32::INFINITY` turns
    /// this detector off.
    pub min_novelty_db: f32,
    /// Cosine distance (0..1) of the mean chroma (pitch-class profile)
    /// before and after a frame from which a change of key or harmony
    /// suggests a new track, with the same windows as the timbre. Finds
    /// the next piece when the instruments stay the same. `f32::INFINITY`
    /// (the default) turns this detector off: on a corpus of real albums
    /// it found no track start the timbre missed but added false ones
    /// inside songs; [`KEY_DISTANCE`] is the threshold to turn it on with.
    pub min_key_distance: f32,
    /// How far (seconds) before and after a timbre or key change to look
    /// for the same sound again. Sections of a song come back (the chorus
    /// after the verse), a new piece does not repeat the last one: a
    /// change only counts when the part after it differs from every
    /// window in this time before it, and the part before it from every
    /// window in this time after it. 0 compares only the two neighbouring
    /// windows.
    pub recurrence_secs: f32,
    /// Shortest track (seconds): of two suggested track starts closer than
    /// this, only the more confident one is kept. Keeps a talk with long
    /// pauses or rain with a few loud drops from turning into dozens of
    /// tracks.
    pub min_track_secs: f32,
    /// Share (0..1) of content frames a pause needs in the
    /// [`DetectParams::min_track_secs`] before and after it to suggest a
    /// track start: a track is mostly voice or music. 0 turns this off.
    pub min_track_content: f32,
}

impl Default for DetectParams {
    fn default() -> Self {
        DetectParams {
            floor_percentile: 0.10,
            loud_percentile: 0.95,
            relative_margin: 0.25,
            min_margin_db: 6.0,
            max_quiet_db: -45.0,
            noise_band_db: 25.0,
            noise_flatness: 0.3,
            flatness_smoothing_secs: 0.25,
            steady_flux_db: 1.0,
            flux_smoothing_secs: 1.5,
            burst_rise_db: 9.0,
            burst_window_secs: 1.0,
            min_pause_secs: 0.5,
            min_content_secs: 0.2,
            min_track_gap_secs: 2.0,
            lead_in_secs: 0.25,
            novelty_window_secs: 4.0,
            min_novelty_db: 6.0,
            min_key_distance: f32::INFINITY,
            recurrence_secs: 60.0,
            min_track_secs: 30.0,
            min_track_content: 0.7,
        }
    }
}

/// What a frame contains, see the module documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameClass {
    /// Voice, music or loud sound.
    Content,
    /// Below the quiet threshold.
    Quiet,
    /// Moderately loud, spectrally flat sound.
    Noise,
}

/// The levels [`classify`] derived from a recording.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// Estimated noise floor (dBFS).
    pub floor_db: f32,
    /// Estimated level of loud content (dBFS).
    pub loud_db: f32,
    /// Frames at or below this level are quiet (dBFS).
    pub quiet_db: f32,
}

/// Why a [`Region`] can be deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionKind {
    /// Mostly quiet frames.
    Silence,
    /// Mostly noise frames.
    Noise,
}

/// A stretch without voice or music.
#[derive(Debug, Clone, PartialEq)]
pub struct Region {
    /// Source sample frames, half-open.
    pub range: Range<SamplePos>,
    /// Silence or noise.
    pub kind: RegionKind,
    /// Mean level of the region's frames (dBFS), for display.
    pub level_db: f32,
}

/// Why a track start is suggested.
#[derive(Debug, Clone, PartialEq)]
pub enum SplitReason {
    /// After a pause (the region before the suggestion).
    Gap {
        /// The pause.
        gap: Range<SamplePos>,
    },
    /// A change of timbre without a pause: `novelty_db` is the distance of
    /// the mean band spectra before and after, the standard deviation of
    /// their per-band difference plus half its mean (so a mere change of
    /// loudness counts half).
    TimbreChange {
        /// The distance in dB.
        novelty_db: f32,
    },
    /// A change of key or harmony without a pause: `distance` is the
    /// cosine distance (0..1) of the mean chroma before and after.
    KeyChange {
        /// The distance.
        distance: f32,
    },
}

/// A suggested track start.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitSuggestion {
    /// Where the new track would start (source sample frame, strictly
    /// inside the recording).
    pub pos: SamplePos,
    /// Why.
    pub reason: SplitReason,
    /// 0..1: 0.5 at the configured minimum (gap length or novelty), 1 at
    /// twice of it and above.
    pub confidence: f32,
}

/// The result of [`detect`].
#[derive(Debug, Clone, PartialEq)]
pub struct Detection {
    /// The levels used.
    pub thresholds: Thresholds,
    /// Deletable regions, sorted and disjoint.
    pub regions: Vec<Region>,
    /// Suggested track starts, sorted, all different.
    pub splits: Vec<SplitSuggestion>,
}

/// Value at quantile `q` (0..1) of `v` (sorted in place); `None` if empty.
fn percentile(v: &mut [f32], q: f32) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f32::total_cmp);
    let i = ((v.len() - 1) as f32 * q.clamp(0.0, 1.0)).round() as usize;
    Some(v[i])
}

/// Classifies every frame of `f`, see the module documentation.
pub fn classify(f: &Features, p: &DetectParams) -> (Vec<FrameClass>, Thresholds) {
    let frames = f.frames();
    // Digital silence (and near-silence of fades) would pull the floor
    // estimate to −120 dB and make every bit of background noise content.
    let mut levels: Vec<f32> = frames
        .iter()
        .map(|x| x.level_db)
        .filter(|&l| l > -90.0)
        .collect();
    let floor_db = percentile(&mut levels, p.floor_percentile).unwrap_or(FLOOR_DB);
    let loud_db = percentile(&mut levels, p.loud_percentile).unwrap_or(FLOOR_DB);
    let margin = (p.relative_margin * (loud_db - floor_db)).max(p.min_margin_db);
    let quiet_db = (floor_db + margin).min(p.max_quiet_db);
    let thresholds = Thresholds {
        floor_db,
        loud_db,
        quiet_db,
    };

    let flat = moving_average(
        frames.iter().map(|x| x.flatness),
        f.frames_for_secs(p.flatness_smoothing_secs),
    );
    let flux = one_sided_min_average(
        &spectral_flux(f, quiet_db - INAUDIBLE_BELOW_QUIET_DB),
        f.frames_for_secs(p.flux_smoothing_secs),
    );
    let mut classes: Vec<FrameClass> = frames
        .iter()
        .zip(flat)
        .zip(flux)
        .map(|((x, flat), flux)| {
            if x.level_db <= quiet_db {
                FrameClass::Quiet
            } else if x.level_db <= quiet_db + p.noise_band_db
                && (flat >= p.noise_flatness || flux < p.steady_flux_db)
            {
                FrameClass::Noise
            } else {
                FrameClass::Content
            }
        })
        .collect();
    rescue_bursts(f, p, &mut classes);
    (classes, thresholds)
}

/// Turns noise frames that rise as a burst above the level before and
/// after them into content, see the module documentation.
fn rescue_bursts(f: &Features, p: &DetectParams, classes: &mut [FrameClass]) {
    if !p.burst_rise_db.is_finite() {
        return;
    }
    let levels: Vec<f32> = f.frames().iter().map(|x| x.level_db).collect();
    let w = f.frames_for_secs(p.burst_window_secs);
    let before = running_min(&levels, w);
    let reversed: Vec<f32> = levels.iter().rev().copied().collect();
    let mut after = running_min(&reversed, w);
    after.reverse();
    for (i, c) in classes.iter_mut().enumerate() {
        if *c == FrameClass::Noise && levels[i] >= before[i].max(after[i]) + p.burst_rise_db {
            *c = FrameClass::Content;
        }
    }
}

/// For every index the minimum of the `len` values ending there (fewer at
/// the start). O(n) with a monotonic queue.
fn running_min(v: &[f32], len: usize) -> Vec<f32> {
    let mut q: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    let mut out = Vec::with_capacity(v.len());
    for (i, &x) in v.iter().enumerate() {
        while q.back().is_some_and(|&j| v[j] >= x) {
            q.pop_back();
        }
        q.push_back(i);
        while q.front().is_some_and(|&j| j + len <= i) {
            q.pop_front();
        }
        // The queue holds `i` at least, so it is never empty here.
        out.push(q.front().map_or(x, |&j| v[j]));
    }
    out
}

/// Spectral flux per frame: the mean absolute change of the band spectrum
/// (dB, bands floored at `band_floor`) from the previous frame, each band
/// weighted by its energy, so that the bands that carry the sound count,
/// not the random fluctuation of near-empty ones. The first frame gets the
/// second one's value.
fn spectral_flux(f: &Features, band_floor: f32) -> Vec<f32> {
    let frames = f.frames();
    let bands = |i: usize| -> [f32; BANDS] {
        std::array::from_fn(|b| frames[i].band_db(b).max(band_floor))
    };
    let mut out = vec![0.0; frames.len()];
    for (i, o) in out.iter_mut().enumerate().skip(1) {
        let (a, c) = (bands(i - 1), bands(i));
        let (mut sum, mut weight) = (0f64, 0f64);
        for (x, y) in a.iter().zip(&c) {
            // Energy relative to the floor, so that the floor itself weighs 0.
            let w = 10f64.powf(f64::from(x.max(*y) - band_floor) / 10.0) - 1.0;
            sum += w * f64::from((y - x).abs());
            weight += w;
        }
        *o = if weight > 0.0 {
            (sum / weight) as f32
        } else {
            0.0
        };
    }
    if out.len() > 1 {
        out[0] = out[1];
    }
    out
}

/// For every index the smaller of the averages over the `len` values
/// ending there and the `len` values starting there (shorter at the ends).
/// A steady stretch then reaches up to its edges, while a centred average
/// would blur half a window of the neighbouring content into it.
fn one_sided_min_average(v: &[f32], len: usize) -> Vec<f32> {
    let mut prefix = vec![0f64; v.len() + 1];
    for (i, x) in v.iter().enumerate() {
        prefix[i + 1] = prefix[i] + f64::from(*x);
    }
    let avg = |a: usize, b: usize| ((prefix[b] - prefix[a]) / (b - a) as f64) as f32;
    (0..v.len())
        .map(|i| {
            let back = avg((i + 1).saturating_sub(len), i + 1);
            let fwd = avg(i, (i + len).min(v.len()));
            back.min(fwd)
        })
        .collect()
}

/// Centred moving average over `len` values (shorter at the ends).
fn moving_average(v: impl Iterator<Item = f32>, len: usize) -> Vec<f32> {
    let v: Vec<f64> = v.map(f64::from).collect();
    let mut prefix = vec![0.0; v.len() + 1];
    for (i, x) in v.iter().enumerate() {
        prefix[i + 1] = prefix[i] + x;
    }
    let half = len / 2;
    (0..v.len())
        .map(|i| {
            let a = i.saturating_sub(half);
            let b = (i + len - half).min(v.len());
            ((prefix[b] - prefix[a]) / (b - a) as f64) as f32
        })
        .collect()
}

/// Maximal runs of equal values in `mask`, as (value, frame range).
fn runs(mask: &[bool]) -> Vec<(bool, Range<usize>)> {
    let mut out: Vec<(bool, Range<usize>)> = Vec::new();
    for (i, &m) in mask.iter().enumerate() {
        match out.last_mut() {
            Some((v, r)) if *v == m => r.end = i + 1,
            _ => out.push((m, i..i + 1)),
        }
    }
    out
}

/// Sets every run of `value` shorter than `min` frames that has a
/// neighbour on both sides to `!value`. Runs touching the start or end of
/// the recording stay: a short pause at the very start is still a pause.
fn close_short_runs(mask: &mut [bool], value: bool, min: usize) {
    let r = runs(mask);
    for (k, (v, range)) in r.iter().enumerate() {
        if *v == value && range.len() < min && k > 0 && k + 1 < r.len() {
            mask[range.clone()].fill(!value);
        }
    }
}

/// Finds deletable regions and track starts in `f`.
pub fn detect(f: &Features, p: &DetectParams) -> Detection {
    let (classes, thresholds) = classify(f, p);
    let frames = f.frames();
    let n = frames.len();

    let mut content: Vec<bool> = classes.iter().map(|&c| c == FrameClass::Content).collect();
    close_short_runs(&mut content, false, f.frames_for_secs(p.min_pause_secs));
    close_short_runs(&mut content, true, f.frames_for_secs(p.min_content_secs));

    let mut regions = Vec::new();
    let mut splits: Vec<SplitSuggestion> = Vec::new();
    let min_gap = f.frames_for_secs(p.min_track_gap_secs);
    let track = f.frames_for_secs(p.min_track_secs);
    let mut content_sum = vec![0usize; n + 1];
    for (i, &c) in content.iter().enumerate() {
        content_sum[i + 1] = content_sum[i] + usize::from(c);
    }
    let lead_in = (f64::from(p.lead_in_secs.max(0.0)) * f64::from(f.sample_rate())) as SamplePos;
    for (is_content, r) in runs(&content) {
        if is_content {
            continue;
        }
        let noise = classes[r.clone()]
            .iter()
            .filter(|&&c| c == FrameClass::Noise)
            .count();
        let quiet = classes[r.clone()]
            .iter()
            .filter(|&&c| c == FrameClass::Quiet)
            .count();
        let level = frames[r.clone()]
            .iter()
            .map(|x| f64::from(x.level_db))
            .sum::<f64>()
            / r.len() as f64;
        let range = f.frame_start(r.start)..f.frame_start(r.end);
        regions.push(Region {
            range: range.clone(),
            kind: if noise > quiet {
                RegionKind::Noise
            } else {
                RegionKind::Silence
            },
            level_db: level as f32,
        });
        let interior = r.start > 0 && r.end < n;
        // A track is mostly content: rain with a few loud drops, or a
        // fan with someone briefly talking, is no sequence of tracks.
        let mostly_content = |a: usize, b: usize| {
            b > a
                && (content_sum[b] - content_sum[a]) as f32 >= p.min_track_content * (b - a) as f32
        };
        if interior
            && r.len() >= min_gap
            && mostly_content(r.start.saturating_sub(track), r.start)
            && mostly_content(r.end, (r.end + track).min(n))
        {
            splits.push(SplitSuggestion {
                pos: range.end.saturating_sub(lead_in).max(range.start),
                reason: SplitReason::Gap { gap: range },
                confidence: confidence(r.len() as f32, min_gap as f32),
            });
        }
    }

    if p.min_novelty_db.is_finite() || p.min_key_distance.is_finite() {
        splits.extend(timbre_changes(f, &content, thresholds.quiet_db, p));
    }
    splits.retain(|s| s.pos > 0 && s.pos < f.total_samples());
    let splits = keep_apart(splits, f, p);
    Detection {
        thresholds,
        regions,
        splits,
    }
}

/// Keeps the most confident suggestions at least
/// [`DetectParams::min_track_secs`] (and one novelty window) apart, a pause
/// winning a tie; returns them sorted by position.
fn keep_apart(
    mut splits: Vec<SplitSuggestion>,
    f: &Features,
    p: &DetectParams,
) -> Vec<SplitSuggestion> {
    let secs = p.min_track_secs.max(p.novelty_window_secs).max(0.0);
    let min_dist = (f64::from(secs) * f64::from(f.sample_rate())) as SamplePos;
    let is_gap = |s: &SplitSuggestion| matches!(s.reason, SplitReason::Gap { .. });
    splits.sort_by(|a, b| {
        b.confidence
            .total_cmp(&a.confidence)
            .then(is_gap(b).cmp(&is_gap(a)))
            .then(a.pos.cmp(&b.pos))
    });
    let mut kept: Vec<SplitSuggestion> = Vec::new();
    for s in splits {
        if kept
            .iter()
            .all(|k| k.pos.abs_diff(s.pos) >= min_dist.max(1))
        {
            kept.push(s);
        }
    }
    kept.sort_by_key(|s| s.pos);
    kept
}

/// How far below the quiet threshold (dB) band powers still count for
/// [`SplitReason::TimbreChange`].
const INAUDIBLE_BELOW_QUIET_DB: f32 = 20.0;

/// 0.5 at `min`, rising linearly to 1 at twice of it.
fn confidence(value: f32, min: f32) -> f32 {
    if min <= 0.0 {
        return 1.0;
    }
    (value / min / 2.0).clamp(0.0, 1.0)
}

/// Mean band spectrum (dB) and chroma of the content frames in a span,
/// from prefix sums, so that every window costs O(BANDS + 12).
struct WindowMeans {
    bands: Vec<[f64; BANDS]>,
    chroma: Vec<[f64; 12]>,
    content: Vec<usize>,
    /// Content frames a window needs to be compared at all.
    min_content: usize,
}

/// Mean band spectrum and chroma of one window.
type Means = ([f64; BANDS], [f64; 12]);

impl WindowMeans {
    /// Prefix sums over `f`'s frames; pause frames are left out of the
    /// means (a little noise at the edge of a window must not look like a
    /// change), band values are floored at `band_floor`.
    fn new(f: &Features, content: &[bool], band_floor: f32, min_content: usize) -> Self {
        let n = f.frames().len();
        let mut bands = vec![[0f64; BANDS]; n + 1];
        let mut chroma = vec![[0f64; 12]; n + 1];
        let mut count = vec![0usize; n + 1];
        for (i, fr) in f.frames().iter().enumerate() {
            let weight = if content[i] { 1.0 } else { 0.0 };
            let prev = bands[i];
            bands[i + 1] = std::array::from_fn(|b| {
                prev[b] + weight * f64::from(fr.band_db(b).max(band_floor))
            });
            let prev = chroma[i];
            chroma[i + 1] = std::array::from_fn(|c| prev[c] + weight * f64::from(fr.chroma(c)));
            count[i + 1] = count[i] + usize::from(content[i]);
        }
        WindowMeans {
            bands,
            chroma,
            content: count,
            min_content,
        }
    }

    /// The means of frames `a..b`, or `None` when fewer than
    /// `min_content` of them are content (a pause or a fade).
    fn get(&self, a: usize, b: usize) -> Option<Means> {
        let c = self.content[b] - self.content[a];
        if c < self.min_content.max(1) {
            return None;
        }
        let c = c as f64;
        Some((
            std::array::from_fn(|k| (self.bands[b][k] - self.bands[a][k]) / c),
            std::array::from_fn(|k| (self.chroma[b][k] - self.chroma[a][k]) / c),
        ))
    }
}

/// Timbre distance (dB, see [`SplitReason::TimbreChange`]) and chroma
/// distance (0..1) of two windows.
fn distances(x: &Means, y: &Means) -> (f32, f32) {
    let d: [f64; BANDS] = std::array::from_fn(|b| y.0[b] - x.0[b]);
    let mean = d.iter().sum::<f64>() / BANDS as f64;
    let var = d.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / BANDS as f64;
    let timbre = (var.sqrt() + 0.5 * mean.abs()) as f32;
    (timbre, cosine_distance(&x.1, &y.1))
}

/// Finds [`SplitReason::TimbreChange`] and [`SplitReason::KeyChange`]
/// candidates: peaks of the novelty between the windows before and after
/// a frame, at least one window apart, that survive the recurrence test
/// (see [`DetectParams::recurrence_secs`]).
///
/// Band values are compared only down to [`INAUDIBLE_BELOW_QUIET_DB`] below
/// `quiet_db`: what happens below that (dither, an encoder emptying a band,
/// rounding noise) is no change of timbre anyone hears.
fn timbre_changes(
    f: &Features,
    content: &[bool],
    quiet_db: f32,
    p: &DetectParams,
) -> Vec<SplitSuggestion> {
    let n = f.frames().len();
    let w = f.frames_for_secs(p.novelty_window_secs);
    if n < 2 * w {
        return Vec::new();
    }
    // Both windows must be at least this much content, so that pauses
    // (handled by the gap rule) and fades do not count as a change.
    let min_content = (w as f64 * 0.9).ceil() as usize;
    let means = WindowMeans::new(f, content, quiet_db - INAUDIBLE_BELOW_QUIET_DB, min_content);
    // Each criterion as a multiple of its threshold; NaN (a threshold of
    // infinity divided into 0) counts as 0.
    let ratio = |(timbre, key): (f32, f32)| {
        (
            (timbre / p.min_novelty_db).max(0.0),
            (key / p.min_key_distance).max(0.0),
        )
    };
    // (timbre novelty in dB, chroma distance) at every frame boundary.
    let novelty: Vec<(f32, f32)> = (0..=n)
        .map(|t| {
            if t < w || t + w > n {
                return (0.0, 0.0);
            }
            match (means.get(t - w, t), means.get(t, t + w)) {
                (Some(a), Some(b)) => distances(&a, &b),
                _ => (0.0, 0.0),
            }
        })
        .collect();
    let score = |t: usize| {
        let (a, b) = ratio(novelty[t]);
        a.max(b)
    };
    // Greedy peak picking: strongest first, a window's distance apart.
    let mut candidates: Vec<usize> = (0..=n).filter(|&t| score(t) >= 1.0).collect();
    candidates.sort_by(|&a, &b| score(b).total_cmp(&score(a)).then(a.cmp(&b)));
    let mut peaks: Vec<usize> = Vec::new();
    for t in candidates {
        if peaks.iter().all(|&q| q.abs_diff(t) >= w) {
            peaks.push(t);
        }
    }

    // Recurrence: the smallest distance of the window after `t` to any
    // window in the `l` frames before it, and of the window before `t` to
    // any window in the `l` frames after it.
    let l = if p.recurrence_secs > 0.0 {
        f.frames_for_secs(p.recurrence_secs)
    } else {
        0
    };
    let step = f.frames_for_secs(0.5);
    let mut out = Vec::new();
    for t in peaks {
        let (Some(before), Some(after)) = (means.get(t - w, t), means.get(t, t + w)) else {
            continue;
        };
        let (mut timbre, mut key) = novelty[t];
        // Windows ending at e, from t − l + w up to t − w (the one ending
        // at t is the neighbour already compared).
        let back = (t.saturating_sub(l)..=t.saturating_sub(w)).step_by(step);
        let earlier = back.filter(|&e| e >= w && e + w <= t).map(|e| (e - w, e));
        // Windows starting at s, from t + w on.
        let fwd = (t + w..=(t + l).min(n)).step_by(step);
        let later = fwd
            .filter(|&s| s + w <= n && s + w <= t + l)
            .map(|s| (s, s + w));
        for ((a, b), other) in earlier
            .map(|r| (r, &after))
            .chain(later.map(|r| (r, &before)))
        {
            if let Some(m) = means.get(a, b) {
                let (dt, dk) = distances(&m, other);
                timbre = timbre.min(dt);
                key = key.min(dk);
            }
        }
        let (rt, rk) = ratio((timbre, key));
        if rt.max(rk) < 1.0 {
            continue;
        }
        let reason = if rt >= rk {
            SplitReason::TimbreChange { novelty_db: timbre }
        } else {
            SplitReason::KeyChange { distance: key }
        };
        out.push(SplitSuggestion {
            pos: f.frame_start(t),
            reason,
            confidence: confidence(rt.max(rk), 1.0),
        });
    }
    out
}

/// `1 − cos` of the angle between `a` and `b`: 0 for the same direction,
/// 1 for orthogonal (non-negative vectors never get further). 0 if either
/// is zero.
fn cosine_distance(a: &[f64; 12], b: &[f64; 12]) -> f32 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f64>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (1.0 - dot / (na * nb)).clamp(0.0, 1.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FeatureExtractor;
    use crate::features::tests::{noise, tone};

    const RATE: u32 = 16000;

    /// Makes `n` samples of one part of a test recording.
    type Generator<'a> = &'a dyn Fn(usize) -> Vec<f32>;

    /// Builds a recording from `(seconds, generator)` parts; every part
    /// gets a −60 dBFS noise floor added, as a real microphone would.
    fn recording(parts: &[(f32, Generator)]) -> Vec<f32> {
        let mut out = Vec::new();
        for (secs, g) in parts {
            out.extend(g((secs * RATE as f32) as usize));
        }
        let floor = noise(out.len(), 0.0017, 99); // RMS ≈ −60 dB
        out.iter().zip(floor).map(|(a, b)| a + b).collect()
    }

    fn analyse(s: &[f32]) -> Features {
        let mut x = FeatureExtractor::new(RATE, 1);
        x.push(s);
        x.finish()
    }

    fn secs(pos: SamplePos) -> f32 {
        pos as f32 / RATE as f32
    }

    /// A melody: notes of 0.3 s on a scale starting at `f0`, each decaying
    /// like a plucked string, peak amplitude about 0.4.
    fn music(f0: f32) -> impl Fn(usize) -> Vec<f32> {
        melody(f0, 0.4)
    }

    fn melody(f0: f32, amplitude: f32) -> impl Fn(usize) -> Vec<f32> {
        const STEPS: [f32; 8] = [1.0, 1.122, 1.26, 1.335, 1.498, 1.335, 1.26, 1.122];
        move |n| {
            let note = (0.3 * RATE as f32) as usize;
            (0..n)
                .step_by(note)
                .enumerate()
                .flat_map(|(k, start)| {
                    let len = note.min(n - start);
                    let t = tone(len, RATE, f0 * STEPS[k % 8], 10, amplitude);
                    t.into_iter()
                        .enumerate()
                        .map(|(i, x)| x * (-3.0 * i as f32 / RATE as f32).exp())
                })
                .collect()
        }
    }

    fn silence(n: usize) -> Vec<f32> {
        vec![0.0; n]
    }

    #[test]
    fn pauses_between_pieces_become_regions_and_track_starts() {
        let a = music(220.0);
        let b = music(330.0);
        let s = recording(&[
            (1.0, &silence),
            (6.0, &a),
            (3.0, &silence),
            (6.0, &b),
            (1.0, &silence),
        ]);
        let d = detect(&analyse(&s), &DetectParams::default());
        let r: Vec<(f32, f32)> = d
            .regions
            .iter()
            .map(|r| (secs(r.range.start), secs(r.range.end)))
            .collect();
        assert_eq!(r.len(), 3, "{r:?}");
        let near = |x: f32, y: f32| (x - y).abs() < 0.05;
        assert!(near(r[0].0, 0.0) && near(r[0].1, 1.0), "{r:?}");
        assert!(near(r[1].0, 7.0) && near(r[1].1, 10.0), "{r:?}");
        assert!(near(r[2].0, 16.0) && near(r[2].1, 17.0), "{r:?}");
        assert!(d.regions.iter().all(|r| r.kind == RegionKind::Silence));
        // One track start, a lead-in before the second piece; the pauses
        // at the very start and end are regions only.
        assert_eq!(d.splits.len(), 1, "{:?}", d.splits);
        assert!(near(secs(d.splits[0].pos), 9.75), "{:?}", d.splits);
        assert!(matches!(d.splits[0].reason, SplitReason::Gap { .. }));
        assert!((d.splits[0].confidence - 0.75).abs() < 0.05);
    }

    #[test]
    fn short_pauses_are_content_and_medium_ones_no_track_start() {
        let a = music(220.0);
        let s = recording(&[
            (3.0, &a),
            (0.3, &silence), // breath: ignored
            (3.0, &a),
            (1.0, &silence), // pause: deletable, but no new track
            (3.0, &a),
        ]);
        let d = detect(&analyse(&s), &DetectParams::default());
        assert_eq!(d.regions.len(), 1, "{:?}", d.regions);
        assert!((secs(d.regions[0].range.start) - 6.3).abs() < 0.05);
        assert!(d.splits.is_empty(), "{:?}", d.splits);
    }

    #[test]
    fn flat_noise_between_pieces_is_not_content() {
        // A loud hiss (−29 dB RMS, far above the quiet threshold) between
        // two pieces: only its flat spectrum tells it from music.
        let a = music(220.0);
        let hiss = |n| noise(n, 0.06, 5);
        let s = recording(&[(5.0, &a), (3.0, &hiss), (5.0, &a)]);
        let d = detect(&analyse(&s), &DetectParams::default());
        assert_eq!(d.regions.len(), 1, "{:?}", d.regions);
        assert_eq!(d.regions[0].kind, RegionKind::Noise);
        assert!((secs(d.regions[0].range.start) - 5.0).abs() < 0.1);
        assert!((secs(d.regions[0].range.end) - 8.0).abs() < 0.1);
        assert_eq!(d.splits.len(), 1);

        // Music at the same level is content: it is the flatness, not the
        // level, that decided.
        let soft = melody(330.0, 0.1);
        let s = recording(&[(5.0, &a), (3.0, &soft), (5.0, &a)]);
        let d = detect(&analyse(&s), &DetectParams::default());
        assert!(d.regions.is_empty(), "{:?}", d.regions);
    }

    #[test]
    fn steady_hum_between_pieces_is_not_content() {
        // Mains hum (50 Hz and harmonics, −27 dB RMS): tonal, so not flat,
        // but it never changes.
        let a = music(220.0);
        let hum = |n| tone(n, RATE, 50.0, 6, 0.1);
        let s = recording(&[(5.0, &a), (3.0, &hum), (5.0, &a)]);
        let d = detect(&analyse(&s), &DetectParams::default());
        assert_eq!(d.regions.len(), 1, "{:?} {:?}", d.thresholds, d.regions);
        assert_eq!(d.regions[0].kind, RegionKind::Noise);
        assert!(
            (secs(d.regions[0].range.start) - 5.0).abs() < 0.2,
            "{:?}",
            d.regions
        );
        assert!((secs(d.regions[0].range.end) - 8.0).abs() < 0.2);

        // Without the flux rule, the hum would be content.
        let p = DetectParams {
            steady_flux_db: 0.0,
            ..DetectParams::default()
        };
        assert!(detect(&analyse(&s), &p).regions.is_empty());
    }

    #[test]
    fn gapless_change_of_timbre_is_a_track_start() {
        // A low, dull piece followed directly by a bright one.
        let a = |n| tone(n, RATE, 110.0, 4, 0.4);
        let b = |n| {
            let t = tone(n, RATE, 880.0, 4, 0.3);
            let h = noise(n, 0.02, 3);
            t.iter().zip(h).map(|(x, y)| x + y).collect()
        };
        let s = recording(&[(10.0, &a), (10.0, &b)]);
        let d = detect(&analyse(&s), &DetectParams::default());
        assert!(d.regions.is_empty(), "{:?}", d.regions);
        assert_eq!(d.splits.len(), 1, "{:?}", d.splits);
        assert!((secs(d.splits[0].pos) - 10.0).abs() < 0.1, "{:?}", d.splits);
        assert!(matches!(
            d.splits[0].reason,
            SplitReason::TimbreChange { .. }
        ));
    }

    #[test]
    fn a_pause_at_the_edge_of_the_window_is_no_change_of_timbre() {
        // A piece without any noise floor (its empty bands at −128 dB, as
        // after an MP3 encoder) followed by a noisy pause: the windows
        // that reach into the pause must not see a change of timbre. This
        // was a false track start on an MP3 test file.
        let mut s = tone(10 * RATE as usize, RATE, 110.0, 4, 0.4);
        s.extend(noise(3 * RATE as usize, 0.06, 5));
        let d = detect(&analyse(&s), &DetectParams::default());
        assert!(d.splits.is_empty(), "{:?}", d.splits);
        assert_eq!(d.regions.len(), 1);
    }

    #[test]
    fn changes_far_below_the_quiet_threshold_are_no_change_of_timbre() {
        // Noise at −100 dBFS, 55 dB below the quiet threshold, starts in
        // the middle of a piece (like dither switched on, or an encoder no
        // longer emptying its top bands): inaudible, so no new track,
        // although the piece's empty bands rise from −128 to about −110 dB.
        let a = tone(20 * RATE as usize, RATE, 110.0, 4, 0.4);
        let half = a.len() / 2;
        let s: Vec<f32> = a
            .iter()
            .zip(noise(a.len(), 1.7e-5, 11))
            .enumerate()
            .map(|(i, (x, r))| if i < half { *x } else { x + r })
            .collect();
        let d = detect(&analyse(&s), &DetectParams::default());
        assert!(d.splits.is_empty(), "{:?}", d.splits);
    }

    #[test]
    fn gapless_change_of_key_is_a_track_start() {
        // The same instrument plays a melody in C, then directly one a
        // major third higher: the pitch classes change (cosine distance
        // about 0.5; the same melody on both sides stays below 0.08).
        // With so few partials, the band spectrum changes too (about
        // 10 dB), so the timbre detector is turned off to see the key
        // detector alone.
        let a = music(262.0);
        let b = music(330.0);
        let s = recording(&[(10.0, &a), (10.0, &b)]);
        let key_only = DetectParams {
            min_novelty_db: f32::INFINITY,
            min_key_distance: KEY_DISTANCE,
            ..DetectParams::default()
        };
        let d = detect(&analyse(&s), &key_only);
        assert!(d.regions.is_empty(), "{:?}", d.regions);
        assert_eq!(d.splits.len(), 1, "{:?}", d.splits);
        assert!((secs(d.splits[0].pos) - 10.0).abs() < 0.5, "{:?}", d.splits);
        let SplitReason::KeyChange { distance } = d.splits[0].reason else {
            panic!("{:?}", d.splits)
        };
        assert!(distance > 0.4, "{distance}");

        let off = DetectParams {
            min_key_distance: f32::INFINITY,
            ..key_only.clone()
        };
        assert!(detect(&analyse(&s), &off).splits.is_empty());
        // Off by default.
        let d = detect(&analyse(&s), &DetectParams::default());
        assert!(
            d.splits
                .iter()
                .all(|s| !matches!(s.reason, SplitReason::KeyChange { .. })),
            "{:?}",
            d.splits
        );

        // The same melody throughout: nothing.
        let s = recording(&[(20.0, &a)]);
        let d = detect(&analyse(&s), &DetectParams::default());
        assert!(d.splits.is_empty(), "{:?}", d.splits);
    }

    #[test]
    fn sections_that_come_back_are_no_track_start() {
        // Verse and chorus of one song: a dull and a bright section,
        // alternating every 8 s. Each change is as large as the one
        // between two pieces in gapless_change_of_timbre_is_a_track_start,
        // but the sound after it was heard before (or the sound before it
        // comes back), which a new piece never does. Real songs gave over
        // a hundred false track starts per hour without this test.
        let a = |n| tone(n, RATE, 110.0, 4, 0.4);
        let b = |n| {
            let t = tone(n, RATE, 880.0, 4, 0.3);
            let h = noise(n, 0.02, 3);
            t.iter().zip(h).map(|(x, y)| x + y).collect()
        };
        let parts: Vec<(f32, Generator)> = (0..8)
            .map(|k| (8.0, if k % 2 == 0 { &a as Generator } else { &b }))
            .collect();
        let s = recording(&parts);
        let d = detect(&analyse(&s), &DetectParams::default());
        assert!(d.splits.is_empty(), "{:?}", d.splits);

        // Comparing only the neighbouring windows finds every change,
        // as far apart as the shortest track allows.
        let neighbours_only = DetectParams {
            recurrence_secs: 0.0,
            min_track_secs: 0.0,
            ..DetectParams::default()
        };
        let d = detect(&analyse(&s), &neighbours_only);
        assert_eq!(d.splits.len(), 7, "{:?}", d.splits);
    }

    #[test]
    fn track_starts_are_at_least_a_shortest_track_apart() {
        // Talk with a 3 s pause every 10 s: each pause is a deletable
        // region, but not each one a new track.
        let talk = music(220.0);
        let mut parts: Vec<(f32, Generator)> = Vec::new();
        for _ in 0..9 {
            parts.push((7.0, &talk));
            parts.push((3.0, &silence));
        }
        parts.push((7.0, &talk));
        let s = recording(&parts);
        let d = detect(&analyse(&s), &DetectParams::default());
        assert_eq!(d.regions.len(), 9, "{:?}", d.regions);
        let min = DetectParams::default().min_track_secs;
        assert!(!d.splits.is_empty());
        for w in d.splits.windows(2) {
            assert!(secs(w[1].pos - w[0].pos) >= min, "{:?}", d.splits);
        }
        assert!(d.splits.len() <= 3, "{:?}", d.splits);

        let unlimited = DetectParams {
            min_track_secs: 0.0,
            ..DetectParams::default()
        };
        assert_eq!(detect(&analyse(&s), &unlimited).splits.len(), 9);
    }

    #[test]
    fn pauses_between_short_blips_are_no_track_starts() {
        // Rain: 4 s of quiet, then 1 s of something loud, over and over
        // for two minutes. Each quiet stretch is a deletable pause, but
        // the "tracks" between them would be mostly pause; real rain
        // recordings got a track start every 30 s without this rule.
        let blip = music(440.0);
        let mut parts: Vec<(f32, Generator)> = Vec::new();
        for _ in 0..24 {
            parts.push((4.0, &silence));
            parts.push((1.0, &blip));
        }
        let s = recording(&parts);
        let d = detect(&analyse(&s), &DetectParams::default());
        assert!(d.regions.len() >= 20, "{:?}", d.regions);
        assert!(d.splits.is_empty(), "{:?}", d.splits);

        let any_share = DetectParams {
            min_track_content: 0.0,
            ..DetectParams::default()
        };
        assert!(!detect(&analyse(&s), &any_share).splits.is_empty());
    }

    #[test]
    fn steady_content_has_no_suggestions() {
        let a = music(220.0);
        let d = detect(
            &analyse(&recording(&[(20.0, &a)])),
            &DetectParams::default(),
        );
        assert!(d.regions.is_empty() && d.splits.is_empty(), "{d:?}");
    }

    #[test]
    fn timbre_detector_can_be_turned_off() {
        let a = |n| tone(n, RATE, 110.0, 4, 0.4);
        let b = |n| tone(n, RATE, 880.0, 4, 0.4);
        let s = recording(&[(10.0, &a), (10.0, &b)]);
        let p = DetectParams {
            min_novelty_db: f32::INFINITY,
            min_key_distance: f32::INFINITY,
            ..DetectParams::default()
        };
        assert!(detect(&analyse(&s), &p).splits.is_empty());
    }

    #[test]
    fn silent_and_empty_recordings() {
        let d = detect(
            &analyse(&vec![0.0; 3 * RATE as usize]),
            &DetectParams::default(),
        );
        assert_eq!(d.regions.len(), 1);
        assert_eq!(d.regions[0].range, 0..3 * RATE as SamplePos);
        assert!(d.splits.is_empty());

        let d = detect(&analyse(&[]), &DetectParams::default());
        assert!(d.regions.is_empty() && d.splits.is_empty());
    }

    #[test]
    fn quiet_threshold_is_capped_for_recordings_without_pauses() {
        // Loud music throughout with a quieter passage: nothing deletable.
        let loud = music(220.0);
        let soft = melody(220.0, 0.05); // 18 dB softer
        let s = recording(&[(5.0, &loud), (5.0, &soft), (5.0, &loud)]);
        let d = detect(&analyse(&s), &DetectParams::default());
        assert!(
            d.regions.is_empty(),
            "{:?} {:?} {:?}",
            d.thresholds,
            d.regions,
            d.splits
        );
    }

    #[test]
    fn noisy_pauses_above_the_level_cap_are_found_by_flatness() {
        // Pauses at −40 dB hiss: louder than `max_quiet_db`, so only their
        // flat spectrum makes them pauses.
        let hiss = |n| noise(n, 0.017, 8);
        let mix = |n| {
            let m = music(220.0)(n);
            m.iter().zip(hiss(n)).map(|(x, y)| x + y).collect()
        };
        let s = recording(&[(1.0, &hiss), (5.0, &mix), (3.0, &hiss), (5.0, &mix)]);
        let d = detect(&analyse(&s), &DetectParams::default());
        assert_eq!(d.regions.len(), 2, "{:?} {:?}", d.thresholds, d.regions);
        assert!(d.regions.iter().all(|r| r.kind == RegionKind::Noise));
        assert_eq!(d.splits.len(), 1, "{:?}", d.splits);
        assert!((secs(d.splits[0].pos) - 8.75).abs() < 0.1);
    }

    #[test]
    fn close_short_runs_keeps_runs_at_the_ends() {
        let mut m = [false, true, true, false, true, true, true, false];
        close_short_runs(&mut m, false, 2);
        assert_eq!(m, [false, true, true, true, true, true, true, false]);
    }

    #[test]
    fn one_sided_min_average_takes_the_steadier_side() {
        let v = one_sided_min_average(&[4.0, 4.0, 0.0, 0.0, 4.0], 2);
        assert_eq!(v, vec![4.0, 2.0, 0.0, 0.0, 2.0]);
    }

    #[test]
    fn running_min_looks_back_len_values() {
        let v = running_min(&[3.0, 1.0, 4.0, 1.5, 5.0, 9.0, 2.0], 3);
        assert_eq!(v, vec![3.0, 1.0, 1.0, 1.0, 1.5, 1.5, 2.0]);
    }

    /// Whispering: flat bursts of 0.25 s, 15 dB above the hiss between
    /// them, four per second.
    fn whisper(n: usize) -> Vec<f32> {
        let syllable = (0.25 * RATE as f32) as usize;
        let period = (0.4 * RATE as f32) as usize;
        noise(n, 1.0, 21)
            .into_iter()
            .enumerate()
            .map(|(i, x)| x * if i % period < syllable { 0.06 } else { 0.011 })
            .collect()
    }

    #[test]
    fn whispers_are_content() {
        let a = music(220.0);
        let s = recording(&[(4.0, &a), (4.0, &whisper), (4.0, &a)]);
        let d = detect(&analyse(&s), &DetectParams::default());
        assert!(d.regions.is_empty(), "{:?} {:?}", d.thresholds, d.regions);

        // Without the burst rule, the whisper is (flat) noise.
        let p = DetectParams {
            burst_rise_db: f32::INFINITY,
            ..DetectParams::default()
        };
        let d = detect(&analyse(&s), &p);
        assert_eq!(d.regions.len(), 1, "{:?}", d.regions);
        assert_eq!(d.regions[0].kind, RegionKind::Noise);
    }

    #[test]
    fn a_pause_that_gets_louder_stays_a_pause() {
        // Silence, then lasting hiss 20 dB louder (applause starting):
        // a dip before, but none after, so no burst.
        let a = music(220.0);
        let hiss = |n| noise(n, 0.05, 4);
        let s = recording(&[(4.0, &a), (2.0, &silence), (3.0, &hiss), (4.0, &a)]);
        let d = detect(&analyse(&s), &DetectParams::default());
        assert_eq!(d.regions.len(), 1, "{:?}", d.regions);
        assert!((secs(d.regions[0].range.start) - 4.0).abs() < 0.1);
        assert!(
            (secs(d.regions[0].range.end) - 9.0).abs() < 0.1,
            "{:?}",
            d.regions
        );
    }

    #[test]
    fn moving_average_is_centred() {
        let v = moving_average([0.0, 0.0, 3.0, 0.0, 0.0].into_iter(), 3);
        assert_eq!(v, vec![0.0, 1.0, 1.0, 1.0, 0.0]);
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]

        /// Whatever the recording, the result is well formed: regions
        /// sorted, disjoint and inside the recording, track starts strictly
        /// inside and sorted, timbre changes a window away from others.
        #[test]
        fn results_are_well_formed(
            parts in proptest::collection::vec((0u8..4, 1u32..60), 0..8),
            seed in 1u64..1000,
        ) {
            let mut s = Vec::new();
            for (kind, tenths) in parts {
                let n = tenths as usize * RATE as usize / 10;
                s.extend(match kind {
                    0 => silence(n),
                    1 => noise(n, 0.05, seed),
                    2 => tone(n, RATE, 110.0, 6, 0.4),
                    _ => tone(n, RATE, 1320.0, 2, 0.2),
                });
            }
            let f = analyse(&s);
            let p = DetectParams::default();
            let d = detect(&f, &p);
            let total = s.len() as SamplePos;
            for w in d.regions.windows(2) {
                proptest::prop_assert!(w[0].range.end < w[1].range.start);
            }
            for r in &d.regions {
                proptest::prop_assert!(r.range.start < r.range.end && r.range.end <= total);
            }
            for w in d.splits.windows(2) {
                proptest::prop_assert!(w[0].pos < w[1].pos);
            }
            let window = f.frame_start(f.frames_for_secs(p.novelty_window_secs));
            for s in &d.splits {
                proptest::prop_assert!(s.pos > 0 && s.pos < total);
                proptest::prop_assert!((0.0..=1.0).contains(&s.confidence));
                if let SplitReason::TimbreChange { .. } | SplitReason::KeyChange { .. } = s.reason {
                    for o in d.splits.iter().filter(|o| o.pos != s.pos) {
                        proptest::prop_assert!(o.pos.abs_diff(s.pos) >= window);
                    }
                }
            }
        }
    }
}
