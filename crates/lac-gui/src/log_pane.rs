//! The log pane: every message the status bar shows, with the time, in a
//! read-only text control (hidden until the user opens it from the View
//! menu). [`lac_app::log::Log`] decides what goes in.
//!
//! Messages come from many places, some of them while the window state is
//! borrowed, so the pane lives in a thread-local of the GUI thread rather
//! than in that state.

use std::cell::RefCell;

use lac_app::log::{Log, Update};
use wxdragon::DateTime;
use wxdragon::prelude::*;

/// Lines kept; the oldest half goes when there are more.
const MAX_LINES: usize = 2000;

thread_local! {
    /// The pane's control and its lines, once created.
    static PANE: RefCell<Option<(TextCtrl, Log)>> = const { RefCell::new(None) };
}

/// Creates the pane's text control as a child of `parent`.
pub fn create(parent: &Frame) -> TextCtrl {
    let ctrl = TextCtrl::builder(parent)
        .with_style(TextCtrlStyle::MultiLine | TextCtrlStyle::ReadOnly | TextCtrlStyle::DontWrap)
        .build();
    PANE.with(|p| *p.borrow_mut() = Some((ctrl, Log::new(MAX_LINES))));
    ctrl
}

/// Adds `text` to the log, if the pane exists.
pub fn append(text: &str) {
    let now = DateTime::now();
    let time = (now.hour(), now.minute(), now.second());
    PANE.with(|p| {
        let Some((ctrl, log)) = &mut *p.borrow_mut() else {
            return;
        };
        match log.push(time, text) {
            Update::None => {}
            // Appending scrolls to the end, so the newest line shows.
            Update::Append(line) => ctrl.append_text(&line),
            Update::Replace(all) => {
                ctrl.change_value("");
                ctrl.append_text(&all);
            }
        }
    });
}
