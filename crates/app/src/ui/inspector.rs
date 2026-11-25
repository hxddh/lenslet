//! The inspector: everything about the selected call. A summary (what went
//! wrong and why, what it acted on, who sent it, how long it took, the forms
//! to replay it), the request and the response (headers and body), and the
//! IAM permissions it needed.

use std::sync::Arc;

use nsh_core::prelude::*;

use super::{CopyAs, Lang, Tab, Ui, calls, lang, sidebar::service_title, style::*, t, ts};
use crate::{resources::clock, session::Call};

pub fn inspector(ui: Ui) -> impl IntoElement {
    let has_call = move || ui.selected.get().is_some();
    view()
        .flex_1()
        .bg(content_bg())
        .border_l_1()
        .border_color(separator())
        .child(when(move || !has_call(), move || nothing_selected(ui)))
        .child(when(has_call, move || selected(ui)))
}

fn nothing_selected(ui: Ui) -> impl IntoElement {
    view()
        .flex_1()
        .items_center()
        .justify_center()
        .p(px(24.0))
        .child(
            text(move || {
                if ui.snap.with(|s| s.calls.is_empty()) {
                    ts(
                        "Calls appear here as your code makes them.",
                        "代码发出调用后，会显示在这里。",
                    )
                } else {
                    ts(
                        "Select a call to see everything about it.",
                        "选中一次调用，查看它的全部信息。",
                    )
                }
            })
            .text_size(body())
            .text_color(tertiary_text()),
        )
}

fn selected(ui: Ui) -> impl IntoElement {
    let call = computed(move || ui.call().map(CallRef));
    let header = view()
        .px(px(14.0))
        .pt(px(12.0))
        .pb(px(8.0))
        .gap(px(3.0))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(
                    text(move || {
                        call.with(|c| {
                            c.as_ref()
                                .map(|c| c.0.operation.clone())
                                .unwrap_or_default()
                        })
                    })
                    .text_size(px(15.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .selectable(true),
                )
                .child(status_pill(call)),
        )
        .child(
            text(move || {
                call.with(|c| {
                    c.as_ref()
                        .map(|c| {
                            format!(
                                "{} · {} · {} ms",
                                service_title(&c.0.service),
                                clock(c.0.at_ms),
                                c.0.ms
                            )
                        })
                        .unwrap_or_default()
                })
            })
            .text_size(small())
            .text_color(secondary_text()),
        );
    let tab = move |which: Tab, label: fn() -> String| {
        let on = move || ui.tab.get() == which;
        nsh_base::tab()
            .selected(on)
            .on_press(move || ui.tab.set(which))
            .px(px(10.0))
            .h(px(24.0))
            .items_center()
            .justify_center()
            .rounded(radius())
            .bg(move || {
                if on() {
                    fill_bg()
                } else {
                    AdaptiveColor::rgb(0, 0).opacity(0.0)
                }
            })
            .hover(|s| s.bg(hover_bg()))
            .child(
                text(label)
                    .text_size(small())
                    .text_color(move || if on() { label_text() } else { secondary_text() }),
            )
    };
    let tabs = nsh_base::tabs()
        .flex_row()
        .gap(px(2.0))
        .px(px(10.0))
        .pb(px(8.0))
        .child(tab(Tab::Summary, || ts("Summary", "概览")))
        .child(tab(Tab::Request, || ts("Request", "请求")))
        .child(tab(Tab::Response, || ts("Response", "响应")))
        .child(tab(Tab::Access, || ts("Permissions", "权限")));
    let tab_kind = computed(move || ui.tab.get());
    let body = scroll().flex_1().child(dynamic(move || {
        let which = tab_kind.get();
        view()
            .px(px(14.0))
            .pb(px(16.0))
            .gap(px(14.0))
            .child(match which {
                Tab::Summary => summary(ui, call).into_element(),
                Tab::Request => exchange(call, true).into_element(),
                Tab::Response => exchange(call, false).into_element(),
                Tab::Access => access(ui, call).into_element(),
            })
    }));
    view()
        .flex_1()
        .child(header)
        .child(tabs)
        .child(hairline())
        .child(body)
}

/// A call held for the views, compared by identity (bodies are large).
#[derive(Clone)]
pub struct CallRef(pub Arc<Call>);

impl PartialEq for CallRef {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

type Held = Memo<Option<CallRef>>;

fn status_pill(call: Held) -> impl IntoElement {
    let color = move || {
        call.with(|c| match c {
            Some(c) if c.0.unsupported => orange(),
            Some(c) if c.0.failed() => red(),
            _ => green(),
        })
    };
    text(move || {
        call.with(|c| {
            c.as_ref()
                .map(|c| calls::status_text(&c.0))
                .unwrap_or_default()
        })
    })
    .text_size(small())
    .font_weight(FontWeight::SEMIBOLD)
    .px(px(6.0))
    .py(px(1.0))
    .rounded(px(4.0))
    .text_color(color)
    .border_1()
    .border_color(color)
}

fn summary(ui: Ui, call: Held) -> impl IntoElement {
    let field = |label: fn() -> String, value: Box<dyn Fn(&Call) -> Option<String>>| {
        let value = std::rc::Rc::new(value);
        let shown = {
            let value = value.clone();
            move || call.with(|c| c.as_ref().and_then(|c| value(&c.0)).is_some())
        };
        when(shown, move || {
            let value = value.clone();
            view()
                .flex_row()
                .gap(px(10.0))
                .py(px(2.0))
                .child(
                    text(label)
                        .w(px(96.0))
                        .text_size(small())
                        .text_color(secondary_text()),
                )
                .child(
                    text(move || {
                        call.with(|c| c.as_ref().and_then(|c| value(&c.0)).unwrap_or_default())
                    })
                    .flex_1()
                    .text_size(small())
                    .font_family(MONO)
                    .selectable(true),
                )
        })
    };
    let failure = when(
        move || {
            call.with(|c| {
                c.as_ref()
                    .is_some_and(|c| c.0.failure.is_some() || c.0.unsupported)
            })
        },
        move || failure_box(call),
    );
    let facts = view()
        .gap(px(2.0))
        .child(super::style::section(|| ts("Details", "详细信息")))
        .child(field(
            || ts("Resource", "资源"),
            Box::new(|c| c.target.clone()),
        ))
        .child(field(
            || ts("Status", "状态"),
            Box::new(|c| Some(c.status.to_string())),
        ))
        .child(field(
            || ts("Duration", "耗时"),
            Box::new(|c| Some(format!("{} ms", c.ms))),
        ))
        .child(field(
            || ts("Client", "客户端"),
            Box::new(|c| c.client.clone()),
        ))
        .child(field(
            || ts("Attempt", "第几次尝试"),
            Box::new(|c| {
                c.attempt.map(|a| match c.max_attempts {
                    Some(m) => format!("{a} / {m}"),
                    None => a.to_string(),
                })
            }),
        ))
        .child(field(
            || ts("Region", "区域"),
            Box::new(|c| c.region.clone()),
        ))
        .child(field(
            || ts("Request ID", "请求 ID"),
            Box::new(|c| c.request_id.clone()),
        ))
        .child(field(
            || ts("Request", "请求"),
            Box::new(|c| Some(format!("{} {}", c.method, c.path))),
        ))
        .child(field(
            || ts("Sizes", "大小"),
            Box::new(|c| {
                Some(format!(
                    "{} → {}",
                    crate::resources::bytes_label(c.request.bytes as u64),
                    crate::resources::bytes_label(c.response.bytes as u64)
                ))
            }),
        ));
    let replay = view()
        .gap(px(6.0))
        .child(super::style::section(|| ts("Replay", "重放")))
        .child(
            view()
                .flex_row()
                .gap(px(6.0))
                .flex_wrap()
                .child(when(
                    move || {
                        call.with(|c| {
                            c.as_ref()
                                .is_some_and(|c| crate::copy_as::aws_cli(&c.0).is_some())
                        })
                    },
                    move || {
                        push_button(
                            || ts("Copy as AWS CLI", "复制为 AWS CLI"),
                            move || ui.copy_call_as(CopyAs::Cli),
                        )
                    },
                ))
                .child(push_button(
                    || ts("Copy as curl", "复制为 curl"),
                    move || ui.copy_call_as(CopyAs::Curl),
                ))
                .child(when(
                    move || {
                        call.with(|c| {
                            c.as_ref()
                                .is_some_and(|c| crate::copy_as::boto3(&c.0).is_some())
                        })
                    },
                    move || {
                        push_button(
                            || ts("Copy as boto3", "复制为 boto3"),
                            move || ui.copy_call_as(CopyAs::Boto3),
                        )
                    },
                )),
        );
    let show_resource = when(
        move || {
            call.with(|c| {
                c.as_ref()
                    .is_some_and(|c| calls::resource_page(&c.0).is_some())
            })
        },
        move || {
            view().flex_row().child(push_button(
                || ts("Show Resource", "查看资源"),
                move || {
                    if let Some(page) =
                        call.with_untracked(|c| c.as_ref().and_then(|c| calls::resource_page(&c.0)))
                    {
                        ui.page.set(page);
                    }
                },
            ))
        },
    );
    // Where the call came from: the call it happened inside, and the command.
    let cause = move || {
        let parent = call.with(|c| c.as_ref().and_then(|c| c.0.parent))?;
        ui.snap
            .with(|s| s.calls.iter().rev().find(|c| c.id == parent).cloned())
    };
    let origin = view()
        .gap(px(4.0))
        .child(when(
            move || call.with(|c| c.as_ref().is_some_and(|c| c.0.internal)),
            move || {
                text(move || {
                    ts(
                        "AWS made this call, not your code: a delivery, a trigger, or a function's own call.",
                        "这次调用由 AWS 发起，不是你的代码：一次投递、一次触发，或函数自己的调用。",
                    )
                })
                .text_size(small())
                .text_color(accent())
            },
        ))
        .child(when(
            move || cause().is_some(),
            move || {
                view()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(text(move || ts("Caused by", "由此引起")).w(px(96.0)).text_size(small()).text_color(secondary_text()))
                    .child(
                        nsh_base::button()
                            .on_press(move || {
                                if let Some(parent) = cause() {
                                    ui.show_call(parent.seq);
                                }
                            })
                            .rounded(radius())
                            .hover(|s| s.bg(hover_bg()))
                            .child(
                                text(move || {
                                    cause()
                                        .map(|p| format!("{} {} ↗", service_title(&p.service), p.operation))
                                        .unwrap_or_default()
                                })
                                .text_size(small())
                                .text_color(accent()),
                            ),
                    )
            },
        ))
        .child(when(
            move || call.with(|c| c.as_ref().is_some_and(|c| c.0.run.is_some())),
            move || {
                view()
                    .flex_row()
                    .gap(px(8.0))
                    .child(text(move || ts("Run", "所属运行")).w(px(96.0)).text_size(small()).text_color(secondary_text()))
                    .child(
                        text(move || {
                            let run = call.with(|c| c.as_ref().and_then(|c| c.0.run));
                            ui.snap.with(|s| s.runs.iter().find(|r| Some(r.id) == run).map(|r| r.command.clone()).unwrap_or_default())
                        })
                        .flex_1()
                        .text_size(small())
                        .font_family(MONO),
                    )
            },
        ));
    // A call denied to a role (a function's): allow the role what it needed.
    let denied_role = move || {
        call.with(|c| {
            let c = c.as_ref()?;
            let f = c.0.failure.as_ref()?;
            if !f.code.contains("AccessDenied") || c.0.access.is_empty() {
                return None;
            }
            crate::resources::denied_role(f.message.as_deref()?)
        })
    };
    let grant = when(
        move || denied_role().is_some(),
        move || {
            view().flex_row().child(push_button(
                move || {
                    format!(
                        "{} {}",
                        t("Allow for role", "允许角色"),
                        denied_role().unwrap_or_default()
                    )
                },
                move || {
                    let Some(role) = denied_role() else { return };
                    let accesses = call.with_untracked(|c| {
                        c.as_ref().map(|c| c.0.access.clone()).unwrap_or_default()
                    });
                    let done = super::engine().grant(role.clone(), accesses);
                    ui.spawn(async move {
                        match done.await {
                            Ok(()) => ui.say(format!("{} {role}", t("Allowed for", "已授权给"))),
                            Err(e) => ui.say(e),
                        }
                    });
                },
            ))
        },
    );
    view()
        .gap(px(16.0))
        .pt(px(12.0))
        .child(failure)
        .child(grant)
        .child(origin)
        .child(facts)
        .child(replay)
        .child(show_resource)
}

fn failure_box(call: Held) -> impl IntoElement {
    let unsupported = move || call.with(|c| c.as_ref().is_some_and(|c| c.0.unsupported));
    let color = move || if unsupported() { orange() } else { red() };
    view()
        .gap(px(4.0))
        .p(px(10.0))
        .rounded(px(8.0))
        .bg(move || color().opacity(0.1))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .child(dynamic(move || icon_colored(Icon::Warning, 15.0, if unsupported() { orange() } else { red() })))
                .child(
                    text(move || {
                        if unsupported() {
                            ts("Not emulated", "模拟器不支持")
                        } else {
                            call.with(|c| c.as_ref().and_then(|c| c.0.failure.as_ref().map(|f| f.code.clone())).unwrap_or_default())
                        }
                    })
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(color)
                    .selectable(true),
                ),
        )
        .child(
            text(move || {
                if unsupported() {
                    ts(
                        "AWS Emulator does not serve this service or operation yet, or Lenslet does not include it. Your code may be right.",
                        "AWS Emulator 还不支持这个服务或操作，或者 Lenslet 没有内置它。你的代码可能没有问题。",
                    )
                } else {
                    call.with(|c| c.as_ref().and_then(|c| c.0.failure.as_ref().and_then(|f| f.message.clone())).unwrap_or_default())
                }
            })
            .text_size(body())
            .selectable(true),
        )
        .child(when(
            move || call.with(|c| c.as_ref().is_some_and(|c| c.0.failure.as_ref().is_some_and(|f| f.hint.is_some()))),
            move || {
                text(move || call.with(|c| c.as_ref().and_then(|c| c.0.failure.as_ref().and_then(|f| f.hint.clone())).unwrap_or_default()))
                    .text_size(small())
                    .text_color(secondary_text())
            },
        ))
        .child(known_gap(call))
}

/// Whether AWS Emulator is known to get this call wrong: then the failure
/// may be the emulator's, not your code's.
fn known_gap(call: Held) -> impl IntoElement {
    let known = computed(move || {
        call.with(|c| {
            let c = &c.as_ref()?.0;
            crate::gaps::known(
                &c.service,
                &c.operation,
                c.failure.as_ref().map(|f| f.code.as_str()),
            )
        })
    });
    when(
        move || known.with(|k| k.is_some()),
        move || {
            view()
                .gap(px(4.0))
                .mt(px(4.0))
                .pt(px(6.0))
                .border_t_1()
                .border_color(separator())
                .child(
                    text(move || {
                        known.with(|k| {
                            let Some(k) = k else { return String::new() };
                            let op = call.with(|c| {
                                c.as_ref()
                                    .map(|c| c.0.operation.clone())
                                    .unwrap_or_default()
                            });
                            let code = call.with(|c| {
                                c.as_ref()
                                    .and_then(|c| c.0.failure.as_ref().map(|f| f.code.clone()))
                                    .unwrap_or_default()
                            });
                            // Only an operation the emulator does not do yet is
                            // surely its fault; otherwise this is what it is known
                            // to get wrong there, for you to weigh.
                            let missing = k.exact && code == "NotImplemented";
                            match (missing, k.exact, lang().get()) {
                                (true, _, Lang::Zh) => format!("AWS Emulator 还没有实现这个操作：{}", k.title),
                                (true, _, Lang::En) => format!("AWS Emulator does not do this yet: {}", k.title),
                                (false, true, Lang::Zh) => format!(
                                    "AWS Emulator 列出了 {op} 返回 {code} 时的已知差异：{}",
                                    k.title
                                ),
                                (false, true, Lang::En) => format!(
                                    "AWS Emulator lists a known difference from AWS for {op} answering {code}: {}",
                                    k.title
                                ),
                                (false, false, Lang::Zh) => format!(
                                    "AWS Emulator 列出了 {op} 的 {} 个已知差异，例如：{}",
                                    k.count, k.title
                                ),
                                (false, false, Lang::En) => format!(
                                    "AWS Emulator lists {} known differences from AWS in {op}, such as: {}",
                                    k.count, k.title
                                ),
                            }
                        })
                    })
                    .text_size(small())
                    .selectable(true),
                )
                .child(
                    nsh_base::button()
                        .on_press(move || {
                            if let Some(k) = known.get_untracked() {
                                drop(open_url(&k.url));
                            }
                        })
                        .child(
                            text(move || ts("Open the issue", "查看这个问题"))
                                .text_size(small())
                                .text_color(accent()),
                        ),
                )
        },
    )
}

fn exchange(call: Held, request: bool) -> impl IntoElement {
    let part = move |c: &Call| {
        if request {
            c.request.clone()
        } else {
            c.response.clone()
        }
    };
    let headers = move || {
        call.with(|c| {
            c.as_ref()
                .map(|c| {
                    part(&c.0)
                        .headers
                        .iter()
                        .map(|(n, v)| format!("{n}: {v}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default()
        })
    };
    let body = move || {
        call.with(|c| {
            c.as_ref()
                .map(|c| {
                    let e = part(&c.0);
                    if e.body.is_empty() {
                        ts("(no body)", "（无正文）")
                    } else {
                        pretty(&e.body)
                    }
                })
                .unwrap_or_default()
        })
    };
    let code = |content: Box<dyn Fn() -> String>| {
        text(content)
            .font_family(MONO)
            .text_size(px(11.5))
            .selectable(true)
            .p(px(8.0))
            .rounded(px(6.0))
            .bg(fill_bg())
    };
    view()
        .gap(px(6.0))
        .pt(px(12.0))
        .child(super::style::section(|| ts("Body", "正文")))
        .child(code(Box::new(body)))
        .child(view().h(px(8.0)))
        .child(super::style::section(|| ts("Headers", "请求头 / 响应头")))
        .child(code(Box::new(headers)))
}

fn access(ui: Ui, call: Held) -> impl IntoElement {
    let lines = move || {
        call.with(|c| {
            c.as_ref()
                .map(|c| {
                    c.0.access
                        .iter()
                        .map(|a| format!("{}\n  {}", a.action, a.resource))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    };
    view()
        .gap(px(8.0))
        .pt(px(12.0))
        .child(super::style::section(|| {
            ts("This call needed", "这次调用需要")
        }))
        .child(
            text(move || {
                let lines = lines();
                if lines.is_empty() {
                    ts("No IAM permission", "不需要 IAM 权限")
                } else {
                    lines.join("\n")
                }
            })
            .font_family(MONO)
            .text_size(px(11.5))
            .selectable(true),
        )
        .child(when(
            move || !lines().is_empty(),
            move || {
                push_button(
                    || ts("Copy as Policy", "复制为策略"),
                    move || {
                        let accesses = call.with_untracked(|c| {
                            c.as_ref().map(|c| c.0.access.clone()).unwrap_or_default()
                        });
                        ui.copy(
                            lenslet_core::policy::compile(&accesses),
                            "Copied the policy",
                            "已复制策略",
                        );
                    },
                )
            },
        ))
}

/// JSON pretty-printed; anything else as it came.
pub fn pretty(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| body.to_string())
}

#[allow(dead_code)]
fn unused(_: &str) -> &'static str {
    t("", "")
}
