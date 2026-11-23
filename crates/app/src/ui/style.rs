//! The look: the platform's fonts, its accent, its semantic colors, and the
//! few small parts every view uses (icons, toolbar buttons, section titles,
//! badges). Colors are adaptive: the platform switches them with its
//! appearance, so dark mode needs no code.

use nsh_base::theme;
use nsh_core::{platform::Os, prelude::*};

/// The platform's UI font and monospaced font.
pub const UI: &str = "system-ui";
pub const MONO: &str = "ui-monospace";

/// Body text, as each platform sets it.
pub fn body() -> Points {
    match os() {
        Os::MacOs => px(13.0),
        Os::Windows => px(14.0),
        _ => px(14.0),
    }
}

pub fn small() -> Points {
    match os() {
        Os::MacOs => px(11.0),
        _ => px(12.0),
    }
}

/// The platform's accent: macOS's and Windows' default blue, GNOME's blue.
pub fn accent() -> AdaptiveColor {
    match os() {
        Os::MacOs => AdaptiveColor::rgb(0x007aff, 0x0a84ff),
        Os::Windows => AdaptiveColor::rgb(0x005fb8, 0x60cdff),
        _ => AdaptiveColor::rgb(0x3584e4, 0x78aeed),
    }
}

pub fn red() -> AdaptiveColor {
    AdaptiveColor::rgb(0xd70015, 0xff6961)
}
pub fn green() -> AdaptiveColor {
    AdaptiveColor::rgb(0x248a3d, 0x30d158)
}
pub fn orange() -> AdaptiveColor {
    AdaptiveColor::rgb(0xc93400, 0xffb340)
}

/// The window's content background, and the sidebar's where the platform
/// draws no material behind it.
pub fn content_bg() -> AdaptiveColor {
    match os() {
        Os::MacOs => AdaptiveColor::rgb(0xffffff, 0x1e1e1e),
        Os::Windows => AdaptiveColor::rgb(0xfafafa, 0x202020),
        _ => AdaptiveColor::rgb(0xfafafb, 0x222226),
    }
}
pub fn sidebar_bg() -> AdaptiveColor {
    match os() {
        // The window's sidebar material shows through.
        Os::MacOs | Os::Windows => AdaptiveColor::rgb(0x000000, 0x000000).opacity(0.0),
        _ => AdaptiveColor::rgb(0xebebed, 0x2e2e32),
    }
}
pub fn chrome_bg() -> AdaptiveColor {
    match os() {
        Os::MacOs => AdaptiveColor::rgb(0xf6f6f6, 0x2a2a2a),
        Os::Windows => AdaptiveColor::rgb(0xf3f3f3, 0x2b2b2b),
        _ => AdaptiveColor::rgb(0xebebed, 0x2e2e32),
    }
}
pub fn separator() -> AdaptiveColor {
    AdaptiveColor::rgb(0xdcdcdc, 0x3a3a3a)
}
pub fn secondary_text() -> AdaptiveColor {
    AdaptiveColor::rgb(0x6e6e73, 0x98989d)
}
pub fn tertiary_text() -> AdaptiveColor {
    AdaptiveColor::rgb(0xa1a1a6, 0x6e6e73)
}
pub fn hover_bg() -> AdaptiveColor {
    AdaptiveColor::rgb(0x000000, 0xffffff).opacity(0.06)
}
pub fn fill_bg() -> AdaptiveColor {
    AdaptiveColor::rgb(0x000000, 0xffffff).opacity(0.045)
}

/// The rounding of buttons and fields.
pub fn radius() -> Points {
    match os() {
        Os::MacOs => px(6.0),
        Os::Windows => px(4.0),
        _ => px(6.0),
    }
}

/// A line icon, drawn in the current text color of the appearance.
pub fn icon(name: Icon, size: f32) -> impl IntoElement {
    let paths = name.paths();
    svg(move || {
        let stroke = if dark() { "#e5e5e7" } else { "#3a3a3c" };
        svg_uri(paths, stroke)
    })
    .w(px(size))
    .h(px(size))
}

/// A line icon in a given color.
pub fn icon_colored(name: Icon, size: f32, color: AdaptiveColor) -> impl IntoElement {
    let paths = name.paths();
    svg(move || {
        let c = if dark() { color.dark } else { color.light };
        let rgba: Rgba = c.into();
        let hex = format!(
            "#{:02x}{:02x}{:02x}",
            (rgba.r * 255.0) as u8,
            (rgba.g * 255.0) as u8,
            (rgba.b * 255.0) as u8
        );
        svg_uri(paths, &hex)
    })
    .w(px(size))
    .h(px(size))
}

fn svg_uri(paths: &str, stroke: &str) -> String {
    let doc = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24' fill='none' stroke='{stroke}' \
         stroke-width='1.7' stroke-linecap='round' stroke-linejoin='round'>{paths}</svg>"
    );
    let mut uri = String::from("data:image/svg+xml,");
    for b in doc.bytes() {
        match b {
            b'<' => uri.push_str("%3C"),
            b'>' => uri.push_str("%3E"),
            b'#' => uri.push_str("%23"),
            b'"' => uri.push_str("%22"),
            b'%' => uri.push_str("%25"),
            _ => uri.push(b as char),
        }
    }
    uri
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Sidebar,
    Inspector,
    Terminal,
    Search,
    Clear,
    NewAccount,
    Menu,
    Overview,
    Calls,
    Shield,
    Bucket,
    Table,
    Queue,
    Topic,
    Function,
    Logs,
    Key,
    Parameter,
    Rule,
    StateMachine,
    Stream,
    Lock,
    Role,
    Warning,
    Folder,
    Restart,
    Close,
}

impl Icon {
    fn paths(self) -> &'static str {
        match self {
            Icon::Sidebar => {
                "<rect x='3' y='4' width='18' height='16' rx='2.5'/><path d='M9 4v16'/>"
            }
            Icon::Inspector => {
                "<rect x='3' y='4' width='18' height='16' rx='2.5'/><path d='M15 4v16'/>"
            }
            Icon::Terminal => {
                "<rect x='3' y='4' width='18' height='16' rx='2.5'/><path d='m7 9 3 3-3 3M12 15h5'/>"
            }
            Icon::Search => "<circle cx='11' cy='11' r='6.5'/><path d='m20 20-4.2-4.2'/>",
            Icon::Clear => "<circle cx='12' cy='12' r='8.5'/><path d='m6 6 12 12'/>",
            Icon::NewAccount => "<path d='M4 12a8 8 0 1 0 2.4-5.7L4 8.5'/><path d='M4 4v4.5h4.5'/>",
            Icon::Menu => "<path d='M4 7h16M4 12h16M4 17h16'/>",
            Icon::Overview => {
                "<rect x='3.5' y='3.5' width='7' height='7' rx='1.5'/><rect x='13.5' y='3.5' width='7' height='7' rx='1.5'/><rect x='3.5' y='13.5' width='7' height='7' rx='1.5'/><rect x='13.5' y='13.5' width='7' height='7' rx='1.5'/>"
            }
            Icon::Calls => {
                "<path d='M8 6h12M8 12h12M8 18h12'/><circle cx='4' cy='6' r='.6'/><circle cx='4' cy='12' r='.6'/><circle cx='4' cy='18' r='.6'/>"
            }
            Icon::Shield => {
                "<path d='M12 3 4.5 6v5.5c0 4.6 3.2 8.2 7.5 9.5 4.3-1.3 7.5-4.9 7.5-9.5V6z'/><path d='m9 12 2 2 4-4'/>"
            }
            Icon::Bucket => {
                "<path d='M4 7h16l-1.6 12.2a2 2 0 0 1-2 1.8H7.6a2 2 0 0 1-2-1.8z'/><ellipse cx='12' cy='7' rx='8' ry='2.5'/>"
            }
            Icon::Table => {
                "<ellipse cx='12' cy='5.5' rx='7.5' ry='2.5'/><path d='M4.5 5.5v13c0 1.4 3.4 2.5 7.5 2.5s7.5-1.1 7.5-2.5v-13'/><path d='M4.5 12c0 1.4 3.4 2.5 7.5 2.5s7.5-1.1 7.5-2.5'/>"
            }
            Icon::Queue => {
                "<path d='M3.5 13.5 6 5h12l2.5 8.5V19a1.5 1.5 0 0 1-1.5 1.5H5A1.5 1.5 0 0 1 3.5 19z'/><path d='M3.5 13.5H9l1 2h4l1-2h5.5'/>"
            }
            Icon::Topic => {
                "<path d='M4 10v4h3l6 4V6L7 10z'/><path d='M16.5 9a4 4 0 0 1 0 6M19 6.5a7.5 7.5 0 0 1 0 11'/>"
            }
            Icon::Function => "<path d='M6 20c3 0 4-3 5-8s2-8 5-8'/><path d='M8 10h8'/>",
            Icon::Logs => {
                "<path d='M6 3.5h8l4.5 4.5v12.5H6z'/><path d='M14 3.5V8h4.5M9 12h6M9 15.5h6'/>"
            }
            Icon::Key => {
                "<circle cx='8' cy='15' r='4'/><path d='m11 12 8.5-8.5M16 7l2.5 2.5M14 9l2 2'/>"
            }
            Icon::Parameter => {
                "<path d='M4 7h10M18 7h2M4 17h4M12 17h8'/><circle cx='16' cy='7' r='2'/><circle cx='10' cy='17' r='2'/>"
            }
            Icon::Rule => "<path d='M13 3 5 13.5h6L10 21l8-10.5h-6z'/>",
            Icon::StateMachine => {
                "<circle cx='6' cy='6' r='2.5'/><circle cx='18' cy='18' r='2.5'/><circle cx='18' cy='6' r='2.5'/><path d='M8.5 6h7M18 8.5v7M7.8 7.8l8.4 8.4'/>"
            }
            Icon::Stream => "<path d='M3 12h3.5l2.5-6 4 12 2.5-6H21'/>",
            Icon::Lock => {
                "<rect x='5' y='10.5' width='14' height='10' rx='2'/><path d='M8 10.5V7.5a4 4 0 0 1 8 0v3'/>"
            }
            Icon::Role => {
                "<circle cx='12' cy='8' r='3.5'/><path d='M5 20.5c.8-3.6 3.6-5.5 7-5.5s6.2 1.9 7 5.5'/>"
            }
            Icon::Warning => "<path d='M12 4 2.8 19.5h18.4z'/><path d='M12 10v4.5M12 17.2v.3'/>",
            Icon::Folder => {
                "<path d='M3.5 7.5a2 2 0 0 1 2-2h4l2 2.5h7a2 2 0 0 1 2 2v7.5a2 2 0 0 1-2 2h-13a2 2 0 0 1-2-2z'/>"
            }
            Icon::Restart => {
                "<path d='M20 12a8 8 0 1 1-2.4-5.7L20 8.5'/><path d='M20 4v4.5h-4.5'/>"
            }
            Icon::Close => "<path d='m6 6 12 12M18 6 6 18'/>",
        }
    }

    /// The icon of a service's resources.
    pub fn of_service(service: &str) -> Icon {
        match service {
            "s3" => Icon::Bucket,
            "dynamodb" => Icon::Table,
            "sqs" => Icon::Queue,
            "sns" => Icon::Topic,
            "lambda" => Icon::Function,
            "logs" => Icon::Logs,
            "secretsmanager" => Icon::Key,
            "ssm" => Icon::Parameter,
            "events" => Icon::Rule,
            "states" => Icon::StateMachine,
            "kinesis" => Icon::Stream,
            "kms" => Icon::Lock,
            "iam" => Icon::Role,
            _ => Icon::Calls,
        }
    }
}

/// An icon-only button in a toolbar: no border, a hover and pressed fill,
/// its tooltip naming it and its shortcut.
pub fn tool_button(
    icon_name: Icon,
    tip: impl Fn() -> String + 'static,
    active: impl Fn() -> bool + 'static,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    let active = std::rc::Rc::new(active);
    let a2 = active.clone();
    let tip = text(tip).text_size(small());
    let button = nsh_base::button()
        .on_press(on_press)
        .w(px(30.0))
        .h(px(26.0))
        .items_center()
        .justify_center()
        .rounded(radius())
        .bg(move || {
            if a2() {
                fill_bg()
            } else {
                AdaptiveColor::rgb(0, 0).opacity(0.0)
            }
        })
        .hover(|s| s.bg(hover_bg()))
        .active(|s| s.opacity(0.7))
        .child(icon(icon_name, 17.0))
        .opacity(move || if active() { 1.0_f32 } else { 0.85 });
    view().tooltip(tip).child(button)
}

/// A text button in the platform's push-button shape.
pub fn push_button(
    label: impl Fn() -> String + 'static,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    nsh_base::button()
        .on_press(on_press)
        .px(px(10.0))
        .h(px(24.0))
        .items_center()
        .justify_center()
        .rounded(radius())
        .bg(AdaptiveColor::rgb(0xffffff, 0x3a3a3c))
        .border_1()
        .border_color(AdaptiveColor::rgb(0xd1d1d6, 0x48484a))
        .hover(|s| s.bg(AdaptiveColor::rgb(0xf5f5f7, 0x444446)))
        .active(|s| s.opacity(0.75))
        .child(text(label).text_size(body()).font_family(UI))
}

/// A section's title in a sidebar or an inspector.
pub fn section(label: impl Fn() -> String + 'static) -> impl IntoElement {
    text(label)
        .text_size(small())
        .font_weight(FontWeight::SEMIBOLD)
        .font_family(UI)
        .text_color(secondary_text())
}

/// A count in a sidebar row.
pub fn badge(
    count: impl Fn() -> String + 'static,
    color: impl Fn() -> AdaptiveColor + 'static,
) -> impl IntoElement {
    text(count)
        .text_size(small())
        .font_family(UI)
        .text_color(color)
}

/// Unused-theme guard: keeps the base theme's tokens for components that
/// read them (inputs, popovers).
#[allow(dead_code)]
pub fn base_border() -> AdaptiveColor {
    theme().border
}

/// A thin line between areas.
pub fn hairline() -> impl IntoElement {
    view().h(px(1.0)).bg(separator())
}
