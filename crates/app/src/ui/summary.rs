//! What a run did, above its calls: how it ended, how many calls it made and
//! how many AWS made because of them, the first thing that failed and why,
//! what it changed, and the IAM permissions it needed.

use std::sync::Arc;

use nsh_core::prelude::*;

use super::{Filter, Page, Ui, calls, sidebar::service_title, style::*, t, ts};
use crate::{
    resources::clock,
    server::changes_resources,
    session::{Call, Run},
};

#[derive(Clone, Debug, Default, PartialEq)]
struct Summary {
    run: Option<Run>,
    calls: usize,
    internal: usize,
    failed: usize,
    unsupported: usize,
    p95: u64,
    changed: Vec<String>,
    more_changed: usize,
    permissions: usize,
    first_failure: Option<(usize, String, String)>,
}

impl Summary {
    fn of(calls: &[Arc<Call>], run: Option<Run>) -> Summary {
        let id = run.as_ref().map(|r| r.id);
        let scope: Vec<&Arc<Call>> = calls
            .iter()
            .filter(|c| id.is_none() || c.run == id)
            .collect();
        let mut ms: Vec<u64> = scope.iter().map(|c| c.ms).collect();
        ms.sort_unstable();
        let p95 = ms
            .get(((ms.len() as f64 - 1.0) * 0.95).round().max(0.0) as usize)
            .copied()
            .unwrap_or(0);
        let mut changed: Vec<String> = Vec::new();
        for c in &scope {
            if changes_resources(c) && !c.unsupported {
                let what = c
                    .target
                    .clone()
                    .unwrap_or_else(|| format!("{} {}", service_title(&c.service), c.operation));
                if !changed.contains(&what) {
                    changed.push(what);
                }
            }
        }
        let more_changed = changed.len().saturating_sub(6);
        changed.truncate(6);
        let mut accesses: Vec<_> = scope
            .iter()
            .flat_map(|c| c.access.iter().cloned())
            .collect();
        accesses.sort();
        accesses.dedup();
        let first_failure = scope
            .iter()
            .find(|c| c.failed() && !c.unsupported)
            .map(|c| {
                let code = c
                    .failure
                    .as_ref()
                    .map(|f| f.code.clone())
                    .unwrap_or_else(|| c.status.to_string());
                let message = c
                    .failure
                    .as_ref()
                    .and_then(|f| f.message.clone())
                    .unwrap_or_default();
                (
                    c.seq,
                    format!("{} {} · {code}", service_title(&c.service), c.operation),
                    message,
                )
            });
        Summary {
            run,
            calls: scope.len(),
            internal: scope.iter().filter(|c| c.internal).count(),
            failed: scope
                .iter()
                .filter(|c| c.failed() && !c.unsupported)
                .count(),
            unsupported: scope.iter().filter(|c| c.unsupported).count(),
            p95,
            changed,
            more_changed,
            permissions: accesses.len(),
            first_failure,
        }
    }
}

pub fn summary(ui: Ui) -> impl IntoElement {
    let summary = computed(move || {
        let run = match ui.page.get() {
            Page::Run(id) => ui
                .snap
                .with(|s| s.runs.iter().find(|r| r.id == id).cloned()),
            _ => None,
        };
        ui.snap.with(|s| Summary::of(&s.calls, run))
    });
    let status = move || {
        summary.with(|s| match &s.run {
            None => (
                ts(
                    "Everything since Lenslet started",
                    "Lenslet 启动以来的全部调用",
                ),
                secondary_text(),
            ),
            Some(r) => {
                let when = clock(r.started_ms);
                match (r.exit, r.ended_ms) {
                    (Some(code), Some(end)) => {
                        let took = end.saturating_sub(r.started_ms);
                        let took = if took < 1000 {
                            format!("{took} ms")
                        } else {
                            format!("{:.1} s", took as f64 / 1000.0)
                        };
                        if code == 0 && s.failed == 0 {
                            (
                                format!("{} · {when} · {took}", t("Succeeded", "成功")),
                                green(),
                            )
                        } else if code == 0 {
                            (
                                format!(
                                    "{} · {when} · {took}",
                                    t("Finished, but calls failed", "已结束，但有调用失败")
                                ),
                                red(),
                            )
                        } else {
                            (
                                format!("{} {code} · {when} · {took}", t("Exited with", "退出码"),),
                                red(),
                            )
                        }
                    }
                    _ => (format!("{} · {when}", t("Running…", "运行中…")), accent()),
                }
            }
        })
    };
    let chip = move |label: Box<dyn Fn(&Summary) -> String>,
                     color: Box<dyn Fn(&Summary) -> AdaptiveColor>,
                     on_press: Box<dyn Fn()>| {
        let shown = {
            let label = std::rc::Rc::new(label);
            let l2 = label.clone();
            (move || summary.with(|s| !l2(s).is_empty()), label)
        };
        let (visible, label) = shown;
        let color = std::rc::Rc::new(color);
        let on_press = std::rc::Rc::new(on_press);
        when(visible, move || {
            let label = label.clone();
            let color = color.clone();
            let on_press = on_press.clone();
            text(move || summary.with(|s| label(s)))
                .text_size(small())
                .text_color(move || summary.with(|s| color(s)))
                .px(px(8.0))
                .py(px(2.0))
                .rounded(px(10.0))
                .bg(fill_bg())
                .on_press(move || on_press())
        })
    };
    let count = |n: usize, one: &'static str, many: &'static str, zh: &'static str| -> String {
        match n {
            0 => String::new(),
            1 if super::lang().get() == super::Lang::En => format!("1 {one}"),
            n if super::lang().get() == super::Lang::En => format!("{n} {many}"),
            n => format!("{n} {zh}"),
        }
    };
    let chips = view()
        .flex_row()
        .flex_wrap()
        .gap(px(6.0))
        .child(chip(
            Box::new(move |s| count(s.calls, "call", "calls", "次调用")),
            Box::new(|_| secondary_text()),
            Box::new(move || ui.filter.set(Filter::All)),
        ))
        .child(chip(
            Box::new(move |s| count(s.internal, "call by AWS", "calls by AWS", "次由 AWS 发起")),
            Box::new(|_| accent()),
            Box::new(|| {}),
        ))
        .child(chip(
            Box::new(move |s| count(s.failed, "failed", "failed", "次失败")),
            Box::new(|_| red()),
            Box::new(move || ui.filter.set(Filter::Failed)),
        ))
        .child(chip(
            Box::new(move |s| count(s.unsupported, "not emulated", "not emulated", "次未模拟")),
            Box::new(|_| orange()),
            Box::new(move || ui.filter.set(Filter::NotEmulated)),
        ))
        .child(chip(
            Box::new(|s| {
                if s.calls > 0 {
                    format!("p95 {} ms", s.p95)
                } else {
                    String::new()
                }
            }),
            Box::new(|_| secondary_text()),
            Box::new(|| {}),
        ));
    let failure = when(
        move || summary.with(|s| s.first_failure.is_some()),
        move || {
            view()
                .gap(px(2.0))
                .px(px(10.0))
                .py(px(7.0))
                .rounded(px(8.0))
                .bg(red().opacity(0.09))
                .on_press(move || {
                    if let Some(seq) =
                        summary.with_untracked(|s| s.first_failure.as_ref().map(|f| f.0))
                    {
                        ui.show_call(seq);
                    }
                })
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(icon_colored(Icon::Warning, 14.0, red()))
                        .child(
                            text(move || {
                                summary.with(|s| {
                                    s.first_failure
                                        .as_ref()
                                        .map(|f| f.1.clone())
                                        .unwrap_or_default()
                                })
                            })
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(red()),
                        ),
                )
                .child(
                    text(move || {
                        summary.with(|s| {
                            s.first_failure
                                .as_ref()
                                .map(|f| f.2.clone())
                                .unwrap_or_default()
                        })
                    })
                    .text_size(small())
                    .text_color(secondary_text()),
                )
        },
    );
    let changed = when(
        move || summary.with(|s| !s.changed.is_empty()),
        move || {
            view()
                .flex_row()
                .gap(px(8.0))
                .child(
                    text(move || ts("Changed", "改动"))
                        .text_size(small())
                        .text_color(secondary_text())
                        .w(px(72.0)),
                )
                .child(
                    text(move || {
                        summary.with(|s| {
                            let mut line = s.changed.join(",  ");
                            if s.more_changed > 0 {
                                line.push_str(&format!("  +{}", s.more_changed));
                            }
                            line
                        })
                    })
                    .flex_1()
                    .text_size(small())
                    .font_family(MONO),
                )
        },
    );
    let permissions = when(
        move || summary.with(|s| s.permissions > 0),
        move || {
            view()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(
                    text(move || ts("Needs", "需要"))
                        .text_size(small())
                        .text_color(secondary_text())
                        .w(px(72.0)),
                )
                .child(
                    text(move || {
                        let n = summary.with(|s| s.permissions);
                        if super::lang().get() == super::Lang::Zh {
                            format!("{n} 项 IAM 权限")
                        } else if n == 1 {
                            "1 IAM permission".into()
                        } else {
                            format!("{n} IAM permissions")
                        }
                    })
                    .text_size(small()),
                )
                .child(push_button(
                    || ts("Copy Policy", "复制策略"),
                    move || ui.copy(ui.policy(), "Copied the IAM policy", "已复制 IAM 策略"),
                ))
                .child(push_button(
                    || ts("Save…", "保存…"),
                    move || ui.save_policy(),
                ))
        },
    );
    let rewind = when(
        move || summary.with(|s| s.run.as_ref().is_some_and(|r| r.ended_ms.is_some())),
        move || {
            nsh_base::button()
                .on_press(move || {
                    if let Some(id) = summary.with_untracked(|s| s.run.as_ref().map(|r| r.id)) {
                        ui.rewind(id);
                    }
                })
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .px(px(8.0))
                .h(px(22.0))
                .rounded(radius())
                .hover(|s| s.bg(hover_bg()))
                .child(icon(Icon::NewAccount, 13.0))
                .child(text(move || ts("Rewind to Before", "回到运行之前")).text_size(small()))
        },
    );
    view()
        .px(px(16.0))
        .pt(px(12.0))
        .pb(px(10.0))
        .gap(px(8.0))
        .child(
            view()
                .flex_row()
                .items_center()
                .child(
                    text(move || status().0)
                        .flex_1()
                        .text_size(small())
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(move || status().1),
                )
                .child(rewind),
        )
        .child(chips)
        .child(failure)
        .child(changed)
        .child(permissions)
}

#[allow(dead_code)]
fn unused(row: &calls::Row) -> usize {
    row.depth as usize
}
