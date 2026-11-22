//! Design tokens for Lenslet: precision-instrument dark and light surfaces
//! with one contrast-calibrated accent. The active look (appearance and
//! language) is a gpui [`Global`]: set when the language or the system
//! appearance changes, read by every render through [`Ui`].

use gpui::{App, Global, Hsla, Rgba, WindowAppearance, rgb};

use crate::prefs::{Lang, ThemeMode, UiPrefs};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

/// Everything a render needs to look right, copied out of the global once.
#[derive(Clone, Copy, Debug)]
pub struct Ui {
    pub p: Palette,
    pub appearance: Appearance,
    pub lang: Lang,
}

struct UiGlobal(Ui);

impl Global for UiGlobal {}

impl Ui {
    pub fn new(prefs: &UiPrefs, window: WindowAppearance) -> Self {
        let appearance = resolve(prefs.theme, window);
        Ui {
            p: palette(appearance),
            appearance,
            lang: prefs.lang,
        }
    }

    /// The current look. Falls back to dark defaults before [`Ui::install`].
    pub fn of(cx: &App) -> Ui {
        cx.try_global::<UiGlobal>()
            .map(|global| global.0)
            .unwrap_or_else(|| Ui::new(&UiPrefs::default(), WindowAppearance::Dark))
    }

    /// Publish a new look; returns whether anything changed.
    pub fn install(cx: &mut App, ui: Ui) -> bool {
        let changed = cx.try_global::<UiGlobal>().is_none_or(|old| {
            let old = old.0;
            old.appearance != ui.appearance || old.lang != ui.lang
        });
        if changed {
            cx.set_global(UiGlobal(ui));
        }
        changed
    }

    /// English or Chinese.
    pub fn t<'a>(&self, en: &'a str, zh: &'a str) -> &'a str {
        if self.lang == Lang::Zh { zh } else { en }
    }

    /// A [`crate::i18n::Tr`] in the current language.
    pub fn tr<'a>(&self, text: &'a crate::i18n::Tr) -> &'a str {
        self.t(&text.en, &text.zh)
    }

    pub fn row_height(&self) -> f32 {
        ROW_H
    }

    pub fn dark(&self) -> bool {
        self.appearance == Appearance::Dark
    }

    pub fn service_color(&self, label: &str) -> Rgba {
        service_color_for(label, self.appearance)
    }

    pub fn status_color(&self, status: u16) -> Rgba {
        let t = &self.p;
        if status == 0 {
            rgb(t.text_muted)
        } else if status >= 500 {
            rgb(t.red)
        } else if status >= 400 {
            rgb(t.red_4xx)
        } else if status >= 300 {
            rgb(t.amber)
        } else {
            rgb(t.green)
        }
    }

    pub fn latency_color(&self, ms: u64) -> Rgba {
        let t = &self.p;
        if ms >= 2_000 {
            rgb(t.red)
        } else if ms >= 500 {
            rgb(t.amber)
        } else {
            rgb(t.text_muted)
        }
    }

    /// Multi-layer elevation shadow for popovers, menus and modals.
    pub fn elevation(&self) -> Vec<gpui::BoxShadow> {
        let dark = self.dark();
        vec![
            gpui::BoxShadow {
                color: h(0x000000, if dark { 0.50 } else { 0.14 }),
                offset: gpui::point(gpui::px(0.0), gpui::px(12.0)),
                blur_radius: gpui::px(28.0),
                spread_radius: gpui::px(-4.0),
            },
            gpui::BoxShadow {
                color: h(0x000000, if dark { 0.30 } else { 0.08 }),
                offset: gpui::point(gpui::px(0.0), gpui::px(2.0)),
                blur_radius: gpui::px(6.0),
                spread_radius: gpui::px(0.0),
            },
        ]
    }
}

pub fn resolve(mode: ThemeMode, window: WindowAppearance) -> Appearance {
    match mode {
        ThemeMode::Dark => Appearance::Dark,
        ThemeMode::Light => Appearance::Light,
        ThemeMode::System => match window {
            WindowAppearance::Light | WindowAppearance::VibrantLight => Appearance::Light,
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Appearance::Dark,
        },
    }
}

/// The palette for an appearance.
pub fn palette(mode: Appearance) -> Palette {
    match mode {
        Appearance::Dark => DARK,
        Appearance::Light => LIGHT,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub canvas: u32,
    pub header: u32,
    pub toolbar: u32,
    pub table_head: u32,
    pub panel: u32,
    pub panel_raised: u32,
    pub code: u32,
    pub input: u32,
    pub input_focused: u32,

    pub row: u32,
    pub row_hover: u32,
    pub row_selected: u32,
    pub row_selected_hover: u32,
    pub mark_bg: u32,

    pub border: u32,
    pub border_subtle: u32,

    pub text: u32,
    pub text_soft: u32,
    pub text_body: u32,
    pub text_dim: u32,
    pub text_muted: u32,
    pub text_faint: u32,

    pub accent: u32,
    pub green: u32,
    pub amber: u32,
    pub red: u32,
    pub red_4xx: u32,
    pub purple: u32,
    pub orange: u32,
    pub blue: u32,
}

pub const DARK: Palette = Palette {
    canvas: 0x0A0C10,
    header: 0x0E1117,
    toolbar: 0x0C0F15,
    table_head: 0x0C0F14,
    panel: 0x0E1117,
    panel_raised: 0x11161E,
    code: 0x090C11,
    input: 0x0B0F15,
    input_focused: 0x0A0E14,

    row: 0x0E1117,
    row_hover: 0x131A24,
    row_selected: 0x152230,
    row_selected_hover: 0x192838,
    mark_bg: 0x131C27,

    border: 0x1E2632,
    border_subtle: 0x171E28,

    text: 0xE7EDF6,
    text_soft: 0xC5D0DF,
    text_body: 0xB9C6D8,
    text_dim: 0x8C99AC,
    text_muted: 0x68768B,
    text_faint: 0x4C586B,

    accent: 0x7DD3FC,
    green: 0x62D6A7,
    amber: 0xF5B76B,
    red: 0xFF7188,
    red_4xx: 0xFF8A9B,
    purple: 0xB79CFF,
    orange: 0xF3A95F,
    blue: 0x67A9FF,
};

pub const LIGHT: Palette = Palette {
    canvas: 0xF5F7FA,
    header: 0xEEF2F6,
    toolbar: 0xF4F7FA,
    table_head: 0xF0F4F8,
    panel: 0xFFFFFF,
    panel_raised: 0xFAFBFD,
    code: 0xF3F6FA,
    input: 0xFFFFFF,
    input_focused: 0xFFFFFF,

    row: 0xFFFFFF,
    row_hover: 0xF1F5FA,
    row_selected: 0xE6F2FE,
    row_selected_hover: 0xDAEBFB,
    mark_bg: 0xE2EEF9,

    border: 0xD2DCE7,
    border_subtle: 0xE3EAF2,

    text: 0x0F172A,
    text_soft: 0x1E293B,
    text_body: 0x334155,
    text_dim: 0x475569,
    text_muted: 0x5B6B82,
    text_faint: 0x75859B,

    accent: 0x0369A1,
    green: 0x047857,
    amber: 0xB45309,
    red: 0xDC2626,
    red_4xx: 0xBE123C,
    purple: 0x6D28D9,
    orange: 0xC2410C,
    blue: 0x1D4ED8,
};

// Layout rhythm: a 4/8-pt grid, one type scale.
pub const HEADER_H: f32 = 44.0;
pub const SUBBAR_H: f32 = 36.0;
pub const TABLE_HEAD_H: f32 = 28.0;
/// The splitter's grab area; only a 1px line is drawn.
pub const SPLITTER_HIT_W: f32 = 7.0;
pub const INSPECTOR_DEFAULT_W: f32 = 480.0;
pub const BODY_LINE_H: f32 = 18.0;

/// Type scale (px).
pub mod text {
    pub const XS: f32 = 10.0;
    pub const SM: f32 = 11.0;
    pub const MD: f32 = 12.0;
    pub const LG: f32 = 13.0;
    pub const XL: f32 = 15.0;
}

/// Calls table row height (two lines: operation, then target).
pub const ROW_H: f32 = 44.0;

// Calls table columns.
pub const ROW_PAD_X: f32 = 16.0;
pub const COL_TIME: f32 = 84.0;
pub const COL_SERVICE: f32 = 92.0;
pub const COL_STATUS: f32 = 56.0;
pub const COL_LATENCY: f32 = 112.0;
pub const COL_GAP: f32 = 12.0;

pub fn h(hex: u32, alpha: f32) -> Hsla {
    let mut color: Hsla = rgb(hex).into();
    color.a = alpha;
    color
}

pub fn tint(color: Rgba, alpha: f32) -> Hsla {
    let mut color: Hsla = color.into();
    color.a = alpha;
    color
}

pub fn service_color_for(label: &str, mode: Appearance) -> Rgba {
    let label = label.trim().to_ascii_uppercase();
    match mode {
        Appearance::Dark => match label.as_str() {
            "S3" => rgb(DARK.orange),
            "SQS" => rgb(DARK.purple),
            "DYNAMODB" => rgb(DARK.blue),
            "SNS" => rgb(0xF58AC2),
            "LAMBDA" => rgb(0xFFB26E),
            "CLOUDWATCH" => rgb(0x7BD8C8),
            "STS" => rgb(0xE0B76B),
            "IAM" => rgb(0xD6A2FF),
            "" | "AWS" => rgb(0x8B94A6),
            _ => {
                const PALETTE: [u32; 8] = [
                    0x7BD8C8, 0xF0A3C8, 0xA9D18E, 0xE89B7C, 0x8FB8FF, 0xE0B76B, 0xC7A6F2, 0x84C7A8,
                ];
                rgb(PALETTE[fnv_index(&label, PALETTE.len())])
            }
        },
        Appearance::Light => match label.as_str() {
            "S3" => rgb(0xC2410C),
            "SQS" => rgb(0x6D28D9),
            "DYNAMODB" => rgb(0x1D4ED8),
            "SNS" => rgb(0xBE185D),
            "LAMBDA" => rgb(0xB45309),
            "CLOUDWATCH" => rgb(0x0F766E),
            "STS" => rgb(0x92400E),
            "IAM" => rgb(0x7E22CE),
            "" | "AWS" => rgb(0x475569),
            _ => {
                const PALETTE: [u32; 8] = [
                    0x0F766E, 0xBE185D, 0x3F6212, 0x9A3412, 0x1D4ED8, 0x92400E, 0x6D28D9, 0x047857,
                ];
                rgb(PALETTE[fnv_index(&label, PALETTE.len())])
            }
        },
    }
}

fn fnv_index(label: &str, len: usize) -> usize {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in label.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash as usize % len
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(hex: u32) -> f64 {
        let chan = |shift: u32| -> f64 {
            let srgb = ((hex >> shift) & 0xff) as f64 / 255.0;
            if srgb <= 0.03928 {
                srgb / 12.92
            } else {
                ((srgb + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * chan(16) + 0.7152 * chan(8) + 0.0722 * chan(0)
    }

    fn contrast(a: u32, b: u32) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn unknown_services_get_a_stable_color() {
        for mode in [Appearance::Dark, Appearance::Light] {
            let kinesis = service_color_for("kinesis", mode);
            assert_eq!(kinesis, service_color_for("KINESIS", mode));
            assert_ne!(kinesis, service_color_for("", mode));
        }
    }

    #[test]
    fn light_and_dark_palettes_maintain_wcag_aa_contrast() {
        for token in [
            LIGHT.text,
            LIGHT.text_soft,
            LIGHT.text_body,
            LIGHT.text_dim,
            LIGHT.text_muted,
            LIGHT.green,
            LIGHT.amber,
            LIGHT.red,
            LIGHT.red_4xx,
            LIGHT.blue,
        ] {
            let ratio = contrast(token, LIGHT.panel);
            assert!(
                ratio >= 4.5,
                "token #{token:06x} has insufficient contrast {ratio:.2} on white"
            );
        }

        for (accent, panel) in [(DARK.accent, DARK.panel), (LIGHT.accent, LIGHT.panel)] {
            let ratio = contrast(accent, panel);
            assert!(
                ratio >= 4.5,
                "accent #{accent:06x} contrast {ratio:.2} < 4.5"
            );
        }
    }
}
