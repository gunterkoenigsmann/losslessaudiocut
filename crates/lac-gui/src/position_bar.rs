//! The position bar: selection start, end and length as editable fields,
//! and a large play clock, editable too (issue #89), as a dockable pane
//! (see [`crate::panes`]).
//!
//! What it shows and what typing into it does is decided in
//! [`lac_app::positions`]; this module only makes and fills the widgets.

use lac_app::i18n::tr;
use lac_app::positions::{CLOCK_WIDEST, Field, Positions};
use wxdragon::prelude::*;

/// Width of a time field, wide enough for `h:mm:ss.mmm`, device
/// independent.
const FIELD_WIDTH: i32 = 100;
/// Point size of the clock.
const CLOCK_POINTS: i32 = 16;

/// The bar's widgets; cheap handles, so it is `Copy`.
#[derive(Clone, Copy)]
pub struct PositionBar {
    panel: Panel,
    start: TextCtrl,
    end: TextCtrl,
    length: TextCtrl,
    clock: TextCtrl,
}

impl PositionBar {
    /// Creates the bar as a child of `parent`. When the user presses Enter
    /// in a field, `on_enter` gets the field and its text.
    pub fn create(parent: &Frame, on_enter: impl Fn(Field, String) + Clone + 'static) -> Self {
        let panel = Panel::builder(parent).build();
        let sizer = BoxSizer::builder(Orientation::Horizontal).build();
        let field = |label: &str, tip: &str, which: Field| {
            let text = StaticText::builder(&panel).with_label(label).build();
            sizer.add(
                &text,
                0,
                SizerFlag::AlignCenterVertical | SizerFlag::Left,
                8,
            );
            let ctrl = TextCtrl::builder(&panel)
                .with_style(TextCtrlStyle::ProcessEnter)
                .with_size(Size::new(panel.from_dip_int(FIELD_WIDTH), -1))
                .build();
            ctrl.set_tooltip(tip);
            let on_enter = on_enter.clone();
            ctrl.on_enter_pressed(move |_| on_enter(which, ctrl.get_value()));
            sizer.add(&ctrl, 0, SizerFlag::AlignCenterVertical | SizerFlag::All, 4);
            ctrl
        };
        // The tooltips say what Enter does (see `lac_app::positions::apply`).
        let start = field(
            &tr("Start"),
            &tr("Selection start; type a time (s, m:ss, h:mm:ss or @frames) and press Enter"),
            Field::Start,
        );
        let end = field(
            &tr("End"),
            &tr("Selection end; type a time and press Enter to select up to there"),
            Field::End,
        );
        let length = field(
            &tr("Length"),
            &tr("Selection length; type a time and press Enter to change the end"),
            Field::Length,
        );
        sizer.add_spacer(panel.from_dip_int(16));
        // A text field in a large font, so a time can be typed into it.
        let clock = TextCtrl::builder(&panel)
            .with_style(TextCtrlStyle::ProcessEnter)
            .build();
        let font = Font::builder()
            .with_point_size(CLOCK_POINTS)
            .with_family(FontFamily::Teletype)
            .with_weight(FontWeight::Bold)
            .build();
        if let Some(font) = font {
            clock.set_font(&font);
        }
        // Room for the widest time it can show (see `CLOCK_WIDEST`) plus
        // the field's border, fixed: the bar is not laid out anew while
        // playback runs, and no time is cropped (issue #94).
        let text = clock.get_text_extent(CLOCK_WIDEST);
        let best = clock.get_best_size();
        let size = Size::new(
            text.width + panel.from_dip_int(16),
            best.height.max(text.height),
        );
        clock.set_min_size(size);
        clock.set_size(size);
        clock.set_tooltip(&tr(
            "Where playback is, or where it starts (source time); type a time and press Enter to go there",
        ));
        {
            let on_enter = on_enter.clone();
            clock.on_enter_pressed(move |_| on_enter(Field::Clock, clock.get_value()));
        }
        sizer.add(
            &clock,
            0,
            SizerFlag::AlignCenterVertical | SizerFlag::Right,
            8,
        );
        panel.set_sizer_and_fit(sizer, true);
        clock.change_value("0:00.000");
        PositionBar {
            panel,
            start,
            end,
            length,
            clock,
        }
    }

    /// The panel to dock.
    pub fn panel(&self) -> Panel {
        self.panel
    }

    /// Shows `p`. A field the user is typing in keeps its text, and
    /// nothing is set that did not change, so a refresh from the timer
    /// costs no redraw.
    pub fn show(&self, p: &Positions) {
        for (ctrl, field) in [
            (self.start, Field::Start),
            (self.end, Field::End),
            (self.length, Field::Length),
            (self.clock, Field::Clock),
        ] {
            let want = p.get(field);
            if !ctrl.has_focus() && ctrl.get_value() != want {
                // Unlike `set_value`, sends no text event.
                ctrl.change_value(want);
            }
        }
    }
}
