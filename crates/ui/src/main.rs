//! Lenslet: see what AWS access your code needs, and why its AWS calls failed.

mod app;
mod body;
mod daemon;
mod feed;
mod fonts;
mod fsguard;
mod glyph;
mod i18n;
mod icons;
mod lockfile;
mod model;
mod prefs;
mod search;
mod store;
mod syntax;
#[cfg(test)]
mod testdata;
mod theme;

#[cfg(test)]
mod bench;

use std::path::PathBuf;

use futures::StreamExt;
use gpui::{
    App, Application, Bounds, KeyBinding, Menu, MenuItem, OsAction, SystemMenuType,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowOptions, prelude::*, px, size,
};

use crate::{app::*, prefs::Lang};

pub const DOCS_URL: &str = "https://github.com/hxddh/lenslet#readme";
const ISSUES_URL: &str = "https://github.com/hxddh/lenslet/issues";

/// Context predicates for the key map.
const LIST: &str = "LensletApp && !Search && !Body && !Field";
const NOT_TYPING: &str = "LensletApp && !Search && !Field";
const TYPING: &str = "Search || Field";
const ACCESS: &str = "Access && !Search && !Field";

/// The whole key map. Every `cmd-` binding gets a `ctrl-` twin off macOS.
fn key_bindings() -> Vec<KeyBinding> {
    type Make = Box<dyn Fn(&str) -> KeyBinding>;
    let app = Some("LensletApp");
    let primary: Vec<(&str, Make)> = vec![
        ("o", Box::new(move |k| KeyBinding::new(k, OpenFile, app))),
        (
            "shift-o",
            Box::new(move |k| KeyBinding::new(k, OpenProject, app)),
        ),
        ("r", Box::new(move |k| KeyBinding::new(k, RunTests, app))),
        (".", Box::new(move |k| KeyBinding::new(k, StopRun, app))),
        ("[", Box::new(move |k| KeyBinding::new(k, PrevRun, app))),
        ("]", Box::new(move |k| KeyBinding::new(k, NextRun, app))),
        ("1", Box::new(move |k| KeyBinding::new(k, ShowCalls, app))),
        ("2", Box::new(move |k| KeyBinding::new(k, ShowAccess, app))),
        ("f", Box::new(move |k| KeyBinding::new(k, FocusSearch, app))),
        (
            "b",
            Box::new(move |k| KeyBinding::new(k, ToggleInspector, app)),
        ),
        (
            "j",
            Box::new(move |k| KeyBinding::new(k, ToggleOutput, app)),
        ),
        (
            "shift-c",
            Box::new(|k| KeyBinding::new(k, CopyCurl, Some(NOT_TYPING))),
        ),
        (
            "shift-a",
            Box::new(|k| KeyBinding::new(k, AcceptAll, Some(ACCESS))),
        ),
        ("x", Box::new(|k| KeyBinding::new(k, Cut, Some(TYPING)))),
        ("v", Box::new(|k| KeyBinding::new(k, Paste, Some(TYPING)))),
        ("z", Box::new(|k| KeyBinding::new(k, Undo, Some(TYPING)))),
        (
            "shift-z",
            Box::new(|k| KeyBinding::new(k, Redo, Some(TYPING))),
        ),
        (
            "c",
            Box::new(|k| KeyBinding::new(k, Copy, Some("Body || Search || Field"))),
        ),
        (
            "a",
            Box::new(|k| KeyBinding::new(k, SelectAll, Some("Body || Search || Field"))),
        ),
        ("w", Box::new(move |k| KeyBinding::new(k, CloseWindow, app))),
        ("q", Box::new(|k| KeyBinding::new(k, Quit, None))),
    ];
    let mut bindings = Vec::new();
    for (key, make) in &primary {
        bindings.push(make(&format!("cmd-{key}")));
        if !cfg!(target_os = "macos") {
            bindings.push(make(&format!("ctrl-{key}")));
        }
    }
    if cfg!(target_os = "macos") {
        bindings.extend([
            KeyBinding::new("cmd-h", HideApp, None),
            KeyBinding::new("alt-cmd-h", HideOthers, None),
            KeyBinding::new("cmd-m", MinimizeWindow, app),
            KeyBinding::new("ctrl-cmd-f", ToggleFullScreen, app),
        ]);
    } else {
        bindings.push(KeyBinding::new("f11", ToggleFullScreen, app));
    }
    bindings.extend([
        KeyBinding::new("/", FocusSearch, Some(NOT_TYPING)),
        KeyBinding::new("j", SelectNext, Some(LIST)),
        KeyBinding::new("k", SelectPrev, Some(LIST)),
        KeyBinding::new("down", SelectNext, Some(LIST)),
        KeyBinding::new("up", SelectPrev, Some(LIST)),
        KeyBinding::new("pagedown", PageDown, Some(LIST)),
        KeyBinding::new("pageup", PageUp, Some(LIST)),
        KeyBinding::new("home", SelectFirst, Some(LIST)),
        KeyBinding::new("end", SelectLast, Some(LIST)),
        KeyBinding::new("enter", Confirm, Some(NOT_TYPING)),
        // Esc closes whatever is on top, from anywhere, text fields included.
        KeyBinding::new("escape", Cancel, Some(NOT_TYPING)),
        KeyBinding::new("escape", Cancel, Some("Field")),
        KeyBinding::new("enter", SubmitField, Some("Field")),
        // Tab / Shift-Tab move between a dialog's fields and buttons.
        KeyBinding::new("tab", FocusNextControl, app),
        KeyBinding::new("shift-tab", FocusPrevControl, app),
        // While typing a search, ↑/↓ still move through the results.
        KeyBinding::new("down", SelectNext, Some("Search")),
        KeyBinding::new("up", SelectPrev, Some("Search")),
        // The Access view's lock keys.
        KeyBinding::new("a", AcceptPermission, Some(ACCESS)),
        KeyBinding::new("t", ToggleTestMark, Some(ACCESS)),
        KeyBinding::new("backspace", RemovePermission, Some(ACCESS)),
        KeyBinding::new("delete", RemovePermission, Some(ACCESS)),
    ]);
    bindings
}

/// The macOS menu bar. `Window` and `Help` keep their English titles so
/// macOS attaches the window list and menu search.
pub(crate) fn menus(lang: Lang) -> Vec<Menu> {
    let t = |en: &'static str, zh: &'static str| -> &'static str {
        match lang {
            Lang::En => en,
            Lang::Zh => zh,
        }
    };
    let check = |on: bool, label: &'static str| -> String {
        if on {
            format!("✓ {label}")
        } else {
            label.to_string()
        }
    };
    vec![
        Menu {
            name: "Lenslet".into(),
            items: vec![
                MenuItem::action(t("About Lenslet", "关于 Lenslet"), OpenAbout),
                MenuItem::separator(),
                MenuItem::action(check(lang == Lang::En, "Language: English"), UseEnglish),
                MenuItem::action(check(lang == Lang::Zh, "Language: 中文"), UseChinese),
                MenuItem::separator(),
                MenuItem::os_submenu(t("Services", "服务"), SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action(t("Hide Lenslet", "隐藏 Lenslet"), HideApp),
                MenuItem::action(t("Hide Others", "隐藏其他"), HideOthers),
                MenuItem::action(t("Show All", "全部显示"), ShowAll),
                MenuItem::separator(),
                MenuItem::action(t("Quit Lenslet", "退出 Lenslet"), Quit),
            ],
        },
        Menu {
            name: t("File", "文件").into(),
            items: vec![
                MenuItem::action(t("Open Project Folder…", "打开项目目录…"), OpenProject),
                MenuItem::action(t("Open Run File…", "打开运行文件…"), OpenFile),
                MenuItem::separator(),
                MenuItem::action(t("Close Window", "关闭窗口"), CloseWindow),
            ],
        },
        Menu {
            name: t("Edit", "编辑").into(),
            items: vec![
                MenuItem::os_action(t("Undo", "撤销"), Undo, OsAction::Undo),
                MenuItem::os_action(t("Redo", "重做"), Redo, OsAction::Redo),
                MenuItem::separator(),
                MenuItem::os_action(t("Cut", "剪切"), Cut, OsAction::Cut),
                MenuItem::os_action(t("Copy", "拷贝"), Copy, OsAction::Copy),
                MenuItem::os_action(t("Paste", "粘贴"), Paste, OsAction::Paste),
                MenuItem::os_action(t("Select All", "全选"), SelectAll, OsAction::SelectAll),
                MenuItem::separator(),
                MenuItem::action(t("Copy as curl", "复制为 curl"), CopyCurl),
                MenuItem::action(t("Find…", "查找…"), FocusSearch),
            ],
        },
        Menu {
            name: t("View", "视图").into(),
            items: vec![
                MenuItem::action(t("Calls", "调用"), ShowCalls),
                MenuItem::action(t("Access", "权限"), ShowAccess),
                MenuItem::separator(),
                MenuItem::action(t("Inspector", "详情栏"), ToggleInspector),
                MenuItem::action(t("Output", "输出"), ToggleOutput),
                MenuItem::separator(),
                MenuItem::action(t("Enter Full Screen", "进入全屏幕"), ToggleFullScreen),
            ],
        },
        Menu {
            name: t("Run", "运行").into(),
            items: vec![
                MenuItem::action(t("Run Tests", "运行测试"), RunTests),
                MenuItem::action(t("Stop", "停止"), StopRun),
                MenuItem::action(t("Edit Test Command…", "编辑测试命令…"), EditCommand),
                MenuItem::separator(),
                MenuItem::action(t("Previous Run", "上一个运行"), PrevRun),
                MenuItem::action(t("Next Run", "下一个运行"), NextRun),
                MenuItem::action(t("Follow Newest Run", "跟随最新运行"), FollowNewest),
            ],
        },
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::action(t("Minimize", "最小化"), MinimizeWindow),
                MenuItem::action(t("Zoom", "缩放"), ZoomWindow),
            ],
        },
        Menu {
            name: "Help".into(),
            items: vec![
                MenuItem::action(t("Lenslet Documentation", "Lenslet 使用文档"), OpenDocs),
                MenuItem::action(t("Report an Issue…", "反馈问题…"), OpenIssues),
            ],
        },
    ]
}

fn decode_file_url(url: &str) -> Option<PathBuf> {
    let raw = url.strip_prefix("file://").unwrap_or(url);
    let raw = raw
        .strip_prefix("localhost/")
        .map(|s| format!("/{s}"))
        .unwrap_or_else(|| raw.to_string());
    let mut bytes = Vec::with_capacity(raw.len());
    let src = raw.as_bytes();
    let mut i = 0;
    while i < src.len() {
        if src[i] == b'%' && i + 2 < src.len() {
            let hi = (src[i + 1] as char).to_digit(16);
            let lo = (src[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                bytes.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
        }
        bytes.push(src[i]);
        i += 1;
    }
    String::from_utf8(bytes).ok().map(PathBuf::from)
}

fn open_main_window(initial_paths: Vec<PathBuf>, cx: &mut App) -> WindowHandle<LensletApp> {
    let saved = prefs::load(&app::home());
    let width = saved.window_width.unwrap_or(1360.0).clamp(960.0, 3840.0);
    let height = saved.window_height.unwrap_or(850.0).clamp(640.0, 2400.0);
    let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
    let mac = cfg!(target_os = "macos");
    let titlebar = gpui::TitlebarOptions {
        title: Some("Lenslet".into()),
        appears_transparent: mac,
        traffic_light_position: mac.then(|| gpui::point(px(14.0), px(15.0))),
    };
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(960.0), px(640.0))),
                window_background: WindowBackgroundAppearance::Opaque,
                app_id: Some("dev.lenslet.app".to_string()),
                titlebar: Some(titlebar),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| LensletApp::new(saved, initial_paths, window, cx)),
        )
        .expect("open Lenslet window");
    let _ = handle.update(cx, |_, _, cx| cx.activate(true));
    handle
}

fn ensure_window(cx: &mut App) -> WindowHandle<LensletApp> {
    if let Some(existing) = cx
        .windows()
        .into_iter()
        .find_map(|w| w.downcast::<LensletApp>())
    {
        return existing;
    }
    open_main_window(Vec::new(), cx)
}

/// Signal every window's run; never waits.
fn stop_all_window_children(cx: &mut App) {
    for win in cx.windows() {
        if let Some(handle) = win.downcast::<LensletApp>() {
            let _ = handle.update(cx, |app, _, _| app.stop_all_children());
        }
    }
}

/// `LENSLET_LOG=1` prints gpui's own log (renderer, platform) to stderr.
struct StderrLog;

impl log::Log for StderrLog {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }
    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!(
                "[{}] {}: {}",
                record.level(),
                record.target(),
                record.args()
            );
        }
    }
    fn flush(&self) {}
}

fn parse_cli_args() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-V" | "--version" => {
                println!("lenslet-ui {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                println!(
                    "Usage: lenslet-ui [RUN_FILE_OR_PROJECT_FOLDER...]\n\nOpen the Lenslet window."
                );
                std::process::exit(0);
            }
            // macOS LaunchServices process serial number flag.
            s if s.starts_with("-psn_") => {}
            other => paths.push(PathBuf::from(other)),
        }
    }
    paths
}

/// Run a window action, opening the window first when none is open (macOS
/// keeps running with no windows after ⌘W).
fn with_window(cx: &mut App, action: Box<dyn gpui::Action>) {
    if cx.windows().is_empty() {
        let win = ensure_window(cx);
        let _ = win.update(cx, |_, window, cx| window.dispatch_action(action, cx));
    }
}

fn main() {
    let cli_paths = parse_cli_args();
    fsguard::mark_start();
    if std::env::var_os("LENSLET_LOG").is_some() {
        static LOGGER: StderrLog = StderrLog;
        let _ = log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Info));
    }

    // Files opened from Finder arrive on a channel the app awaits: no timer.
    let (url_tx, mut url_rx) = futures::channel::mpsc::unbounded::<PathBuf>();
    let app = Application::new().with_assets(icons::Assets);
    app.on_open_urls(move |urls| {
        for url in urls {
            if let Some(path) = decode_file_url(&url) {
                let _ = url_tx.unbounded_send(path);
            }
        }
    });
    app.on_reopen(|cx| {
        if cx.windows().is_empty() {
            open_main_window(Vec::new(), cx);
        } else {
            cx.activate(true);
        }
    });

    app.run(move |cx: &mut App| {
        fonts::install(cx);
        cx.bind_keys(key_bindings());

        cx.on_app_quit(|cx| {
            stop_all_window_children(cx);
            std::future::ready(())
        })
        .detach();

        cx.on_action(|_: &Quit, cx| {
            stop_all_window_children(cx);
            cx.quit();
        });
        cx.on_action(|_: &HideApp, cx| cx.hide());
        cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
        cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
        cx.on_action(|_: &OpenDocs, cx| cx.open_url(DOCS_URL));
        cx.on_action(|_: &OpenIssues, cx| cx.open_url(ISSUES_URL));
        cx.on_action(|_: &RunTests, cx| with_window(cx, Box::new(RunTests)));
        cx.on_action(|_: &OpenFile, cx| with_window(cx, Box::new(OpenFile)));
        cx.on_action(|_: &OpenProject, cx| with_window(cx, Box::new(OpenProject)));
        cx.on_action(|_: &OpenAbout, cx| with_window(cx, Box::new(OpenAbout)));

        if cfg!(target_os = "macos") {
            cx.set_menus(menus(prefs::load(&app::home()).lang));
        }

        open_main_window(cli_paths, cx);

        cx.spawn(async move |cx| {
            while let Some(path) = url_rx.next().await {
                let _ = cx.update(|cx| {
                    let win = ensure_window(cx);
                    let _ = win.update(cx, |app, _, cx| app.open_external_path(path, cx));
                    cx.activate(true);
                });
            }
        })
        .detach();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bound(bindings: &[KeyBinding], key: &str, action: &str) -> bool {
        bindings.iter().any(|b| {
            b.keystrokes().len() == 1
                && b.keystrokes()[0].unparse() == key
                && b.action().name().ends_with(action)
        })
    }

    /// The whole keymap, and every shortcut a tooltip or menu advertises, is
    /// bound (12.0 advertised ⌥O and ⌥S without binding them).
    #[test]
    fn every_advertised_shortcut_is_bound() {
        let bindings = key_bindings();
        let cmd = if cfg!(target_os = "macos") {
            "cmd"
        } else {
            "ctrl"
        };
        for (key, action) in [
            ("o", "OpenFile"),
            ("shift-o", "OpenProject"),
            ("r", "RunTests"),
            (".", "StopRun"),
            ("[", "PrevRun"),
            ("]", "NextRun"),
            ("1", "ShowCalls"),
            ("2", "ShowAccess"),
            ("f", "FocusSearch"),
            ("b", "ToggleInspector"),
            ("j", "ToggleOutput"),
            ("shift-c", "CopyCurl"),
            ("shift-a", "AcceptAll"),
            ("c", "Copy"),
            ("a", "SelectAll"),
            ("w", "CloseWindow"),
            ("q", "Quit"),
        ] {
            let key = format!("{cmd}-{key}");
            assert!(bound(&bindings, &key, action), "{key} → {action}");
        }
        for (key, action) in [
            ("/", "FocusSearch"),
            ("j", "SelectNext"),
            ("k", "SelectPrev"),
            ("down", "SelectNext"),
            ("up", "SelectPrev"),
            ("home", "SelectFirst"),
            ("end", "SelectLast"),
            ("pageup", "PageUp"),
            ("pagedown", "PageDown"),
            ("enter", "Confirm"),
            ("escape", "Cancel"),
            ("tab", "FocusNextControl"),
            ("a", "AcceptPermission"),
            ("t", "ToggleTestMark"),
            ("backspace", "RemovePermission"),
        ] {
            assert!(bound(&bindings, key, action), "{key} → {action}");
        }

        // Every shortcut the views advertise (`i18n::shortcut("…")`) is one
        // of the bindings above.
        let advertised = [
            include_str!("app/header.rs"),
            include_str!("app/calls.rs"),
            include_str!("app/inspector.rs"),
            include_str!("app/access.rs"),
            include_str!("app/mod.rs"),
            include_str!("search.rs"),
        ];
        for source in advertised {
            for piece in source.split("i18n::shortcut(\"").skip(1) {
                let key = piece.split('"').next().unwrap();
                let spelled = key.replace('⇧', "shift-").to_lowercase();
                let combo = format!("{cmd}-{spelled}");
                assert!(
                    bindings
                        .iter()
                        .any(|b| b.keystrokes().len() == 1 && b.keystrokes()[0].unparse() == combo),
                    "advertised {key} ({combo}) is not bound"
                );
            }
        }
    }

    #[test]
    fn the_keymap_stays_small_and_has_ctrl_twins_off_macos() {
        let bindings = key_bindings();
        let platform = [
            "HideApp",
            "HideOthers",
            "MinimizeWindow",
            "ToggleFullScreen",
        ];
        let cmd = bindings
            .iter()
            .filter(|b| b.keystrokes()[0].modifiers().platform)
            .filter(|b| {
                !platform
                    .iter()
                    .any(|name| b.action().name().ends_with(name))
            })
            .count();
        if !cfg!(target_os = "macos") {
            let ctrl = bindings
                .iter()
                .filter(|b| b.keystrokes()[0].modifiers().control)
                .count();
            assert!(ctrl >= cmd, "{ctrl} ctrl bindings for {cmd} cmd bindings");
        }
        assert!(cmd <= 22, "{cmd} command shortcuts; keep the key map small");
    }

    #[test]
    fn menus_are_bilingual_and_keep_the_standard_sections() {
        for lang in [Lang::En, Lang::Zh] {
            let built = menus(lang);
            assert_eq!(built.len(), 7);
            assert_eq!(built[0].name.as_ref(), "Lenslet");
            assert_eq!(built[5].name.as_ref(), "Window");
            assert_eq!(built[6].name.as_ref(), "Help");
            let os_actions: Vec<OsAction> = built[2]
                .items
                .iter()
                .filter_map(|item| match item {
                    MenuItem::Action { os_action, .. } => *os_action,
                    _ => None,
                })
                .collect();
            assert_eq!(os_actions.len(), 6);
        }
        let zh = menus(Lang::Zh);
        assert_eq!(zh[1].name.as_ref(), "文件");
    }

    #[test]
    fn decode_file_url_handles_spaces_and_utf8() {
        assert_eq!(
            decode_file_url("file:///tmp/a%20b/%E2%9C%93.jsonl"),
            Some(PathBuf::from("/tmp/a b/✓.jsonl"))
        );
    }
}
