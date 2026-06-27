//! Owns the live profile windows. Materializes them from config and (Task 7)
//! applies reconciliations. Impure (Tauri + AppKit) — verified at checkpoints.

use crate::plan::{window_specs, WindowSpec};
use crate::registry::{Registry, TabDto};
use crate::surface::PixelRect;
use std::collections::HashMap;
use tauri::{AppHandle, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use warden_config::Config;

/// Initial surface rect: offset by the 160px sidebar so the surface never
/// overlaps it before the first JS rect report arrives. (Matches the spike.)
const INITIAL_RECT: PixelRect = PixelRect { x: 160.0, y: 0.0, width: 740.0, height: 600.0 };

#[derive(serde::Serialize, Clone)]
pub struct InitDto {
    pub name: String,
    pub colour: String,
    pub tabs: Vec<TabDto>,
}

pub struct WindowState {
    pub window: WebviewWindow,
    pub registry: Registry,
    pub name: String,
    pub colour: String,
}

pub struct WindowManager {
    pub windows: HashMap<String, WindowState>, // key = Tauri label
    pub names: HashMap<String, String>,        // profile name -> label
    pub last_good: Config,
}

impl WindowManager {
    pub fn new() -> Self {
        WindowManager {
            windows: HashMap::new(),
            names: HashMap::new(),
            last_good: Config { profiles: Vec::new() },
        }
    }

    /// Build one Tauri window for `spec`, mount its tabs, activate the first.
    /// Returns the new `WindowState` (caller inserts it + wires events).
    pub fn build_window(&self, app: &AppHandle, spec: &WindowSpec) -> WindowState {
        let window =
            WebviewWindowBuilder::new(app, &spec.label, WebviewUrl::App("index.html".into()))
                .title(&spec.name)
                .inner_size(900.0, 600.0)
                .transparent(true)
                .build()
                .expect("build profile window");

        let ns_window = window.ns_window().expect("ns_window") as *mut std::os::raw::c_void;

        let mut registry = Registry::new(ns_window, INITIAL_RECT);
        for t in &spec.tabs {
            registry.add(&t.spec, t.keep_alive);
        }
        if let Some(first) = spec.tabs.first() {
            registry.activate(&first.spec.id);
        }

        WindowState { window, registry, name: spec.name.clone(), colour: spec.colour.clone() }
    }

    /// Materialize every profile as a window. Sets `last_good = config`.
    pub fn materialize(&mut self, app: &AppHandle, config: Config) {
        for spec in window_specs(&config) {
            let state = self.build_window(app, &spec);
            self.names.insert(spec.name.clone(), spec.label.clone());
            self.windows.insert(spec.label.clone(), state);
        }
        self.last_good = config;
    }

    pub fn init_dto(&self, label: &str) -> Option<InitDto> {
        self.windows.get(label).map(|ws| InitDto {
            name: ws.name.clone(),
            colour: ws.colour.clone(),
            tabs: ws.registry.tab_dtos(),
        })
    }
}
