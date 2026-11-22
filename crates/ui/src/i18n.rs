//! English and 中文. Every string the window draws goes through
//! [`crate::theme::Ui::t`] (two literals) or a [`Tr`] built with [`tr!`];
//! the test at the bottom fails when an English-only label sneaks into a
//! view. AWS and HTTP identifiers (`s3:GetObject`, `NoSuchKey`, `curl`,
//! `lenslet.lock`) are data and stay as they are.

use crate::prefs::Lang;

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

/// The English noun for `n` permissions (Chinese needs no plural).
pub fn permissions(n: usize) -> &'static str {
    if n == 1 { "permission" } else { "permissions" }
}

pub fn more_count(n: usize, lang: Lang) -> String {
    match lang {
        Lang::Zh => format!("还有 {n} 项"),
        Lang::En => format!("+{n} more"),
    }
}

/// The core's one-line hints (`lenslet_core::problem::hint`), in Chinese.
/// An unknown hint is shown as recorded.
pub fn hint(hint: &str, lang: Lang) -> &str {
    if lang == Lang::En {
        return hint;
    }
    const ZH: [(&str, &str); 7] = [
        (
            "The SigV4 signature did not verify",
            "SigV4 签名校验失败：请求在签名后被修改，或密钥不是模拟器的。",
        ),
        (
            "The emulator does not know these credentials",
            "模拟器不认识这组凭证。请使用模拟器的凭证文件（lenslet run 会设置），不要用真实 AWS 密钥。",
        ),
        (
            "Region mismatch",
            "区域不匹配：请求签名的区域与资源所在区域不同。",
        ),
        (
            "Denied by the emulator's IAM evaluation",
            "被模拟器对该主体的 IAM 评估拒绝。",
        ),
        (
            "The emulator does not implement this operation",
            "模拟器未实现此操作，或 SDK 发送了未知的操作。",
        ),
        (
            "The resource does not exist in the emulator",
            "模拟器中不存在该资源。它是否在本次运行中、在同一区域里创建过？",
        ),
        (
            "Lenslet could not reach the emulator",
            "Lenslet 无法连接到模拟器。它是否在该端口上运行？",
        ),
    ];
    ZH.iter()
        .find(|(en, _)| hint.starts_with(en) || hint.contains(en))
        .map_or(hint, |(_, zh)| zh)
}

/// `⌘K` on macOS, `Ctrl+K` elsewhere. Modifier glyphs in `key` (`⇧`, `⌥`,
/// `⏎`, `⌫`) are spelled out off macOS, so Linux reads `Ctrl+Shift+A`.
pub fn shortcut(key: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("⌘{key}")
    } else {
        format!("Ctrl+{}", spell_glyphs(key))
    }
}

/// A bare key hint (`⏎`, `⌫`) in the platform's spelling.
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
        .replace('⌫', "Backspace")
}

/// A message in both languages, built where the words are known (an error,
/// a toast) and shown with [`crate::theme::Ui::tr`].
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

    #[cfg(test)]
    pub fn get(&self, lang: Lang) -> &str {
        match lang {
            Lang::En => &self.en,
            Lang::Zh => &self.zh,
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
            assert_eq!(shortcut("⇧A"), "Ctrl+Shift+A");
            assert_eq!(shortcut("J"), "Ctrl+J");
            assert_eq!(key("⏎"), "Enter");
            assert_eq!(key("⌫"), "Backspace");
        }
    }

    #[test]
    fn tr_formats_both_languages() {
        let n = 3;
        let msg = tr!("{n} calls", "{n} 次调用");
        assert_eq!(msg.en, "3 calls");
        assert_eq!(msg.get(Lang::Zh), "3 次调用");
    }

    #[test]
    fn every_core_hint_has_a_chinese_version() {
        let codes = [
            ("SignatureDoesNotMatch", 403),
            ("InvalidAccessKeyId", 403),
            ("AuthorizationHeaderMalformed", 400),
            ("AccessDenied", 403),
            ("NotImplemented", 501),
            ("NoSuchKey", 404),
        ];
        for (code, status) in codes {
            let en = lenslet_core::problem::hint(code, status).expect(code);
            assert_ne!(hint(en, Lang::Zh), en, "{code}: {en}");
        }
        let unreachable = lenslet_core::problem::unreachable("http://x", "refused");
        let en = unreachable.hint.unwrap();
        assert_ne!(hint(&en, Lang::Zh), en);
    }

    /// Every label a view draws is bilingual: a string literal that looks
    /// like an English label (a capitalised word, or a badge in capitals)
    /// must sit inside `ui.t(…)`, `tr!(…)` or `t(…)` — on its own line or the
    /// line before, as rustfmt splits long calls.
    #[test]
    fn views_render_no_english_only_labels() {
        let sources = [
            ("app/mod.rs", include_str!("app/mod.rs")),
            ("app/calls.rs", include_str!("app/calls.rs")),
            ("app/inspector.rs", include_str!("app/inspector.rs")),
            ("app/access.rs", include_str!("app/access.rs")),
            ("app/output.rs", include_str!("app/output.rs")),
            ("app/header.rs", include_str!("app/header.rs")),
            ("app/widgets.rs", include_str!("app/widgets.rs")),
            ("app/bodyview.rs", include_str!("app/bodyview.rs")),
            ("app/session.rs", include_str!("app/session.rs")),
            ("search.rs", include_str!("search.rs")),
            ("daemon.rs", include_str!("daemon.rs")),
            ("feed.rs", include_str!("feed.rs")),
            ("main.rs", include_str!("main.rs")),
        ];
        // Identifiers and formats that are data, not words.
        let allowed = [
            "LensletApp",
            "Lenslet",
            "Search",
            "Body",
            "Field",
            "Window",
            "Help",
            "IAM JSON",
            "Ctrl+",
            "GET",
            "Shift+",
            "Alt+",
            "Enter",
            "Backspace",
            "HOME",
            "USERPROFILE",
            "Language: English",
            "PATH",
            "SHELL",
        ];
        let mut offenders = Vec::new();
        for (file, text) in sources {
            let text = text.split("#[cfg(test)]\nmod tests").next().unwrap_or(text);
            let lines: Vec<&str> = text.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") || code.starts_with("#[") {
                    continue;
                }
                let prev = if index > 0 { lines[index - 1] } else { "" };
                let wrapped = |l: &str| {
                    l.contains("ui.t(")
                        || l.contains("tr!(")
                        || l.contains("t(\"")
                        || l.trim_end().ends_with("t(")
                        || l.contains("Tr::new(")
                };
                if wrapped(line) || wrapped(prev) {
                    continue;
                }
                for literal in string_literals(line) {
                    if looks_like_english_label(&literal)
                        && !allowed.iter().any(|ok| literal == *ok)
                    {
                        offenders.push(format!("{file}:{}: {literal:?}", index + 1));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "English-only labels (wrap them in ui.t / tr!):\n{}",
            offenders.join("\n")
        );
    }

    fn string_literals(line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut chars = line.char_indices().peekable();
        while let Some((_, c)) = chars.next() {
            if c == '\'' {
                // A char literal ('"') is not the start of a string.
                let rest: String = chars.clone().take(3).map(|(_, c)| c).collect();
                if rest.starts_with("\"'") || rest.starts_with("\\\"'") {
                    chars.next();
                    chars.next();
                }
                continue;
            }
            if c != '"' {
                continue;
            }
            let mut literal = String::new();
            let mut escaped = false;
            for (_, c) in chars.by_ref() {
                if escaped {
                    escaped = false;
                    literal.push(c);
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    break;
                } else {
                    literal.push(c);
                }
            }
            out.push(literal);
        }
        out
    }

    /// `NOT IN LOCK` (a badge) or `Accept into lock` (a label).
    fn looks_like_english_label(text: &str) -> bool {
        let badge = text.len() >= 4
            && text.starts_with(|c: char| c.is_ascii_uppercase())
            && text.chars().all(|c| c.is_ascii_uppercase() || c == ' ')
            && text.contains(|c: char| c.is_ascii_uppercase());
        let words = text.split_whitespace().count() >= 2
            && text.starts_with(|c: char| c.is_ascii_uppercase())
            && text
                .chars()
                .all(|c| c.is_ascii_alphabetic() || " ,.…'?!:-".contains(c));
        let word = text.len() >= 3
            && text.starts_with(|c: char| c.is_ascii_uppercase())
            && text[1..].chars().all(|c| c.is_ascii_lowercase());
        badge || words || word
    }

    #[test]
    fn the_label_detector_catches_badges_and_labels() {
        assert!(looks_like_english_label("NOT IN LOCK"));
        assert!(looks_like_english_label("Accept into lock"));
        assert!(looks_like_english_label("Calls"));
        assert!(!looks_like_english_label("s3:GetObject"));
        assert!(!looks_like_english_label("lenslet.lock"));
        assert!(!looks_like_english_label("{} ms"));
        assert_eq!(string_literals(r#"x("a", '"', "b")"#), ["a", "b"]);
    }
}
