//! The sidebar, full height as each platform draws one: the endpoint at the
//! top (where the window's controls sit), the session's views, then every
//! resource the local account holds, grouped by service, each with its count.

use std::collections::HashMap;

use nsh_core::{element::desktop::title_bar, prelude::*};

use super::{Page, Ui, bar_height, engine, style::*, t, ts};
use crate::session::{Group, Resource, now_ms};

/// A resource that just appeared stays marked this long.
const NEW_FOR_MS: u64 = 4000;

/// A service's name as AWS writes it.
pub fn service_title(service: &str) -> String {
    match service {
        "s3" => "S3",
        "dynamodb" => "DynamoDB",
        "sqs" => "SQS",
        "sns" => "SNS",
        "sts" => "STS",
        "iam" => "IAM",
        "kms" => "KMS",
        "lambda" => "Lambda",
        "secretsmanager" => "Secrets Manager",
        "ssm" => "Parameter Store",
        "events" => "EventBridge",
        "logs" => "CloudWatch Logs",
        "states" => "Step Functions",
        "kinesis" => "Kinesis",
        "aws" => "AWS",
        other => return other.to_uppercase(),
    }
    .to_string()
}

fn count_label(service: &str, n: u64) -> String {
    let (one, many, zh) = match service {
        "s3" => ("object", "objects", "个对象"),
        "dynamodb" => ("item", "items", "项"),
        "sqs" => ("message", "messages", "条消息"),
        _ => return n.to_string(),
    };
    if super::lang().get() == super::Lang::Zh {
        format!("{n} {zh}")
    } else if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

pub fn sidebar(ui: Ui) -> impl IntoElement {
    // When each resource was first seen, to mark the new ones.
    let first_seen = signal(HashMap::<String, u64>::new());
    let now = signal(now_ms());
    effect(move || {
        let groups = ui.snap.with(|s| s.resources.clone());
        let t = now_ms();
        first_seen.update(|seen| {
            for g in groups.iter() {
                for r in &g.items {
                    seen.entry(format!("{}/{}", g.service, r.name)).or_insert(t);
                }
            }
        });
    });
    set_interval(std::time::Duration::from_millis(1000), move || {
        let t = now_ms();
        if first_seen.with_untracked(|s| s.values().any(|&at| t.saturating_sub(at) < NEW_FOR_MS + 1000)) {
            now.set(t);
        }
    });
    // Resources that existed before the window opened are not new.
    let opened = now_ms();

    let header = view()
        .h(bar_height())
        .pl(move || title_bar().left.max(px(12.0)))
        .pr(px(8.0))
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .window_drag(true)
        // macOS's window controls sit here; elsewhere the app's name does.
        .child(when(
            move || os() != nsh_core::platform::Os::MacOs,
            move || text("Lenslet").font_weight(FontWeight::SEMIBOLD).pl(px(6.0)),
        ))
        .child(view().flex_1())
        .child(tool_button(
            Icon::Sidebar,
            move || format!("{} ({})", t("Hide Sidebar", "隐藏侧栏"), key_label(&super::sidebar_key())),
            || false,
            move || ui.sidebar.set(false),
        ));

    let endpoint = nsh_base::button()
        .on_press(move || ui.copy_env())
        .on_context_menu(move |e: &ContextMenuEvent| {
            drop(nsh_core::element::desktop::popup_menu_at(
                vec![
                    nsh_core::element::desktop::MenuItem::new(t("Copy Environment", "复制环境变量"))
                        .on_select(move || ui.copy_env()),
                    nsh_core::element::desktop::MenuItem::new(t("Copy Endpoint URL", "复制端点地址"))
                        .on_select(move || ui.copy(engine().endpoint(), "Copied the endpoint", "已复制端点")),
                ],
                e.page_position,
            ))
        })
        .mx(px(10.0))
        .mb(px(6.0))
        .px(px(8.0))
        .py(px(6.0))
        .gap(px(2.0))
        .rounded(px(8.0))
        .hover(|s| s.bg(hover_bg()))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .child(view().w(px(7.0)).h(px(7.0)).rounded(px(4.0)).bg(green()))
                .child(text(move || ts("Local AWS", "本地 AWS")).font_weight(FontWeight::SEMIBOLD)),
        )
        .child(
            text(move || engine().endpoint().trim_start_matches("http://").to_string())
                .font_family(MONO)
                .text_size(small())
                .text_color(secondary_text()),
        );

    let nav = view()
        .px(px(10.0))
        .gap(px(1.0))
        .child(row(
            ui,
            Icon::Overview,
            Box::new(|| ts("Overview", "概览")),
            Box::new(|| String::new()),
            Box::new(secondary_text),
            Page::Overview,
        ))
        .child(row(
            ui,
            Icon::Calls,
            Box::new(|| ts("Calls", "调用")),
            Box::new(move || {
                let failed = ui.snap.with(|s| s.calls.iter().filter(|c| c.failed() && !c.unsupported).count());
                if failed > 0 { failed.to_string() } else { ui.snap.with(|s| s.calls.len()).to_string() }
            }),
            Box::new(move || {
                if ui.snap.with(|s| s.calls.iter().any(|c| c.failed() && !c.unsupported)) { red() } else { secondary_text() }
            }),
            Page::Calls,
        ))
        .child(row(
            ui,
            Icon::Shield,
            Box::new(|| ts("Permissions", "权限")),
            Box::new(move || ui.snap.with(|s| s.permissions.len()).to_string()),
            Box::new(secondary_text),
            Page::Permissions,
        ));

    let groups = each(
        move || ui.snap.with(|s| s.resources.iter().cloned().collect::<Vec<Group>>()),
        |g: &Group| g.service,
        move |group, _| {
            let g = group.get_untracked();
            let service = g.service;
            view()
                .gap(px(1.0))
                .child(
                    view()
                        .flex_row()
                        .px(px(8.0))
                        .pt(px(10.0))
                        .pb(px(3.0))
                        .child(section(move || service_title(service)))
                        .child(view().flex_1())
                        .child(text(move || group.get().items.len().to_string()).text_size(small()).text_color(tertiary_text())),
                )
                .child(each(
                    move || group.get().items,
                    |r: &Resource| r.name.clone(),
                    move |resource, _| {
                        let r = resource.get_untracked();
                        let name = r.name.clone();
                        let key = format!("{service}/{name}");
                        let is_new = move || {
                            let t = now.get();
                            first_seen.with(|s| s.get(&key).is_some_and(|&at| at > opened && t.saturating_sub(at) < NEW_FOR_MS))
                        };
                        let name_for_label = name.clone();
                        resource_row(
                            ui,
                            service,
                            name.clone(),
                            Box::new(move || name_for_label.clone()),
                            Box::new(move || resource.get().count.map(|n| count_label(service, n)).unwrap_or_default()),
                            Box::new(is_new),
                        )
                    },
                ))
        },
    );

    let empty_resources = when(
        move || ui.snap.with(|s| s.resources.is_empty()),
        move || {
            text(move || ts("Nothing yet. What your code creates appears here.", "还没有资源。代码创建的资源会显示在这里。"))
                .px(px(18.0))
                .pt(px(4.0))
                .text_size(small())
                .text_color(tertiary_text())
        },
    );

    view()
        .flex_1()
        .bg(sidebar_bg())
        .child(header)
        .child(endpoint)
        .child(
            scroll().flex_1().child(
                view()
                    .pb(px(16.0))
                    .child(nav)
                    .child(
                        view()
                            .px(px(10.0))
                            .child(view().px(px(8.0)).pt(px(14.0)).child(section(|| ts("Resources", "资源"))))
                            .child(empty_resources)
                            .child(groups),
                    ),
            ),
        )
}

/// A row of the sidebar that shows a page.
fn row(
    ui: Ui,
    icon_name: Icon,
    label: Box<dyn Fn() -> String>,
    count: Box<dyn Fn() -> String>,
    count_color: Box<dyn Fn() -> AdaptiveColor>,
    page: Page,
) -> impl IntoElement {
    let target = page.clone();
    let selected = move || ui.page.get() == page;
    nsh_base::button()
        .on_press(move || ui.page.set(target.clone()))
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(8.0))
        .h(px(28.0))
        .rounded(radius())
        .bg(move || if selected() { selected_bg() } else { AdaptiveColor::rgb(0, 0).opacity(0.0) })
        .hover(|s| s.bg(hover_bg()))
        .child(icon_colored(icon_name, 16.0, accent()))
        .child(text(label).flex_1().text_size(body()))
        .child(badge(count, count_color))
}

fn resource_row(
    ui: Ui,
    service: &'static str,
    name: String,
    label: Box<dyn Fn() -> String>,
    count: Box<dyn Fn() -> String>,
    is_new: Box<dyn Fn() -> bool>,
) -> impl IntoElement {
    let page = Page::Resource(service, name);
    let target = page.clone();
    let selected = move || ui.page.get() == page;
    nsh_base::button()
        .on_press(move || ui.page.set(target.clone()))
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(8.0))
        .h(px(26.0))
        .rounded(radius())
        .bg(move || if selected() { selected_bg() } else { AdaptiveColor::rgb(0, 0).opacity(0.0) })
        .hover(|s| s.bg(hover_bg()))
        .child(icon_colored(Icon::of_service(service), 15.0, accent()))
        .child(text(label).flex_1().text_size(body()).truncate())
        .child(view().w(px(6.0)).h(px(6.0)).rounded(px(3.0)).bg(move || if is_new() { accent() } else { accent().opacity(0.0) }))
        .child(badge(count, || tertiary_text()))
}

/// A selected sidebar row's fill.
fn selected_bg() -> AdaptiveColor {
    AdaptiveColor::rgb(0x000000, 0xffffff).opacity(0.09)
}
