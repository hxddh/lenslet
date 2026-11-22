//! The window's few remembered choices, in `~/.lenslet/ui.json`: the
//! language, the window size, the inspector and output panes, and the last
//! project folder. Appearance follows the system (`LENSLET_THEME` overrides
//! it, for screenshots); the language can be forced with `LENSLET_LANG`.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// The appearance: the system's unless `LENSLET_THEME` says otherwise.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    En,
    Zh,
}

impl Default for Lang {
    fn default() -> Self {
        detect_system_lang()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiPrefs {
    pub lang: Lang,
    pub window_width: Option<f32>,
    pub window_height: Option<f32>,
    pub inspector_width: f32,
    pub inspector_open: bool,
    pub output_open: bool,
    pub output_height: f32,
    pub project_dir: Option<String>,
    #[serde(skip)]
    pub theme: ThemeMode,
}

impl Default for UiPrefs {
    fn default() -> Self {
        UiPrefs {
            lang: Lang::default(),
            window_width: None,
            window_height: None,
            inspector_width: crate::theme::INSPECTOR_DEFAULT_W,
            inspector_open: true,
            output_open: false,
            output_height: 240.0,
            project_dir: None,
            theme: ThemeMode::System,
        }
    }
}

impl UiPrefs {
    pub fn clamped_inspector_width(&self) -> f32 {
        self.inspector_width.clamp(320.0, 780.0)
    }
}

pub fn path(home: &Path) -> PathBuf {
    home.join(".lenslet/ui.json")
}

pub fn load(home: &Path) -> UiPrefs {
    crate::fsguard::assert_io_allowed();
    let mut prefs = fs::read_to_string(path(home))
        .ok()
        .and_then(|text| serde_json::from_str::<UiPrefs>(&text).ok())
        .unwrap_or_default();
    prefs.inspector_width = prefs.clamped_inspector_width();
    if let Some(forced) = env_lang() {
        prefs.lang = forced;
    }
    prefs.theme = env_theme().unwrap_or_default();
    prefs
}

/// Write the preferences. Blocking IO: the window calls it from the
/// background executor, debounced.
pub fn save(home: &Path, prefs: &UiPrefs) {
    crate::fsguard::assert_io_allowed();
    let target = path(home);
    if let Some(parent) = target.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(prefs) {
        let _ = fs::write(&target, format!("{text}\n"));
    }
}

fn detect_system_lang() -> Lang {
    if let Some(forced) = env_lang() {
        return forced;
    }
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(val) = std::env::var_os(var) {
            let lower = val.to_string_lossy().to_ascii_lowercase();
            if !lower.is_empty() && lower != "c" && !lower.starts_with("c.") {
                return if lower.starts_with("zh") {
                    Lang::Zh
                } else {
                    Lang::En
                };
            }
        }
    }
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleLanguages"])
        .output()
        && out.status.success()
    {
        let text = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
        if let Some(first) = text
            .lines()
            .map(|line| line.trim().trim_matches(['"', ',', '(', ')', ' ']))
            .find(|line| !line.is_empty())
        {
            return if first.starts_with("zh") {
                Lang::Zh
            } else {
                Lang::En
            };
        }
    }
    Lang::En
}

fn env_lang() -> Option<Lang> {
    let raw = std::env::var_os("LENSLET_LANG")?;
    let lower = raw.to_string_lossy().trim().to_ascii_lowercase();
    if lower.starts_with("zh") {
        Some(Lang::Zh)
    } else if lower.starts_with("en") {
        Some(Lang::En)
    } else {
        None
    }
}

fn env_theme() -> Option<ThemeMode> {
    let raw = std::env::var_os("LENSLET_THEME")?;
    match raw.to_string_lossy().trim().to_ascii_lowercase().as_str() {
        "light" => Some(ThemeMode::Light),
        "dark" => Some(ThemeMode::Dark),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_the_window_choices() {
        let dir = crate::testdata::scratch("prefs");
        let prefs = UiPrefs {
            lang: Lang::Zh,
            inspector_width: 520.0,
            inspector_open: false,
            output_open: true,
            output_height: 300.0,
            window_width: Some(1440.0),
            window_height: Some(900.0),
            project_dir: Some("/p".into()),
            ..UiPrefs::default()
        };
        save(&dir, &prefs);
        let text = fs::read_to_string(path(&dir)).unwrap();
        let keys: Vec<String> =
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text)
                .unwrap()
                .keys()
                .cloned()
                .collect();
        assert_eq!(
            keys,
            [
                "lang",
                "window_width",
                "window_height",
                "inspector_width",
                "inspector_open",
                "output_open",
                "output_height",
                "project_dir"
            ]
        );
        let parsed: UiPrefs = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, prefs);

        let mut out_of_range = parsed;
        out_of_range.inspector_width = 120.0;
        assert_eq!(out_of_range.clamped_inspector_width(), 320.0);
    }
}
