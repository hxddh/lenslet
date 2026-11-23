//! The content pages besides the calls: the overview (how to start, then
//! what the session did), the permissions (each with the calls that needed
//! it, and the policy), and one resource (what it holds, and the calls that
//! touched it).

use std::{collections::BTreeMap, sync::Arc};

use nsh_base::{DataTableColumn, DataTablePress, DataTableRows};
use nsh_core::prelude::*;

use super::{Page, Ui, calls, engine, sidebar::service_title, style::*, t, ts};
use crate::{
    resources::{Contents, clock},
    session::{Call, Resource},
};

// -- Overview -----------------------------------------------------------------

pub fn overview(ui: Ui) -> AnyElement {
    let started = computed(move || ui.snap.with(|s| !s.calls.is_empty()));
    scroll()
        .flex_1()
        .child(dynamic(move || {
            if started.get() {
                session(ui).into_element()
            } else {
                welcome(ui).into_element()
            }
        }))
        .into_element()
}

/// Before the first call: what Lenslet is and how to use it, in one step.
fn welcome(ui: Ui) -> impl IntoElement {
    let example = move |command: &'static str, about: fn() -> String| {
        nsh_base::button()
            .on_press(move || {
                ui.terminal.set(true);
                ui.run.set(Some(command.to_string()));
            })
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(10.0))
            .py(px(8.0))
            .rounded(px(8.0))
            .bg(fill_bg())
            .hover(|s| s.bg(hover_bg()))
            .child(icon(Icon::Terminal, 15.0))
            .child(text(command).font_family(MONO).text_size(px(12.0)).flex_1())
            .child(text(about).text_size(small()).text_color(secondary_text()))
    };
    view()
        .p(px(36.0))
        .gap(px(18.0))
        .max_w(px(720.0))
        .child(
            view()
                .gap(px(6.0))
                .child(
                    text(move || ts("Your local AWS account is ready", "你的本地 AWS 账户已就绪"))
                        .text_size(px(22.0))
                        .font_weight(FontWeight::BOLD),
                )
                .child(
                    text(move || {
                        format!(
                            "{} {} {}",
                            t("S3, DynamoDB, SQS, Lambda and ten more services, on", "S3、DynamoDB、SQS、Lambda 等 14 个服务，运行在"),
                            engine().endpoint(),
                            t("— in memory, in this app.", "——在内存里，就在这个应用中。"),
                        )
                    })
                    .text_size(body())
                    .text_color(secondary_text()),
                ),
        )
        .child(
            view()
                .gap(px(8.0))
                .child(section(|| ts("Run your code in the terminal below", "在下方终端里运行你的代码")))
                .child(
                    text(move || {
                        ts(
                            "It is already pointed here: any AWS SDK or the AWS CLI you run there talks to this account. Try:",
                            "它已经指向这里：在终端里运行的任何 AWS SDK 或 AWS CLI 都会访问这个账户。试试：",
                        )
                    })
                    .text_size(body()),
                )
                .child(example("aws s3 mb s3://demo-bucket", || ts("create a bucket", "创建存储桶")))
                .child(example(
                    "aws sqs create-queue --queue-name jobs",
                    || ts("create a queue", "创建队列"),
                ))
                .child(example(
                    "aws dynamodb create-table --table-name orders --attribute-definitions AttributeName=id,AttributeType=S --key-schema AttributeName=id,KeyType=HASH --billing-mode PAY_PER_REQUEST",
                    || ts("create a table", "创建表"),
                )),
        )
        .child(
            view()
                .gap(px(8.0))
                .child(section(|| ts("Or from any shell", "或者在任何终端里")))
                .child(
                    text(move || engine().env())
                        .font_family(MONO)
                        .text_size(px(12.0))
                        .selectable(true)
                        .p(px(10.0))
                        .rounded(px(8.0))
                        .bg(fill_bg()),
                )
                .child(view().flex_row().child(push_button(|| ts("Copy Environment", "复制环境变量"), move || ui.copy_env()))),
        )
}

/// After calls: the numbers, by service, by client, the latest failures.
fn session(ui: Ui) -> impl IntoElement {
    let stats = computed(move || {
        ui.snap.with(|s| {
            Stats::of(
                &s.calls,
                s.permissions.len(),
                s.resources.iter().map(|g| g.items.len()).sum(),
            )
        })
    });
    let card = move |label: fn() -> String,
                     value: Box<dyn Fn(&Stats) -> String>,
                     color: Box<dyn Fn(&Stats) -> AdaptiveColor>,
                     page: Option<(Page, Option<super::Filter>)>| {
        nsh_base::button()
            .on_press(move || {
                if let Some((page, filter)) = page.clone() {
                    if let Some(f) = filter {
                        ui.filter.set(f);
                    }
                    ui.page.set(page);
                }
            })
            .flex_1()
            .min_w(px(120.0))
            .p(px(12.0))
            .gap(px(2.0))
            .rounded(px(10.0))
            .bg(fill_bg())
            .hover(|s| s.bg(hover_bg()))
            .child(text(label).text_size(small()).text_color(secondary_text()))
            .child(
                text(move || stats.with(|s| value(s)))
                    .text_size(px(22.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(move || stats.with(|s| color(s))),
            )
    };
    let plain = |_: &Stats| AdaptiveColor::rgb(0x1d1d1f, 0xf5f5f7);
    let cards = view()
        .flex_row()
        .flex_wrap()
        .gap(px(10.0))
        .child(card(
            || ts("Calls", "调用"),
            Box::new(|s| s.calls.to_string()),
            Box::new(plain),
            Some((Page::Calls, Some(super::Filter::All))),
        ))
        .child(card(
            || ts("Failed", "失败"),
            Box::new(|s| s.failed.to_string()),
            Box::new(move |s| if s.failed > 0 { red() } else { plain(s) }),
            Some((Page::Calls, Some(super::Filter::Failed))),
        ))
        .child(card(
            || ts("Not emulated", "未模拟"),
            Box::new(|s| s.unsupported.to_string()),
            Box::new(move |s| {
                if s.unsupported > 0 {
                    orange()
                } else {
                    plain(s)
                }
            }),
            Some((Page::Calls, Some(super::Filter::NotEmulated))),
        ))
        .child(card(
            || ts("Median", "中位耗时"),
            Box::new(|s| format!("{} ms", s.p50)),
            Box::new(plain),
            None,
        ))
        .child(card(
            || ts("95th percentile", "P95 耗时"),
            Box::new(|s| format!("{} ms", s.p95)),
            Box::new(plain),
            None,
        ))
        .child(card(
            || ts("Retries", "重试"),
            Box::new(|s| s.retries.to_string()),
            Box::new(plain),
            None,
        ))
        .child(card(
            || ts("Permissions", "权限"),
            Box::new(|s| s.permissions.to_string()),
            Box::new(plain),
            Some((Page::Permissions, None)),
        ))
        .child(card(
            || ts("Resources", "资源"),
            Box::new(|s| s.resources.to_string()),
            Box::new(plain),
            None,
        ));

    let by_service = table_of(
        move || {
            let _ = super::lang().get();
            vec![
                col("service", ts("Service", "服务"), 160.0),
                col("calls", ts("Calls", "调用"), 70.0),
                col("failed", ts("Failed", "失败"), 70.0),
                col("p50", ts("Median", "中位"), 80.0),
                col("p95", "P95".into(), 80.0),
                col("ops", ts("Operations", "操作"), 320.0),
            ]
        },
        move || {
            stats.with(|s| {
                let rows = &s.services;
                DataTableRows::new(rows.len())
                    .texts("service", rows.iter().map(|r| service_title(&r.service)))
                    .texts("calls", rows.iter().map(|r| r.calls.to_string()))
                    .texts("failed", rows.iter().map(|r| r.failed.to_string()))
                    .texts("p50", rows.iter().map(|r| format!("{} ms", r.p50)))
                    .texts("p95", rows.iter().map(|r| format!("{} ms", r.p95)))
                    .texts("ops", rows.iter().map(|r| r.operations.clone()))
            })
        },
        move |row| {
            if let Some(service) =
                stats.with_untracked(|s| s.services.get(row).map(|r| r.service.clone()))
            {
                ui.service.set(Some(service));
                ui.page.set(Page::Calls);
            }
        },
        move || stats.with(|s| s.services.len()),
    );
    let clients = text(move || {
        stats.with(|s| {
            s.clients
                .iter()
                .map(|(client, n)| format!("{client} — {n}"))
                .collect::<Vec<_>>()
                .join("\n")
        })
    })
    .text_size(body())
    .selectable(true);
    let failures = each(
        move || stats.with(|s| s.recent_failures.clone()),
        |c: &FailureRow| c.seq,
        move |row, _| {
            let r = row.get_untracked();
            let seq = r.seq;
            nsh_base::button()
                .on_press(move || ui.show_call(seq))
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .px(px(8.0))
                .h(px(26.0))
                .rounded(radius())
                .hover(|s| s.bg(hover_bg()))
                .child(
                    text(r.time.clone())
                        .font_family(MONO)
                        .text_size(small())
                        .text_color(secondary_text()),
                )
                .child(
                    text(r.operation.clone())
                        .text_size(body())
                        .w(px(220.0))
                        .truncate(),
                )
                .child(
                    text(r.error.clone())
                        .text_size(body())
                        .text_color(red())
                        .flex_1()
                        .truncate(),
                )
        },
    );
    view()
        .p(px(24.0))
        .gap(px(22.0))
        .child(cards)
        .child(
            view()
                .gap(px(8.0))
                .child(section(|| ts("By service", "按服务")))
                .child(by_service),
        )
        .child(
            view()
                .flex_row()
                .gap(px(28.0))
                .child(
                    view()
                        .flex_1()
                        .gap(px(8.0))
                        .child(section(|| ts("Latest failures", "最近的失败")))
                        .child(failures)
                        .child(when(
                            move || stats.with(|s| s.recent_failures.is_empty()),
                            move || {
                                text(move || ts("None", "无"))
                                    .text_size(body())
                                    .text_color(tertiary_text())
                            },
                        )),
                )
                .child(
                    view()
                        .w(px(260.0))
                        .gap(px(8.0))
                        .child(section(|| ts("Clients", "客户端")))
                        .child(clients),
                ),
        )
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Stats {
    calls: usize,
    failed: usize,
    unsupported: usize,
    retries: usize,
    p50: u64,
    p95: u64,
    permissions: usize,
    resources: usize,
    services: Vec<ServiceRow>,
    clients: Vec<(String, usize)>,
    recent_failures: Vec<FailureRow>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct ServiceRow {
    service: String,
    calls: usize,
    failed: usize,
    p50: u64,
    p95: u64,
    operations: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct FailureRow {
    seq: usize,
    time: String,
    operation: String,
    error: String,
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

impl Stats {
    fn of(calls: &[Arc<Call>], permissions: usize, resources: usize) -> Stats {
        let mut ms: Vec<u64> = calls.iter().map(|c| c.ms).collect();
        ms.sort_unstable();
        let mut services: BTreeMap<&str, Vec<&Call>> = BTreeMap::new();
        let mut clients: BTreeMap<String, usize> = BTreeMap::new();
        for c in calls {
            services.entry(&c.service).or_default().push(c);
            *clients
                .entry(c.client.clone().unwrap_or_else(|| "—".into()))
                .or_default() += 1;
        }
        let mut services: Vec<ServiceRow> = services
            .into_iter()
            .map(|(service, calls)| {
                let mut ms: Vec<u64> = calls.iter().map(|c| c.ms).collect();
                ms.sort_unstable();
                let mut ops: BTreeMap<&str, usize> = BTreeMap::new();
                for c in &calls {
                    *ops.entry(&c.operation).or_default() += 1;
                }
                let mut ops: Vec<(&str, usize)> = ops.into_iter().collect();
                ops.sort_by(|a, b| b.1.cmp(&a.1));
                ServiceRow {
                    service: service.to_string(),
                    calls: calls.len(),
                    failed: calls.iter().filter(|c| c.failed()).count(),
                    p50: percentile(&ms, 0.5),
                    p95: percentile(&ms, 0.95),
                    operations: ops
                        .iter()
                        .map(|(o, n)| format!("{o} ×{n}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                }
            })
            .collect();
        services.sort_by(|a, b| b.calls.cmp(&a.calls));
        let mut clients: Vec<(String, usize)> = clients.into_iter().collect();
        clients.sort_by(|a, b| b.1.cmp(&a.1));
        Stats {
            calls: calls.len(),
            failed: calls
                .iter()
                .filter(|c| c.failed() && !c.unsupported)
                .count(),
            unsupported: calls.iter().filter(|c| c.unsupported).count(),
            retries: calls.iter().filter(|c| c.is_retry()).count(),
            p50: percentile(&ms, 0.5),
            p95: percentile(&ms, 0.95),
            permissions,
            resources,
            services,
            clients,
            recent_failures: calls
                .iter()
                .rev()
                .filter(|c| c.failed())
                .take(8)
                .map(|c| FailureRow {
                    seq: c.seq,
                    time: clock(c.at_ms),
                    operation: format!("{} {}", service_title(&c.service), c.operation),
                    error: calls::summary(c),
                })
                .collect(),
        }
    }
}

fn col(key: &str, title: String, width: f32) -> DataTableColumn {
    let mut c = DataTableColumn::default();
    c.key = SharedString::from(key.to_string());
    c.title = Some(title.into());
    c.width = Some(px(width));
    c.min_width = Some(px(50.0));
    c.resizable = Some(true);
    c
}

/// A small read-only table, as tall as its rows.
fn table_of(
    columns: impl Fn() -> Vec<DataTableColumn> + 'static,
    data: impl Fn() -> DataTableRows + 'static,
    on_open: impl Fn(usize) + 'static,
    rows: impl Fn() -> usize + 'static,
) -> impl IntoElement {
    let mut classes = nsh_base::DataTableClasses::default();
    classes.header_cell = Some("text-[11px] font-medium text-muted-foreground px-2".into());
    classes.cell = Some("px-2".into());
    classes.row_hover = Some("bg-black/[0.03] dark:bg-white/[0.04]".into());
    nsh_base::data_table()
        .columns(columns)
        .data(data)
        .row_height(px(24.0))
        .header_height(px(26.0))
        .stripe(true)
        .resizable_columns(true)
        .text_size(px(12.5))
        .classes(classes)
        .on_row_press(move |p: &DataTablePress| {
            if p.clicks == 2 {
                on_open(p.row as usize)
            }
        })
        .h(move || px(26.0 + 24.0 * rows().clamp(1, 14) as f32 + 2.0))
        .rounded(px(8.0))
        .border_1()
        .border_color(separator())
}

// -- Permissions ------------------------------------------------------------

pub fn permissions(ui: Ui) -> AnyElement {
    let policy = computed(move || {
        ui.snap.with(|s| {
            lenslet_core::policy::compile(
                &s.permissions
                    .iter()
                    .map(|p| p.access.clone())
                    .collect::<Vec<_>>(),
            )
        })
    });
    let rows = move || ui.snap.with(|s| s.permissions.len());
    let table = table_of(
        move || {
            let _ = super::lang().get();
            vec![
                col("action", ts("Action", "操作"), 220.0),
                col("resource", ts("Resource", "资源"), 380.0),
                col("calls", ts("Needed by", "由哪些调用引入"), 300.0),
            ]
        },
        move || {
            ui.snap.with(|s| {
                let p = &s.permissions;
                DataTableRows::new(p.len())
                    .texts("action", p.iter().map(|p| p.access.action.clone()))
                    .texts("resource", p.iter().map(|p| p.access.resource.clone()))
                    .texts(
                        "calls",
                        p.iter().map(|p| {
                            let names: Vec<String> = p
                                .calls
                                .iter()
                                .rev()
                                .take(3)
                                .filter_map(|seq| {
                                    s.calls
                                        .iter()
                                        .find(|c| c.seq == *seq)
                                        .map(|c| c.operation.clone())
                                })
                                .collect();
                            let more = p.calls.len().saturating_sub(3);
                            if more > 0 {
                                format!("{} +{more}", names.join(", "))
                            } else {
                                names.join(", ")
                            }
                        }),
                    )
            })
        },
        move |row| {
            if let Some(seq) = ui
                .snap
                .with_untracked(|s| s.permissions.get(row).and_then(|p| p.calls.last().copied()))
            {
                ui.show_call(seq);
            }
        },
        rows,
    );
    scroll()
        .flex_1()
        .child(
            view()
                .p(px(24.0))
                .gap(px(16.0))
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            text(move || {
                                ts(
                                    "The least-privilege IAM policy for what this session did. Double-click a permission to see the call that needed it.",
                                    "本次会话所需的最小权限 IAM 策略。双击一项权限，查看引入它的调用。",
                                )
                            })
                            .flex_1()
                            .text_size(body())
                            .text_color(secondary_text()),
                        )
                        .child(push_button(|| ts("Copy Policy", "复制策略"), move || {
                            ui.copy(engine().policy(), "Copied the IAM policy", "已复制 IAM 策略")
                        }))
                        .child(push_button(|| ts("Save…", "保存…"), move || ui.save_policy())),
                )
                .child(table)
                .child(section(|| ts("Policy", "策略")))
                .child(
                    text(move || policy.get())
                        .font_family(MONO)
                        .text_size(px(11.5))
                        .selectable(true)
                        .p(px(10.0))
                        .rounded(px(8.0))
                        .bg(fill_bg()),
                ),
        )
        .into_element()
}

// -- One resource -------------------------------------------------------------

pub fn resource(ui: Ui) -> AnyElement {
    let which = computed(move || match ui.page.get() {
        Page::Resource(service, name) => Some((service, name)),
        _ => None,
    });
    let holder = computed(move || {
        let (service, name) = which.get()?;
        ui.snap.with(|s| {
            s.resources
                .iter()
                .find(|g| g.service == service)
                .and_then(|g| g.items.iter().find(|r| r.name == name).cloned())
                .map(|r| (service, r))
        })
    });
    let contents = signal(None::<Contents>);
    let preview = signal(None::<(String, String)>);
    // Fetch what it holds when it is opened, and again when the account changed.
    effect(move || {
        let Some((service, resource)) = holder.get() else {
            return;
        };
        let _ = ui.snap.with(|s| s.version);
        let fetch = engine().contents(service, resource);
        spawn(async move {
            contents.set(Some(fetch.await));
        });
    });
    effect(move || {
        let _ = which.get();
        preview.set(None);
    });
    let touched = computed(move || {
        let Some((service, name)) = which.get() else {
            return Vec::new();
        };
        ui.snap.with(|s| {
            s.calls
                .iter()
                .rev()
                .filter(|c| c.touches(service, &name))
                .take(200)
                .map(|c| {
                    (
                        c.seq,
                        clock(c.at_ms),
                        c.operation.clone(),
                        calls::status_text(c),
                        c.failed(),
                    )
                })
                .collect::<Vec<_>>()
        })
    });

    let header = view()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .child(dynamic(move || {
            let service = which.get().map(|w| w.0).unwrap_or("s3");
            icon_colored(Icon::of_service(service), 26.0, accent())
        }))
        .child(
            view()
                .flex_1()
                .child(
                    text(move || which.get().map(|w| w.1).unwrap_or_default())
                        .text_size(px(18.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .selectable(true),
                )
                .child(
                    text(move || {
                        holder
                            .get()
                            .map(|(s, r)| {
                                if r.id == r.name {
                                    service_title(s)
                                } else {
                                    format!("{} · {}", service_title(s), r.id)
                                }
                            })
                            .unwrap_or_default()
                    })
                    .text_size(small())
                    .text_color(secondary_text())
                    .selectable(true),
                ),
        )
        .child(push_button(
            || ts("Copy Name", "复制名称"),
            move || {
                if let Some((_, r)) = holder.get_untracked() {
                    ui.copy(r.id, "Copied", "已复制");
                }
            },
        ))
        .child(push_button(
            || ts("Refresh", "刷新"),
            move || {
                if let Some((service, resource)) = holder.get_untracked() {
                    let fetch = engine().contents(service, resource);
                    spawn(async move { contents.set(Some(fetch.await)) });
                }
            },
        ));

    let fields = each(
        move || contents.with(|c| c.as_ref().map(|c| c.fields.clone()).unwrap_or_default()),
        |f: &(String, String)| f.0.clone(),
        move |field, _| {
            let (label, _) = field.get_untracked();
            view()
                .flex_row()
                .gap(px(10.0))
                .py(px(2.0))
                .child(
                    text(label)
                        .w(px(150.0))
                        .text_size(small())
                        .text_color(secondary_text()),
                )
                .child(
                    text(move || field.get().1)
                        .flex_1()
                        .text_size(small())
                        .font_family(MONO)
                        .selectable(true),
                )
        },
    );
    let value = when(
        move || contents.with(|c| c.as_ref().is_some_and(|c| c.text.is_some())),
        move || {
            view()
                .gap(px(6.0))
                .child(section(|| ts("Value", "值")))
                .child(
                    text(move || {
                        contents
                            .with(|c| c.as_ref().and_then(|c| c.text.clone()).unwrap_or_default())
                    })
                    .font_family(MONO)
                    .text_size(px(11.5))
                    .selectable(true)
                    .p(px(10.0))
                    .rounded(px(8.0))
                    .bg(fill_bg()),
                )
        },
    );
    let rows_table = when(
        move || contents.with(|c| c.as_ref().is_some_and(|c| !c.columns.is_empty())),
        move || {
            let n = move || contents.with(|c| c.as_ref().map_or(0, |c| c.rows.len()));
            view()
                .gap(px(6.0))
                .child(
                    view()
                        .flex_row()
                        .child(section(move || match which.get().map(|w| w.0) {
                            Some("s3") => ts("Objects", "对象"),
                            Some("dynamodb") => ts("Items", "表项"),
                            Some("logs") => ts("Log events", "日志事件"),
                            Some("sns") => ts("Subscriptions", "订阅"),
                            Some("states") => ts("Executions", "执行"),
                            Some("events") => ts("Targets", "目标"),
                            Some("iam") => ts("Attached policies", "附加的策略"),
                            _ => ts("Contents", "内容"),
                        }))
                        .child(view().flex_1())
                        .child(
                            text(move || {
                                let more =
                                    contents.with(|c| c.as_ref().is_some_and(|c| c.truncated));
                                format!("{}{}", n(), if more { "+" } else { "" })
                            })
                            .text_size(small())
                            .text_color(tertiary_text()),
                        ),
                )
                .child(table_of(
                    move || {
                        contents.with(|c| {
                            c.as_ref()
                                .map(|c| {
                                    c.columns
                                        .iter()
                                        .enumerate()
                                        .map(|(i, name)| {
                                            let width = if name == "Message" {
                                                520.0
                                            } else if i == 0 {
                                                240.0
                                            } else {
                                                160.0
                                            };
                                            col(&format!("c{i}"), name.clone(), width)
                                        })
                                        .collect()
                                })
                                .unwrap_or_default()
                        })
                    },
                    move || {
                        contents.with(|c| {
                            let Some(c) = c else {
                                return DataTableRows::new(0);
                            };
                            let mut rows = DataTableRows::new(c.rows.len());
                            for i in 0..c.columns.len() {
                                rows = rows.texts(
                                    &format!("c{i}"),
                                    c.rows.iter().map(|r| r.get(i).cloned().unwrap_or_default()),
                                );
                            }
                            rows
                        })
                    },
                    move |row| {
                        // An S3 object opens its text.
                        let Some((service, r)) = holder.get_untracked() else {
                            return;
                        };
                        if service != "s3" {
                            return;
                        }
                        let Some(key) = contents.with_untracked(|c| {
                            c.as_ref()
                                .and_then(|c| c.rows.get(row).and_then(|r| r.first().cloned()))
                        }) else {
                            return;
                        };
                        let fetch = engine().object_text(r.id.clone(), key.clone());
                        spawn(async move {
                            let text = fetch
                                .await
                                .unwrap_or_else(|| ts("(not text)", "（不是文本）"));
                            preview.set(Some((key, text)));
                        });
                    },
                    n,
                ))
        },
    );
    let object = when(
        move || preview.with(|p| p.is_some()),
        move || {
            view()
                .gap(px(6.0))
                .child(section(move || {
                    preview.with(|p| p.as_ref().map(|p| p.0.clone()).unwrap_or_default())
                }))
                .child(
                    text(move || {
                        preview.with(|p| p.as_ref().map(|p| p.1.clone()).unwrap_or_default())
                    })
                    .font_family(MONO)
                    .text_size(px(11.5))
                    .selectable(true)
                    .p(px(10.0))
                    .rounded(px(8.0))
                    .bg(fill_bg()),
                )
        },
    );
    let calls_list = each(
        move || touched.get(),
        |c: &(usize, String, String, String, bool)| c.0,
        move |row, _| {
            let (seq, time, operation, status, failed) = row.get_untracked();
            nsh_base::button()
                .on_press(move || ui.show_call(seq))
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .px(px(8.0))
                .h(px(24.0))
                .rounded(radius())
                .hover(|s| s.bg(hover_bg()))
                .child(
                    text(time)
                        .font_family(MONO)
                        .text_size(small())
                        .text_color(secondary_text()),
                )
                .child(text(operation).flex_1().text_size(body()))
                .child(text(status).text_size(small()).text_color(if failed {
                    red()
                } else {
                    secondary_text()
                }))
        },
    );
    scroll()
        .flex_1()
        .child(
            view()
                .p(px(24.0))
                .gap(px(18.0))
                .child(header)
                .child(when(
                    move || contents.with(|c| c.is_none()),
                    move || {
                        text(move || ts("Loading…", "加载中…"))
                            .text_size(body())
                            .text_color(tertiary_text())
                    },
                ))
                .child(view().gap(px(2.0)).child(fields))
                .child(value)
                .child(rows_table)
                .child(object)
                .child(
                    view()
                        .gap(px(6.0))
                        .child(section(|| ts("Calls that touched it", "访问过它的调用")))
                        .child(calls_list)
                        .child(when(
                            move || touched.with(|t| t.is_empty()),
                            move || {
                                text(move || ts("None in this list", "列表里没有"))
                                    .text_size(body())
                                    .text_color(tertiary_text())
                            },
                        )),
                ),
        )
        .into_element()
}

#[allow(dead_code)]
fn unused(_: Resource) -> &'static str {
    t("", "")
}
