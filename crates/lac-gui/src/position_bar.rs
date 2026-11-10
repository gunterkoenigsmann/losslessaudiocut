//! The position bar: selection start, end and length as editable fields,
//! and a large play clock, as a dockable pane (see [`crate::panes`]).
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
    clock: StaticText,
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
        // Created with the widest label it can show, so that its size and
        // the panel's fit (and with that the dock pane's best size) leave
        // room for it; see `CLOCK_WIDEST`. The real label follows below.
        let clock = StaticText::builder(&panel).with_label(CLOCK_WIDEST).build();
        let font = Font::builder()
            .with_point_size(CLOCK_POINTS)
            .with_family(FontFamily::Teletype)
            .with_weight(FontWeight::Bold)
            .build();
        if let Some(font) = font {
            clock.set_font(&font);
        }
        // Keep that room: a static text resizes itself to every new
        // label, and a later layout of the bar must not shrink the clock to
        // a short label and crop the next, longer one (issue #94).
        clock.set_min_size(clock.get_best_size());
        clock.set_tooltip(&tr("Where playback is, or where it starts (source time)"));
        sizer.add(
            &clock,
            0,
            SizerFlag::AlignCenterVertical | SizerFlag::Right,
            8,
        );
        panel.set_sizer_and_fit(sizer, true);
        clock.set_label("0:00.000");
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
        ] {
            let want = p.get(field);
            if !ctrl.has_focus() && ctrl.get_value() != want {
                // Unlike `set_value`, sends no text event.
                ctrl.change_value(want);
            }
        }
        if self.clock.get_label() != p.clock {
            self.clock.set_label(&p.clock);
        }
    }
}
