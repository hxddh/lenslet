//! Selective UI localization (`En` / `Zh`).
//!
//! Only window chrome, settings, buttons and empty-state guidance are
//! translated. AWS, IAM and HTTP terms (`Service`, `Operation`, `Status`,
//! `Headers`, `Action`, `Resource`, `curl`, `lenslet.lock`, flags) stay in
//! canonical English. The active language is part of the [`crate::theme::Ui`]
//! global; strings are picked with `ui.t(en, zh)`.

use crate::prefs::{AccentColor, Density, Lang, ThemeMode};

pub fn theme_label(mode: ThemeMode, lang: Lang) -> &'static str {
    match (mode, lang) {
        (ThemeMode::System, Lang::En) => "System",
        (ThemeMode::Light, Lang::En) => "Light",
        (ThemeMode::Dark, Lang::En) => "Dark",
        (ThemeMode::System, Lang::Zh) => "跟随系统",
        (ThemeMode::Light, Lang::Zh) => "浅色",
        (ThemeMode::Dark, Lang::Zh) => "深色",
    }
}

pub fn accent_label(accent: AccentColor, lang: Lang) -> &'static str {
    match (accent, lang) {
        (AccentColor::Sky, Lang::En) => "Sky",
        (AccentColor::Indigo, Lang::En) => "Indigo",
        (AccentColor::Violet, Lang::En) => "Violet",
        (AccentColor::Emerald, Lang::En) => "Emerald",
        (AccentColor::Amber, Lang::En) => "Amber",
        (AccentColor::Rose, Lang::En) => "Rose",
        (AccentColor::Sky, Lang::Zh) => "天蓝",
        (AccentColor::Indigo, Lang::Zh) => "靛蓝",
        (AccentColor::Violet, Lang::Zh) => "紫罗兰",
        (AccentColor::Emerald, Lang::Zh) => "翠绿",
        (AccentColor::Amber, Lang::Zh) => "琥珀",
        (AccentColor::Rose, Lang::Zh) => "玫红",
    }
}

pub fn density_label(density: Density, lang: Lang) -> &'static str {
    match (density, lang) {
        (Density::Comfortable, Lang::En) => "Comfortable",
        (Density::Compact, Lang::En) => "Compact",
        (Density::Comfortable, Lang::Zh) => "舒适",
        (Density::Compact, Lang::Zh) => "紧凑",
    }
}

pub fn copied(item: &str) -> Tr {
    crate::tr!("Copied {item}", "已复制 {item}")
}

pub fn calls_count(n: usize, lang: Lang) -> String {
    match (n, lang) {
        (_, Lang::Zh) => format!("{n} 次调用"),
        (1, Lang::En) => "1 call".to_string(),
        (n, Lang::En) => format!("{n} calls"),
    }
}

pub fn hits_count(n: usize, lang: Lang) -> String {
    match lang {
        Lang::Zh => format!("{n} 处匹配"),
        Lang::En => format!("{n} hits"),
    }
}

pub fn not_in_lock_badge(n: usize, lang: Lang) -> String {
    match lang {
        Lang::Zh => format!("+{n} 未入锁"),
        Lang::En => format!("+{n} not in lock"),
    }
}

pub fn more_count(n: usize, lang: Lang) -> String {
    match lang {
        Lang::Zh => format!("+{n} 更多"),
        Lang::En => format!("+{n} more"),
    }
}

/// `⌘K` on macOS, `Ctrl+K` elsewhere.
/// Modifier glyphs in `key` (`⇧`, `⌥`, `⏎`) become `Shift+`, `Alt+` and
/// `Enter` off macOS, so Linux reads `Ctrl+Shift+N`.
pub fn shortcut(key: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("⌘{key}")
    } else {
        format!("Ctrl+{}", spell_glyphs(key))
    }
}

/// A bare key hint (`⏎`, `Esc`) in the platform's spelling.
pub fn key(key: &str) -> String {
    if cfg!(target_os = "macos") {
        key.to_string()
    } else {
        spell_glyphs(key)
    }
}

fn spell_glyphs(key: &str) -> String {
    key.replace('⇧', "Shift+")
        .replace('⌥', "Alt+")
        .replace('⏎', "Enter")
}

/// A message in both languages, built where the words are known (a daemon or
/// replay error, a toast) and shown with [`crate::theme::Ui::tr`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tr {
    pub en: String,
    pub zh: String,
}

/// The English text, for logs and test messages.
impl std::fmt::Display for Tr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.en)
    }
}

impl Tr {
    pub fn new(en: impl Into<String>, zh: impl Into<String>) -> Self {
        Tr {
            en: en.into(),
            zh: zh.into(),
        }
    }
}

/// Two format strings, one per language: `tr!("{n} calls", "{n} 次调用")`.
#[macro_export]
macro_rules! tr {
    ($en:literal, $zh:literal $(,)?) => {
        $crate::i18n::Tr::new(format!($en), format!($zh))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_spell_modifiers_off_macos() {
        if !cfg!(target_os = "macos") {
            assert_eq!(shortcut("⇧N"), "Ctrl+Shift+N");
            assert_eq!(shortcut("⇧⏎"), "Ctrl+Shift+Enter");
            assert_eq!(shortcut("J"), "Ctrl+J");
            assert_eq!(key("⏎"), "Enter");
        }
    }

    #[test]
    fn tr_formats_both_languages() {
        let n = 3;
        let msg = tr!("{n} calls", "{n} 次调用");
        assert_eq!(msg.en, "3 calls");
        assert_eq!(msg.zh, "3 次调用");
    }
}
