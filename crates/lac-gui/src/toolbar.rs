//! The toolbar: a row of flat icon buttons for the most used menu
//! commands, shown as a dockable pane (see [`crate::panes`]).
//!
//! It is a panel of `wxBitmapButton`s rather than a `wxAuiToolBar`,
//! because wxdragon's AUI toolbar cannot show bitmaps yet and a
//! `wxToolBar` can only be the frame's own, undockable one.
//!
//! A click runs the same command as the menu item with the same id; the
//! caller binds that (see [`create`]). The one exception is the speed
//! selector ([`Tool::Speed`]), a drop-down list of the playback speeds:
//! the menu can only step through them one at a time.

use lac_app::i18n::tr;
use lac_playback::{STEPS, Speed};
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
    /// The playback speed selector, listing [`STEPS`]. At most one per
    /// toolbar.
    Speed {
        /// The tooltip.
        tip: &'static str,
    },
    /// A gap between groups of buttons.
    Gap,
}

/// The created toolbar.
#[derive(Clone, Copy)]
pub struct Toolbar {
    /// The pane's window.
    pub panel: Panel,
    /// The speed selector, if the tools have one. Its selection is the
    /// index into [`STEPS`]; [`Toolbar::show_speed`] keeps it current
    /// when the speed changes otherwise (keys, menu).
    pub speed: Option<Choice>,
}

impl Toolbar {
    /// Shows `speed` in the speed selector. Setting the selection from
    /// code does not send a selection event, so this cannot loop.
    pub fn show_speed(&self, speed: Speed) {
        if let Some(c) = &self.speed {
            c.set_selection(u32::try_from(speed.step_index()).unwrap_or(0));
        }
    }
}

/// Icon size in device independent pixels.
const ICON: i32 = 20;
/// Width of a [`Tool::Gap`], device independent.
const GAP: i32 = 10;

/// Creates the toolbar with `tools` as a child of `parent`. It calls
/// `on_click` with the command id whenever a button is clicked, and
/// `on_speed` with the index into [`STEPS`] when a speed is chosen.
pub fn create(
    parent: &Frame,
    tools: &[Tool],
    on_click: impl Fn(Id) + Clone + 'static,
    on_speed: impl Fn(usize) + 'static,
) -> Toolbar {
    let panel = Panel::builder(parent).build();
    let mut speed = None;
    // `on_speed` goes to the one selector there is.
    let mut on_speed = Some(on_speed);
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
                button.set_tooltip(&tr(tip));
                let on_click = on_click.clone();
                button.on_click(move |_| on_click(id));
                sizer.add(
                    &button,
                    0,
                    SizerFlag::All | SizerFlag::AlignCenterVertical,
                    2,
                );
            }
            Tool::Speed { tip } => {
                let Some(on_speed) = on_speed.take() else {
                    continue;
                };
                let labels: Vec<String> = (0..STEPS.len())
                    .map(|i| Speed::at_step(i, true).label())
                    .collect();
                let choice = Choice::builder(&panel)
                    .with_choices(labels)
                    .with_selection(u32::try_from(Speed::NORMAL.step_index()).ok())
                    .build();
                choice.set_tooltip(&tr(tip));
                choice.on_selection_changed(move |e| {
                    if let Some(i) = e.get_selection().and_then(|i| usize::try_from(i).ok()) {
                        on_speed(i);
                    }
                });
                sizer.add(
                    &choice,
                    0,
                    SizerFlag::All | SizerFlag::AlignCenterVertical,
                    2,
                );
                speed = Some(choice);
            }
            Tool::Gap => {
                sizer.add_spacer(panel.from_dip_int(GAP));
            }
        }
    }
    panel.set_sizer_and_fit(sizer, true);
    Toolbar { panel, speed }
}
