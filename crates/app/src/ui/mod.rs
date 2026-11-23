//! The window. One structure on every platform, drawn the platform's way:
//! a full-height sidebar (the session's views and every resource), a toolbar
//! over the content, the content (overview, calls, permissions, or one
//! resource), an inspector for the selected call, and a terminal at the
//! bottom that is already pointed at Lenslet. Every action is in the menu
//! bar (macOS) or the main menu (☰ elsewhere), with its shortcut, and on the
//! context menu of what it acts on.

mod calls;
mod inspector;
mod pages;
mod sidebar;
pub mod style;
mod terminal;

use std::{
    rc::Rc,
    sync::{Arc, OnceLock},
    time::Duration,
};

use nsh_base::DataTableSort;
use nsh_core::{
    desktop::{Backdrop, MenuRole, TitleBarOptions, TitleBarStyle, WindowAttributes},
    element::desktop::{
        MenuItem, popup_menu, set_application_menu, title_bar, window as app_window,
    },
    platform::Os,
    prelude::*,
};

use crate::{
    copy_as,
    engine::Engine,
    session::{Call, Snapshot},
};
use style::*;

// -- Language -------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Default)]
pub enum Lang {
    #[default]
    En,
    Zh,
}

pub fn lang() -> Signal<Lang> {
    global::<Lang>()
}

/// English or Chinese, following the window's language.
pub fn t(en: &'static str, zh: &'static str) -> &'static str {
    if lang().get() == Lang::Zh { zh } else { en }
}

/// `t` for a string to own.
pub fn ts(en: &'static str, zh: &'static str) -> String {
    t(en, zh).to_string()
}

fn system_lang() -> Lang {
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| locale().to_string());
    if locale.starts_with("zh") {
        Lang::Zh
    } else {
        Lang::En
    }
}

// -- State --------------------------------------------------------------------------

/// What the content area shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Page {
    Overview,
    Calls,
    Permissions,
    /// One resource: its service and name.
    Resource(&'static str, String),
}

/// Which calls the list shows.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum Filter {
    #[default]
    All,
    Failed,
    NotEmulated,
}

/// The inspector's tabs.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum Tab {
    #[default]
    Summary,
    Request,
    Response,
    Access,
}

/// The window's state: signals, so every part reads what it shows.
#[derive(Clone, Copy)]
pub struct Ui {
    pub snap: Signal<Rc<Snapshot>>,
    pub page: Signal<Page>,
    /// The selected call, by sequence number.
    pub selected: Signal<Option<usize>>,
    pub query: Signal<String>,
    pub filter: Signal<Filter>,
    pub service: Signal<Option<String>>,
    pub sort: Signal<Option<DataTableSort>>,
    /// The calls the list shows, filtered and sorted.
    pub visible: Signal<Rc<Vec<Arc<Call>>>>,
    pub sidebar: Signal<bool>,
    pub inspector: Signal<bool>,
    pub terminal: Signal<bool>,
    pub tab: Signal<Tab>,
    pub toast: Signal<Option<String>>,
    /// Bumped to ask the search field for the focus.
    pub focus_search: Signal<u64>,
    /// A command for the terminal to run.
    pub run: Signal<Option<String>>,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

/// The engine the window shows.
pub fn engine() -> &'static Engine {
    ENGINE.get().expect("the engine started")
}

impl Ui {
    fn new() -> Ui {
        Ui {
            snap: signal_with(Rc::new(Snapshot::default()), SignalEquals::Never),
            page: signal(Page::Overview),
            selected: signal(None),
            query: signal(String::new()),
            filter: signal(Filter::All),
            service: signal(None),
            sort: signal(None),
            visible: signal_with(Rc::new(Vec::new()), SignalEquals::Never),
            sidebar: signal(true),
            inspector: signal(true),
            terminal: signal(true),
            tab: signal(Tab::Summary),
            toast: signal(None),
            focus_search: signal(0),
            run: signal(None),
        }
    }

    /// The selected call.
    pub fn call(&self) -> Option<Arc<Call>> {
        let seq = self.selected.get()?;
        self.snap
            .with(|s| s.calls.iter().rev().find(|c| c.seq == seq).cloned())
    }

    pub fn say(&self, message: impl Into<String>) {
        let toast = self.toast;
        toast.set(Some(message.into()));
        set_timeout(Duration::from_millis(1500), move || toast.set(None));
    }

    pub fn copy(&self, what: String, said: &'static str, said_zh: &'static str) {
        set_clipboard(what);
        self.say(t(said, said_zh));
    }

    /// Show a call in the list and the inspector.
    pub fn show_call(&self, seq: usize) {
        self.page.set(Page::Calls);
        self.selected.set(Some(seq));
        self.inspector.set(true);
    }

    pub fn copy_env(&self) {
        self.copy(engine().env(), "Copied the environment", "已复制环境变量");
    }

    pub fn clear(&self) {
        engine().clear();
        self.selected.set(None);
        self.say(t("Cleared the calls", "已清空调用"));
    }

    /// A new, empty account, once the user confirms it.
    pub fn new_account(&self) {
        let ui = *self;
        let answer = nsh_base::alert_dialog()
            .title(t("Start with a new, empty account?", "换成一个全新的空账户？"))
            .description(t(
                "Every bucket, table, queue and other resource in the local account is deleted. The calls stay in the list.",
                "本地账户里的所有存储桶、表、队列等资源都会被删除。调用记录保留。",
            ))
            .cancel("cancel", t("Cancel", "取消"))
            .destructive("new", t("New Account", "新建账户"))
            .close_on_escape(true)
            .show();
        spawn(async move {
            if answer.await.ok().and_then(|r| r.action).as_deref() == Some("new") {
                engine().fresh();
                if let Page::Resource(..) = ui.page.get_untracked() {
                    ui.page.set(Page::Overview);
                }
                ui.say(t("A new, empty account", "已换成全新的空账户"));
            }
        });
    }

    pub fn save_policy(&self) {
        let ui = *self;
        let policy = engine().policy();
        spawn(async move {
            let dialog = nsh_core::file_dialog::SaveDialog::new()
                .file_name("lenslet-policy.json")
                .button_label(t("Save", "保存"));
            if let Ok(Some(path)) = pick_save_path(dialog).await {
                match std::fs::write(&path, policy) {
                    Ok(()) => ui.say(t("Saved the IAM policy", "已保存 IAM 策略")),
                    Err(e) => ui.say(e.to_string()),
                }
            }
        });
    }

    pub fn export_session(&self) {
        let ui = *self;
        let calls = self.snap.with_untracked(|s| s.calls.clone());
        spawn(async move {
            let dialog = nsh_core::file_dialog::SaveDialog::new()
                .file_name("lenslet-session.json")
                .button_label(t("Export", "导出"));
            if let Ok(Some(path)) = pick_save_path(dialog).await {
                let json = session_json(&calls);
                match std::fs::write(&path, json) {
                    Ok(()) => ui.say(t("Exported the session", "已导出会话")),
                    Err(e) => ui.say(e.to_string()),
                }
            }
        });
    }

    pub fn copy_call_as(&self, form: CopyAs) {
        let Some(call) = self.call() else {
            self.say(t("Select a call first", "请先选中一次调用"));
            return;
        };
        let text = match form {
            CopyAs::Cli => copy_as::aws_cli(&call),
            CopyAs::Curl => Some(copy_as::curl(&call, &engine().endpoint())),
            CopyAs::Boto3 => copy_as::boto3(&call),
        };
        match text {
            Some(text) => self.copy(text, "Copied", "已复制"),
            None => self.say(t(
                "Only calls with a JSON body can be copied this way; copy as curl instead",
                "只有 JSON 正文的调用能这样复制；可以复制为 curl",
            )),
        }
    }
}

#[derive(Clone, Copy)]
pub enum CopyAs {
    Cli,
    Curl,
    Boto3,
}

fn session_json(calls: &[Arc<Call>]) -> String {
    let entries: Vec<serde_json::Value> = calls
        .iter()
        .map(|c| {
            serde_json::json!({
                "time": c.at_ms,
                "service": c.service,
                "operation": c.operation,
                "method": c.method,
                "path": c.path,
                "status": c.status,
                "ms": c.ms,
                "target": c.target,
                "client": c.client,
                "attempt": c.attempt,
                "requestId": c.request_id,
                "error": c.failure.as_ref().map(|f| serde_json::json!({"code": f.code, "message": f.message})),
                "notEmulated": c.unsupported,
                "permissions": c.access.iter().map(|a| serde_json::json!({"action": a.action, "resource": a.resource})).collect::<Vec<_>>(),
                "request": {"headers": c.request.headers, "body": c.request.body},
                "response": {"headers": c.response.headers, "body": c.response.body},
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({"lenslet": 1, "calls": entries}))
        .unwrap_or_default()
}

// -- The window ---------------------------------------------------------------------

/// The height of the window's top bar, where the system's controls sit.
pub fn bar_height() -> Points {
    match os() {
        Os::MacOs => px(52.0),
        Os::Windows => px(40.0),
        _ => px(46.0),
    }
}

#[component]
pub fn App() -> impl IntoElement {
    lang().set(system_lang());
    let started = Engine::start(Engine::port_from_env());
    if let Ok(engine) = &started {
        let _ = ENGINE.set(engine.clone());
    }
    let mut bar = TitleBarOptions::default();
    bar.style = TitleBarStyle::Hidden;
    bar.height = Some(bar_height());
    let mut attributes = WindowAttributes::default();
    attributes.title_bar = Some(bar);
    attributes.min_size = Some(Some(size(px(760.0), px(480.0))));
    if matches!(os(), Os::MacOs | Os::Windows) {
        attributes.backdrop = Some(Backdrop::Sidebar);
    }
    app_window()
        .id("main")
        .title("Lenslet")
        .default_size(size(px(1320.0), px(840.0)))
        .restore_bounds(true)
        .attributes(attributes)
        .content(move || match &started {
            Ok(_) => workbench(),
            Err(error) => cannot_start(error.clone()),
        })
}

fn cannot_start(error: String) -> AnyElement {
    view()
        .flex_1()
        .items_center()
        .justify_center()
        .gap(px(10.0))
        .bg(content_bg())
        .window_drag(true)
        .child(icon_colored(Icon::Warning, 40.0, orange()))
        .child(
            text(move || {
                ts(
                    "Lenslet could not start its endpoint",
                    "Lenslet 无法启动本地端点",
                )
            })
            .text_size(px(17.0))
            .font_weight(FontWeight::SEMIBOLD),
        )
        .child(
            text(error.clone())
                .font_family(MONO)
                .text_size(small())
                .text_color(secondary_text()),
        )
        .child(
            text(move || {
                ts(
                    "Set LENSLET_PORT to a free port, or quit what uses this one.",
                    "把 LENSLET_PORT 设为一个空闲端口，或退出占用它的程序。",
                )
            })
            .text_size(body())
            .text_color(secondary_text()),
        )
        .into_element()
}

fn workbench() -> AnyElement {
    let ui = Ui::new();

    // Take the session when it changed: a pointer copy, not the bodies.
    {
        let version = std::cell::Cell::new(None::<u64>);
        set_interval(Duration::from_millis(80), move || {
            if let Some(next) = engine().snapshot_since(version.get()) {
                version.set(Some(next.version));
                ui.snap.set(Rc::new(next));
            }
        });
    }
    // The list: the calls that pass the filters, in the chosen order.
    effect(move || {
        let query = ui.query.get().to_lowercase();
        let filter = ui.filter.get();
        let service = ui.service.get();
        let sort = ui.sort.get();
        let mut calls: Vec<Arc<Call>> = ui.snap.with(|s| {
            s.calls
                .iter()
                .filter(|c| match filter {
                    Filter::All => true,
                    Filter::Failed => c.failed() && !c.unsupported,
                    Filter::NotEmulated => c.unsupported,
                })
                .filter(|c| service.as_deref().is_none_or(|s| s == c.service))
                .filter(|c| query.is_empty() || calls::matches(c, &query))
                .cloned()
                .collect()
        });
        calls::sort(&mut calls, sort.as_ref());
        ui.visible.set(Rc::new(calls));
    });
    // The Dock's badge counts the failed calls.
    effect(move || {
        let failed = ui.snap.with(|s| {
            s.calls
                .iter()
                .filter(|c| c.failed() && !c.unsupported)
                .count()
        });
        set_badge(failed as u32);
    });
    effect(move || {
        // Follows the language and the views' state, so checks stay right.
        let _ = (
            lang().get(),
            ui.sidebar.get(),
            ui.inspector.get(),
            ui.terminal.get(),
            ui.filter.get(),
        );
        set_application_menu(menu(ui));
    });

    let sidebar_column = sidebar::sidebar(ui);
    let content_column = view()
        .flex_1()
        .bg(content_bg())
        .child(toolbar(ui))
        .child(hairline())
        .child(
            nsh_base::v_resizable()
                .flex_1()
                .handle_class("h-px bg-border")
                .child(
                    nsh_base::resizable_panel().child(
                        nsh_base::h_resizable()
                            .handle_class("w-px bg-border")
                            .child(nsh_base::resizable_panel().child(page(ui)))
                            .child(
                                nsh_base::resizable_panel()
                                    .size(px(400.0))
                                    .size_range(px(300.0)..px(720.0))
                                    .visible(move || {
                                        ui.inspector.get() && ui.page.get() == Page::Calls
                                    })
                                    .child(inspector::inspector(ui)),
                            ),
                    ),
                )
                .child(
                    nsh_base::resizable_panel()
                        .size(px(250.0))
                        .size_range(px(120.0)..px(700.0))
                        .visible(move || ui.terminal.get())
                        .child(terminal::panel(ui)),
                ),
        );

    view()
        .flex_1()
        .font_family(UI)
        .text_size(body())
        .child(
            nsh_base::h_resizable()
                .flex_1()
                .handle_class("w-px bg-border")
                .child(
                    nsh_base::resizable_panel()
                        .size(px(230.0))
                        .size_range(px(180.0)..px(360.0))
                        .visible(move || ui.sidebar.get())
                        .child(sidebar_column),
                )
                .child(nsh_base::resizable_panel().child(content_column)),
        )
        .child(toast(ui))
        .into_element()
}

/// The content area for the page.
fn page(ui: Ui) -> AnyElement {
    let kind = computed(move || match ui.page.get() {
        Page::Overview => 0,
        Page::Calls => 1,
        Page::Permissions => 2,
        Page::Resource(..) => 3,
    });
    dynamic(move || match kind.get() {
        0 => pages::overview(ui),
        1 => calls::calls(ui),
        2 => pages::permissions(ui),
        _ => pages::resource(ui),
    })
}

/// The bar over the content: what it shows, and the actions on it.
fn toolbar(ui: Ui) -> impl IntoElement {
    let title = move || match ui.page.get() {
        Page::Overview => ts("Overview", "概览"),
        Page::Calls => ts("Calls", "调用"),
        Page::Permissions => ts("Permissions", "权限"),
        Page::Resource(_, name) => name,
    };
    let subtitle = move || match ui.page.get() {
        Page::Calls => {
            let (shown, all) = (
                ui.visible.with(|v| v.len()),
                ui.snap.with(|s| s.calls.len()),
            );
            if shown == all {
                format!("{all} {}", t("calls", "次调用"))
            } else {
                format!("{shown} / {all}")
            }
        }
        Page::Permissions => format!(
            "{} {}",
            ui.snap.with(|s| s.permissions.len()),
            t("permissions", "项权限")
        ),
        Page::Resource(service, _) => sidebar::service_title(service),
        Page::Overview => format!(
            "{} · {}",
            engine().endpoint().trim_start_matches("http://"),
            crate::engine::REGION
        ),
    };
    let left_inset = move || {
        if ui.sidebar.get() {
            px(12.0)
        } else {
            title_bar().left + px(4.0)
        }
    };
    view()
        .flex_row()
        .items_center()
        .h(bar_height())
        .pl(left_inset)
        .pr(move || title_bar().right + px(10.0))
        .gap(px(6.0))
        .bg(chrome_bg())
        .window_drag(true)
        .child(when(
            move || !ui.sidebar.get(),
            move || {
                tool_button(
                    Icon::Sidebar,
                    move || {
                        format!(
                            "{} ({})",
                            t("Show Sidebar", "显示侧栏"),
                            key_label(&sidebar_key())
                        )
                    },
                    || false,
                    move || ui.sidebar.set(true),
                )
            },
        ))
        .child(
            view()
                .px(px(4.0))
                .child(
                    text(title)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_size(body()),
                )
                .child(
                    text(subtitle)
                        .text_size(small())
                        .text_color(secondary_text()),
                ),
        )
        .child(view().flex_1())
        .child(search_field(ui))
        .child(tool_button(
            Icon::Clear,
            move || format!("{} ({})", t("Clear Calls", "清空调用"), key_label("mod+k")),
            || false,
            move || ui.clear(),
        ))
        .child(tool_button(
            Icon::NewAccount,
            move || {
                format!(
                    "{} ({})",
                    t("New Empty Account…", "新建空账户…"),
                    key_label("mod+shift+n")
                )
            },
            || false,
            move || ui.new_account(),
        ))
        .child(tool_button(
            Icon::Terminal,
            move || format!("{} ({})", t("Terminal", "终端"), key_label("ctrl+`")),
            move || ui.terminal.get(),
            move || ui.terminal.update(|v| *v = !*v),
        ))
        .child(when(
            move || ui.page.get() == Page::Calls,
            move || {
                tool_button(
                    Icon::Inspector,
                    move || format!("{} ({})", t("Inspector", "检查器"), key_label("mod+alt+i")),
                    move || ui.inspector.get(),
                    move || ui.inspector.update(|v| *v = !*v),
                )
            },
        ))
        .child(when(
            move || os() != Os::MacOs,
            move || {
                nsh_base::button()
                    .on_press(move || drop(popup_menu(menu(ui))))
                    .w(px(30.0))
                    .h(px(26.0))
                    .items_center()
                    .justify_center()
                    .rounded(radius())
                    .hover(|s| s.bg(hover_bg()))
                    .child(icon(Icon::Menu, 17.0))
            },
        ))
}

fn search_field(ui: Ui) -> impl IntoElement {
    let field = element_ref();
    effect(move || {
        if ui.focus_search.get() > 0 {
            field.focus();
        }
    });
    view()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .w(px(240.0))
        .h(px(26.0))
        .px(px(8.0))
        .rounded(radius())
        .bg(fill_bg())
        .window_drag(false)
        .child(icon(Icon::Search, 14.0))
        .child(
            nsh_base::input()
                .element_ref(&field)
                .flex_1()
                .text_size(body())
                .value(move || ui.query.get())
                .on_change(move |v: &str| {
                    ui.query.set(v.to_owned());
                    if !v.is_empty() && ui.page.get_untracked() != Page::Calls {
                        ui.page.set(Page::Calls);
                    }
                })
                .on_key_press(move |k: &str| {
                    if k == "Escape" {
                        ui.query.set(String::new());
                    }
                })
                .placeholder(move || ts("Search calls", "搜索调用"))
                .return_key_type(ReturnKeyType::Search),
        )
}

fn toast(ui: Ui) -> impl IntoElement {
    view()
        .absolute()
        .bottom(px(28.0))
        .left(px(0.0))
        .right(px(0.0))
        .items_center()
        .pointer_events(PointerEvents::None)
        .child(when(
            move || ui.toast.get().is_some(),
            move || {
                text(move || ui.toast.get().unwrap_or_default())
                    .px(px(14.0))
                    .py(px(7.0))
                    .rounded(px(9.0))
                    .bg(AdaptiveColor::rgb(0x1c1c1e, 0xf2f2f7).opacity(0.92))
                    .text_color(AdaptiveColor::rgb(0xffffff, 0x1c1c1e))
                    .text_size(body())
            },
        ))
}

// -- Menus and shortcuts --------------------------------------------------------------------

fn sidebar_key() -> String {
    if os() == Os::MacOs {
        "ctrl+cmd+s".into()
    } else {
        "mod+alt+s".into()
    }
}

/// The application's menus: the menu bar on macOS, the main menu (☰)
/// elsewhere. Each command is here with its shortcut.
fn menu(ui: Ui) -> Vec<MenuItem> {
    let mac = os() == Os::MacOs;
    let check = |label: &'static str, label_zh: &'static str, on: bool| {
        MenuItem::new(t(label, label_zh)).checked(on)
    };
    let mut menus = Vec::new();
    if mac {
        menus.push(MenuItem::submenu(
            "Lenslet",
            [
                MenuItem::role(MenuRole::About),
                MenuItem::separator(),
                MenuItem::role(MenuRole::Hide),
                MenuItem::role(MenuRole::HideOthers),
                MenuItem::role(MenuRole::ShowAll),
                MenuItem::separator(),
                MenuItem::role(MenuRole::Quit),
            ],
        ));
    }
    let mut file = vec![
        MenuItem::new(t("Export Session…", "导出会话…"))
            .shortcut("mod+e")
            .on_select(move || ui.export_session()),
        MenuItem::new(t("Save IAM Policy…", "保存 IAM 策略…"))
            .shortcut("mod+shift+s")
            .on_select(move || ui.save_policy()),
        MenuItem::separator(),
        MenuItem::new(t("Copy Environment", "复制环境变量"))
            .shortcut("mod+shift+e")
            .on_select(move || ui.copy_env()),
        MenuItem::separator(),
        MenuItem::role(MenuRole::Close),
    ];
    if !mac {
        file.push(MenuItem::role(MenuRole::Quit));
    }
    menus.push(MenuItem::submenu(t("File", "文件"), file));
    menus.push(MenuItem::submenu(
        t("Edit", "编辑"),
        [
            MenuItem::role(MenuRole::Undo),
            MenuItem::role(MenuRole::Redo),
            MenuItem::separator(),
            MenuItem::role(MenuRole::Cut),
            MenuItem::role(MenuRole::Copy),
            MenuItem::role(MenuRole::Paste),
            MenuItem::role(MenuRole::SelectAll),
            MenuItem::separator(),
            MenuItem::new(t("Find", "查找"))
                .shortcut("mod+f")
                .on_select(move || {
                    ui.focus_search.update(|n| *n += 1);
                }),
            MenuItem::separator(),
            MenuItem::new(t("Copy as AWS CLI", "复制为 AWS CLI"))
                .shortcut("mod+shift+c")
                .on_select(move || ui.copy_call_as(CopyAs::Cli)),
            MenuItem::new(t("Copy as curl", "复制为 curl"))
                .shortcut("mod+alt+c")
                .on_select(move || ui.copy_call_as(CopyAs::Curl)),
            MenuItem::new(t("Copy as boto3", "复制为 boto3"))
                .on_select(move || ui.copy_call_as(CopyAs::Boto3)),
        ],
    ));
    let filter = ui.filter.get_untracked();
    menus.push(MenuItem::submenu(
        t("View", "显示"),
        [
            MenuItem::new(t("Overview", "概览"))
                .shortcut("mod+1")
                .on_select(move || ui.page.set(Page::Overview)),
            MenuItem::new(t("Calls", "调用"))
                .shortcut("mod+2")
                .on_select(move || ui.page.set(Page::Calls)),
            MenuItem::new(t("Permissions", "权限"))
                .shortcut("mod+3")
                .on_select(move || ui.page.set(Page::Permissions)),
            MenuItem::separator(),
            check("All Calls", "全部调用", filter == Filter::All).on_select(move || {
                ui.filter.set(Filter::All);
                ui.page.set(Page::Calls);
            }),
            check("Failed Calls", "失败的调用", filter == Filter::Failed)
                .shortcut("mod+shift+f")
                .on_select(move || {
                    ui.filter.set(Filter::Failed);
                    ui.page.set(Page::Calls);
                }),
            check(
                "Not Emulated",
                "未模拟的调用",
                filter == Filter::NotEmulated,
            )
            .on_select(move || {
                ui.filter.set(Filter::NotEmulated);
                ui.page.set(Page::Calls);
            }),
            MenuItem::separator(),
            check("Sidebar", "侧栏", ui.sidebar.get_untracked())
                .shortcut(&sidebar_key())
                .on_select(move || ui.sidebar.update(|v| *v = !*v)),
            check("Inspector", "检查器", ui.inspector.get_untracked())
                .shortcut("mod+alt+i")
                .on_select(move || ui.inspector.update(|v| *v = !*v)),
            check("Terminal", "终端", ui.terminal.get_untracked())
                .shortcut("ctrl+`")
                .on_select(move || ui.terminal.update(|v| *v = !*v)),
            MenuItem::separator(),
            MenuItem::submenu(
                t("Language", "语言"),
                [
                    MenuItem::new("English")
                        .checked(lang().get_untracked() == Lang::En)
                        .on_select(|| lang().set(Lang::En)),
                    MenuItem::new("中文")
                        .checked(lang().get_untracked() == Lang::Zh)
                        .on_select(|| lang().set(Lang::Zh)),
                ],
            ),
        ],
    ));
    menus.push(MenuItem::submenu(
        t("Account", "账户"),
        [
            MenuItem::new(t("Clear Calls", "清空调用"))
                .shortcut("mod+k")
                .on_select(move || ui.clear()),
            MenuItem::new(t("Refresh Resources", "刷新资源"))
                .shortcut("mod+r")
                .on_select(|| engine().refresh()),
            MenuItem::separator(),
            MenuItem::new(t("New Empty Account…", "新建空账户…"))
                .shortcut("mod+shift+n")
                .on_select(move || ui.new_account()),
        ],
    ));
    if mac {
        menus.push(MenuItem::submenu(
            t("Window", "窗口"),
            [
                MenuItem::role(MenuRole::Minimize),
                MenuItem::role(MenuRole::Zoom),
                MenuItem::role(MenuRole::ToggleFullScreen),
                MenuItem::separator(),
                MenuItem::role(MenuRole::Close),
            ],
        ));
    }
    menus.push(MenuItem::submenu(
        t("Help", "帮助"),
        [
            MenuItem::new(t("Lenslet on GitHub", "Lenslet 的 GitHub 主页"))
                .on_select(|| drop(open_url("https://github.com/hxddh/lenslet"))),
        ],
    ));
    menus
}
