//! The window layout's view model: which dockable panes the main window
//! has, what they are called, whether they are shown at first, and how a
//! layout remembered from an earlier run is restored.
//!
//! The GUI arranges its panes with wxWidgets' AUI manager, which saves the
//! whole arrangement as one "perspective" string. This module knows just
//! enough of that format to answer one question without a display: which
//! panes does a stored perspective know about? A pane added in a later
//! version of the program is missing from a perspective saved by an older
//! one; loading such a perspective hides it, so the GUI shows it again
//! according to [`Pane::shown_by_default`] (see [`restore`]).
//!
//! Perspective format (`wxAuiManager::SavePerspective`; wxWidgets 3.3
//! writes `layout3`, which adds fields, 3.2 wrote `layout2`):
//!
//! ```text
//! layout3|name=toolbar;caption=Toolbar;state=…;dir=1;…|name=…|dock_size(1,10,0)=40|
//! ```
//!
//! Parts are separated by `|`, the fields of a pane by `;`; both are
//! escaped with a backslash inside values (`EscapeDelimiters`).

use crate::i18n::n_;

/// A dockable pane of the main window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pane {
    /// Buttons for the most used commands.
    Toolbar,
    /// Selection start, end and length, and the play clock (see
    /// [`crate::positions`]).
    Positions,
    /// The tracks and their tags, editable.
    TrackList,
    /// The whole recording, with the visible part framed (see
    /// [`crate::minimap`]).
    Minimap,
    /// The messages of the status bar, kept (see [`crate::log`]).
    Log,
    /// The picture of a video file at the cursor or playhead (see
    /// [`crate::video`]).
    Video,
}

impl Pane {
    /// Every pane, in the order of the View menu.
    pub const ALL: [Pane; 6] = [
        Pane::Toolbar,
        Pane::Minimap,
        Pane::Video,
        Pane::Positions,
        Pane::TrackList,
        Pane::Log,
    ];

    /// The pane's name in the stored layout. Never change one: a layout
    /// stored by an earlier version would lose the pane.
    pub fn name(self) -> &'static str {
        match self {
            Pane::Toolbar => "toolbar",
            Pane::Positions => "positions",
            Pane::TrackList => "track_list",
            Pane::Minimap => "minimap",
            Pane::Log => "log",
            Pane::Video => "video",
        }
    }

    /// The pane's title (its caption, and its entry in the View menu),
    /// in English; [`crate::i18n::tr`] translates it.
    pub fn caption(self) -> &'static str {
        match self {
            Pane::Toolbar => n_("Toolbar"),
            Pane::Positions => n_("Selection and clock"),
            Pane::TrackList => n_("Track list"),
            Pane::Minimap => n_("Minimap"),
            Pane::Log => n_("Log"),
            Pane::Video => n_("Video"),
        }
    }

    /// Whether the pane is shown when there is no stored layout.
    pub fn shown_by_default(self) -> bool {
        match self {
            Pane::Toolbar | Pane::Positions | Pane::TrackList | Pane::Minimap => true,
            // Wanted now and then, e.g. to read a warning again.
            Pane::Log => false,
            // Shown when a file with a video track is opened.
            Pane::Video => false,
        }
    }

    /// The pane called `name` in a stored layout.
    pub fn from_name(name: &str) -> Option<Pane> {
        Pane::ALL.into_iter().find(|p| p.name() == name)
    }
}

/// Prefixes of the perspective formats this module understands.
const PERSPECTIVE_PREFIXES: [&str; 2] = ["layout3|", "layout2|"];

/// `wxAuiPaneInfo::optionHidden`: the pane is hidden.
const STATE_HIDDEN: u64 = 1 << 1;

/// What the AUI manager should do with a stored layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restore {
    /// The perspective to load, `None` to keep the default arrangement.
    pub perspective: Option<String>,
    /// After loading, show (`true`) or hide these panes: the ones the
    /// stored layout does not know.
    pub then_show: Vec<(Pane, bool)>,
}

/// How to restore the stored perspective `stored` (if any).
///
/// A perspective that does not look like one (another wxWidgets format,
/// a damaged config file) is ignored rather than loaded, because loading
/// it could hide every pane. `default_shown` says whether a pane missing
/// from the stored layout is shown; usually [`Pane::shown_by_default`].
pub fn restore(stored: Option<&str>, default_shown: impl Fn(Pane) -> bool) -> Restore {
    let usable = stored.filter(|s| !panes(s).is_empty());
    let known: Vec<Pane> = usable.map_or_else(Vec::new, |s| {
        panes(s)
            .iter()
            .filter_map(|(n, _)| Pane::from_name(n))
            .collect()
    });
    let missing = Pane::ALL.into_iter().filter(|p| !known.contains(p));
    Restore {
        perspective: usable.map(str::to_owned),
        then_show: missing.map(|p| (p, default_shown(p))).collect(),
    }
}

/// `perspective` with the best size of each pane in `sizes` replaced by
/// the given `(width, height)`.
///
/// A perspective stores every pane's best size (`bestw`, `besth`) and
/// `wxAuiManager::LoadPerspective` restores it. For panes whose content
/// is laid out by the program, such as the toolbar and the position bar,
/// the stored size is that of the version that saved it; after the content
/// grew (a new button, a wider clock, issue #94) it would crop the new
/// content until the user resets the layout. So the GUI puts in the
/// current best sizes before loading. Everything else (place, docking,
/// visibility, sizes of panes the user resizes) is kept as stored.
pub fn with_best_sizes(perspective: &str, sizes: &[(Pane, (i32, i32))]) -> String {
    let Some(prefix) = PERSPECTIVE_PREFIXES
        .iter()
        .find(|p| perspective.starts_with(*p))
    else {
        return perspective.to_owned();
    };
    let body = &perspective[prefix.len()..];
    let mut out = String::from(*prefix);
    for part in split_unescaped(body, '|') {
        let fields = split_unescaped(&part, ';');
        let size = fields.iter().find_map(|f| match f.split_once('=') {
            Some(("name", v)) => Pane::from_name(&unescape(v))
                .and_then(|pane| sizes.iter().find(|(p, _)| *p == pane))
                .map(|(_, size)| *size),
            _ => None,
        });
        let fields: Vec<String> = match size {
            Some((w, h)) => fields
                .into_iter()
                .map(|f| match f.split_once('=') {
                    Some(("bestw", _)) => format!("bestw={w}"),
                    Some(("besth", _)) => format!("besth={h}"),
                    _ => f,
                })
                .collect(),
            None => fields,
        };
        out.push_str(&fields.join(";"));
        // wxWidgets ends every part, the last one too, with a `|`.
        out.push('|');
    }
    out
}

/// The panes in a perspective string: each name with whether it is shown.
/// Unknown names are kept (they may belong to a newer version).
pub fn panes(perspective: &str) -> Vec<(String, bool)> {
    let Some(body) = PERSPECTIVE_PREFIXES
        .iter()
        .find_map(|p| perspective.strip_prefix(p))
    else {
        return Vec::new();
    };
    split_unescaped(body, '|')
        .into_iter()
        .filter_map(|part| {
            let mut name = None;
            let mut shown = true;
            for field in split_unescaped(&part, ';') {
                match field.split_once('=') {
                    Some(("name", v)) => name = Some(unescape(v)),
                    Some(("state", v)) => {
                        shown = v.parse::<u64>().is_ok_and(|s| s & STATE_HIDDEN == 0);
                    }
                    _ => {}
                }
            }
            name.map(|n| (n, shown))
        })
        .collect()
}

/// Splits `s` at every `sep` not preceded by a backslash. The pieces keep
/// their escapes, so that splitting a piece again sees them.
fn split_unescaped(s: &str, sep: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            cur.push(c);
            if let Some(n) = chars.next() {
                cur.push(n);
            }
        } else if c == sep {
            parts.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

/// Removes the escaping backslashes of a value.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.extend(chars.next());
        } else {
            out.push(c);
        }
    }
    out
}

/// Size and state of the main window, remembered between runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowGeometry {
    /// Width in pixels (when not maximised).
    pub width: i32,
    /// Height in pixels (when not maximised).
    pub height: i32,
    /// Whether the window fills the screen.
    pub maximized: bool,
}

impl Default for WindowGeometry {
    fn default() -> Self {
        WindowGeometry {
            width: 1000,
            height: 600,
            maximized: false,
        }
    }
}

impl WindowGeometry {
    /// Smallest size restored: a stored size below it (a window shrunk to
    /// nothing, a damaged config) would hide the window.
    pub const MIN: (i32, i32) = (400, 250);

    /// The stored geometry made usable: too small sizes become the
    /// minimum.
    pub fn sanitized(self) -> Self {
        WindowGeometry {
            width: self.width.max(Self::MIN.0),
            height: self.height.max(Self::MIN.1),
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A perspective as wxWidgets 3.3.4 writes it (toolbar shown, track
    /// list closed).
    const SAVED: &str = "layout3|name=waveform;caption=;state=768;dir=5;layer=0;row=0;pos=0;prop=100000;bestw=200;besth=80;minw=200;minh=80;maxw=-1;maxh=-1;floatx=-1;floaty=-1;floatw=-1;floath=-1;floatw_cli=-1;floath_cli=-1|name=toolbar;caption=Toolbar;state=2108156;dir=1;layer=10;row=0;pos=0;prop=100000;bestw=328;besth=24;minw=-1;minh=-1;maxw=-1;maxh=-1;floatx=-1;floaty=-1;floatw=-1;floath=-1;floatw_cli=-1;floath_cli=-1|name=track_list;caption=Track list;state=6293502;dir=3;layer=0;row=0;pos=0;prop=100000;bestw=600;besth=160;minw=200;minh=60;maxw=-1;maxh=-1;floatx=-1;floaty=-1;floatw=-1;floath=-1;floatw_cli=-1;floath_cli=-1|dock_size(5,0,0)=202|";

    #[test]
    fn names_are_unique_and_stable() {
        // Stored layouts refer to these; renaming one loses the pane.
        let names: Vec<&str> = Pane::ALL.iter().map(|p| p.name()).collect();
        assert_eq!(
            names,
            [
                "toolbar",
                "minimap",
                "video",
                "positions",
                "track_list",
                "log"
            ]
        );
        for p in Pane::ALL {
            assert_eq!(Pane::from_name(p.name()), Some(p));
        }
        assert_eq!(Pane::from_name("waveform"), None);
    }

    #[test]
    fn reads_names_and_visibility() {
        assert_eq!(
            panes(SAVED),
            [
                ("waveform".to_string(), true),
                ("toolbar".to_string(), true),
                // 6293502 is even, but its bit 1 (hidden) is set.
                ("track_list".to_string(), false),
            ]
        );
    }

    #[test]
    fn reads_the_older_format_too() {
        let p = "layout2|name=toolbar;caption=Toolbar;state=2106102|dock_size(1,10,0)=32|";
        assert_eq!(panes(p), [("toolbar".to_string(), false)]);
    }

    #[test]
    fn escaped_delimiters_stay_in_values() {
        let p = r"layout2|name=a\|b\;c;caption=x\;y;state=0|dock_size(1,0,0)=3|";
        assert_eq!(panes(p), [("a|b;c".to_string(), true)]);
    }

    #[test]
    fn a_stored_layout_is_loaded_as_it_is() {
        let r = restore(Some(SAVED), |_| true);
        assert_eq!(r.perspective.as_deref(), Some(SAVED));
        // Saved before the minimap, the video pane, the position bar and
        // the log existed.
        assert_eq!(
            r.then_show,
            [
                (Pane::Minimap, true),
                (Pane::Video, true),
                (Pane::Positions, true),
                (Pane::Log, true)
            ]
        );
    }

    #[test]
    fn panes_newer_than_the_layout_get_their_default() {
        let old = "layout2|name=toolbar;caption=Toolbar;state=2106100|";
        let r = restore(Some(old), |p| p == Pane::TrackList);
        assert_eq!(r.perspective.as_deref(), Some(old));
        assert_eq!(
            r.then_show,
            [
                (Pane::Minimap, false),
                (Pane::Video, false),
                (Pane::Positions, false),
                (Pane::TrackList, true),
                (Pane::Log, false),
            ]
        );
    }

    #[test]
    fn without_a_usable_layout_every_pane_gets_its_default() {
        for stored in [
            None,
            Some(""),
            Some("garbage"),
            Some("layout9|name=toolbar"),
            Some("layout2|"),
        ] {
            let r = restore(stored, Pane::shown_by_default);
            assert_eq!(r.perspective, None, "{stored:?}");
            assert_eq!(
                r.then_show,
                Pane::ALL.map(|p| (p, p.shown_by_default())),
                "{stored:?}"
            );
        }
    }

    #[test]
    fn stored_best_sizes_of_program_laid_out_panes_are_replaced() {
        let fixed = with_best_sizes(
            SAVED,
            &[(Pane::Toolbar, (400, 30)), (Pane::Positions, (500, 40))],
        );
        // Only the toolbar's best size changed; the position bar is not in
        // this layout, and nothing else of it is touched.
        assert_eq!(
            fixed,
            SAVED.replace("bestw=328;besth=24", "bestw=400;besth=30")
        );
        assert_eq!(panes(&fixed), panes(SAVED));
    }

    #[test]
    fn best_sizes_keep_escapes_and_ignore_other_formats() {
        let p = r"layout2|name=a\|b;bestw=1;besth=2|name=positions;caption=x\;y;bestw=3;besth=4|dock_size(3,10,0)=40|";
        assert_eq!(
            with_best_sizes(p, &[(Pane::Positions, (30, 40))]),
            r"layout2|name=a\|b;bestw=1;besth=2|name=positions;caption=x\;y;bestw=30;besth=40|dock_size(3,10,0)=40|"
        );
        for other in ["", "garbage", "layout9|name=positions;bestw=1"] {
            assert_eq!(with_best_sizes(other, &[(Pane::Positions, (5, 5))]), other);
        }
    }

    #[test]
    fn tiny_windows_grow_to_the_minimum() {
        let g = WindowGeometry {
            width: 10,
            height: 5000,
            maximized: true,
        };
        assert_eq!(
            g.sanitized(),
            WindowGeometry {
                width: 400,
                height: 5000,
                maximized: true
            }
        );
        assert_eq!(
            WindowGeometry::default().sanitized(),
            WindowGeometry::default()
        );
    }
}
