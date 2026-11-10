//! Toolbar icons, as SVG so they stay sharp at every screen scale.
//!
//! wxWidgets renders them with its built-in SVG reader (NanoSVG), which
//! knows plain shapes and paths but not `currentColor` or CSS, so the
//! colours are spelled out. A mid grey reads on light and dark themes.
//!
//! All icons share a 24×24 view box and 2-unit round strokes, like most
//! icon sets, so they look like one family.

/// The SVG of one icon: shapes drawn in [`INK`] on a transparent 24×24
/// view box.
fn svg(shapes: &str) -> String {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" width="24" height="24" fill="none" stroke="{INK}" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">{shapes}</svg>"#
    )
}

/// Line colour of every icon.
const INK: &str = "#5c6670";
/// Fill of solid shapes (the play triangle).
const SOLID: &str = r##"fill="#5c6670""##;

/// The icons of the toolbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// File > Open: a folder.
    Open,
    /// File > Export: an arrow leaving a tray.
    Export,
    /// Edit > Undo: an arrow turning back to the left.
    Undo,
    /// Edit > Redo: an arrow turning on to the right.
    Redo,
    /// Edit > Delete selection: scissors.
    Delete,
    /// Edit > Restore selection: a circular arrow over a bar.
    Restore,
    /// Edit > Split track: a boundary line with a flag.
    Split,
    /// Play > Play/Stop: a triangle.
    Play,
    /// Play > Cursor to start: a bar and a triangle pointing to it.
    ToStart,
    /// Play > Play track start: a track boundary and a triangle playing
    /// away from it.
    PlayTrackStart,
    /// Play > Play track end: a triangle playing up to a track boundary.
    PlayTrackEnd,
    /// View > Zoom in: a magnifier with a plus.
    ZoomIn,
    /// View > Zoom out: a magnifier with a minus.
    ZoomOut,
    /// View > Zoom to fit: arrows pointing to both sides.
    ZoomFit,
    /// Settings page "Detection": a lens over a pause between two sound
    /// bursts.
    Detection,
    /// Settings page "Waveform": a waveform over two tinted tracks.
    Waveform,
    /// Settings page "Language": two speech bubbles.
    Language,
}

impl Icon {
    /// The icon as an SVG document.
    pub fn svg(self) -> String {
        // A magnifier: lens and handle, for the zoom icons.
        const LENS: &str =
            r#"<circle cx="10" cy="10" r="6.5"/><line x1="15" y1="15" x2="21" y2="21"/>"#;
        let shapes = match self {
            Icon::Open => {
                r#"<path d="M3 6.5V18a1.5 1.5 0 0 0 1.5 1.5h15A1.5 1.5 0 0 0 21 18V9a1.5 1.5 0 0 0-1.5-1.5H12l-2-2.5H4.5A1.5 1.5 0 0 0 3 6.5z"/>"#.to_string()
            }
            Icon::Export => {
                r#"<path d="M4 14v5h16v-5"/><line x1="12" y1="15" x2="12" y2="3"/><polyline points="7.5,7.5 12,3 16.5,7.5"/>"#.to_string()
            }
            Icon::Undo => {
                r#"<path d="M9 14L4 9l5-5"/><path d="M4 9h10a6 6 0 0 1 0 12h-3"/>"#.to_string()
            }
            Icon::Redo => {
                r#"<path d="M15 14l5-5-5-5"/><path d="M20 9H10a6 6 0 0 0 0 12h3"/>"#.to_string()
            }
            Icon::Delete => {
                r#"<circle cx="6" cy="6" r="3"/><circle cx="6" cy="18" r="3"/><line x1="20" y1="4" x2="8.1" y2="15.9"/><line x1="14.5" y1="14.5" x2="20" y2="20"/><line x1="8.1" y1="8.1" x2="12" y2="12"/>"#.to_string()
            }
            Icon::Restore => {
                r#"<path d="M4 10a8 8 0 0 1 14.5-4.5"/><polyline points="19,2 19,6 15,6"/><line x1="4" y1="18" x2="20" y2="18"/><line x1="4" y1="14" x2="4" y2="22"/><line x1="20" y1="14" x2="20" y2="22"/>"#.to_string()
            }
            Icon::Split => {
                // A track marker: a boundary line with a flag on top.
                format!(r#"<line x1="8" y1="3" x2="8" y2="21"/><polygon points="8,3.5 19,7.5 8,11.5" {SOLID}/><line x1="3" y1="21" x2="21" y2="21"/>"#)
            }
            Icon::Play => format!(r#"<polygon points="7,4 20,12 7,20" {SOLID}/>"#),
            Icon::ToStart => {
                format!(r#"<line x1="5" y1="5" x2="5" y2="19"/><polygon points="19,5 9,12 19,19" {SOLID}/>"#)
            }
            Icon::PlayTrackStart => {
                format!(r#"<line x1="4" y1="4" x2="4" y2="20"/><polygon points="9,6 19,12 9,18" {SOLID}/>"#)
            }
            Icon::PlayTrackEnd => {
                format!(r#"<polygon points="5,6 15,12 5,18" {SOLID}/><line x1="20" y1="4" x2="20" y2="20"/>"#)
            }
            Icon::ZoomIn => {
                format!(r#"{LENS}<line x1="10" y1="7" x2="10" y2="13"/><line x1="7" y1="10" x2="13" y2="10"/>"#)
            }
            Icon::ZoomOut => format!(r#"{LENS}<line x1="7" y1="10" x2="13" y2="10"/>"#),
            Icon::ZoomFit => {
                r#"<line x1="3" y1="4" x2="3" y2="20"/><line x1="21" y1="4" x2="21" y2="20"/><line x1="7" y1="12" x2="17" y2="12"/><polyline points="10,9 7,12 10,15"/><polyline points="14,9 17,12 14,15"/>"#.to_string()
            }
            Icon::Detection => {
                // Two bursts of sound with a pause between them, a lens on
                // the pause.
                r#"<line x1="3" y1="9" x2="3" y2="15"/><line x1="6" y1="7" x2="6" y2="17"/><line x1="18" y1="7" x2="18" y2="17"/><line x1="21" y1="9" x2="21" y2="15"/><circle cx="12" cy="12" r="4"/><line x1="15" y1="15" x2="19" y2="20"/>"#.to_string()
            }
            Icon::Waveform => {
                // Two track areas side by side, shaded in two hues, with
                // a wave across both.
                r##"<rect x="2" y="4" width="10" height="16" fill="#5090f0" fill-opacity="0.35" stroke="none"/><rect x="12" y="4" width="10" height="16" fill="#f0b040" fill-opacity="0.35" stroke="none"/><path d="M2 12c2-6 4-6 5 0s3 6 5 0 3-6 5 0 3 6 5 0"/>"##.to_string()
            }
            Icon::Language => {
                // Two overlapping speech bubbles: one language and another.
                r#"<path d="M3 4h11a1 1 0 0 1 1 1v6a1 1 0 0 1-1 1H8l-3 3v-3H3a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1z"/><path d="M17 9h4a1 1 0 0 1 1 1v6a1 1 0 0 1-1 1h-1v3l-3-3h-5a1 1 0 0 1-1-1v-2"/>"#.to_string()
            }
        };
        svg(&shapes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Icon; 17] = [
        Icon::Open,
        Icon::Export,
        Icon::Undo,
        Icon::Redo,
        Icon::Delete,
        Icon::Restore,
        Icon::Split,
        Icon::Play,
        Icon::ToStart,
        Icon::PlayTrackStart,
        Icon::PlayTrackEnd,
        Icon::ZoomIn,
        Icon::ZoomOut,
        Icon::ZoomFit,
        Icon::Detection,
        Icon::Waveform,
        Icon::Language,
    ];

    /// A crude well-formedness check: NanoSVG skips what it cannot parse
    /// silently, so a typo would only show as a blank button.
    #[test]
    fn every_icon_is_a_closed_svg_document() {
        for icon in ALL {
            let s = icon.svg();
            assert!(s.starts_with("<svg ") && s.ends_with("</svg>"), "{icon:?}");
            assert_eq!(s.matches('<').count(), s.matches('>').count(), "{icon:?}");
            // Every element but the root closes itself.
            assert_eq!(
                s.matches("/>").count(),
                s.matches('<').count() - 2,
                "{icon:?}"
            );
            assert_eq!(s.matches('"').count() % 2, 0, "{icon:?}");
            assert!(!s.contains("currentColor"), "NanoSVG does not know it");
        }
    }

    #[test]
    fn icons_differ() {
        for (i, a) in ALL.iter().enumerate() {
            for b in &ALL[i + 1..] {
                assert_ne!(a.svg(), b.svg(), "{a:?} and {b:?}");
            }
        }
    }
}
