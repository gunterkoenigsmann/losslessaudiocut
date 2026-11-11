//! The main window's dockable panes, arranged by wxWidgets' AUI manager:
//! the waveform in the centre, the toolbar, the minimap, the position bar,
//! the track list, the log and the video preview around it.
//! The user can drag panes to another edge, float them, resize and close
//! them; the View menu shows and hides each one.
//!
//! The arrangement (AUI "perspective") and the window size are stored in
//! the config store when the window closes and restored at the next start.
//! Which panes exist, their stable names and how a stored arrangement is
//! read are decided in [`lac_app::layout`], where they are tested.

use lac_app::i18n::{n_, tr, tr_menu};
use lac_app::layout::{Pane, Restore, WindowGeometry};
use wxdragon::prelude::*;
use wxdragon::widgets::aui_manager::{AuiManager, PaneInfo};

use crate::options::config;

/// Key of the stored AUI perspective.
const PERSPECTIVE: &str = "/Layout/Perspective";
/// Keys of the stored window geometry.
const WIDTH: &str = "/Layout/Width";
const HEIGHT: &str = "/Layout/Height";
const MAXIMIZED: &str = "/Layout/Maximized";
/// Where the track list's visibility was stored before panes were
/// dockable; still the default for a layout that lacks the track list.
const LEGACY_TRACK_LIST: &str = "/View/TrackList";

/// Name of the centre pane (the waveform). It cannot be closed, so it has
/// no [`Pane`] of its own, but the perspective still names it.
const WAVEFORM: &str = "waveform";

/// First View menu id of the pane toggles; one per [`Pane::ALL`] entry.
/// Far above the other menu ids, so that adding commands never collides.
const ID_FIRST_PANE: Id = ID_HIGHEST + 100;

/// The View menu command that shows or hides `pane`.
pub fn menu_id(pane: Pane) -> Id {
    let i = Pane::ALL.iter().position(|&p| p == pane).unwrap_or(0);
    ID_FIRST_PANE + i as Id
}

/// The pane that View menu command `id` toggles.
pub fn pane_for(id: Id) -> Option<Pane> {
    let i = usize::try_from(id - ID_FIRST_PANE).ok()?;
    Pane::ALL.get(i).copied()
}

/// The View menu label of `pane`, with its keyboard shortcut.
pub fn menu_label(pane: Pane) -> String {
    match pane {
        Pane::Toolbar => tr_menu("Tool&bar"),
        Pane::Positions => tr_menu("&Selection and clock"),
        Pane::Minimap => tr_menu("&Minimap"),
        Pane::Log => tr_menu("&Log"),
        // The shortcut the track list had before it was a pane.
        Pane::TrackList => tr_menu("&Track list\tCtrl+L"),
        Pane::Video => tr_menu("&Video"),
    }
}

/// The status bar help of the View menu entry of `pane`.
pub fn menu_help(pane: Pane) -> String {
    tr(match pane {
        Pane::Toolbar => n_("Show the buttons for the most used commands"),
        Pane::Positions => n_("Show the selection's start, end and length, and the play clock"),
        Pane::Minimap => n_("Show the whole recording, with the part in the window framed"),
        Pane::Log => n_("Show every message of the status bar, with the time"),
        Pane::TrackList => n_("Show the tracks and their tags"),
        Pane::Video => n_("Show the video's picture at the cursor or while playing"),
    })
    .into_owned()
}

/// The stored window geometry, for building the frame.
pub fn stored_geometry() -> WindowGeometry {
    let c = config();
    let d = WindowGeometry::default();
    let int =
        |key, default: i32| i32::try_from(c.read_long(key, i64::from(default))).unwrap_or(default);
    WindowGeometry {
        width: int(WIDTH, d.width),
        height: int(HEIGHT, d.height),
        maximized: c.read_bool(MAXIMIZED, d.maximized),
    }
    .sanitized()
}

/// The AUI manager of the main window and the frame it manages. Copy, so
/// event handlers can hold it.
#[derive(Clone, Copy)]
pub struct Panes {
    aui: AuiManager,
    frame: Frame,
    /// Current best sizes of the panes whose content the program lays
    /// out; they override the ones in a stored layout (see
    /// [`lac_app::layout::with_best_sizes`]).
    fitted: [(Pane, (i32, i32)); 2],
}

/// The windows the panes hold.
pub struct Contents<'a> {
    /// The waveform, in the centre.
    pub waveform: &'a Panel,
    /// The toolbar, at the top.
    pub toolbar: &'a Panel,
    /// The minimap, under the toolbar.
    pub minimap: &'a Panel,
    /// The selection fields and play clock, at the bottom.
    pub positions: &'a Panel,
    /// The track list, above the positions.
    pub track_list: &'a Grid,
    /// The log, right of the track list.
    pub log: &'a TextCtrl,
    /// The video preview, right of the waveform.
    pub video: &'a Panel,
}

impl Panes {
    /// Puts the waveform in the centre of `frame`, docks the toolbar and
    /// under it the minimap at the top, and the track list and under it
    /// the positions at the bottom; the log goes right of the track list,
    /// the video right of the waveform. Call [`Panes::restore`] next.
    pub fn new(frame: &Frame, c: &Contents) -> Panes {
        let Contents {
            waveform,
            toolbar,
            minimap,
            positions,
            track_list,
            log,
            video,
        } = *c;
        let aui = AuiManager::builder(frame).build();
        aui.add_pane_with_info(
            waveform,
            PaneInfo::new()
                .with_name(WAVEFORM)
                .center_pane()
                .min_size(200, 80),
        );
        let tb = toolbar.get_best_size();
        aui.add_pane_with_info(
            toolbar,
            PaneInfo::new()
                .with_name(Pane::Toolbar.name())
                .with_caption(&tr(Pane::Toolbar.caption()))
                .toolbar_pane()
                .top()
                .best_size(tb.width, tb.height),
        );
        aui.add_pane_with_info(
            minimap,
            PaneInfo::new()
                .with_name(Pane::Minimap.name())
                .with_caption(&tr(Pane::Minimap.caption()))
                .top()
                .best_size(600, 56)
                .min_size(100, 24)
                .close_button(true),
        );
        let pb = positions.get_best_size();
        aui.add_pane_with_info(
            positions,
            PaneInfo::new()
                .with_name(Pane::Positions.name())
                .with_caption(&tr(Pane::Positions.caption()))
                .toolbar_pane()
                .bottom()
                // Outside the track list, next to the status bar.
                .layer(10)
                .best_size(pb.width, pb.height),
        );
        aui.add_pane_with_info(
            track_list,
            PaneInfo::new()
                .with_name(Pane::TrackList.name())
                .with_caption(&tr(Pane::TrackList.caption()))
                .bottom()
                .best_size(600, 160)
                .min_size(200, 60)
                .close_button(true)
                .maximize_button(true),
        );
        aui.add_pane_with_info(
            log,
            PaneInfo::new()
                .with_name(Pane::Log.name())
                .with_caption(&tr(Pane::Log.caption()))
                .bottom()
                .position(1)
                .best_size(400, 160)
                .min_size(150, 60)
                .close_button(true)
                .maximize_button(true),
        );
        aui.add_pane_with_info(
            video,
            PaneInfo::new()
                .with_name(Pane::Video.name())
                .with_caption(&tr(Pane::Video.caption()))
                .right()
                .best_size(360, 240)
                .min_size(120, 80)
                .close_button(true)
                .maximize_button(true),
        );
        let frame = *frame;
        let panes = Panes {
            aui,
            frame,
            fitted: [
                (Pane::Toolbar, (tb.width, tb.height)),
                (Pane::Positions, (pb.width, pb.height)),
            ],
        };
        // Keep the View menu's check marks right when the user closes a
        // pane with its own close button, or before the menu is shown.
        frame.on_menu_opened(move |e| {
            panes.sync_menu();
            e.skip(true);
        });
        panes
    }

    /// Restores the arrangement stored by [`Panes::save`] and shows the
    /// result.
    pub fn restore(&self) {
        let c = config();
        let stored = c.read_string(PERSPECTIVE, "");
        let legacy_track_list = c.read_bool(LEGACY_TRACK_LIST, true);
        let Restore {
            perspective,
            then_show,
        } = lac_app::layout::restore(Some(&stored), |p| match p {
            Pane::TrackList => legacy_track_list,
            p => p.shown_by_default(),
        });
        if let Some(p) = perspective {
            let p = lac_app::layout::with_best_sizes(&p, &self.fitted);
            self.aui.load_perspective(&p, false);
        }
        for (pane, shown) in then_show {
            self.aui.set_pane_shown(pane.name(), shown);
        }
        self.aui.update();
        self.sync_menu();
    }

    /// Stores the arrangement and the window size, for the next start.
    /// Failing to store them only means they are not remembered.
    pub fn save(&self) {
        let c = config();
        c.write_string(PERSPECTIVE, &self.aui.save_perspective());
        let maximized = self.frame.is_maximized();
        c.write_bool(MAXIMIZED, maximized);
        // A maximised window's size is the screen's; keep the one it
        // returns to.
        if !maximized && !self.frame.is_iconized() {
            let size = self.frame.get_size();
            c.write_long(WIDTH, i64::from(size.width));
            c.write_long(HEIGHT, i64::from(size.height));
        }
        c.flush(false);
    }

    /// Whether `pane` is shown.
    pub fn is_shown(&self, pane: Pane) -> bool {
        self.aui.is_pane_shown(pane.name())
    }

    /// Shows `pane` (if hidden).
    pub fn show(&self, pane: Pane) {
        if !self.is_shown(pane) {
            self.aui.set_pane_shown(pane.name(), true);
            self.aui.update();
            self.sync_menu();
        }
    }

    /// Shows `pane` if hidden, else hides it.
    pub fn toggle(&self, pane: Pane) {
        // Ask the manager rather than the menu's check mark: a keyboard
        // shortcut does not see a mark that is out of date after the user
        // closed the pane with its close button.
        self.aui.set_pane_shown(pane.name(), !self.is_shown(pane));
        self.aui.update();
        self.sync_menu();
    }

    /// Sets the View menu's check marks from what is shown.
    pub fn sync_menu(&self) {
        let Some(bar) = self.frame.get_menu_bar() else {
            return;
        };
        for pane in Pane::ALL {
            bar.check_item(menu_id(pane), self.is_shown(pane));
        }
    }

    /// Detaches the panes before the frame goes away, as wxWidgets asks.
    pub fn uninit(&self) {
        self.aui.uninit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pane_has_its_own_menu_command() {
        for pane in Pane::ALL {
            assert_eq!(pane_for(menu_id(pane)), Some(pane));
            assert!(!menu_label(pane).is_empty());
            assert!(!menu_help(pane).is_empty());
        }
        assert_eq!(pane_for(ID_FIRST_PANE - 1), None);
        assert_eq!(pane_for(ID_FIRST_PANE + Pane::ALL.len() as Id), None);
    }

    #[test]
    fn the_waveform_is_no_pane_one_can_close() {
        assert_eq!(Pane::from_name(WAVEFORM), None);
    }
}
