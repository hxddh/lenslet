//! The content pages besides the calls: the welcome (how to start, before
//! anything happened) and one resource (what it holds, and the calls that
//! touched it).

use nsh_base::{DataTableColumn, DataTablePress, DataTableRows};
use nsh_core::prelude::*;
use std::sync::Arc;

use super::{Page, Ui, calls, engine, sidebar::service_title, style::*, t, ts};
use crate::{
    resources::{Contents, clock},
    session::Resource,
};

// -- Welcome ---------------------------------------------------------------------

/// Before anything happened: what Lenslet is and how to start, in one step.
pub fn welcome(ui: Ui) -> AnyElement {
    scroll().flex_1().child(welcome_steps(ui)).into_element()
}

fn welcome_steps(ui: Ui) -> impl IntoElement {
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
    // Fetch what it holds when it is opened, and again when what exists
    // changed (not on every call). A fetch for another resource than the
    // one now open is dropped.
    let fetch = move || {
        let Some((service, resource)) = holder.get_untracked() else {
            return;
        };
        let asked = which.get_untracked();
        let fetch = engine().contents(service, resource);
        ui.spawn(async move {
            let contents_of = fetch.await;
            if which.get_untracked() == asked {
                contents.set(Some(contents_of));
            }
        });
    };
    let inventory = computed(move || ui.snap.with(|s| Arc::as_ptr(&s.resources) as usize));
    effect(move || {
        let _ = which.get();
        contents.set(None);
        preview.set(None);
    });
    effect(move || {
        let _ = (which.get(), inventory.get());
        fetch();
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
                fetch();
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
