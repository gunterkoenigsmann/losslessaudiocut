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
}

impl Pane {
    /// Every pane, in the order of the View menu.
    pub const ALL: [Pane; 3] = [Pane::Toolbar, Pane::Positions, Pane::TrackList];

    /// The pane's name in the stored layout. Never change one: a layout
    /// stored by an earlier version would lose the pane.
    pub fn name(self) -> &'static str {
        match self {
            Pane::Toolbar => "toolbar",
            Pane::Positions => "positions",
            Pane::TrackList => "track_list",
        }
    }

    /// The pane's title (its caption, and its entry in the View menu).
    pub fn caption(self) -> &'static str {
        match self {
            Pane::Toolbar => "Toolbar",
            Pane::Positions => "Selection and clock",
            Pane::TrackList => "Track list",
        }
    }

    /// Whether the pane is shown when there is no stored layout.
    pub fn shown_by_default(self) -> bool {
        match self {
            Pane::Toolbar | Pane::Positions | Pane::TrackList => true,
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
        assert_eq!(names, ["toolbar", "positions", "track_list"]);
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
    fn a_complete_layout_is_loaded_as_it_is() {
        let r = restore(Some(SAVED), |_| true);
        assert_eq!(r.perspective.as_deref(), Some(SAVED));
        // Saved before the position bar existed.
        assert_eq!(r.then_show, [(Pane::Positions, true)]);
    }

    #[test]
    fn panes_newer_than_the_layout_get_their_default() {
        let old = "layout2|name=toolbar;caption=Toolbar;state=2106100|";
        let r = restore(Some(old), |p| p == Pane::TrackList);
        assert_eq!(r.perspective.as_deref(), Some(old));
        assert_eq!(
            r.then_show,
            [(Pane::Positions, false), (Pane::TrackList, true)]
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
