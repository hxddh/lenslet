//! Lenslet: a window onto a local AWS. It embeds AWS Emulator, serves it on
//! one fixed local endpoint, and shows every call your code makes, the
//! resources that exist, and the IAM permissions the session needed.

pub mod emulator;
pub mod engine;
pub mod resources;
pub mod server;
pub mod session;

use std::{collections::HashMap, time::Duration};

use nsh_base::{button, theme};
use nsh_core::element::desktop::window as app_window;
use nsh_core::prelude::*;

use engine::Engine;
use session::{Call, Group, Snapshot};

#[native_module("lenslet/native")]
fn native(registry: &mut Registry) {
    registry.register::<App>();
}

const MONO: &str = "monospace";
const GREEN: u32 = 0x2da44e;
const RED: u32 = 0xcf222e;
const AMBER: u32 = 0xbf8700;
/// A resource that just appeared stays marked this long.
const NEW_FOR_MS: u64 = 3000;

// -- Language -----------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Default)]
enum Lang {
    #[default]
    En,
    Zh,
}

fn lang() -> Signal<Lang> {
    global::<Lang>()
}

/// English or Chinese, following the window's language.
fn t(en: &'static str, zh: &'static str) -> &'static str {
    if lang().get() == Lang::Zh { zh } else { en }
}

fn system_lang() -> Lang {
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_default();
    if locale.starts_with("zh") { Lang::Zh } else { Lang::En }
}

// -- The window -----------------------------------------------------------------

#[component]
pub fn App() -> impl IntoElement {
    lang().set(system_lang());
    let content = match Engine::start(Engine::port_from_env()) {
        Ok(engine) => workbench(engine),
        Err(error) => cannot_start(error),
    };
    app_window()
        .id("main")
        .title("Lenslet")
        .default_size(size(px(1280.0), px(800.0)))
        .content(move || content.clone())
}

fn cannot_start(error: String) -> AnyElement {
    view()
        .flex_1()
        .items_center()
        .justify_center()
        .gap_3()
        .bg(move || theme().background)
        .text_color(move || theme().foreground)
        .child(text(move || t("Lenslet could not start its endpoint", "Lenslet 无法启动本地端点")).text_lg())
        .child(text(error.clone()).font_family(MONO).text_sm().text_color(rgb(RED)))
        .child(
            text(move || t(
                "Another Lenslet may be running. Set LENSLET_PORT to use another port.",
                "可能已有另一个 Lenslet 在运行。设置 LENSLET_PORT 可换一个端口。",
            ))
            .text_color(move || theme().muted_foreground),
        )
        .into_any_element()
}

fn workbench(engine: Engine) -> AnyElement {
    let snapshot = signal(Snapshot::default());
    let selected = signal(None::<usize>);
    let first_seen = signal(HashMap::<String, u64>::new());
    let now = signal(session::now_ms());
    let toast = signal(None::<&'static str>);
    {
        let engine = engine.clone();
        set_interval(Duration::from_millis(100), move || {
            let next = engine.snapshot();
            if next != snapshot.get_untracked() {
                // Mark resources that were not there before.
                first_seen.update(|seen| {
                    for group in &next.resources {
                        for (name, _) in &group.items {
                            seen.entry(format!("{}/{name}", group.service)).or_insert_with(session::now_ms);
                        }
                    }
                });
                let fresh_selection = selected.get_untracked().is_none_or(|i| i >= next.calls.len());
                if fresh_selection {
                    selected.set(next.calls.iter().position(|c| c.failure.is_some() || c.unsupported));
                }
                snapshot.set(next);
            }
            let t = session::now_ms();
            if first_seen.with_untracked(|s| s.values().any(|&at| t.saturating_sub(at) < NEW_FOR_MS + 500)) {
                now.set(t);
            }
        });
    }
    let say = move |message: &'static str| {
        toast.set(Some(message));
        set_timeout(Duration::from_millis(1600), move || toast.set(None));
    };

    let header = {
        let (copy_env, fresh) = (engine.clone(), engine.clone());
        let endpoint = engine.endpoint();
        view()
            .flex_row()
            .items_center()
            .gap_3()
            .px_4()
            .py_2()
            .border_b_1()
            .border_b_color(move || theme().border)
            .child(text("●").text_color(rgb(GREEN)))
            .child(text(move || t("Local AWS", "本地 AWS")).font_weight(FontWeight::SEMIBOLD))
            .child(
                text(format!("{} · {}", engine::ACCOUNT, engine::REGION))
                    .font_family(MONO)
                    .text_sm()
                    .text_color(move || theme().muted_foreground),
            )
            .child(view().flex_1())
            .child(text(move || toast.get().unwrap_or("")).text_sm().text_color(rgb(GREEN)))
            .child(
                small_button(move || endpoint.clone()).on_press(move || {
                    set_clipboard(copy_env.env());
                    say(t("Copied the environment", "已复制环境变量"));
                }),
            )
            .child(small_button(move || t("⟲ Fresh start", "⟲ 全新开始").to_string()).on_press(move || {
                fresh.fresh();
                say(t("A new, empty AWS", "已换成全新的空 AWS"));
            }))
            .child(
                small_button(move || if lang().get() == Lang::Zh { "EN".into() } else { "中文".into() })
                    .on_press(move || lang().update(|l| *l = if *l == Lang::Zh { Lang::En } else { Lang::Zh })),
            )
    };

    let tree = scroll().w(px(240.0)).border_r_1().border_r_color(move || theme().border).child(
        view()
            .p_3()
            .gap_3()
            .child(caption(move || t("Resources", "资源")))
            .child(text(move || if snapshot.with(|s| s.resources.is_empty()) { t("Nothing yet", "暂无") } else { "" })
                .text_sm()
                .text_color(move || theme().muted_foreground))
            .child(each(
                move || snapshot.with(|s| s.resources.clone()),
                |group: &Group| group.service,
                move |group, _| resource_group(group.get_untracked(), first_seen, now),
            )),
    );

    let calls = virtual_list(
        move || snapshot.with(|s| s.calls.len()),
        move |at| call_row(at.index(), snapshot, selected),
    )
    .estimated_item_size(px(52.0))
    .flex_1();

    let center = view()
        .flex_1()
        .child(calls)
        .child(empty_state(engine.endpoint(), snapshot));

    let detail = scroll()
        .w(px(400.0))
        .border_l_1()
        .border_l_color(move || theme().border)
        .child(detail_view(snapshot, selected));

    let footer = {
        let engine = engine.clone();
        view()
            .flex_row()
            .items_center()
            .gap_3()
            .px_4()
            .py_2()
            .border_t_1()
            .border_t_color(move || theme().border)
            .child(text(move || {
                let n = snapshot.with(|s| s.permissions.len());
                if lang().get() == Lang::Zh {
                    format!("本次会话用到 {n} 项 IAM 权限")
                } else if n == 1 {
                    "This session needed 1 IAM permission".to_string()
                } else {
                    format!("This session needed {n} IAM permissions")
                }
            }))
            .child(small_button(move || t("Copy IAM policy", "复制 IAM 策略").to_string()).on_press(move || {
                set_clipboard(engine.policy());
                say(t("Copied the IAM policy", "已复制 IAM 策略"));
            }))
    };

    view()
        .flex_1()
        .bg(move || theme().background)
        .text_color(move || theme().foreground)
        .child(header)
        .child(view().flex_1().flex_row().child(tree).child(center).child(detail))
        .child(footer)
        .into_any_element()
}

// -- Pieces ---------------------------------------------------------------------

fn small_button(label: impl Fn() -> String + 'static) -> nsh_base::Button {
    button()
        .px_3()
        .py_1()
        .border_1()
        .border_color(move || theme().border)
        .rounded(move || theme().radius)
        .active(|s| s.opacity(0.6))
        .child(text(label).text_sm())
}

fn caption(label: impl Fn() -> &'static str + 'static) -> impl IntoElement {
    text(label).text_xs().text_color(move || theme().muted_foreground)
}

fn resource_group(group: Group, first_seen: Signal<HashMap<String, u64>>, now: Signal<u64>) -> impl IntoElement {
    let service = group.service;
    let mut column = view().gap_1().child(text(service).text_sm().font_weight(FontWeight::SEMIBOLD));
    for (name, detail) in group.items {
        let key = format!("{service}/{name}");
        let is_new = move || {
            let at = first_seen.with(|s| s.get(&key).copied().unwrap_or(0));
            now.get().saturating_sub(at) < NEW_FOR_MS
        };
        column = column.child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .pl_2()
                .child(text("●").text_xs().text_color(move || if is_new() { rgb(GREEN).into() } else { theme().border }))
                .child(text(name.clone()).font_family(MONO).text_sm().flex_1())
                .child(text(detail.clone().unwrap_or_default()).text_xs().text_color(move || theme().muted_foreground)),
        );
    }
    column
}

fn call_row(i: usize, snapshot: Signal<Snapshot>, selected: Signal<Option<usize>>) -> impl IntoElement {
    let call = move || snapshot.with(|s| s.calls.get(i).cloned());
    let starts_generation = move || {
        snapshot.with(|s| match (i.checked_sub(1).and_then(|p| s.calls.get(p)), s.calls.get(i)) {
            (Some(prev), Some(this)) => prev.generation != this.generation,
            _ => false,
        })
    };
    let status_color = move || match call() {
        Some(c) if c.unsupported => rgb(AMBER),
        Some(c) if c.status >= 400 => rgb(RED),
        _ => rgb(GREEN),
    };
    view()
        .child(
            text(move || if starts_generation() { t("— fresh start —", "— 全新开始 —") } else { "" })
                .text_xs()
                .self_center()
                .text_color(move || theme().muted_foreground),
        )
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_3()
                .px_3()
                .py_2()
                .border_b_1()
                .border_b_color(move || theme().border)
                .bg(move || if selected.get() == Some(i) { theme().accent } else { theme().background })
                .on_press(move || selected.set(Some(i)))
                .child(
                    text(move || call().map(|c| c.service.to_uppercase()).unwrap_or_default())
                        .w(px(88.0))
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .child(
                    view()
                        .flex_1()
                        .child(text(move || call().map(|c| c.operation).unwrap_or_default()))
                        .child(
                            text(move || call().map(|c| subtitle(&c)).unwrap_or_default())
                                .text_xs()
                                .font_family(MONO)
                                .text_color(move || {
                                    if call().is_some_and(|c| c.status >= 400) { status_color().into() } else { theme().muted_foreground }
                                }),
                        ),
                )
                .child(
                    text(move || call().map(|c| format!("{} ms", c.ms)).unwrap_or_default())
                        .font_family(MONO)
                        .text_xs()
                        .text_color(move || theme().muted_foreground),
                )
                .child(
                    text(move || call().map(|c| status_label(&c)).unwrap_or_default())
                        .font_family(MONO)
                        .text_sm()
                        .text_color(status_color),
                ),
        )
}

fn subtitle(call: &Call) -> String {
    match (&call.failure, &call.target) {
        (Some(f), _) => format!("{}  {}", f.code, f.message.clone().unwrap_or_default()),
        (None, Some(target)) => target.clone(),
        (None, None) => call.path.clone(),
    }
}

fn status_label(call: &Call) -> String {
    if call.unsupported {
        t("not emulated", "未模拟").to_string()
    } else {
        call.status.to_string()
    }
}

fn empty_state(endpoint: String, snapshot: Signal<Snapshot>) -> impl IntoElement {
    let shown = move || snapshot.with(|s| s.calls.is_empty());
    view()
        .absolute()
        .inset_0()
        .items_center()
        .justify_center()
        .gap_3()
        .opacity(move || if shown() { 1.0 } else { 0.0 })
        .child(text(move || if shown() { t("Your local AWS is ready", "本地 AWS 已就绪") } else { "" }).text_xl())
        .child(
            text(move || if shown() { t("Point your code at it, and every call shows up here:", "把代码指向它，每次调用都会出现在这里：") } else { "" })
                .text_color(move || theme().muted_foreground),
        )
        .child(
            text(move || {
                if shown() {
                    format!("export AWS_ENDPOINT_URL={endpoint}\naws s3 mb s3://hello-lenslet")
                } else {
                    String::new()
                }
            })
            .font_family(MONO)
            .text_sm()
            .selectable(true),
        )
}

fn detail_view(snapshot: Signal<Snapshot>, selected: Signal<Option<usize>>) -> impl IntoElement {
    let call = move || selected.get().and_then(|i| snapshot.with(|s| s.calls.get(i).cloned()));
    view()
        .p_4()
        .gap_3()
        .child(
            text(move || call().map(|c| format!("{} {}", c.service.to_uppercase(), c.operation)).unwrap_or_else(|| t("Select a call", "选择一次调用").into()))
                .text_lg()
                .font_weight(FontWeight::SEMIBOLD),
        )
        .child(
            text(move || call().map(|c| format!("{} {}  ·  {}  ·  {} ms", c.method, c.path, c.status, c.ms)).unwrap_or_default())
                .font_family(MONO)
                .text_xs()
                .text_color(move || theme().muted_foreground),
        )
        .child(
            text(move || match call() {
                Some(c) if c.unsupported => t(
                    "AWS Emulator does not serve this yet, or Lenslet does not include this service. Your code may be right.",
                    "AWS Emulator 暂未支持，或 Lenslet 未包含此服务。你的代码可能没有问题。",
                )
                .to_string(),
                Some(Call { failure: Some(f), .. }) => {
                    let mut s = format!("{}\n{}", f.code, f.message.unwrap_or_default());
                    if let Some(hint) = f.hint {
                        s.push_str(&format!("\n→ {hint}"));
                    }
                    s
                }
                _ => String::new(),
            })
            .text_sm()
            .text_color(move || if call().is_some_and(|c| c.unsupported) { rgb(AMBER) } else { rgb(RED) }),
        )
        .child(caption(move || if call().is_some() { t("Needs", "所需权限") } else { "" }))
        .child(
            text(move || {
                call()
                    .map(|c| {
                        if c.access.is_empty() {
                            t("no IAM permission", "无需 IAM 权限").to_string()
                        } else {
                            c.access.iter().map(|a| format!("{}\n  {}", a.action, a.resource)).collect::<Vec<_>>().join("\n")
                        }
                    })
                    .unwrap_or_default()
            })
            .font_family(MONO)
            .text_sm()
            .selectable(true),
        )
        .child(caption(move || if call().is_some() { t("Request", "请求") } else { "" }))
        .child(text(move || call().map(|c| pretty(&c.request.body)).unwrap_or_default()).font_family(MONO).text_xs().selectable(true))
        .child(caption(move || if call().is_some() { t("Response", "响应") } else { "" }))
        .child(text(move || call().map(|c| pretty(&c.response.body)).unwrap_or_default()).font_family(MONO).text_xs().selectable(true))
}

/// JSON indented; anything else as it came.
fn pretty(body: &str) -> String {
    if body.is_empty() {
        return "—".into();
    }
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| body.to_string())
}
