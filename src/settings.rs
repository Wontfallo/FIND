//! Persistent user settings (JSON in the platform config directory).

use crate::query::MatchMode;
use crate::util::{default_exclusions, default_roots};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum ThemePreset {
    /// Warm dark grays and browns (default).
    #[default]
    Graphite,
    /// Pure blacks.
    Carbon,
    /// The original dark navy.
    Navy,
}

impl ThemePreset {
    pub const ALL: [ThemePreset; 3] = [ThemePreset::Graphite, ThemePreset::Carbon, ThemePreset::Navy];
    pub fn label(self) -> &'static str {
        match self {
            ThemePreset::Graphite => "Graphite",
            ThemePreset::Carbon => "Carbon",
            ThemePreset::Navy => "Navy",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum AccentColor {
    #[default]
    Blue,
    Green,
    Amber,
    Violet,
}

impl AccentColor {
    pub const ALL: [AccentColor; 4] = [
        AccentColor::Blue,
        AccentColor::Green,
        AccentColor::Amber,
        AccentColor::Violet,
    ];
    pub fn label(self) -> &'static str {
        match self {
            AccentColor::Blue => "Blue",
            AccentColor::Green => "Green",
            AccentColor::Amber => "Amber",
            AccentColor::Violet => "Violet",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemePreset,
    pub accent: AccentColor,
    /// UI zoom factor (also adjustable live with Ctrl+= / Ctrl+-).
    pub ui_scale: f32,
    /// "HH:MM" 24h local time for a daily automatic rescan; empty = off.
    pub auto_rescan_time: String,
    pub roots: Vec<PathBuf>,
    pub exclusions: Vec<String>,
    pub match_mode: MatchMode,
    pub case_sensitive: bool,
    pub max_results: usize,
    pub show_preview: bool,
    pub watch_filesystem: bool,
    /// Closing the window hides to the system tray instead of quitting.
    pub minimize_to_tray: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: ThemePreset::default(),
            accent: AccentColor::default(),
            ui_scale: 1.0,
            auto_rescan_time: String::new(),
            roots: default_roots(),
            exclusions: default_exclusions(),
            match_mode: MatchMode::Substring,
            case_sensitive: false,
            max_results: 5_000,
            show_preview: true,
            watch_filesystem: true,
            minimize_to_tray: true,
        }
    }
}

fn settings_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "find", "FIND")
        .map(|d| d.config_dir().join("settings.json"))
}

impl Settings {
    pub fn load() -> Settings {
        settings_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = settings_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}
