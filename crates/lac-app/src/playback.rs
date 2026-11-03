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

/// Opens `path` for playback of `timeline` on the default audio device,
/// paused at the start. The message is for the user.
pub fn open_player(path: &Path, timeline: Timeline) -> Result<Player, String> {
    let source = Source::open(path).map_err(|e| e.to_string())?;
    Player::open(SourcePcm::new(source), timeline).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
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
