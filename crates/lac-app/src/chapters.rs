//! Track starts that the file itself marks, and how they combine with the
//! ones the analysis suggests.
//!
//! A chained Ogg file (a stream ripper's capture) holds one link per song,
//! each with its own title in its comment header. Those link boundaries
//! are offered as suggested track starts, together with the analysis'
//! ones, and splitting there gives each new track its link's tags (issue
//! #47). See [`lac_container::Chapter`].

use lac_core::{SamplePos, Tags};

pub use lac_container::Chapter;

/// A detected track start this close to a chapter start (in seconds) is
/// taken to mean the same boundary, and dropped in favour of the chapter:
/// the analysis' estimate of where a song starts is rarely exactly the
/// link boundary, and a track of a fraction of a second is never wanted.
/// The same as the analysis' default minimum track gap.
pub const SAME_START_SECS: u64 = 2;

/// A suggested track start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestedStart {
    /// Where the new track would start (source sample frame).
    pub pos: SamplePos,
    /// The tags the file gives that track (a chapter's), if any.
    pub tags: Option<Tags>,
}

/// The track starts to suggest, sorted: every chapter start after the
/// first, with its tags, and every `detected` start that is not within
/// [`SAME_START_SECS`] of a chapter start (`rate` is the sample rate).
pub fn suggested_starts(
    chapters: &[Chapter],
    detected: impl IntoIterator<Item = SamplePos>,
    rate: u32,
) -> Vec<SuggestedStart> {
    let near = SAME_START_SECS * u64::from(rate);
    let mut out: Vec<SuggestedStart> = chapters
        .iter()
        .filter(|c| c.start > 0)
        .map(|c| SuggestedStart {
            pos: c.start,
            tags: Some(c.tags.clone()),
        })
        .collect();
    out.extend(
        detected
            .into_iter()
            .filter(|&p| chapters.iter().all(|c| c.start.abs_diff(p) > near))
            .map(|pos| SuggestedStart { pos, tags: None }),
    );
    out.sort_by_key(|s| s.pos);
    out.dedup_by_key(|s| s.pos);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chapter(start: SamplePos, title: &str) -> Chapter {
        Chapter {
            start,
            tags: Tags {
                title: Some(title.into()),
                ..Tags::default()
            },
        }
    }

    #[test]
    fn chapters_win_over_nearby_detected_starts() {
        let rate = 10;
        let chapters = [
            chapter(0, "One"),
            chapter(100, "Two"),
            chapter(300, "Three"),
        ];
        // 85 and 119 are within 2 s (20 frames) of 100: the same boundary.
        // 50 and 200 are starts the file does not mark (e.g. two songs
        // in one link); 79 is just outside the window.
        let got = suggested_starts(&chapters, [50, 79, 85, 119, 200], rate);
        let pos: Vec<SamplePos> = got.iter().map(|s| s.pos).collect();
        assert_eq!(pos, [50, 79, 100, 200, 300]);
        let titles: Vec<Option<&str>> = got
            .iter()
            .map(|s| s.tags.as_ref().and_then(|t| t.title.as_deref()))
            .collect();
        assert_eq!(titles, [None, None, Some("Two"), None, Some("Three")]);
    }

    #[test]
    fn without_chapters_the_detected_starts_stay() {
        let got = suggested_starts(&[], [30, 10, 30], 44_100);
        assert_eq!(
            got,
            [
                SuggestedStart {
                    pos: 10,
                    tags: None
                },
                SuggestedStart {
                    pos: 30,
                    tags: None
                }
            ]
        );
    }
}
