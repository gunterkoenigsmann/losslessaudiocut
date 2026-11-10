//! Edit > Settings (issue #103): a dialog of pages listed down its left
//! side, each with an icon, like wxMaxima's configuration dialog.
//!
//! Pages so far:
//! - **Detection**: whether the streams of a chained Ogg file are offered
//!   as track starts (issue #115).
//! - **Waveform**: how strongly tracks and the selection are coloured
//!   (issue #132).
//!
//! Changes show at once (the caller's `preview`), so the user sees what a
//! slider does; Cancel goes back to what was there before. The rules
//! (ranges, the selection staying ahead of the tints) are
//! `lac_app::appearance`'s; the dialog only shows them.

use std::cell::Cell;
use std::rc::Rc;

use lac_app::appearance::{Appearance, SELECTION_RANGE, TRACK_TINT_RANGE};
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
    page.set_sizer_and_fit(sizer, true);
    page
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
    let slider = |range: &std::ops::RangeInclusive<u8>, value: u8| {
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
