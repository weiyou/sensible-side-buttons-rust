//! Persisted user preferences, mirroring the four `SBF*` user defaults
//! from the original `AppDelegate.m:126–131`:
//!
//!   `SBFWasEnabled`   → [`Settings::enabled`]
//!   `SBFMouseDown`    → [`Settings::mouse_down`]
//!   `SBFSwapButtons`  → [`Settings::swap_buttons`]
//!   `SBFDonated`      → not modelled (donation gating dropped)
//!
//! Stored as TOML at `~/Library/Application Support/SensibleSideButtons/settings.toml`
//! rather than `NSUserDefaults` — keeps the format human-readable and
//! detaches us from the bundle identifier.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Whether the event tap is currently swallowing M4/M5 events.
    pub enabled: bool,
    /// `true` = fire the swipe on mouse-DOWN (snappy, default).
    /// `false` = fire on mouse-UP (matches default macOS button behavior).
    pub mouse_down: bool,
    /// Swap which physical button maps to back vs forward.
    pub swap_buttons: bool,
}

impl Default for Settings {
    fn default() -> Self {
        // Same defaults as the original (`AppDelegate.m:126–131`).
        Self {
            enabled: true,
            mouse_down: true,
            swap_buttons: false,
        }
    }
}

impl Settings {
    pub fn config_dir() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join("Library/Application Support/SensibleSideButtons")
    }

    pub fn path() -> PathBuf {
        Self::config_dir().join("settings.toml")
    }

    pub fn load() -> Self {
        match fs::read_to_string(Self::path()) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                eprintln!("[ssb] settings.toml parse error, using defaults: {e}");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        fs::create_dir_all(Self::config_dir())?;
        let body = toml::to_string_pretty(self)
            .map_err(std::io::Error::other)?;
        fs::write(Self::path(), body)
    }
}
