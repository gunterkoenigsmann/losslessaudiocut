//! The application logo: a strip of film cut in two, with a waveform
//! running across the cut.
//!
//! The SVG lives in `packaging/icons/` because the release packaging
//! (Windows `.ico`, macOS `.icns`, Linux `.desktop` icon) and the snap use
//! the same file; the GUI embeds it at compile time so the binary needs no
//! data files.

use wxdragon::prelude::*;

/// The logo as an SVG document (256×256 view box).
pub const SVG: &str = include_str!("../../../packaging/icons/losslessaudiocut.svg");

/// Edge length of the window icon bitmap in pixels.
///
/// `wxFrame::SetIcon` takes a single bitmap; window managers and the
/// Windows taskbar scale it down to what they show (16 to 48 px), and
/// scaling down from 128 keeps the edges smooth.
const WINDOW_ICON: i32 = 128;

/// Shows the logo as `frame`'s icon (title bar, taskbar, Alt+Tab).
///
/// On macOS the Dock shows the application bundle's `.icns` instead, so
/// this is a no-op there in effect. If the SVG cannot be rendered the
/// window simply keeps the default icon.
pub fn set_window_icon(frame: &Frame) {
    let size = Size::new(WINDOW_ICON, WINDOW_ICON);
    if let Some(bitmap) = BitmapBundle::from_svg_text(SVG, size).and_then(|b| b.get_bitmap(size)) {
        frame.set_icon(&bitmap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NanoSVG, wxWidgets' SVG renderer, silently drops what it does not
    /// know; the logo must stick to plain shapes or parts of it vanish.
    #[test]
    fn logo_uses_only_what_nanosvg_renders() {
        let shapes = without_comments(SVG);
        for unsupported in [
            "<style",
            "<use",
            "clipPath",
            "<mask",
            "<text",
            "<image",
            "currentColor",
            "<filter",
        ] {
            assert!(!shapes.contains(unsupported), "logo SVG uses {unsupported}");
        }
    }

    /// `svg` with every `<!-- ... -->` removed: the logo's comments may
    /// name the elements it avoids.
    fn without_comments(svg: &str) -> String {
        let mut out = String::new();
        let mut rest = svg;
        while let Some(start) = rest.find("<!--") {
            out.push_str(&rest[..start]);
            let end = rest[start..].find("-->").expect("unterminated comment");
            rest = &rest[start + end + 3..];
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn comments_are_stripped() {
        assert_eq!(without_comments("a<!-- <use> -->b<!--c-->d"), "abd");
    }

    /// Icon generators (rsvg-convert, iconutil, snapcraft) expect a
    /// square image.
    #[test]
    fn logo_is_square() {
        assert!(SVG.contains(r#"viewBox="0 0 256 256""#));
        assert!(SVG.contains(r#"width="256" height="256""#));
    }
}
