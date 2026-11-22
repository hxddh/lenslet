//! Embedded crisp 16×16 vector SVG icon set (`gpui::AssetSource`).
//!
//! Eliminates low-res `PathBuilder` line approximations with pixel-aligned
//! geometric vector icons rendered natively by GPUI's SVG rasterizer.

use std::borrow::Cow;

use gpui::{AssetSource, Hsla, Result, SharedString, Svg, prelude::*, px, svg};

pub struct Assets;

const ICONS: &[(&str, &str)] = &[
    (
        "icons/search.svg",
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><circle cx="7" cy="7" r="4.5"/><path d="M10.4 10.4L14 14"/></svg>"#,
    ),
    (
        "icons/chevron-down.svg",
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" fill="none" stroke="black" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4.2 6.2L8 10l3.8-3.8"/></svg>"#,
    ),
    (
        "icons/menu.svg",
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round"><path d="M2.5 4.2h11M2.5 8h11M2.5 11.8h11"/></svg>"#,
    ),
];

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, data)| Cow::Borrowed(data.as_bytes())))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}

pub fn icon(path: &'static str, size_px: f32, color: impl Into<Hsla>) -> Svg {
    svg()
        .path(path)
        .size(px(size_px))
        .flex_shrink_0()
        .text_color(color.into())
}
