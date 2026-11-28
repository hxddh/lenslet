//! What a run did, above its calls: how it ended, how many calls it made and
//! how many AWS made because of them, the first thing that failed and why,
//! what it changed, and the IAM permissions it needed. And how it spent its
//! time: in AWS and out of it, the slowest calls, each service, the retries
//! the SDKs made, what its functions spent, and how long AWS took to act on
//! what the run did.

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
    /// What changed since the last run of the same command.
    compared: Option<Compared>,
    functions: Functions,
    retries: Retries,
    timing: Timing,
}

/// What the functions a run invoked spent, from their runs' `REPORT`s.
#[derive(Clone, Debug, Default, PartialEq)]
struct Functions {
    runs: usize,
    /// Runs that started a new instance.
    cold: usize,
    /// The slowest run: its call, and how long it took.
    slowest: Option<(usize, f64)>,
    /// The most memory a run used, and what it could use.
    memory: Option<(u64, u64)>,
    /// The first run that came near its timeout or its memory, and which.
    near: Option<(usize, bool)>,
}

/// The retries the SDKs made.
#[derive(Clone, Debug, Default, PartialEq)]
struct Retries {
    /// Attempts after the first.
    attempts: usize,
    /// SDK calls that took more than one attempt, and how many of those
    /// came through in the end.
    calls: usize,
    rescued: usize,
    /// Answers that said slow down.
    throttled: usize,
}

/// How the run spent its time.
#[derive(Clone, Debug, Default, PartialEq)]
struct Timing {
    /// Time with a call of the run's own in flight.
    in_aws_ms: u64,
    /// The slowest calls: each one's `seq`, what it was, and how long.
    slowest: Vec<(usize, String, u64)>,
    /// Each service: calls, failed, p95.
    services: Vec<(String, usize, usize, u64)>,
    /// The longest AWS took to act on a call (a delivery, a trigger; not a
    /// function's own calls): the call it made, from what to what, and how
    /// long after.
    lag: Option<(usize, String, u64)>,
}

/// An answer that says the caller is going too fast.
pub fn throttled(call: &Call) -> bool {
    call.status == 429
        || call.failure.as_ref().is_some_and(|f| {
            matches!(
                f.code.as_str(),
                "Throttling"
                    | "ThrottlingException"
                    | "ThrottledException"
                    | "TooManyRequestsException"
                    | "ProvisionedThroughputExceededException"
                    | "RequestLimitExceeded"
                    | "RequestThrottled"
                    | "RequestThrottledException"
                    | "SlowDown"
                    | "LimitExceededException"
            )
        })
}

fn p95(mut ms: Vec<u64>) -> u64 {
    ms.sort_unstable();
    ms.get(((ms.len() as f64 - 1.0) * 0.95).round().max(0.0) as usize)
        .copied()
        .unwrap_or(0)
}

fn name(c: &Call) -> String {
    format!("{} {}", service_title(&c.service), c.operation)
}

fn functions(scope: &[&Arc<Call>]) -> Functions {
    let mut f = Functions::default();
    for c in scope {
        let Some(r) = &c.function else { continue };
        f.runs += 1;
        f.cold += r.init_ms.is_some() as usize;
        if f.slowest.is_none_or(|(_, ms)| r.duration_ms > ms) {
            f.slowest = Some((c.seq, r.duration_ms));
        }
        if r.max_memory_mb > 0 && f.memory.is_none_or(|(used, _)| r.max_memory_mb > used) {
            f.memory = Some((r.max_memory_mb, r.memory_mb));
        }
        if f.near.is_none() && (r.near_timeout() || r.near_memory()) {
            f.near = Some((c.seq, r.near_timeout()));
        }
    }
    f
}

fn retries(scope: &[&Arc<Call>]) -> Retries {
    // Each SDK call: its last attempt so far, and whether it failed.
    let mut last: std::collections::HashMap<&str, (u32, bool)> = Default::default();
    for c in scope {
        if let (Some(id), Some(attempt)) = (c.invocation.as_deref(), c.attempt) {
            let entry = last.entry(id).or_insert((attempt, c.failed()));
            if attempt >= entry.0 {
                *entry = (attempt, c.failed());
            }
        }
    }
    let retried: Vec<_> = last.values().filter(|(a, _)| *a > 1).collect();
    Retries {
        attempts: scope.iter().filter(|c| c.is_retry()).count(),
        calls: retried.len(),
        rescued: retried.iter().filter(|(_, failed)| !failed).count(),
        throttled: scope.iter().filter(|c| throttled(c)).count(),
    }
}

fn timing(scope: &[&Arc<Call>]) -> Timing {
    // The run's own calls in flight, as one stretch where they overlap.
    let mut spans: Vec<(u64, u64)> = scope
        .iter()
        .filter(|c| !c.internal)
        .map(|c| (c.at_ms, c.at_ms + c.ms))
        .collect();
    spans.sort_unstable();
    let mut in_aws_ms = 0;
    let mut open: Option<(u64, u64)> = None;
    for (start, end) in spans {
        match &mut open {
            Some((_, e)) if start <= *e => *e = (*e).max(end),
            _ => {
                if let Some((s, e)) = open {
                    in_aws_ms += e - s;
                }
                open = Some((start, end));
            }
        }
    }
    if let Some((s, e)) = open {
        in_aws_ms += e - s;
    }
    let mut slowest: Vec<&&Arc<Call>> = scope.iter().filter(|c| c.ms > 0).collect();
    slowest.sort_by_key(|c| std::cmp::Reverse(c.ms));
    let slowest = slowest
        .into_iter()
        .take(3)
        .map(|c| (c.seq, name(c), c.ms))
        .collect();
    let mut by_service: std::collections::BTreeMap<&str, (usize, usize, Vec<u64>)> =
        Default::default();
    for c in scope {
        let entry = by_service.entry(&c.service).or_default();
        entry.0 += 1;
        entry.1 += (c.failed() && !c.unsupported) as usize;
        entry.2.push(c.ms);
    }
    let mut services: Vec<_> = by_service
        .into_iter()
        .map(|(s, (calls, failed, ms))| (s.to_string(), calls, failed, p95(ms)))
        .collect();
    services.sort_by_key(|s| std::cmp::Reverse(s.1));
    let at: std::collections::HashMap<u64, &Arc<Call>> = scope
        .iter()
        .filter(|c| c.id != 0)
        .map(|c| (c.id, *c))
        .collect();
    let lag = scope
        .iter()
        .filter(|c| c.internal)
        .filter_map(|c| {
            let parent = at.get(&c.parent?)?;
            // A trigger or a delivery, not a function's own call from its code.
            if crate::functions::invoked(parent).is_some() {
                return None;
            }
            let after = c.at_ms.checked_sub(parent.at_ms)?;
            Some((c.seq, format!("{} → {}", name(parent), name(c)), after))
        })
        .max_by_key(|l| l.2);
    Timing {
        in_aws_ms,
        slowest,
        services,
        lag,
    }
}

/// The failures a run no longer has, and the ones it has anew, since the
/// last run of the same command: whether the fix worked.
#[derive(Clone, Debug, Default, PartialEq)]
struct Compared {
    fixed: Vec<String>,
    new: Vec<String>,
    /// How many failures both have.
    still: usize,
}

/// The failures of the run `id`, each once: what failed, and how.
fn failures(calls: &[Arc<Call>], id: u64) -> std::collections::BTreeSet<String> {
    calls
        .iter()
        .filter(|c| c.run == Some(id) && c.failed() && !c.unsupported)
        .map(|c| {
            let code = c
                .failure
                .as_ref()
                .map_or_else(|| c.status.to_string(), |f| f.code.clone());
            let target = c
                .target
                .as_deref()
                .map(|t| format!(" {t}"))
                .unwrap_or_default();
            format!(
                "{} {}{target} · {code}",
                service_title(&c.service),
                c.operation
            )
        })
        .collect()
}

/// `run` against the last run before it of the same command, both over.
fn compared(calls: &[Arc<Call>], runs: &[Run], run: &Run) -> Option<Compared> {
    run.ended_ms?;
    let before = runs
        .iter()
        .filter(|r| r.id < run.id && r.command == run.command && r.ended_ms.is_some())
        .last()?;
    // A run whose calls are not all kept would show failures as fixed.
    if run.cut || before.cut {
        return None;
    }
    let (then, now) = (failures(calls, before.id), failures(calls, run.id));
    Some(Compared {
        fixed: then.difference(&now).cloned().collect(),
        new: now.difference(&then).cloned().collect(),
        still: then.intersection(&now).count(),
    })
}

impl Summary {
    fn of(calls: &[Arc<Call>], run: Option<Run>, runs: &[Run]) -> Summary {
        let id = run.as_ref().map(|r| r.id);
        let scope: Vec<&Arc<Call>> = calls
            .iter()
            .filter(|c| id.is_none() || c.run == id)
            .collect();
        let p95 = p95(scope.iter().map(|c| c.ms).collect());
        // The first few, in order, and how many more: a set, not a scan of
        // the list, so thousands of objects stay cheap.
        let mut changed: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for c in &scope {
            if changes_resources(c) && !c.unsupported {
                let what = c
                    .target
                    .clone()
                    .unwrap_or_else(|| format!("{} {}", service_title(&c.service), c.operation));
                if seen.insert(what.clone()) && changed.len() < 6 {
                    changed.push(what);
                }
            }
        }
        let more_changed = seen.len() - changed.len();
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
            compared: run.as_ref().and_then(|r| compared(calls, runs, r)),
            functions: functions(&scope),
            retries: retries(&scope),
            timing: timing(&scope),
            run,
        }
    }
}

/// One line on what changed since the last run of the command; `None` when
/// neither run failed.
fn compared_line(c: &Compared) -> Option<String> {
    let names = |list: &[String]| {
        let mut shown: Vec<&str> = list.iter().take(2).map(String::as_str).collect();
        if list.len() > 2 {
            shown.push("…");
        }
        shown.join(", ")
    };
    if c.fixed.is_empty() && c.new.is_empty() {
        return (c.still > 0).then(|| {
            ts(
                "Since the last run of this command: the same failures.",
                "与上次运行这条命令相比：失败相同。",
            )
        });
    }
    let mut parts = Vec::new();
    if !c.fixed.is_empty() {
        parts.push(format!(
            "{} {} ({})",
            c.fixed.len(),
            t("fixed", "个已修好"),
            names(&c.fixed)
        ));
    }
    if !c.new.is_empty() {
        parts.push(format!(
            "{} {} ({})",
            c.new.len(),
            t("new", "个新增"),
            names(&c.new)
        ));
    }
    if c.still > 0 {
        parts.push(format!("{} {}", c.still, t("still failing", "个仍失败")));
    }
    Some(format!(
        "{} {}",
        t(
            "Since the last run of this command:",
            "与上次运行这条命令相比："
        ),
        parts.join(t(", ", "，"))
    ))
}

pub fn summary(ui: Ui) -> impl IntoElement {
    let summary = computed(move || {
        let run = match ui.page.get() {
            Page::Run(id) => ui
                .snap
                .with(|s| s.runs.iter().find(|r| r.id == id).cloned()),
            _ => None,
        };
        ui.snap.with(|s| Summary::of(&s.calls, run, &s.runs))
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
                        } else if code == -1 {
                            // The shell ended (exit, restart) before it said.
                            (
                                format!(
                                    "{} · {when} · {took}",
                                    t("Ended with the shell", "随 shell 结束")
                                ),
                                secondary_text(),
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
        ))
        .child(chip(
            Box::new(move |s| {
                let r = &s.retries;
                let mut line = count(r.attempts, "retry", "retries", "次重试");
                if r.calls > 0 && super::lang().get() == super::Lang::Zh {
                    line.push_str(&format!("（{} 次后成功）", r.rescued));
                } else if r.calls > 0 {
                    line.push_str(&format!(" ({} came through)", r.rescued));
                }
                if r.throttled > 0 {
                    if !line.is_empty() {
                        line.push_str(" · ");
                    }
                    line.push_str(&count(r.throttled, "throttled", "throttled", "次被限流"));
                }
                line
            }),
            Box::new(|_| orange()),
            Box::new(move || ui.filter.set(Filter::Retried)),
        ));
    // Whether the fix worked: what changed since this command last ran.
    let since = when(
        move || summary.with(|s| s.compared.as_ref().and_then(compared_line).is_some()),
        move || {
            text(move || {
                summary.with(|s| {
                    s.compared
                        .as_ref()
                        .and_then(compared_line)
                        .unwrap_or_default()
                })
            })
            .text_size(body())
            .text_color(secondary_text())
            .selectable(true)
        },
    );
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
    // A labelled line of the summary, as Changed and Needs are.
    let line = |label: fn() -> String, content: AnyElement| {
        view()
            .flex_row()
            .items_center()
            .flex_wrap()
            .gap(px(8.0))
            .child(
                text(label)
                    .text_size(small())
                    .text_color(secondary_text())
                    .w(px(72.0)),
            )
            .child(content)
    };
    // A part of a line that opens what it names.
    let link = move |label: Box<dyn Fn(&Summary) -> String>, on_press: Box<dyn Fn(&Summary)>| {
        nsh_base::button()
            .on_press(move || summary.with_untracked(|s| on_press(s)))
            .rounded(radius())
            .hover(|s| s.bg(hover_bg()))
            .child(
                text(move || summary.with(|s| label(s)))
                    .text_size(small())
                    .text_color(accent()),
            )
    };
    let took = |ms: u64| crate::functions::ms_label(ms as f64);
    // In AWS and out of it, the slowest calls, how long AWS took to act.
    let time = when(
        move || summary.with(|s| s.calls > 0),
        move || {
            let slow = |i: usize| {
                when(
                    move || summary.with(|s| s.timing.slowest.len() > i),
                    move || {
                        link(
                            Box::new(move |s| {
                                s.timing
                                    .slowest
                                    .get(i)
                                    .map(|(_, what, ms)| format!("{what} {}", took(*ms)))
                                    .unwrap_or_default()
                            }),
                            Box::new(move |s| {
                                if let Some((seq, ..)) = s.timing.slowest.get(i) {
                                    ui.show_call(*seq);
                                }
                            }),
                        )
                    },
                )
            };
            line(
                || ts("Time", "耗时"),
                view()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.0))
                    .flex_1()
                    .child(
                        text(move || {
                            summary.with(|s| {
                                let aws = took(s.timing.in_aws_ms);
                                match s
                                    .run
                                    .as_ref()
                                    .and_then(|r| Some(r.ended_ms?.saturating_sub(r.started_ms)))
                                {
                                    Some(all) if super::lang().get() == super::Lang::Zh => {
                                        format!("{} 中 {aws} 在 AWS 调用里 · 最慢：", took(all))
                                    }
                                    Some(all) => {
                                        format!("{aws} of {} in AWS calls · slowest:", took(all))
                                    }
                                    None => format!(
                                        "{aws} {}",
                                        t("in AWS calls · slowest:", "在 AWS 调用里 · 最慢：")
                                    ),
                                }
                            })
                        })
                        .text_size(small()),
                    )
                    .child(slow(0))
                    .child(slow(1))
                    .child(slow(2))
                    .child(when(
                        move || summary.with(|s| s.timing.lag.is_some()),
                        move || {
                            link(
                                Box::new(move |s| {
                                    s.timing
                                        .lag
                                        .as_ref()
                                        .map(|(_, what, ms)| match super::lang().get() {
                                            super::Lang::Zh => {
                                                format!(
                                                    "· AWS 最晚在 {} 后才处理（{what}）",
                                                    took(*ms)
                                                )
                                            }
                                            super::Lang::En => {
                                                format!(
                                                    "· AWS acted up to {} later ({what})",
                                                    took(*ms)
                                                )
                                            }
                                        })
                                        .unwrap_or_default()
                                }),
                                Box::new(move |s| {
                                    if let Some((seq, ..)) = s.timing.lag {
                                        ui.show_call(seq);
                                    }
                                }),
                            )
                        },
                    ))
                    .into_element(),
            )
        },
    );
    // Each service the run called: click one to list its calls.
    let services = when(
        move || summary.with(|s| s.timing.services.len() > 1),
        move || {
            let service = |i: usize| {
                when(
                    move || summary.with(|s| s.timing.services.len() > i),
                    move || {
                        link(
                            Box::new(move |s| {
                                s.timing
                                    .services
                                    .get(i)
                                    .map(|(name, calls, failed, p95)| {
                                        let mut part = format!("{} {calls}", service_title(name));
                                        if *failed > 0 {
                                            part.push_str(&format!(
                                                " ({failed} {})",
                                                t("failed", "失败")
                                            ));
                                        }
                                        part.push_str(&format!(" · p95 {}", took(*p95)));
                                        part
                                    })
                                    .unwrap_or_default()
                            }),
                            Box::new(move |s| {
                                if let Some((name, ..)) = s.timing.services.get(i) {
                                    ui.service.set(Some(name.clone()));
                                }
                            }),
                        )
                    },
                )
            };
            let mut parts = view().flex_row().flex_wrap().gap(px(6.0)).flex_1();
            for i in 0..8 {
                parts = parts.child(service(i));
            }
            line(|| ts("Services", "服务"), parts.into_element())
        },
    );
    // What the run's functions spent, from their REPORTs.
    let functions = when(
        move || summary.with(|s| s.functions.runs > 0),
        move || {
            line(
                || ts("Functions", "函数"),
                view()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.0))
                    .flex_1()
                    .child(link(
                        Box::new(move |s| {
                            let f = &s.functions;
                            let zh = super::lang().get() == super::Lang::Zh;
                            let mut parts = vec![if zh {
                                format!("运行 {} 次", f.runs)
                            } else if f.runs == 1 {
                                "1 run".into()
                            } else {
                                format!("{} runs", f.runs)
                            }];
                            match (f.cold, zh) {
                                (0, _) => {}
                                (n, true) => parts.push(format!("冷启动 {n} 次")),
                                (1, false) => parts.push("1 cold start".into()),
                                (n, false) => parts.push(format!("{n} cold starts")),
                            }
                            if let Some((_, ms)) = f.slowest {
                                parts.push(format!(
                                    "{} {}",
                                    t("slowest", "最慢"),
                                    crate::functions::ms_label(ms)
                                ));
                            }
                            if let Some((used, of)) = f.memory {
                                parts.push(format!("{} {used} / {of} MB", t("memory", "内存峰值")));
                            }
                            parts.join(" · ")
                        }),
                        Box::new(move |s| {
                            if let Some((seq, _)) = s.functions.slowest {
                                ui.show_call(seq);
                            }
                        }),
                    ))
                    .child(when(
                        move || summary.with(|s| s.functions.near.is_some()),
                        move || {
                            nsh_base::button()
                                .on_press(move || {
                                    if let Some((seq, _)) =
                                        summary.with_untracked(|s| s.functions.near)
                                    {
                                        ui.show_call(seq);
                                    }
                                })
                                .flex_row()
                                .items_center()
                                .gap(px(4.0))
                                .rounded(radius())
                                .hover(|s| s.bg(hover_bg()))
                                .child(icon_colored(Icon::Warning, 12.0, orange()))
                                .child(
                                    text(move || {
                                        summary.with(|s| match s.functions.near {
                                            Some((_, true)) => ts(
                                                "A run came near its timeout",
                                                "有一次运行接近超时",
                                            ),
                                            Some((_, false)) => ts(
                                                "A run came near its memory size",
                                                "有一次运行接近内存上限",
                                            ),
                                            None => String::new(),
                                        })
                                    })
                                    .text_size(small())
                                    .text_color(orange()),
                                )
                        },
                    ))
                    .into_element(),
            )
        },
    );
    // Some of the run's calls are no longer kept.
    let cut = when(
        move || summary.with(|s| s.run.as_ref().is_some_and(|r| r.cut)),
        move || {
            text(move || {
                ts(
                    "The run's first calls are no longer kept (the list keeps the latest 20,000): this is what the rest did.",
                    "这次运行最早的调用已不再保留（列表只保留最近 20,000 次）：以下是其余调用的情况。",
                )
            })
            .text_size(small())
            .text_color(orange())
        },
    );
    let rewind = when(
        move || {
            summary.with(|s| {
                s.run
                    .as_ref()
                    .is_some_and(|r| r.ended_ms.is_some() && !r.undone)
            })
        },
        move || {
            nsh_base::button()
                .on_press(move || {
                    if let Some(id) = summary.with_untracked(|s| s.run.as_ref().map(|r| r.id)) {
                        ui.rewind(id, false);
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
    // After a fix: back to before the run, and run it again.
    let again = when(
        move || {
            summary.with(|s| {
                s.run
                    .as_ref()
                    .is_some_and(|r| r.ended_ms.is_some() && !r.undone)
            })
        },
        move || {
            nsh_base::button()
                .on_press(move || {
                    if let Some(id) = summary.with_untracked(|s| s.run.as_ref().map(|r| r.id)) {
                        ui.rewind(id, true);
                    }
                })
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .px(px(8.0))
                .h(px(22.0))
                .rounded(radius())
                .bg(fill_bg())
                .hover(|s| s.bg(hover_bg()))
                .child(icon(Icon::Restart, 13.0))
                .child(
                    text(move || ts("Rewind and Run Again", "回到之前并再运行")).text_size(small()),
                )
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
                .child(rewind)
                .child(again),
        )
        .child(chips)
        .child(cut)
        .child(since)
        .child(failure)
        .child(time)
        .child(functions)
        .child(services)
        .child(changed)
        .child(permissions)
}

#[allow(dead_code)]
fn unused(row: &calls::Row) -> usize {
    row.depth as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: u64, command: &str) -> Run {
        Run {
            id,
            command: command.into(),
            started_ms: id,
            ended_ms: Some(id + 1),
            exit: Some(0),
            generation: 0,
            undone: false,
            cut: false,
        }
    }

    fn failed(run: u64, code: &str) -> Arc<Call> {
        let mut c = Call {
            service: "lambda".into(),
            ..Default::default()
        };
        c.run = Some(run);
        c.operation = "Invoke".into();
        c.target = Some("fn/resize".into());
        c.failure = Some(crate::session::Failure {
            code: code.into(),
            message: None,
            hint: None,
        });
        Arc::new(c)
    }

    #[test]
    fn says_whether_the_fix_worked_since_the_command_last_ran() {
        let runs = vec![run(1, "pytest"), run(2, "ls"), run(3, "pytest")];
        let calls = vec![
            failed(1, "AccessDeniedException"),
            failed(1, "ValueError"),
            failed(3, "ValueError"),
        ];
        let c = compared(&calls, &runs, &runs[2]).expect("an earlier run of pytest");
        assert_eq!(
            c.fixed,
            vec!["Lambda Invoke fn/resize · AccessDeniedException".to_string()]
        );
        assert!(c.new.is_empty());
        assert_eq!(c.still, 1);
        // No earlier run of `ls`; the first `pytest` has none before it.
        assert!(compared(&calls, &runs, &runs[1]).is_none());
        assert!(compared(&calls, &runs, &runs[0]).is_none());
        // Calls of the earlier run are no longer all kept: no comparison.
        let mut cut = runs.clone();
        cut[0].cut = true;
        assert!(compared(&calls, &cut, &cut[2]).is_none());
    }

    fn call(seq: usize, at_ms: u64, ms: u64) -> Call {
        Call {
            seq,
            id: seq as u64 + 1,
            service: "s3".into(),
            operation: "PutObject".into(),
            at_ms,
            ms,
            status: 200,
            ..Default::default()
        }
    }

    #[test]
    fn says_how_a_run_spent_its_time() {
        let mut lambda = call(2, 120, 900);
        lambda.service = "lambda".into();
        lambda.operation = "Invoke".into();
        lambda.internal = true;
        lambda.parent = Some(1);
        lambda.function = Some(crate::functions::Report {
            duration_ms: 880.0,
            init_ms: Some(200.0),
            memory_mb: 128,
            max_memory_mb: 120,
            ..Default::default()
        });
        let calls: Vec<Arc<Call>> = vec![
            Arc::new(call(0, 100, 50)),
            Arc::new(lambda),
            // Overlaps the first: counted once.
            Arc::new(call(1, 110, 60)),
            Arc::new(call(3, 1000, 10)),
        ];
        let scope: Vec<&Arc<Call>> = calls.iter().collect();
        let t = timing(&scope);
        assert_eq!(t.in_aws_ms, 70 + 10);
        assert_eq!(t.slowest[0].0, 2);
        assert_eq!(t.services[0], ("s3".to_string(), 3, 0, 60));
        // The function began 20 ms after the call that caused it.
        assert_eq!(t.lag.as_ref().map(|l| l.2), Some(20));
        let f = functions(&scope);
        assert_eq!((f.runs, f.cold, f.memory), (1, 1, Some((120, 128))));
        assert_eq!(f.near, Some((2, false)));
    }

    #[test]
    fn counts_the_retries_and_whether_they_came_through() {
        let attempt = |seq, invocation: &str, attempt, code: Option<&str>| {
            let mut c = call(seq, 0, 1);
            c.invocation = Some(invocation.into());
            c.attempt = Some(attempt);
            c.failure = code.map(|code| crate::session::Failure {
                code: code.into(),
                message: None,
                hint: None,
            });
            if code.is_some() {
                c.status = 400;
            }
            Arc::new(c)
        };
        let calls = vec![
            attempt(0, "a", 1, Some("ThrottlingException")),
            attempt(1, "a", 2, None),
            attempt(2, "b", 1, Some("SlowDown")),
            attempt(3, "b", 2, Some("SlowDown")),
            attempt(4, "c", 1, None),
        ];
        let scope: Vec<&Arc<Call>> = calls.iter().collect();
        assert_eq!(
            retries(&scope),
            Retries {
                attempts: 2,
                calls: 2,
                rescued: 1,
                throttled: 3
            }
        );
    }
}
