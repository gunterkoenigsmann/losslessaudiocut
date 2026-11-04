//! The toolbar: a row of flat icon buttons for the most used menu
//! commands, shown as a dockable pane (see [`crate::panes`]).
//!
//! It is a panel of `wxBitmapButton`s rather than a `wxAuiToolBar`,
//! because wxdragon's AUI toolbar cannot show bitmaps yet and a
//! `wxToolBar` can only be the frame's own, undockable one.
//!
//! A click runs the same command as the menu item with the same id; the
//! caller binds that (see [`create`]).

use wxdragon::prelude::*;

use crate::icons::Icon;

/// One entry of the toolbar.
#[derive(Debug, Clone, Copy)]
pub enum Tool {
    /// A button running the menu command `id`.
    Button {
        /// The menu command.
        id: Id,
        /// Its picture.
        icon: Icon,
        /// The tooltip: what it does and its keyboard shortcut.
        tip: &'static str,
    },
    /// A gap between groups of buttons.
    Gap,
}

/// Icon size in device independent pixels.
const ICON: i32 = 20;
/// Width of a [`Tool::Gap`], device independent.
const GAP: i32 = 10;

/// Creates the toolbar with `tools` as a child of `parent` and calls
/// `on_click` with the command id whenever a button is clicked.
pub fn create(parent: &Frame, tools: &[Tool], on_click: impl Fn(Id) + Clone + 'static) -> Panel {
    let panel = Panel::builder(parent).build();
    let sizer = BoxSizer::builder(Orientation::Horizontal).build();
    for tool in tools {
        match *tool {
            Tool::Button { id, icon, tip } => {
                let bundle = BitmapBundle::from_svg_text(&icon.svg(), Size::new(ICON, ICON));
                let Some(bitmap) = bundle.and_then(|b| b.get_bitmap_for(&panel)) else {
                    // Without an SVG reader (never with the bundled
                    // wxWidgets) the menu still has the command.
                    continue;
                };
                let button = BitmapButton::builder(&panel)
                    .with_id(id)
                    .with_bitmap(Some(bitmap))
                    .with_style(BitmapButtonStyle::BorderNone)
                    .build();
                button.set_tooltip(tip);
                let on_click = on_click.clone();
                button.on_click(move |_| on_click(id));
                sizer.add(
                    &button,
                    0,
                    SizerFlag::All | SizerFlag::AlignCenterVertical,
                    2,
                );
            }
            Tool::Gap => {
                sizer.add_spacer(panel.from_dip_int(GAP));
            }
        }
    }
    panel.set_sizer_and_fit(sizer, true);
    panel
}
