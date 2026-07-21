//! Persistent user settings (JSON in the platform config directory).

use crate::query::MatchMode;
use crate::util::{default_exclusions, default_roots};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
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
