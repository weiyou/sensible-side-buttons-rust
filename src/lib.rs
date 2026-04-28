//! sensible-side-buttons-rust
//!
//! Rust port of Alexei Baboulevitch's SensibleSideButtons (GPL-2.0).
//! See `sensible-side-buttons/SideButtonFixer/AppDelegate.m` and
//! `sensible-side-buttons/External/TouchEvents.c` for the original.

#[cfg(not(target_os = "macos"))]
compile_error!("sensible-side-buttons-rust only supports macOS");

pub mod gesture;
pub mod permissions;
pub mod settings;
pub mod tap;
pub mod ui;

use std::sync::Arc;

/// Process-wide application state shared between the menu UI and the
/// event-tap callback. Held as `&'static AppState` via `Box::leak` in
/// `main.rs` — single-process lifetime, accessed only on the main thread
/// after construction.
pub struct AppState {
    /// Persisted user preferences. Read on cold paths only (toggle handlers,
    /// startup); the hot-path tap callback reads from `tap_state` instead.
    pub settings: parking_lot::Mutex<settings::Settings>,

    /// Atomic mirror of the bool fields of `settings`, plus the live
    /// `CFMachPortRef` of the running tap. Read directly by the
    /// CGEventTap callback — must never block.
    pub tap_state: Arc<tap::TapState>,
}

impl AppState {
    pub fn persist(&self) {
        let snapshot = self.settings.lock().clone();
        if let Err(e) = snapshot.save() {
            eprintln!("[ssb] failed to save settings: {e}");
        }
    }
}
