//! Perspectives (issue #100): views of the waveform one key away, for
//! checking a selection's edges in context. Ctrl+1 to Ctrl+6 in the GUI.
//!
//! Each is a source range the waveform zooms to exactly. The context
//! around an edge is [`CONTEXT_SECONDS`]; a range that would reach past
//! either end of the recording is capped there. Without a selection the
//! cursor stands in for both of its edges.

use std::ops::Range;

use lac_core::SamplePos;

use crate::i18n::n_;

/// How much (in seconds) a perspective shows around a selection edge.
pub const CONTEXT_SECONDS: u32 = 5;

/// A view of the waveform relative to the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Perspective {
    /// The whole recording.
    Whole,
    /// From the start of the recording to just after the selection start.
    UpToStart,
    /// Around the selection start.
    AroundStart,
    /// The selection, with some context on either side.
    Selection,
    /// Around the selection end.
    AroundEnd,
    /// From just before the selection end to the end of the recording.
    FromEnd,
}

impl Perspective {
    /// All perspectives, in the order of their keys (Ctrl+1 to Ctrl+6).
    pub const ALL: [Perspective; 6] = [
        Perspective::Whole,
        Perspective::UpToStart,
        Perspective::AroundStart,
        Perspective::Selection,
        Perspective::AroundEnd,
        Perspective::FromEnd,
    ];

    /// The menu text, without the key, in English; [`crate::i18n::tr`]
    /// translates it.
    pub fn label(self) -> &'static str {
        match self {
            Perspective::Whole => n_("&Whole file"),
            Perspective::UpToStart => n_("&Up to the selection start"),
            Perspective::AroundStart => n_("Around the selection &start"),
            Perspective::Selection => n_("The selection with conte&xt"),
            Perspective::AroundEnd => n_("Around the selection &end"),
            Perspective::FromEnd => n_("&From the selection end"),
        }
    }

    /// The source range to show in a recording of `total` frames at
    /// `rate` frames per second, with `selection` selected (or, without
    /// one, the cursor at `cursor`).
    pub fn range(
        self,
        total: SamplePos,
        rate: u32,
        selection: Option<Range<SamplePos>>,
        cursor: SamplePos,
    ) -> Range<SamplePos> {
        let context = SamplePos::from(CONTEXT_SECONDS) * SamplePos::from(rate);
        let sel = selection.unwrap_or(cursor..cursor);
        let before = |p: SamplePos| p.saturating_sub(context);
        let after = |p: SamplePos| p.saturating_add(context).min(total);
        let r = match self {
            Perspective::Whole => 0..total,
            Perspective::UpToStart => 0..after(sel.start),
            Perspective::AroundStart => before(sel.start)..after(sel.start),
            Perspective::Selection => before(sel.start)..after(sel.end),
            Perspective::AroundEnd => before(sel.end)..after(sel.end),
            Perspective::FromEnd => before(sel.end)..total,
        };
        r.start.min(total)..r.end.min(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1000 frames per second: 5 s of context are 5000 frames.
    const RATE: u32 = 1000;
    const TOTAL: SamplePos = 600_000;

    fn range(p: Perspective, sel: Option<Range<SamplePos>>) -> Range<SamplePos> {
        p.range(TOTAL, RATE, sel, 42_000)
    }

    #[test]
    fn perspectives_frame_the_selection_edges() {
        let sel = Some(100_000..200_000);
        assert_eq!(range(Perspective::Whole, sel.clone()), 0..TOTAL);
        assert_eq!(range(Perspective::UpToStart, sel.clone()), 0..105_000);
        assert_eq!(
            range(Perspective::AroundStart, sel.clone()),
            95_000..105_000
        );
        assert_eq!(range(Perspective::Selection, sel.clone()), 95_000..205_000);
        assert_eq!(range(Perspective::AroundEnd, sel.clone()), 195_000..205_000);
        assert_eq!(range(Perspective::FromEnd, sel), 195_000..TOTAL);
    }

    #[test]
    fn context_is_capped_at_the_ends_of_the_recording() {
        let sel = Some(2_000..598_000);
        assert_eq!(range(Perspective::AroundStart, sel.clone()), 0..7_000);
        assert_eq!(range(Perspective::Selection, sel.clone()), 0..TOTAL);
        assert_eq!(range(Perspective::AroundEnd, sel), 593_000..TOTAL);
    }

    #[test]
    fn without_a_selection_the_cursor_stands_in() {
        assert_eq!(range(Perspective::AroundStart, None), 37_000..47_000);
        assert_eq!(range(Perspective::Selection, None), 37_000..47_000);
        assert_eq!(range(Perspective::FromEnd, None), 37_000..TOTAL);
    }

    #[test]
    fn every_perspective_has_its_own_mnemonic() {
        let keys: Vec<char> = Perspective::ALL
            .iter()
            .map(|p| {
                let l = p.label();
                let i = l.find('&').expect("mnemonic");
                l[i + 1..]
                    .chars()
                    .next()
                    .expect("letter")
                    .to_ascii_lowercase()
            })
            .collect();
        for (i, k) in keys.iter().enumerate() {
            assert!(!keys[i + 1..].contains(k), "{k} twice");
        }
    }
}
