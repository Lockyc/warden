//! warden's side of shell-core's shared window layer: the [`WindowHost`] the managed
//! [`WindowSet`] builds and tears down each window's terminals through, and through which the
//! window form's changes reach the config.

use crate::ManagerState;
use shell_core::windows::plan::{Slot, WindowDecl};
use shell_core::windows::{WindowHost, WindowSet, WindowWrite};
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};
use warden_config::edit::{self, At};
use warden_config::resolve::DEFAULT_COLOUR;
use warden_config::Colour;

pub(crate) struct WardenHost;

impl WindowHost for WardenHost {
    /// Build `slot`'s window from `last_good` under the set's label, its remembered tabs keyed by
    /// the window's launch label.
    fn build(&self, app: &AppHandle, slot: &Slot) -> tauri::Result<WebviewWindow> {
        let key = app
            .state::<WindowSet>()
            .launch_label(&slot.label)
            .unwrap_or_else(|| slot.label.clone());
        let st = app.state::<ManagerState>();
        let mut m = st.lock();
        let spec = m
            .spec_for(&slot.label, &slot.decl.title)
            .ok_or(tauri::Error::WindowNotFound)?;
        m.build_window(app, &spec, key)
    }

    fn closed(&self, app: &AppHandle, label: &str) {
        app.state::<ManagerState>().lock().closed(label);
        // During ⌘Q every window is being destroyed and the menu dies with the app.
        if !crate::manager::is_quitting(app) {
            let _ = crate::rebuild_menu(app);
        }
    }

    /// Rename or recolour in place: the terminals keep running; the banner repaints.
    fn retitled(&self, app: &AppHandle, label: &str, old: &str, decl: &WindowDecl) {
        let colour = decl
            .colour
            .as_deref()
            .and_then(|c| Colour::parse(c).ok())
            .unwrap_or(DEFAULT_COLOUR);
        let st = app.state::<ManagerState>();
        let dto = {
            let mut m = st.lock();
            m.retitle(label, old, &decl.title, colour);
            m.init_dto(label)
        };
        if let Some(dto) = dto {
            let _ = app.emit_to(label, "warden:refresh", dto);
        }
    }

    fn write(&self, _app: &AppHandle, write: &WindowWrite) -> Result<(), String> {
        let path = warden_config::config_path();
        match write {
            WindowWrite::Add { title, colour } => edit::add_window(&path, title, colour.as_deref()),
            WindowWrite::Set {
                title,
                new_title,
                colour,
            } => edit::edit_window(
                &path,
                title,
                new_title.as_deref(),
                colour.as_ref().map(Option::as_deref),
            ),
            WindowWrite::Remove { title } => edit::remove(
                &path,
                At {
                    window: title,
                    group: None,
                    tab: None,
                },
            ),
        }
        .map_err(|e| e.to_string())
    }

    /// Every sidebar shows a failed window edit — the same channel as a config that failed to load.
    fn edit_failed(&self, app: &AppHandle, message: &str) {
        eprintln!("warden: window edit failed: {message}");
        let _ = app.emit("warden:error", message.to_string());
    }

    fn changed(&self, app: &AppHandle) {
        let _ = crate::rebuild_menu(app);
    }
}
