//! Playing a recording with its edits applied: glue between [`Source`]
//! (our demuxers and FFmpeg's decoders) and [`lac_playback`].

use std::ops::Range;
use std::path::Path;

use lac_core::SamplePos;
use lac_playback::{PcmSource, Player, Timeline};

use crate::Source;
use crate::editor::Editor;

/// A [`Source`] as audio for the player, decoded with random access
/// ([`Source::decode_range`]), so playback can start anywhere at once.
pub struct SourcePcm {
    source: Source,
    channels: usize,
    rate: u32,
}

impl SourcePcm {
    /// Wraps an opened source.
    pub fn new(source: Source) -> Self {
        let info = source.info();
        SourcePcm {
            source,
            channels: usize::from(info.channels),
            rate: info.sample_rate,
        }
    }
}

impl PcmSource for SourcePcm {
    fn channels(&self) -> usize {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn read(&mut self, range: Range<SamplePos>) -> Result<Vec<f32>, String> {
        self.source.decode_range(range).map_err(|e| e.to_string())
    }
}

/// What playback plays for `editor`'s edits: the kept parts in order.
pub fn timeline(editor: &Editor) -> Timeline {
    Timeline::new(editor.kept())
}

/// Which end of a range [`excerpt`] plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// The beginning.
    Start,
    /// The ending.
    End,
}

/// How long an excerpt (e.g. the start of a selection) plays: long
/// enough to judge a cut, short enough to try several.
pub const EXCERPT_SECONDS: u32 = 3;

/// What playback plays for the start or end (`end`) of source `range`:
/// its kept parts, trimmed to the first (or last) `len` kept frames.
/// Empty if all of `range` is deleted.
pub fn excerpt(editor: &Editor, range: Range<SamplePos>, end: End, len: SamplePos) -> Timeline {
    let mut parts: Vec<_> = editor
        .kept()
        .into_iter()
        .map(|k| k.start.max(range.start)..k.end.min(range.end))
        .filter(|k| k.start < k.end)
        .collect();
    if end == End::End {
        parts.reverse();
    }
    let mut left = len;
    let mut out = Vec::new();
    for p in parts {
        if left == 0 {
            break;
        }
        let take = left.min(p.end - p.start);
        left -= take;
        out.push(match end {
            End::Start => p.start..p.start + take,
            End::End => p.end - take..p.end,
        });
    }
    if end == End::End {
        out.reverse();
    }
    Timeline::new(out)
}

/// The start or end (`end`) of the track containing source position
/// `at`, [`EXCERPT_SECONDS`] long at `rate` frames per second: what the
/// toolbar's "play track start/end" buttons play. Empty if the track is
/// deleted entirely.
pub fn track_excerpt(editor: &Editor, at: SamplePos, end: End, rate: u32) -> Timeline {
    let index = editor.track_at(at.min(editor.total().saturating_sub(1)));
    let Some(track) = editor.tracks().into_iter().nth(index) else {
        return Timeline::default();
    };
    excerpt(editor, track.span, end, excerpt_len(rate))
}

/// [`EXCERPT_SECONDS`] in frames at `rate` frames per second.
pub fn excerpt_len(rate: u32) -> SamplePos {
    SamplePos::from(EXCERPT_SECONDS) * SamplePos::from(rate)
}

/// Opens `path` for playback of `timeline` on the default audio device,
/// paused at the start. The message is for the user.
pub fn open_player(path: &Path, timeline: Timeline) -> Result<Player, String> {
    let source = Source::open(path).map_err(|e| e.to_string())?;
    Player::open(SourcePcm::new(source), timeline).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn track_excerpts_play_the_ends_of_the_track_at_a_position() {
        use super::{End, track_excerpt};
        let mut e = crate::editor::Editor::new(60_000);
        e.split_at_all([10_000, 30_000]);
        // 1000 frames per second: 3 s are 3000 frames.
        let start = track_excerpt(&e, 12_000, End::Start, 1000);
        assert_eq!(start.segments(), std::slice::from_ref(&(10_000..13_000)));
        let end = track_excerpt(&e, 12_000, End::End, 1000);
        assert_eq!(end.segments(), std::slice::from_ref(&(27_000..30_000)));
        // A track's own start belongs to it; the end of the recording to
        // the last track.
        let last = track_excerpt(&e, 60_000, End::Start, 1000);
        assert_eq!(last.segments(), std::slice::from_ref(&(30_000..33_000)));
        e.select(10_000, 30_000);
        e.delete_selection();
        assert!(track_excerpt(&e, 10_000, End::End, 1000).is_empty());
    }

    #[test]
    fn excerpts_take_kept_frames_from_their_end_of_the_range() {
        use super::{End, excerpt};
        let mut e = crate::editor::Editor::new(10_000);
        e.select(2000, 3000);
        e.delete_selection();
        // Kept: 0..2000 and 3000..10_000.
        let start = excerpt(&e, 1000..8000, End::Start, 1500);
        assert_eq!(start.segments(), [1000..2000, 3000..3500]);
        let end = excerpt(&e, 1000..8000, End::End, 1500);
        assert_eq!(end.segments(), std::slice::from_ref(&(6500..8000)));
        let short = excerpt(&e, 1500..3500, End::End, 5000);
        assert_eq!(short.segments(), [1500..2000, 3000..3500], "all of it");
        assert!(excerpt(&e, 2100..2900, End::Start, 100).is_empty());
    }

    use std::time::{Duration, Instant};

    use lac_playback::{Engine, OutputFormat};

    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../lac-container/tests/fixtures")
            .join(name)
    }

    /// Plays the edited timeline of a real file through the engine (no
    /// sound card: the test is the audio callback) and checks that what
    /// comes out is bit for bit the decoded kept parts: playback is the
    /// preview of the export.
    #[test]
    fn plays_the_kept_parts_of_a_file_bit_for_bit() {
        for name in [
            "flac_stereo16_44k.flac",
            "mp3_cbr128_44k.mp3",
            "ogg_opus_stereo.opus",
            "ogg_vorbis_stereo_44k.ogg",
            "mkv_opus_stereo.webm",
        ] {
            plays_bit_for_bit(name);
        }
    }

    fn plays_bit_for_bit(name: &str) {
        let path = fixture(name);
        let mut source = Source::open(&path).expect("open");
        let info = source.info();
        let total = source.total_samples();
        let mut editor = Editor::new(total);
        assert!(total > 40_000, "{name} is too short for this test");
        editor.select(10_000, 30_000);
        editor.delete_selection();
        let tl = timeline(&editor);
        let mut want = Vec::new();
        for r in tl.segments() {
            want.extend(source.decode_range(r.clone()).expect("decode"));
        }

        let format = OutputFormat {
            sample_rate: info.sample_rate,
            channels: usize::from(info.channels),
        };
        let (engine, mut out) = Engine::start(SourcePcm::new(source), tl, format);
        engine.play();
        let mut got = Vec::new();
        let mut buf = vec![0.0; 1024 * format.channels];
        let start = Instant::now();
        while !engine.at_end() && start.elapsed() < Duration::from_secs(20) {
            let n = out.fill(&mut buf) * format.channels;
            got.extend_from_slice(&buf[..n]);
            if n == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        assert_eq!(engine.error(), None);
        assert_eq!(got.len(), want.len());
        assert!(
            got == want,
            "played samples differ from the decoded kept parts"
        );
        assert_eq!(engine.position(), total - 20_000);
    }
}
