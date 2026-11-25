//! The calls: a scope bar (all, failed, not emulated; one service), then a
//! table that sorts by any column, resizes its columns, moves the selection
//! with the arrow keys, and has each call's actions on its context menu.

use std::sync::Arc;

use nsh_base::{
    DataTableCellClass, DataTableColumn, DataTableContextMenu, DataTablePress, DataTableRows,
    DataTableSelection, DataTableSort, DataTableSortDirection, SelectItem,
};
use nsh_core::{
    element::desktop::{MenuItem, popup_menu_at},
    prelude::*,
};

use super::{CopyAs, Filter, Page, Ui, engine, sidebar::service_title, style::*, t, ts};
use crate::{resources::clock, session::Call};

/// Whether a call matches a search (already lowercased).
pub fn matches(call: &Call, query: &str) -> bool {
    let has = |s: &str| s.to_lowercase().contains(query);
    has(&call.operation)
        || has(&call.service)
        || call.target.as_deref().is_some_and(has)
        || call
            .failure
            .as_ref()
            .is_some_and(|f| has(&f.code) || f.message.as_deref().is_some_and(has))
        || call.status.to_string() == query
        || call.client.as_deref().is_some_and(has)
        || call.request_id.as_deref().is_some_and(has)
}

/// One row of the list: a call, and how deep it sits in what caused it.
#[derive(Clone)]
pub struct Row {
    pub call: Arc<Call>,
    pub depth: u8,
}

/// The rows of the list: the calls of `run` (or all) that pass the filters.
/// Unfiltered and unsorted, a tree: each call your code made, newest first,
/// with what it caused inside AWS under it; else flat, in the chosen order.
pub fn rows(
    calls: &[Arc<Call>],
    run: Option<u64>,
    query: &str,
    filter: Filter,
    service: Option<&str>,
    sort: Option<&DataTableSort>,
) -> Vec<Row> {
    let in_scope: Vec<&Arc<Call>> = calls
        .iter()
        .filter(|c| run.is_none() || c.run == run)
        .collect();
    let filtering = !query.is_empty() || filter != Filter::All || service.is_some();
    if filtering || sort.is_some() {
        let mut flat: Vec<Arc<Call>> = in_scope
            .into_iter()
            .filter(|c| match filter {
                Filter::All => true,
                Filter::Failed => c.failed() && !c.unsupported,
                Filter::NotEmulated => c.unsupported,
            })
            .filter(|c| service.is_none_or(|s| s == c.service))
            .filter(|c| query.is_empty() || matches(c, query))
            .cloned()
            .collect();
        order(&mut flat, sort);
        return flat
            .into_iter()
            .map(|call| Row { call, depth: 0 })
            .collect();
    }
    let ids: std::collections::HashSet<u64> = in_scope
        .iter()
        .filter(|c| c.id != 0)
        .map(|c| c.id)
        .collect();
    let mut children: std::collections::HashMap<u64, Vec<&Arc<Call>>> =
        std::collections::HashMap::new();
    let mut roots = Vec::new();
    for c in &in_scope {
        match c.parent.filter(|p| ids.contains(p)) {
            Some(p) => children.entry(p).or_default().push(c),
            None => roots.push(*c),
        }
    }
    for list in children.values_mut() {
        list.sort_by_key(|c| (c.at_ms, c.seq));
    }
    roots.sort_by(|a, b| (b.at_ms, b.seq).cmp(&(a.at_ms, a.seq)));
    let mut out = Vec::with_capacity(in_scope.len());
    fn walk<'a>(
        call: &'a Arc<Call>,
        depth: u8,
        children: &std::collections::HashMap<u64, Vec<&'a Arc<Call>>>,
        out: &mut Vec<Row>,
    ) {
        out.push(Row {
            call: call.clone(),
            depth,
        });
        if let Some(list) = children.get(&call.id) {
            for child in list {
                walk(child, depth.saturating_add(1), children, out);
            }
        }
    }
    for root in roots {
        walk(root, 0, &children, &mut out);
    }
    out
}

/// A flat list's order: by the chosen column, else newest first.
fn order(calls: &mut [Arc<Call>], sort: Option<&DataTableSort>) {
    let Some(sort) = sort else {
        calls.sort_by(|a, b| b.seq.cmp(&a.seq));
        return;
    };
    calls.sort_by(|a, b| {
        let order = match sort.key.as_ref() {
            "service" => a.service.cmp(&b.service),
            "operation" => a.operation.cmp(&b.operation),
            "target" => a.target.cmp(&b.target),
            "status" => a.status.cmp(&b.status),
            "ms" => a.ms.cmp(&b.ms),
            "client" => a.client.cmp(&b.client),
            _ => a.seq.cmp(&b.seq),
        }
        .then(a.seq.cmp(&b.seq));
        if sort.direction == DataTableSortDirection::Descending {
            order.reverse()
        } else {
            order
        }
    });
}

fn column(key: &str, title: String, width: f32, min: f32) -> DataTableColumn {
    let mut c = DataTableColumn::default();
    c.key = SharedString::from(key.to_string());
    c.title = Some(title.into());
    c.width = Some(px(width));
    c.min_width = Some(px(min));
    c.sortable = true;
    c.resizable = Some(true);
    c
}

pub fn status_text(call: &Call) -> String {
    if call.unsupported {
        ts("not emulated", "未模拟")
    } else {
        call.status.to_string()
    }
}

/// The operation, set in by how deep it sits in what caused it.
fn indented(row: &Row) -> String {
    if row.depth == 0 {
        row.call.operation.clone()
    } else {
        format!(
            "{}↳ {}",
            "    ".repeat(row.depth as usize - 1),
            row.call.operation
        )
    }
}

/// One line under the operation: the failure, else what it acted on.
pub fn summary(call: &Call) -> String {
    match &call.failure {
        _ if call.unsupported => format!(
            "{} · {}",
            t("Not emulated", "未模拟"),
            call.service.to_uppercase()
        ),
        Some(f) if call.failed() => f.code.clone(),
        _ => call.target.clone().unwrap_or_default(),
    }
}

pub fn calls(ui: Ui) -> AnyElement {
    let columns = move || {
        let _ = super::lang().get();
        let mut ms = column("ms", ts("Duration", "耗时"), 84.0, 60.0);
        ms.align = nsh_base::DataTableAlign::End;
        let mut status = column("status", ts("Status", "状态"), 72.0, 56.0);
        status.align = nsh_base::DataTableAlign::End;
        // The resource or error takes the room that is left, so the table
        // fills its pane at any width.
        let mut target = column(
            "target",
            ts("Resource or Error", "资源或错误"),
            220.0,
            120.0,
        );
        target.grow = Some(1.0);
        vec![
            column("time", ts("Time", "时间"), 80.0, 70.0),
            status,
            column("service", ts("Service", "服务"), 96.0, 60.0),
            column("operation", ts("Operation", "操作"), 190.0, 80.0),
            ms,
            target,
            column("client", ts("Client", "客户端"), 120.0, 80.0),
        ]
    };
    let data = move || {
        ui.visible.with(|rows| {
            let calls = || rows.iter().map(|r| &r.call);
            DataTableRows::new(rows.len())
                .texts("time", calls().map(|c| clock(c.at_ms)))
                .texts("service", calls().map(|c| service_title(&c.service)))
                .texts("operation", rows.iter().map(|r| indented(r)))
                .texts("target", calls().map(|c| summary(c)))
                .texts("status", calls().map(|c| c.status.to_string()))
                .texts("ms", calls().map(|c| format!("{} ms", c.ms)))
                .texts(
                    "client",
                    calls().map(|c| {
                        if c.internal {
                            "AWS".to_string()
                        } else {
                            c.client.clone().unwrap_or_default()
                        }
                    }),
                )
        })
    };
    // Failures read red and gaps in the emulator orange, in their status and
    // in what they say.
    let cell_classes = move || {
        ui.visible.with(|rows| {
            let mut classes = Vec::new();
            for (row, c) in rows.iter().map(|r| &r.call).enumerate() {
                let class = if c.unsupported {
                    "text-orange-600 dark:text-orange-300"
                } else if c.failed() {
                    "text-red-600 dark:text-red-400"
                } else {
                    continue;
                };
                for column in [1u32, 5] {
                    let mut cell = DataTableCellClass::default();
                    cell.row = row as u32;
                    cell.column = column;
                    cell.class = class.into();
                    classes.push(cell);
                }
            }
            classes
        })
    };
    let selection = move || {
        let seq = ui.selected.get()?;
        let row = ui
            .visible
            .with(|v| v.iter().position(|r| r.call.seq == seq))?;
        let mut s = DataTableSelection::default();
        s.row = Some(row as u32);
        Some(s)
    };
    let call_at = move |row: u32| {
        ui.visible
            .with_untracked(|v| v.get(row as usize).map(|r| r.call.clone()))
    };

    let mut classes = nsh_base::DataTableClasses::default();
    classes.header = Some("bg-transparent".into());
    classes.header_cell = Some("text-[11px] font-medium text-muted-foreground px-2".into());
    classes.cell = Some("px-2".into());
    classes.row_selected = Some(selected_row_class().into());
    classes.row_hover = Some("bg-black/[0.03] dark:bg-white/[0.04]".into());

    // A call selected elsewhere (the summary, a resource, the inspector)
    // scrolls into view.
    let table_ref = component_ref::<nsh_base::DataTable>();
    // Only when the selection changes, and only if the row is out of sight:
    // new calls arriving never pull the list back to it.
    let scrolled = std::rc::Rc::new(std::cell::Cell::new(None::<usize>));
    effect(move || {
        let Some(seq) = ui.selected.get() else {
            scrolled.set(None);
            return;
        };
        if scrolled.get() == Some(seq) {
            return;
        }
        // The list may catch up with the selection a moment later.
        let Some(row) = ui
            .visible
            .with(|v| v.iter().position(|r| r.call.seq == seq))
        else {
            return;
        };
        scrolled.set(Some(seq));
        if let Some(api) = table_ref.api::<nsh_base::DataTableApi>() {
            let shown = api.visible_rows();
            if !(shown.start..shown.end).contains(&(row as u32)) {
                api.scroll_to_row(row as u32, None);
            }
        }
    });
    let table = nsh_base::data_table()
        .component_ref(&table_ref)
        .columns(columns)
        .data(data)
        .cell_classes(cell_classes)
        .row_height(px(24.0))
        .header_height(px(26.0))
        .stripe(true)
        .row_selectable(true)
        .resizable_columns(true)
        .movable_columns(true)
        .text_size(px(12.5))
        .classes(classes)
        .sort(move || ui.sort.get())
        .on_sort_change(move |s: &Option<DataTableSort>| ui.sort.set(s.clone()))
        .selection(selection)
        .on_selection_change(move |s: &Option<DataTableSelection>| {
            if let Some(call) = s.and_then(|s| s.row).and_then(call_at) {
                ui.selected.set(Some(call.seq));
            }
        })
        .on_row_press(move |p: &DataTablePress| {
            if let Some(call) = call_at(p.row) {
                ui.selected.set(Some(call.seq));
                if p.clicks == 2 {
                    ui.inspector.set(true);
                }
            }
        })
        .on_context_menu(move |e: &DataTableContextMenu| {
            let Some(call) = e.row.and_then(call_at) else {
                return;
            };
            ui.selected.set(Some(call.seq));
            drop(popup_menu_at(call_menu(ui, &call), point(e.x, e.y)));
        })
        .empty(empty(ui))
        .flex_1();

    view()
        .flex_1()
        .child(super::summary::summary(ui))
        .child(hairline())
        .child(scope_bar(ui))
        .child(hairline())
        .child(table)
        .into_element()
}

fn selected_row_class() -> &'static str {
    match os() {
        nsh_core::platform::Os::MacOs => "bg-[#0064e1] dark:bg-[#0059d1] text-white",
        nsh_core::platform::Os::Windows => "bg-[#e6f0fa] dark:bg-[#2d3d4f]",
        _ => "bg-[#dbe7f7] dark:bg-[#2a3f5a]",
    }
}

/// The actions on one call.
pub fn call_menu(ui: Ui, call: &Call) -> Vec<MenuItem> {
    let mut items = vec![
        MenuItem::new(t("Copy as AWS CLI", "复制为 AWS CLI"))
            .disabled(crate::copy_as::aws_cli(call).is_none())
            .on_select(move || ui.copy_call_as(CopyAs::Cli)),
        MenuItem::new(t("Copy as curl", "复制为 curl"))
            .on_select(move || ui.copy_call_as(CopyAs::Curl)),
        MenuItem::new(t("Copy as boto3", "复制为 boto3"))
            .disabled(crate::copy_as::boto3(call).is_none())
            .on_select(move || ui.copy_call_as(CopyAs::Boto3)),
        MenuItem::separator(),
    ];
    let body = call.response.body.clone();
    items.push(
        MenuItem::new(t("Copy Response Body", "复制响应正文"))
            .disabled(body.is_empty())
            .on_select(move || ui.copy(body.clone(), "Copied the response", "已复制响应")),
    );
    if let Some(id) = call.request_id.clone() {
        items.push(
            MenuItem::new(t("Copy Request ID", "复制请求 ID"))
                .on_select(move || ui.copy(id.clone(), "Copied the request ID", "已复制请求 ID")),
        );
    }
    let resources: Vec<String> = call
        .access
        .iter()
        .map(|a| a.resource.clone())
        .filter(|r| r != "*")
        .collect();
    if let Some(arn) = resources.first().cloned() {
        items.push(
            MenuItem::new(t("Copy ARN", "复制 ARN"))
                .on_select(move || ui.copy(arn.clone(), "Copied the ARN", "已复制 ARN")),
        );
    }
    items.push(MenuItem::separator());
    let service = call.service.clone();
    items.push(
        MenuItem::new(format!(
            "{} {}",
            t("Show Only", "只看"),
            service_title(&call.service)
        ))
        .on_select(move || {
            ui.service.set(Some(service.clone()));
            ui.show_calls();
        }),
    );
    if let Some(page) = resource_page(call) {
        items.push(
            MenuItem::new(t("Show Resource", "查看资源"))
                .on_select(move || ui.page.set(page.clone())),
        );
    }
    items
}

/// The resource page a call acted on, when Lenslet lists that resource.
pub fn resource_page(call: &Call) -> Option<Page> {
    let snapshot = super::engine().snapshot();
    snapshot
        .resources
        .iter()
        .find(|g| g.service == call.service)
        .and_then(|g| {
            g.items
                .iter()
                .find(|r| call.touches(&call.service, &r.name))
                .map(|r| Page::Resource(g.service, r.name.clone()))
        })
}

/// Which calls show: all, failed, not emulated; one service.
fn scope_bar(ui: Ui) -> impl IntoElement {
    let segment = move |filter: Filter, label: fn() -> String, count: Box<dyn Fn() -> usize>| {
        let selected = move || ui.filter.get() == filter;
        nsh_base::button()
            .on_press(move || ui.filter.set(filter))
            .flex_row()
            .items_center()
            .gap(px(5.0))
            .px(px(10.0))
            .h(px(22.0))
            .rounded(radius())
            .bg(move || {
                if selected() {
                    fill_bg()
                } else {
                    AdaptiveColor::rgb(0, 0).opacity(0.0)
                }
            })
            .hover(|s| s.bg(hover_bg()))
            .child(text(label).text_size(small()).text_color(move || {
                if selected() {
                    AdaptiveColor::rgb(0x1d1d1f, 0xf5f5f7)
                } else {
                    secondary_text()
                }
            }))
            .child(
                text(move || match count() {
                    0 => String::new(),
                    n => n.to_string(),
                })
                .text_size(small())
                .text_color(move || match filter {
                    Filter::Failed => red(),
                    Filter::NotEmulated => orange(),
                    Filter::All => secondary_text(),
                }),
            )
    };
    let tally = move || {
        let run = ui.run();
        ui.snap.with(|s| s.stats.of_run(run))
    };
    let failed = move || tally().failed;
    let unsupported = move || tally().unsupported;
    let services = move || {
        let names: Vec<String> = ui.snap.with(|s| s.stats.services.iter().cloned().collect());
        let mut items = vec![SelectItem::new("", t("All Services", "全部服务"))];
        items.extend(
            names
                .iter()
                .map(|n| SelectItem::new(n.clone(), service_title(n))),
        );
        items
    };
    view()
        .flex_row()
        .items_center()
        .gap(px(2.0))
        .px(px(10.0))
        .h(px(32.0))
        .child(segment(Filter::All, || ts("All", "全部"), Box::new(|| 0)))
        .child(segment(Filter::Failed, || ts("Failed", "失败"), Box::new(failed)))
        .child(segment(Filter::NotEmulated, || ts("Not Emulated", "未模拟"), Box::new(unsupported)))
        .child(view().flex_1())
        .child(
            nsh_base::select()
                .items(services)
                .value(move || Some(SharedString::from(ui.service.get().unwrap_or_default())))
                .on_change(move |v: &Option<SharedString>| {
                    ui.service.set(v.as_ref().map(|s| s.to_string()).filter(|s| !s.is_empty()))
                })
                .trigger(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .px(px(8.0))
                        .h(px(22.0))
                        .rounded(radius())
                        .hover(|s| s.bg(hover_bg()))
                        .child(
                            text(move || match ui.service.get() {
                                Some(s) => service_title(&s),
                                None => ts("All Services", "全部服务"),
                            })
                            .text_size(small()),
                        )
                        .child(text("⌄").text_size(small()).text_color(secondary_text())),
                )
                .popup_class("bg-popover text-popover-foreground border rounded-md shadow-md p-1 text-[12px]")
                .item_class("px-2 py-1 rounded data-[highlighted]:bg-accent"),
        )
}

/// What the list says with nothing to show.
fn empty(ui: Ui) -> impl IntoElement {
    view()
        .flex_1()
        .items_center()
        .justify_center()
        .gap(px(6.0))
        .p(px(24.0))
        .child(
            text(move || {
                if ui.snap.with(|s| s.calls.is_empty()) {
                    ts("No calls yet", "还没有调用")
                } else {
                    ts("No calls match", "没有符合条件的调用")
                }
            })
            .text_size(px(15.0))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(secondary_text()),
        )
        .child(
            text(move || {
                if ui.snap.with(|s| s.calls.is_empty()) {
                    format!(
                        "{} {}",
                        t(
                            "Run your code in the terminal below, or point any SDK at",
                            "在下方终端运行你的代码，或把任何 SDK 指向"
                        ),
                        engine().endpoint()
                    )
                } else {
                    ts(
                        "Change the search or the filters above.",
                        "修改搜索或上方的筛选。",
                    )
                }
            })
            .text_size(body())
            .text_color(tertiary_text()),
        )
}
