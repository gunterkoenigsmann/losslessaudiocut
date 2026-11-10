//! Edit > Settings (issue #103): a dialog of pages listed down its left
//! side, each with an icon, like wxMaxima's configuration dialog.
//!
//! Pages so far:
//! - **Detection**: whether the streams of a chained Ogg file are offered
//!   as track starts (issue #115), and what the pause and track detector
//!   looks for (issue #92).
//! - **Waveform**: how strongly tracks and the selection are coloured
//!   (issue #132).
//!
//! Changes show at once (the caller's `preview`), so the user sees what a
//! slider does; Cancel goes back to what was there before. The rules
//! (ranges, the selection staying ahead of the tints) are
//! `lac_app::appearance`'s; the dialog only shows them.

use std::cell::Cell;
use std::rc::Rc;

use std::ops::RangeInclusive;

use lac_app::appearance::{Appearance, SELECTION_RANGE, TRACK_TINT_RANGE};
use lac_app::detector_settings::{
    DetectorSettings, MIN_PAUSE_RANGE, MIN_TRACK_RANGE, TRACK_GAP_RANGE,
};
use wxdragon::prelude::*;

use crate::icons::Icon;
use crate::options::Options;

/// Size of the page icons in the list, in pixels.
const ICON: i32 = 32;
/// Width the explanations wrap at, in pixels.
const TEXT_WIDTH: i32 = 380;
/// Space around and between the controls, in pixels.
const GAP: i32 = 8;

/// Shows the dialog over `parent`, starting from `current`. Every change
/// is passed to `preview` right away; returns the options to keep on OK,
/// `None` on Cancel (the caller then previews `current` again).
pub fn show(
    parent: &dyn WxWidget,
    current: Options,
    preview: Rc<dyn Fn(Options)>,
) -> Option<Options> {
    let dialog = Dialog::builder(parent, "Settings")
        .with_style(DialogStyle::DefaultDialogStyle | DialogStyle::ResizeBorder)
        .build();
    let chosen = Rc::new(Cell::new(current));
    let book = Notebook::builder(&dialog)
        .with_style(NotebookStyle::Left)
        .build();
    let images = ImageList::new(ICON, ICON, true, 2);
    for icon in [Icon::Detection, Icon::Waveform] {
        let size = Size::new(ICON, ICON);
        if let Some(bitmap) =
            BitmapBundle::from_svg_text(&icon.svg(), size).and_then(|b| b.get_bitmap(size))
        {
            images.add_bitmap(&bitmap);
        }
    }
    book.set_image_list(images);
    let detection = detection_page(&book, &chosen, &preview);
    book.add_page(&detection, "Detection", true, Some(0));
    let waveform = waveform_page(&book, &chosen, &preview);
    book.add_page(&waveform, "Waveform", false, Some(1));

    let buttons = StdDialogButtonSizerBuilder::new().build();
    // wxDialog closes itself for these two ids, with them as the result.
    buttons.add_button(
        &Button::builder(&dialog)
            .with_id(ID_OK)
            .with_label("OK")
            .build(),
    );
    buttons.add_button(
        &Button::builder(&dialog)
            .with_id(ID_CANCEL)
            .with_label("Cancel")
            .build(),
    );
    buttons.realize();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();
    sizer.add(&book, 1, SizerFlag::Expand | SizerFlag::All, GAP);
    sizer.add_sizer(&buttons, 0, SizerFlag::Expand | SizerFlag::All, GAP);
    dialog.set_sizer_and_fit(sizer, true);

    let result = (dialog.show_modal() == ID_OK).then(|| chosen.get());
    dialog.destroy();
    result
}

/// The "Detection" page.
fn detection_page(
    book: &Notebook,
    chosen: &Rc<Cell<Options>>,
    preview: &Rc<dyn Fn(Options)>,
) -> Panel {
    let page = Panel::builder(book).build();
    let chained = CheckBox::builder(&page)
        .with_label("Suggest chained streams as tracks")
        .with_value(chosen.get().chained_links)
        .build();
    let note = StaticText::builder(&page)
        .with_label(
            "A chained Ogg file is several streams one after the other; each \
             one's start is offered as a track start. Turn this off for \
             recorded web radio that changes its title every few seconds.",
        )
        .build();
    note.wrap(TEXT_WIDTH);
    {
        let (chosen, preview) = (Rc::clone(chosen), Rc::clone(preview));
        chained.on_toggled(move |e| {
            let mut o = chosen.get();
            o.chained_links = e.is_checked();
            chosen.set(o);
            preview(o);
        });
    }
    let sizer = BoxSizer::builder(Orientation::Vertical).build();
    sizer.add(&chained, 0, SizerFlag::All, GAP);
    sizer.add(
        &note,
        0,
        SizerFlag::Left | SizerFlag::Right | SizerFlag::Bottom,
        GAP,
    );
    detector_controls(&page, &sizer, chosen, preview);
    page.set_sizer_and_fit(sizer, true);
    page
}

/// The detector settings (issue #92) on the Detection page, added to
/// `sizer`: three times and two switches, and a button back to the
/// defaults. Each change runs the detector again at once (`preview`), so
/// the suggestion marks in the waveform move while the dialog is open.
fn detector_controls(
    page: &Panel,
    sizer: &BoxSizer,
    chosen: &Rc<Cell<Options>>,
    preview: &Rc<dyn Fn(Options)>,
) {
    let d = chosen.get().detector;
    // A time in seconds, shown with `digits` decimals.
    let time = |range: &RangeInclusive<u32>, scale: f64, value: u32, step: f64, digits: u32| {
        let spin = SpinCtrlDouble::builder(page)
            .with_min_value(f64::from(*range.start()) / scale)
            .with_max_value(f64::from(*range.end()) / scale)
            .with_initial_value(f64::from(value) / scale)
            .with_increment(step)
            .build();
        spin.set_digits(digits);
        spin
    };
    let pause = time(&MIN_PAUSE_RANGE, 1000.0, d.min_pause_ms, 0.1, 1);
    let gap = time(&TRACK_GAP_RANGE, 1000.0, d.track_gap_ms, 0.5, 1);
    let track = time(&MIN_TRACK_RANGE, 1.0, d.min_track_secs, 5.0, 0);
    let check = |label: &str, value: bool| {
        CheckBox::builder(page)
            .with_label(label)
            .with_value(value)
            .build()
    };
    let timbre = check(
        "A change of sound without a pause starts a new track",
        d.timbre,
    );
    let key = check(
        "A change of key or harmony starts a new track (finds few, adds false ones in songs)",
        d.key,
    );
    let defaults = Button::builder(page)
        .with_label("Detection defaults")
        .build();

    // Shows `s` in the controls.
    let show = move |s: DetectorSettings| {
        pause.set_value(f64::from(s.min_pause_ms) / 1000.0);
        gap.set_value(f64::from(s.track_gap_ms) / 1000.0);
        track.set_value(f64::from(s.min_track_secs));
        timbre.set_value(s.timbre);
        key.set_value(s.key);
    };
    // Takes `s` (settled by its rules), shows and previews it.
    let take: Rc<dyn Fn(DetectorSettings)> = {
        let (chosen, preview) = (Rc::clone(chosen), Rc::clone(preview));
        Rc::new(move |s: DetectorSettings| {
            let s = s.normalized();
            show(s);
            let mut o = chosen.get();
            o.detector = s;
            chosen.set(o);
            preview(o);
        })
    };
    // What the controls say now.
    let read = move || DetectorSettings {
        min_pause_ms: ms(pause.get_value()),
        track_gap_ms: ms(gap.get_value()),
        min_track_secs: track.get_value().round().max(0.0) as u32,
        timbre: timbre.get_value(),
        key: key.get_value(),
    };
    for spin in [pause, gap, track] {
        let take = Rc::clone(&take);
        spin.on_value_changed(move |_| take(read()));
    }
    for c in [timbre, key] {
        let take = Rc::clone(&take);
        c.on_toggled(move |_| take(read()));
    }
    {
        let take = Rc::clone(&take);
        defaults.on_click(move |_| take(DetectorSettings::default()));
    }

    let grid = FlexGridSizer::builder(3, 2)
        .with_gap(Size::new(GAP, GAP))
        .build();
    for (text, spin) in [
        ("Shortest pause to suggest (s)", pause),
        ("Pause that starts a new track (s)", gap),
        ("Shortest track (s)", track),
    ] {
        let label = StaticText::builder(page).with_label(text).build();
        grid.add(&label, 0, SizerFlag::AlignCenterVertical, 0);
        grid.add(&spin, 0, SizerFlag::Expand, 0);
    }
    sizer.add_sizer(&grid, 0, SizerFlag::All, GAP);
    sizer.add(&timbre, 0, SizerFlag::Left | SizerFlag::Right, GAP);
    sizer.add(&key, 0, SizerFlag::All, GAP);
    sizer.add(&defaults, 0, SizerFlag::All, GAP);
}

/// `secs` seconds as whole milliseconds; never negative.
fn ms(secs: f64) -> u32 {
    (secs * 1000.0).round().clamp(0.0, f64::from(u32::MAX)) as u32
}

/// The "Waveform" page: two sliders in percent. Moving one may move the
/// other, as the selection stays ahead of the tints.
fn waveform_page(
    book: &Notebook,
    chosen: &Rc<Cell<Options>>,
    preview: &Rc<dyn Fn(Options)>,
) -> Panel {
    let page = Panel::builder(book).build();
    let a = chosen.get().appearance;
    let slider = |range: &RangeInclusive<u8>, value: u8| {
        Slider::builder(&page)
            .with_min_value(i32::from(*range.start()))
            .with_max_value(i32::from(*range.end()))
            .with_value(i32::from(value))
            .with_style(SliderStyle::Labels)
            .with_size(Size::new(TEXT_WIDTH, -1))
            .build()
    };
    let tint = slider(&TRACK_TINT_RANGE, a.track_tint);
    let selection = slider(&SELECTION_RANGE, a.selection);
    let label = |text: &str| StaticText::builder(&page).with_label(text).build();
    let tint_label = label("Track colours (percent; 0 turns them off)");
    let selection_label = label("Selection (percent; stays ahead of the track colours)");

    // Either slider: take both values, let the rules settle them, show
    // the result in both sliders and preview it.
    let update: Rc<dyn Fn()> = {
        let (chosen, preview) = (Rc::clone(chosen), Rc::clone(preview));
        Rc::new(move || {
            let mut o = chosen.get();
            o.appearance = Appearance {
                track_tint: u8::try_from(tint.get_value()).unwrap_or(0),
                selection: u8::try_from(selection.get_value()).unwrap_or(0),
            }
            .normalized();
            selection.set_value(i32::from(o.appearance.selection));
            chosen.set(o);
            preview(o);
        })
    };
    for s in [tint, selection] {
        let update = Rc::clone(&update);
        s.on_slider(move |_| update());
    }

    let sizer = BoxSizer::builder(Orientation::Vertical).build();
    for (l, s) in [(tint_label, tint), (selection_label, selection)] {
        sizer.add(
            &l,
            0,
            SizerFlag::Left | SizerFlag::Right | SizerFlag::Top,
            GAP,
        );
        sizer.add(&s, 0, SizerFlag::Expand | SizerFlag::All, GAP);
    }
    page.set_sizer_and_fit(sizer, true);
    page
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_seconds_become_whole_milliseconds() {
        assert_eq!(ms(1.5), 1500);
        assert_eq!(ms(0.0004), 0);
        assert_eq!(ms(-3.0), 0);
        assert_eq!(ms(1e12), u32::MAX);
    }
}
