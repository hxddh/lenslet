//! What a function did in one invocation: what it printed, and how it ended.
//! The emulator logs each run as Lambda does, in the function's log group,
//! from `START RequestId: <id>` to `REPORT RequestId: <id>`; the call that
//! invoked it carries the same request id.
//!
//! A function invoked synchronously says it failed in its answer
//! (`X-Amz-Function-Error`); one invoked asynchronously (an `Event` invoke, a
//! bucket's notification) has been answered 202 before it runs. Lenslet
//! watches those runs' logs to the end and marks the invoking call failed
//! when the run was.
//!
//! Every run ends on its `REPORT`: how long it took, how long it was billed
//! for, how long a cold start spent, and how much of its memory it used.
//! Lenslet reads it for every invocation, as CloudWatch's metrics would.

use std::{collections::HashMap, time::Duration};

use serde_json::{Value, json};

use crate::{
    emulator::Emulator,
    resources::Ask,
    session::{Call, Failure, Session},
};

/// One invocation's run, as its log tells it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Run {
    /// What it printed, and the runtime's lines about it, in order.
    pub lines: Vec<String>,
    /// Its `REPORT` line is in: it is over.
    pub ended: bool,
    /// How it failed, if it did.
    pub failure: Option<Failure>,
    /// What its `REPORT` said it spent.
    pub report: Option<Report>,
}

/// What one run spent, from its `REPORT`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub duration_ms: f64,
    pub billed_ms: u64,
    /// A cold start: the time its new instance took to start.
    pub init_ms: Option<f64>,
    /// The memory it may use, and the most it used (0 where the emulator
    /// cannot measure it, as on macOS).
    pub memory_mb: u64,
    pub max_memory_mb: u64,
    /// The function's timeout when it ran.
    pub timeout_ms: Option<u64>,
}

impl Report {
    /// It came within a fifth of its timeout.
    pub fn near_timeout(&self) -> bool {
        self.timeout_ms
            .is_some_and(|t| t > 0 && self.duration_ms >= t as f64 * 0.8)
    }

    /// It used nine tenths of its memory or more.
    pub fn near_memory(&self) -> bool {
        self.memory_mb > 0 && self.max_memory_mb * 10 >= self.memory_mb * 9
    }

    /// `812 ms (billed 813 ms) · cold start 240 ms · 92 of 128 MB`.
    pub fn line(&self) -> String {
        let mut parts = vec![format!(
            "{} ({} {} ms)",
            ms_label(self.duration_ms),
            crate::ui::t("billed", "计费"),
            self.billed_ms
        )];
        if let Some(init) = self.init_ms {
            parts.push(format!(
                "{} {}",
                crate::ui::t("cold start", "冷启动"),
                ms_label(init)
            ));
        }
        if self.max_memory_mb > 0 {
            parts.push(format!("{} / {} MB", self.max_memory_mb, self.memory_mb));
        }
        parts.join(" · ")
    }
}

/// `12 ms`, `1.24 s`.
pub fn ms_label(ms: f64) -> String {
    if ms < 1000.0 {
        format!("{} ms", ms.round() as u64)
    } else {
        format!("{:.2} s", ms / 1000.0)
    }
}

/// The function a call invoked, and the request id its run is logged under.
pub fn invoked(call: &Call) -> Option<(String, String)> {
    if call.service != "lambda" || !matches!(call.operation.as_str(), "Invoke" | "InvokeAsync") {
        return None;
    }
    let name = call.target.as_deref()?.strip_prefix("fn/")?;
    let name = name.split(':').next().filter(|n| !n.is_empty())?;
    Some((name.to_string(), call.request_id.clone()?))
}

/// The run of the invocation `request_id` of `function`, from its log group,
/// looking from `since_ms` on; `None` while nothing of it is logged.
pub async fn run(
    emulator: &Emulator,
    function: &str,
    request_id: &str,
    since_ms: u64,
) -> Option<Run> {
    let ask = Ask::new(emulator);
    let (group, timeout_ms) = configuration(&ask, function).await;
    let events = events(&ask, &group, since_ms).await?;
    read(&events, request_id).map(|run| with_timeout(run, timeout_ms))
}

/// A function's log group, and its timeout.
async fn configuration(ask: &Ask<'_>, function: &str) -> (String, Option<u64>) {
    let configuration = ask
        .rest_json(
            "lambda",
            "GET",
            &format!("/2015-03-31/functions/{function}/configuration"),
        )
        .await;
    let group = configuration
        .as_ref()
        .and_then(|c| c["LoggingConfig"]["LogGroup"].as_str().map(str::to_string))
        .filter(|g| !g.is_empty())
        .unwrap_or_else(|| format!("/aws/lambda/{function}"));
    let timeout_ms = configuration
        .as_ref()
        .and_then(|c| c["Timeout"].as_u64())
        .map(|s| s * 1000);
    (group, timeout_ms)
}

/// The events of a log group from about `since_ms` on, in order.
async fn events(ask: &Ask<'_>, group: &str, since_ms: u64) -> Option<Vec<String>> {
    let mut events = Vec::new();
    let mut token: Option<String> = None;
    // A few pages: an invocation's lines are near when it was made.
    for _ in 0..10 {
        let mut ask_for = json!({
            "logGroupName": group,
            "startTime": since_ms.saturating_sub(5_000),
            "limit": 10_000,
        });
        if let Some(token) = &token {
            ask_for["nextToken"] = token.clone().into();
        }
        let page = ask
            .json("logs", "Logs_20140328.FilterLogEvents", "1.1", ask_for)
            .await?;
        events.extend(
            page["events"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| e["message"].as_str().map(str::to_string)),
        );
        token = page["nextToken"].as_str().map(str::to_string);
        if token.is_none() {
            break;
        }
    }
    Some(events)
}

fn with_timeout(mut run: Run, timeout_ms: Option<u64>) -> Run {
    if let Some(report) = &mut run.report {
        report.timeout_ms = timeout_ms;
    }
    run
}

/// The run of `request_id` in a log group's `events`, in order.
pub fn read(events: &[String], request_id: &str) -> Option<Run> {
    let start = events.iter().position(|e| e.contains(request_id))?;
    let mut run = Run::default();
    for event in &events[start..] {
        let report = is_report(event, request_id);
        if report {
            run.failure = run.failure.or_else(|| report_failure(event));
            run.report = spent(event);
        } else if run.failure.is_none() {
            run.failure = printed_failure(event);
        }
        run.lines.push(event.trim_end().to_string());
        if report {
            run.ended = true;
            break;
        }
    }
    Some(run)
}

fn is_report(event: &str, request_id: &str) -> bool {
    event.contains(&format!("REPORT RequestId: {request_id}"))
        || (event.contains("\"platform.report\"") && event.contains(request_id))
}

/// What a `REPORT` line (plain, or the JSON `platform.report`) says the run
/// spent.
fn spent(event: &str) -> Option<Report> {
    if let Ok(report) = serde_json::from_str::<Value>(event) {
        let m = &report["record"]["metrics"];
        return Some(Report {
            duration_ms: m["durationMs"].as_f64()?,
            billed_ms: m["billedDurationMs"].as_u64().unwrap_or(0),
            init_ms: m["initDurationMs"].as_f64(),
            memory_mb: m["memorySizeMB"].as_u64().unwrap_or(0),
            max_memory_mb: m["maxMemoryUsedMB"].as_u64().unwrap_or(0),
            timeout_ms: None,
        });
    }
    let field = |name: &str| {
        event.split('\t').find_map(|f| {
            let value = f.trim().strip_prefix(name)?.trim();
            value.split(' ').next()?.parse::<f64>().ok()
        })
    };
    Some(Report {
        duration_ms: field("Duration:")?,
        billed_ms: field("Billed Duration:").unwrap_or(0.0) as u64,
        init_ms: field("Init Duration:"),
        memory_mb: field("Memory Size:").unwrap_or(0.0) as u64,
        max_memory_mb: field("Max Memory Used:").unwrap_or(0.0) as u64,
        timeout_ms: None,
    })
}

/// A failure the runtime printed: Python's `[ERROR] Type: message`, Node's
/// `ERROR` field.
fn printed_failure(event: &str) -> Option<Failure> {
    let line = event.lines().next()?.trim();
    let rest = match line.strip_prefix("[ERROR]") {
        Some(rest) => rest.trim(),
        None => line.split("\tERROR\t").nth(1)?.trim(),
    };
    let (code, message) = rest
        .split_once(':')
        .map_or((rest, ""), |(c, m)| (c.trim(), m.trim()));
    let code = if code.is_empty() || code.contains(' ') {
        "Error"
    } else {
        code
    };
    Some(Failure {
        code: code.to_string(),
        message: Some(event.trim_end().to_string())
            .filter(|_| !message.is_empty() || event.contains('\n')),
        hint: Some(FAILED_RUN.into()),
    })
}

/// A failure the platform reported: `Status: timeout` (or `error`) on the
/// `REPORT` line, with its `Error Type`.
fn report_failure(event: &str) -> Option<Failure> {
    if let Ok(report) = serde_json::from_str::<Value>(event) {
        let status = report["record"]["status"].as_str()?;
        return (status != "success").then(|| Failure {
            code: report["record"]["errorType"]
                .as_str()
                .unwrap_or(status)
                .to_string(),
            message: None,
            hint: Some(FAILED_RUN.into()),
        });
    }
    let field = |name: &str| {
        event
            .split('\t')
            .find_map(|f| f.trim().strip_prefix(name).map(|v| v.trim().to_string()))
    };
    let status = field("Status:")?;
    let code = field("Error Type:").unwrap_or_else(|| match status.as_str() {
        "timeout" => "Sandbox.Timedout".to_string(),
        other => other.to_string(),
    });
    Some(Failure {
        code,
        message: Some(format!("The run ended: {status}.")),
        hint: Some(FAILED_RUN.into()),
    })
}

const FAILED_RUN: &str =
    "The function's own run failed after it was invoked: what it printed is with the call.";

/// How long a run is watched for: Lambda's longest timeout, and its retries.
const WATCH_FOR: Duration = Duration::from_secs(20 * 60);

/// Watch every invocation's run to its end: keep what its `REPORT` says it
/// spent, and mark the invoking call failed when its run failed (Lambda
/// answered an asynchronous one 202 before it ran). Each function's log is
/// read once a second for all of its runs still going.
pub async fn watch(emulator: Emulator, session: Session) {
    struct Pending {
        seq: usize,
        request_id: String,
        at_ms: u64,
        generation: u64,
    }
    let mut pending: HashMap<String, Vec<Pending>> = HashMap::new();
    let mut seen = 0usize;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        for call in session.calls_from(seen) {
            seen = call.seq + 1;
            // A call Lambda refused never ran.
            if call.status < 300
                && call.function.is_none()
                && let Some((function, request_id)) = invoked(&call)
            {
                pending.entry(function).or_default().push(Pending {
                    seq: call.seq,
                    request_id,
                    at_ms: call.at_ms,
                    generation: call.generation,
                });
            }
        }
        let generation = session.generation();
        let now = crate::session::now_ms();
        let ask = Ask::new(&emulator);
        for (function, runs) in pending.iter_mut() {
            // The account it ran in is gone, or it has had its time.
            runs.retain(|p| {
                p.generation == generation
                    && now.saturating_sub(p.at_ms) <= WATCH_FOR.as_millis() as u64
            });
            let Some(since) = runs.iter().map(|p| p.at_ms).min() else {
                continue;
            };
            let (group, timeout_ms) = configuration(&ask, function).await;
            let Some(events) = events(&ask, &group, since).await else {
                continue;
            };
            runs.retain(|p| match read(&events, &p.request_id) {
                Some(run) if run.ended => {
                    let run = with_timeout(run, timeout_ms);
                    session.ran(p.seq, run.report.unwrap_or_default(), run.failure);
                    false
                }
                _ => true,
            });
        }
        pending.retain(|_, runs| !runs.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn reads_one_invocations_run_out_of_its_log_group() {
        let log = events(&[
            "START RequestId: aaa Version: $LATEST",
            "other run",
            "REPORT RequestId: aaa\tDuration: 1.00 ms",
            "START RequestId: bbb Version: $LATEST",
            "hello from bbb\n",
            "[ERROR] KeyError: 'id'\nTraceback (most recent call last):\n  File \"/var/task/app.py\", line 3",
            "END RequestId: bbb",
            "REPORT RequestId: bbb\tDuration: 2.00 ms\tBilled Duration: 2 ms\tMemory Size: 128 MB\tMax Memory Used: 30 MB\t",
            "START RequestId: ccc Version: $LATEST",
        ]);
        let run = read(&log, "bbb").expect("logged");
        assert!(run.ended);
        assert_eq!(
            run.lines.first().unwrap(),
            "START RequestId: bbb Version: $LATEST"
        );
        assert_eq!(run.lines.len(), 5);
        assert_eq!(run.failure.as_ref().unwrap().code, "KeyError");
        let spent = run.report.as_ref().unwrap();
        assert_eq!(
            (spent.billed_ms, spent.memory_mb, spent.max_memory_mb),
            (2, 128, 30)
        );
        assert_eq!(spent.init_ms, None);
        let ok = read(&log, "aaa").unwrap();
        assert!(ok.ended && ok.failure.is_none());
        // Still running: not over yet.
        let running = read(&log, "ccc").unwrap();
        assert!(!running.ended);
        assert!(read(&log, "zzz").is_none());
    }

    #[test]
    fn a_run_the_platform_ended_failed() {
        let log = events(&[
            "START RequestId: t Version: 1",
            "REPORT RequestId: t\tDuration: 3000.00 ms\tBilled Duration: 3000 ms\tMemory Size: 128 MB\tMax Memory Used: 30 MB\tStatus: timeout",
        ]);
        let run = read(&log, "t").unwrap();
        assert_eq!(run.failure.unwrap().code, "Sandbox.Timedout");
        // Node's way of saying so.
        let node = events(&[
            "START RequestId: n Version: 1",
            "2026-11-27T00:00:00.000Z\tn\tERROR\tInvoke Error \t{\"errorType\":\"TypeError\"}",
            "REPORT RequestId: n\tDuration: 1.00 ms",
        ]);
        assert!(read(&node, "n").unwrap().failure.is_some());
    }

    #[test]
    fn reads_what_a_run_spent() {
        let plain = spent("REPORT RequestId: r\tDuration: 812.35 ms\tBilled Duration: 813 ms\tMemory Size: 128 MB\tMax Memory Used: 120 MB\tInit Duration: 240.10 ms\t").unwrap();
        assert_eq!(plain.duration_ms, 812.35);
        assert_eq!(plain.init_ms, Some(240.10));
        assert!(plain.near_memory());
        let json = spent(r#"{"time":"t","type":"platform.report","record":{"requestId":"r","metrics":{"durationMs":2900.5,"billedDurationMs":2901,"memorySizeMB":256,"maxMemoryUsedMB":0},"status":"success"}}"#).unwrap();
        assert_eq!((json.billed_ms, json.max_memory_mb), (2901, 0));
        // Memory not measured: never near it.
        assert!(!json.near_memory());
        let timed = Report {
            timeout_ms: Some(3000),
            ..json
        };
        assert!(timed.near_timeout());
        assert!(spent("START RequestId: r").is_none());
    }
}
