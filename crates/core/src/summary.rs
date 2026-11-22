//! What a run did: the in-memory tally, the findings, the `end` report, and
//! its rendering for the terminal and for a pull request.
//!
//! The report is one object with one schema everywhere: the last line of the
//! run file and `lenslet run --json`.
//!
//! Findings are the run's one ranked list of what is worth a look:
//! - `failure`: calls that failed the same way (service, operation, error
//!   code), with the cause (cross-call root causes included). A failed SDK
//!   attempt that a later attempt of the same invocation recovered from is
//!   not a failure of the run.
//! - `access`: a permission the run used that the lock does not list.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
};

use serde::{Deserialize, Serialize};

use crate::{
    access::{self, Access},
    lock::Lock,
    trace::Trace,
};

/// Findings listed in the human summary; the report keeps all of them.
const LISTED: usize = 10;
/// Access changes listed in the human summary.
const LISTED_ACCESS: usize = 20;
/// Inline new-permission rows in PR Markdown before folding into `<details>`.
const MARKDOWN_INLINE_ADDED: usize = 30;
/// Hard ceiling on PR Markdown permission rows so comments stay under 64 KiB.
const MARKDOWN_MAX_ROWS: usize = 120;
/// Distinct calls kept per permission in `via`.
const VIA_LIMIT: usize = 3;

#[derive(Clone, Debug, Default)]
struct ResourceHistory {
    created: Option<(usize, String, Option<String>)>,
    deleted: Option<(usize, String)>,
}

/// Tracks resource creation, deletion, and region placement across calls in
/// one run, so missing-resource errors explain their cross-call root cause.
#[derive(Clone, Debug, Default)]
pub struct CausalTracker {
    resources: BTreeMap<(String, String), ResourceHistory>,
}

impl CausalTracker {
    /// Enrich `trace.problem.hint` when an earlier call in this run explains
    /// the failure, then note successful creates and deletes. `concrete_own`
    /// is the call's concrete own access. Returns whether the hint changed.
    pub fn observe_and_enrich(
        &mut self,
        trace: &mut Trace,
        concrete_own: &[Access],
        call_no: usize,
    ) -> bool {
        let lookup_keys = causal_keys(concrete_own, true);
        if trace.status >= 400
            && let Some(problem) = trace.problem.as_mut()
            && is_missing_resource_code(&problem.code, trace.status)
        {
            for (svc, stem) in &lookup_keys {
                let Some(hist) = self.resources.get(&(svc.clone(), stem.clone())) else {
                    continue;
                };
                if let Some((del_call, del_op)) = hist.deleted.as_ref() {
                    problem.hint = Some(format!(
                        "`{stem}` was deleted earlier in this run at call #{del_call} ({del_op}) before this `{}` ran.",
                        trace.operation
                    ));
                    return true;
                }
                if !matches!(svc.as_str(), "s3" | "iam")
                    && let Some((cr_call, cr_op, Some(cr_reg))) = hist.created.as_ref()
                    && let Some(req_reg) = trace.region.as_deref().filter(|r| !r.is_empty())
                    && cr_reg != req_reg
                {
                    problem.hint = Some(format!(
                        "Region mismatch: `{stem}` was created in `{cr_reg}` at call #{cr_call} ({cr_op}), but this `{}` targeted `{req_reg}`.",
                        trace.operation
                    ));
                    return true;
                }
            }
        }
        if trace.status < 400 {
            let keys = causal_keys(concrete_own, false);
            if is_create_operation(&trace.operation) {
                for key in keys {
                    let entry = self.resources.entry(key).or_default();
                    entry.created = Some((call_no, trace.operation.clone(), trace.region.clone()));
                    entry.deleted = None;
                }
            } else if is_delete_operation(&trace.operation) {
                for key in keys {
                    self.resources.entry(key).or_default().deleted =
                        Some((call_no, trace.operation.clone()));
                }
            }
        }
        false
    }
}

fn is_missing_resource_code(code: &str, status: u16) -> bool {
    status == 404
        || matches!(
            code,
            "NoSuchBucket"
                | "NoSuchKey"
                | "NotFound"
                | "NotFoundException"
                | "ResourceNotFoundException"
                | "QueueDoesNotExist"
                | "NonExistentQueue"
                | "AWS.SimpleQueueService.NonExistentQueue"
                | "TopicDoesNotExist"
                | "ParameterNotFound"
                | "StateMachineDoesNotExist"
                | "ExecutionDoesNotExist"
                | "RepositoryNotFoundException"
                | "UserPoolAddOnNotEnabledException"
        )
}

fn is_create_operation(op: &str) -> bool {
    matches!(
        op,
        "CreateBucket"
            | "PutObject"
            | "CopyObject"
            | "CompleteMultipartUpload"
            | "CreateTable"
            | "CreateQueue"
            | "CreateTopic"
            | "CreateFunction"
            | "CreateSecret"
            | "PutParameter"
            | "CreateLogGroup"
            | "CreateStream"
            | "CreateDeliveryStream"
            | "CreateEventBus"
            | "CreateStateMachine"
            | "CreateRepository"
            | "CreateUserPool"
            | "CreateSchedule"
    )
}

fn is_delete_operation(op: &str) -> bool {
    matches!(
        op,
        "DeleteBucket"
            | "DeleteObject"
            | "DeleteObjects"
            | "DeleteTable"
            | "DeleteQueue"
            | "DeleteTopic"
            | "DeleteFunction"
            | "DeleteSecret"
            | "DeleteParameter"
            | "DeleteParameters"
            | "DeleteLogGroup"
            | "DeleteStream"
            | "DeleteDeliveryStream"
            | "DeleteEventBus"
            | "DeleteStateMachine"
            | "DeleteRepository"
            | "DeleteUserPool"
            | "DeleteSchedule"
    )
}

/// The resources a call names, as `(service, name)` keys: `("s3",
/// "s3://bucket/key")`, `("dynamodb", "table/orders")`, `("sqs", "jobs")`.
fn causal_keys(concrete_own: &[Access], include_parent_bucket: bool) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for acc in concrete_own {
        let res = acc.resource.as_str();
        if res == "*" {
            continue;
        }
        if let Some(s3_rest) = res.strip_prefix("arn:aws:s3:::") {
            if let Some((bucket, key)) = s3_rest.split_once('/') {
                if !key.is_empty() && key != "*" {
                    out.push(("s3".into(), format!("s3://{bucket}/{key}")));
                }
                if include_parent_bucket {
                    out.push(("s3".into(), format!("s3://{bucket}")));
                }
            } else if !s3_rest.is_empty() {
                out.push(("s3".into(), format!("s3://{s3_rest}")));
            }
            continue;
        }
        let mut parts = res.splitn(6, ':');
        let (Some("arn"), Some(_), Some(arn_svc), Some(_), Some(_), Some(rest)) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            continue;
        };
        let stem = match arn_svc {
            "dynamodb" => rest
                .split_once("/index/")
                .or_else(|| rest.split_once("/stream/"))
                .map_or(rest, |(table, _)| table),
            "secretsmanager" => rest.strip_suffix("-*").unwrap_or(rest),
            _ => rest,
        };
        let key = (arn_svc.to_string(), stem.to_string());
        if !out.contains(&key) {
            out.push(key);
        }
    }
    out
}

/// How a run used one permission.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccessUse {
    pub calls: usize,
    pub errors: usize,
    /// Needed by another call rather than called directly (KMS, PassRole).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub implied: bool,
    /// The calls that needed it, e.g. `DYNAMODB DeleteItem` (first few).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub via: Vec<String>,
    /// A `*` that stands for a resource some call's request body did not let
    /// Lenslet read (binary, or over the inspection limit).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unverified: bool,
}

/// A failed call, kept until the findings are made.
#[derive(Clone, Debug)]
struct FailedCall {
    index: usize,
    id: String,
    service: String,
    operation: String,
    status: u16,
    code: String,
    message: Option<String>,
    hint: Option<String>,
    invocation_id: Option<String>,
}

/// A permission not in the lock, and the calls that needed it.
#[derive(Clone, Debug)]
struct NewAccess {
    index: usize,
    calls: Vec<String>,
    implied: bool,
    unverified: bool,
    via: String,
}

/// The running tally of a run's calls.
#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub calls: usize,
    pub errors: usize,
    pub services: BTreeMap<String, usize>,
    pub unmapped: BTreeMap<String, usize>,
    pub access: BTreeMap<Access, AccessUse>,
    pub causal: CausalTracker,
    failed: Vec<FailedCall>,
    /// Per SDK invocation, the index of its latest successful attempt.
    recovered: HashMap<String, usize>,
    new_access: BTreeMap<Access, NewAccess>,
}

impl Summary {
    /// Count one finished call (its access already recorded on it).
    pub fn observe(&mut self, trace: &Trace) {
        let index = self.calls;
        self.calls += 1;
        *self.services.entry(trace.service.clone()).or_default() += 1;
        if access::is_unmapped(trace) {
            *self.unmapped.entry(trace.service.clone()).or_default() += 1;
        }
        let via = format!("{} {}", trace.service.to_ascii_uppercase(), trace.operation);
        let own = trace.access.iter().map(|item| (item, false));
        let implied = trace.implied.iter().map(|item| (item, true));
        for (item, is_implied) in own.chain(implied) {
            let entry = self.access.entry(item.key()).or_default();
            entry.unverified |= item.unverified;
            entry.calls += 1;
            entry.errors += usize::from(trace.problem.is_some());
            entry.implied = if entry.calls == 1 {
                is_implied
            } else {
                entry.implied && is_implied
            };
            if entry.via.len() < VIA_LIMIT && !entry.via.contains(&via) {
                entry.via.push(via.clone());
            }
            if item.new {
                let new = self
                    .new_access
                    .entry(item.key())
                    .or_insert_with(|| NewAccess {
                        index,
                        calls: Vec::new(),
                        implied: is_implied,
                        unverified: item.unverified,
                        via: via.clone(),
                    });
                if !new.calls.contains(&trace.id) {
                    new.calls.push(trace.id.clone());
                }
            }
        }
        match trace.problem.as_ref() {
            Some(problem) => {
                self.errors += 1;
                self.failed.push(FailedCall {
                    index,
                    id: trace.id.clone(),
                    service: trace.service.clone(),
                    operation: trace.operation.clone(),
                    status: trace.status,
                    code: problem.code.clone(),
                    message: problem.message.clone(),
                    hint: problem.hint.clone(),
                    invocation_id: trace.invocation_id.clone(),
                });
            }
            None => {
                if let Some(id) = trace.invocation_id.as_ref() {
                    self.recovered.insert(id.clone(), index);
                }
            }
        }
    }

    /// The findings so far, failures first, each kind in call order. Access
    /// findings name `lock` (the lock file's label).
    pub fn findings(&self, lock: &str) -> Vec<Finding> {
        let mut found: Vec<(usize, Finding)> = Vec::new();
        let mut groups: BTreeMap<(&str, &str, &str), Vec<&FailedCall>> = BTreeMap::new();
        for call in &self.failed {
            let recovered = call
                .invocation_id
                .as_ref()
                .and_then(|id| self.recovered.get(id))
                .is_some_and(|&ok| ok > call.index);
            if !recovered {
                groups
                    .entry((&call.service, &call.operation, &call.code))
                    .or_default()
                    .push(call);
            }
        }
        for calls in groups.into_values() {
            let first = calls[0];
            // The latest call is the one shown; the first with a cause explains them all.
            let last = calls[calls.len() - 1];
            let hint = calls.iter().find_map(|call| call.hint.clone());
            let message = last.message.clone();
            found.push((
                first.index,
                Finding::Failure(FailureFinding {
                    title: format!(
                        "{} {} → {}{}",
                        first.service.to_ascii_uppercase(),
                        first.operation,
                        first.code,
                        times(calls.len())
                    ),
                    detail: hint.clone().or_else(|| message.clone()),
                    calls: calls.iter().map(|call| call.id.clone()).collect(),
                    service: first.service.clone(),
                    operation: first.operation.clone(),
                    status: last.status,
                    code: first.code.clone(),
                    message,
                    hint,
                }),
            ));
        }
        found.sort_by_key(|(index, _)| *index);
        let mut access: Vec<(usize, Finding)> = self
            .new_access
            .iter()
            .map(|(item, new)| {
                let mut detail = format!(
                    "{} by {}.",
                    if new.implied { "Implied" } else { "Needed" },
                    new.via
                );
                if new.unverified {
                    detail.push_str(
                        " Resource unknown: the request body was not readable (binary or too large).",
                    );
                }
                (
                    new.index,
                    Finding::Access(AccessFinding {
                        title: format!(
                            "{} on {} not in {lock}",
                            item.action,
                            short_resource(&item.resource)
                        ),
                        detail: Some(detail),
                        calls: new.calls.clone(),
                        action: item.action.clone(),
                        resource: item.resource.clone(),
                        implied: new.implied,
                        unverified: new.unverified,
                    }),
                )
            })
            .collect();
        access.sort_by_key(|(index, _)| *index);
        found.extend(access);
        found.into_iter().map(|(_, finding)| finding).collect()
    }
}

/// `table/orders` from `arn:aws:dynamodb:*:*:table/orders`, `bucket/*` from
/// `arn:aws:s3:::bucket/*`.
fn short_resource(resource: &str) -> &str {
    if !resource.starts_with("arn:") {
        return resource;
    }
    resource
        .splitn(6, ':')
        .nth(5)
        .filter(|rest| !rest.is_empty())
        .unwrap_or(resource)
}

/// One finding: calls that failed the same way, or a permission the lock
/// does not list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Finding {
    Failure(FailureFinding),
    Access(AccessFinding),
}

impl Finding {
    pub fn title(&self) -> &str {
        match self {
            Finding::Failure(failure) => &failure.title,
            Finding::Access(access) => &access.title,
        }
    }

    /// Ids of the calls this finding is about, in call order.
    pub fn calls(&self) -> &[String] {
        match self {
            Finding::Failure(failure) => &failure.calls,
            Finding::Access(access) => &access.calls,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureFinding {
    /// `DYNAMODB GetItem → ResourceNotFoundException ×2`.
    pub title: String,
    /// The likely cause, else the error message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub calls: Vec<String>,
    pub service: String,
    pub operation: String,
    /// The HTTP status of the latest failed call.
    pub status: u16,
    pub code: String,
    /// The error message of the latest failed call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The likely cause (cross-call root causes included).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessFinding {
    /// `dynamodb:DeleteItem on table/orders not in lenslet.lock`.
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub calls: Vec<String>,
    pub action: String,
    pub resource: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub implied: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unverified: bool,
}

/// One line of the access table.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccessEntry {
    pub action: String,
    pub resource: String,
    #[serde(flatten)]
    pub used: AccessUse,
    /// Used by the run, not in the lock.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub new: bool,
    /// In the lock, not used by the run (`calls` is 0).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unused: bool,
}

/// A failure kind, for comparing runs: the same call failing the same way.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct FailureKind {
    pub service: String,
    pub operation: String,
    pub code: String,
}

/// What changed since the previous run of the same command in the same
/// directory.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Regressions {
    /// Run id it was compared with.
    pub previous_run: String,
    pub previous_exit_code: Option<i32>,
    /// Failing now, not failing then.
    pub new_failures: Vec<FailureKind>,
    /// Failing then, not failing now.
    pub fixed: Vec<FailureKind>,
}

/// The `end` record: everything a person or an agent needs about one run.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Report {
    pub version: u32,
    pub run: String,
    pub command: Vec<String>,
    /// The test command's exit code (`128 + signal` when a signal ended it).
    pub exit_code: Option<i32>,
    pub ended: String,
    pub duration_ms: u64,
    pub calls: usize,
    pub errors: usize,
    pub services: BTreeMap<String, usize>,
    /// Calls with no recognizable operation, per service.
    pub unmapped: BTreeMap<String, usize>,
    /// Every permission the run used, then the lock lines it did not use.
    pub access: Vec<AccessEntry>,
    /// Everything worth a look: failures, then access not in the lock.
    pub findings: Vec<Finding>,
    pub regressions: Option<Regressions>,
    /// Compared with this lock file; `null` when there is none.
    pub lock: Option<String>,
    /// What answered the calls (see the run header's `emulator`).
    pub emulator: String,
    /// The run file.
    pub trace: String,
    /// The test command's combined stdout and stderr.
    pub log: Option<String>,
}

impl Report {
    /// Fill in the tallies, the access table and the findings from `summary`,
    /// compared with `lock` (its label and contents) when there is one.
    pub fn tally(&mut self, summary: &Summary, lock: Option<(&str, &Lock)>) {
        self.calls = summary.calls;
        self.errors = summary.errors;
        self.services = summary.services.clone();
        self.unmapped = summary.unmapped.clone();
        self.lock = lock.map(|(label, _)| label.to_string());
        self.access = summary
            .access
            .iter()
            .map(|(access, used)| AccessEntry {
                action: access.action.clone(),
                resource: access.resource.clone(),
                used: used.clone(),
                new: lock.is_some_and(|(_, lock)| !lock.access.contains(access)),
                unused: false,
            })
            .collect();
        if let Some((_, lock)) = lock {
            self.access.extend(
                lock.access
                    .iter()
                    .filter(|access| !summary.access.contains_key(*access))
                    .map(|access| AccessEntry {
                        action: access.action.clone(),
                        resource: access.resource.clone(),
                        used: AccessUse::default(),
                        new: false,
                        unused: true,
                    }),
            );
        }
        self.findings = summary.findings(lock.map_or(crate::lock::FILE_NAME, |(label, _)| label));
    }

    /// Permissions the run used that the lock does not list.
    pub fn added(&self) -> impl Iterator<Item = &AccessEntry> {
        self.access.iter().filter(|entry| entry.new)
    }

    /// Lock lines the run did not use.
    pub fn unused(&self) -> impl Iterator<Item = &AccessEntry> {
        self.access.iter().filter(|entry| entry.unused)
    }

    pub fn failures(&self) -> impl Iterator<Item = &FailureFinding> {
        self.findings.iter().filter_map(|finding| match finding {
            Finding::Failure(failure) => Some(failure),
            Finding::Access(_) => None,
        })
    }

    pub fn failure_kinds(&self) -> BTreeSet<FailureKind> {
        self.failures()
            .map(|failure| FailureKind {
                service: failure.service.clone(),
                operation: failure.operation.clone(),
                code: failure.code.clone(),
            })
            .collect()
    }

    pub fn compare_with(&mut self, previous: &Report) {
        let now = self.failure_kinds();
        let then = previous.failure_kinds();
        self.regressions = Some(Regressions {
            previous_run: previous.run.clone(),
            previous_exit_code: previous.exit_code,
            new_failures: now.difference(&then).cloned().collect(),
            fixed: then.difference(&now).cloned().collect(),
        });
    }

    fn new_failures(&self) -> BTreeSet<FailureKind> {
        self.regressions
            .as_ref()
            .map(|regressions| regressions.new_failures.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// The human report, printed to stderr after the command exits. Paths
    /// under `home` are shown as `~/…`. `updating` is set when this run is
    /// about to write the lock, so it does not suggest doing that.
    pub fn render(&self, home: &Path, updating: bool) -> String {
        let mut out = String::new();
        let services = if self.services.is_empty() {
            String::new()
        } else {
            let names: Vec<&str> = self.services.keys().map(String::as_str).collect();
            format!(
                " · {} ({})",
                plural(self.services.len(), "service"),
                names.join(", ")
            )
        };
        out.push_str(&format!(
            "lenslet: {} · {}{services} · {} · against {}\n",
            plural(self.calls, "call"),
            plural(self.errors, "error"),
            seconds(self.duration_ms),
            self.emulator,
        ));
        let failures: Vec<&FailureFinding> = self.failures().collect();
        for failure in failures.iter().take(LISTED) {
            out.push_str(&failure_lines(failure));
        }
        if failures.len() > LISTED {
            out.push_str(&format!("  … and {} more\n", failures.len() - LISTED));
        }
        if let Some(regressions) = self.regressions.as_ref()
            && (!regressions.new_failures.is_empty() || !regressions.fixed.is_empty())
        {
            let new = regressions.new_failures.len();
            out.push_str(&format!(
                "  since run {}: {new} new {}, {} fixed\n",
                regressions.previous_run,
                if new == 1 { "failure" } else { "failures" },
                regressions.fixed.len()
            ));
            for kind in &regressions.new_failures {
                out.push_str(&format!(
                    "    new   {} {} {}\n",
                    kind.service.to_ascii_uppercase(),
                    kind.operation,
                    kind.code
                ));
            }
        }
        let added: Vec<&AccessEntry> = self.added().collect();
        let used = self.access.iter().filter(|entry| !entry.unused).count();
        match self.lock.as_deref() {
            Some(lock) if !added.is_empty() => {
                out.push_str(&format!(
                    "  access: {} not in {lock}\n",
                    plural(added.len(), "permission")
                ));
                for entry in added.iter().take(LISTED_ACCESS) {
                    out.push_str(&format!(
                        "    + {:<32} {}{}\n",
                        entry.action,
                        entry.resource,
                        if entry.used.unverified {
                            "  (resource unknown: request body not readable)"
                        } else {
                            ""
                        }
                    ));
                }
                if added.len() > LISTED_ACCESS {
                    out.push_str(&format!("    … and {} more\n", added.len() - LISTED_ACCESS));
                }
            }
            Some(lock) => out.push_str(&format!(
                "  access: within {lock} ({} used)\n",
                plural(used, "permission")
            )),
            None if used > 0 && updating => out.push_str(&format!(
                "  access: {} observed\n",
                plural(used, "permission")
            )),
            None if used > 0 => out.push_str(&format!(
                "  access: {} observed · `lenslet run --update -- …` writes lenslet.lock\n",
                plural(used, "permission")
            )),
            None => {}
        }
        let unused: Vec<&AccessEntry> = self.unused().collect();
        if let Some(lock) = self.lock.as_deref()
            && !unused.is_empty()
        {
            out.push_str(&format!(
                "  unused: {} in {lock} not exercised by this run\n",
                plural(unused.len(), "permission")
            ));
            for entry in unused.iter().take(LISTED_ACCESS) {
                out.push_str(&format!("    - {:<32} {}\n", entry.action, entry.resource));
            }
            if unused.len() > LISTED_ACCESS {
                out.push_str(&format!(
                    "    … and {} more\n",
                    unused.len() - LISTED_ACCESS
                ));
            }
        }
        if !self.unmapped.is_empty() {
            let total: usize = self.unmapped.values().sum();
            let names: Vec<&str> = self.unmapped.keys().map(String::as_str).collect();
            out.push_str(&format!(
                "  note: {} ({}) had no recognizable operation and {} not in the access list\n",
                plural(total, "call"),
                names.join(", "),
                if total == 1 { "is" } else { "are" }
            ));
        }
        if self.calls == 0 {
            out.push_str(
                "  no AWS calls reached Lenslet: does the program honor AWS_ENDPOINT_URL?\n",
            );
        }
        let log = self
            .log
            .as_deref()
            .map(|log| format!(" · output {}", tilde(log, home)))
            .unwrap_or_default();
        out.push_str(&format!(
            "lenslet: trace {}{log} · `lenslet ui` to inspect\n",
            tilde(&self.trace, home)
        ));
        out
    }

    /// The report as GitHub-flavored Markdown: what a PR comment or a CI job
    /// summary shows. Failures lead, then access changes, each with the calls
    /// behind it.
    pub fn markdown(&self) -> String {
        let mut out = String::new();
        let command = format!("`{}`", self.command.join(" ").replace('`', "'"));
        let exit = match self.exit_code {
            Some(0) => "passed".to_string(),
            Some(code) => format!("**failed** (exit {code})"),
            None => "ended".to_string(),
        };
        out.push_str(&format!("### Lenslet · {command} {exit}\n\n"));
        out.push_str(&format!(
            "{} · {} · {} · against {}\n\n",
            plural(self.calls, "AWS call"),
            plural(self.errors, "error"),
            plural(self.services.len(), "service"),
            self.emulator,
        ));
        let failures: Vec<&FailureFinding> = self.failures().collect();
        if !failures.is_empty() {
            let new = self.new_failures();
            out.push_str("| | Failure | Why |\n|---|---|---|\n");
            for failure in failures.iter().take(LISTED * 2) {
                let marker = if new.contains(&FailureKind {
                    service: failure.service.clone(),
                    operation: failure.operation.clone(),
                    code: failure.code.clone(),
                }) {
                    " **new**"
                } else {
                    ""
                };
                let detail = [
                    failure
                        .message
                        .as_deref()
                        .map(|message| one_line(message, 100)),
                    failure.hint.clone(),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" — ");
                out.push_str(&format!(
                    "| ✗ | {}{marker} | {} |\n",
                    failure.title.replace('|', "\\|"),
                    detail.replace('|', "\\|")
                ));
            }
            if failures.len() > LISTED * 2 {
                out.push_str(&format!(
                    "\n_… and {} more in the JSON report._\n",
                    failures.len() - LISTED * 2
                ));
            }
            out.push('\n');
        }

        let via = |entry: &AccessEntry| {
            let mut text = entry.used.via.join(", ");
            if entry.used.implied {
                text.push_str(" (implied)");
            }
            if entry.used.unverified {
                text.push_str(" (resource unknown: request body not readable)");
            }
            text
        };
        let row = |entry: &AccessEntry| {
            format!(
                "| + | `{}` | `{}` | {} |\n",
                entry.action,
                entry.resource,
                via(entry)
            )
        };
        let added: Vec<&AccessEntry> = self.added().collect();
        let used = self.access.iter().filter(|entry| !entry.unused).count();
        match self.lock.as_deref() {
            Some(lock) if !added.is_empty() => {
                out.push_str(&format!(
                    "**{} not in `{lock}`.** Review them; if they are intended, run `lenslet run --update` and commit `{lock}`.\n\n",
                    plural(added.len(), "permission")
                ));
                out.push_str("| | Permission | Resource | Needed by |\n|---|---|---|---|\n");
                for entry in added.iter().take(MARKDOWN_INLINE_ADDED) {
                    out.push_str(&row(entry));
                }
                out.push('\n');
                if added.len() > MARKDOWN_INLINE_ADDED {
                    let extra = &added[MARKDOWN_INLINE_ADDED..];
                    let shown = extra.len().min(MARKDOWN_MAX_ROWS - MARKDOWN_INLINE_ADDED);
                    out.push_str(&format!(
                        "<details><summary>… and {} more new permissions</summary>\n\n| | Permission | Resource | Needed by |\n|---|---|---|---|\n",
                        extra.len()
                    ));
                    for entry in extra.iter().take(shown) {
                        out.push_str(&row(entry));
                    }
                    if extra.len() > shown {
                        out.push_str(&format!(
                            "\n_… and {} more in the JSON report._\n",
                            extra.len() - shown
                        ));
                    }
                    out.push_str("\n</details>\n\n");
                }
            }
            Some(lock) => out.push_str(&format!(
                "AWS access is within `{lock}` ({} used).\n\n",
                plural(used, "permission")
            )),
            None => out.push_str(&format!(
                "{} observed; there is no `lenslet.lock` to compare with yet.\n\n",
                plural(used, "permission")
            )),
        }
        let unused: Vec<&AccessEntry> = self.unused().collect();
        if let Some(lock) = self.lock.as_deref()
            && !unused.is_empty()
        {
            out.push_str(&format!(
                "<details><summary>{} in <code>{lock}</code> not exercised by this run</summary>\n\n",
                plural(unused.len(), "permission")
            ));
            out.push_str("| | Permission | Resource |\n|---|---|---|\n");
            for entry in unused.iter().take(MARKDOWN_MAX_ROWS) {
                out.push_str(&format!(
                    "| − | `{}` | `{}` |\n",
                    entry.action, entry.resource
                ));
            }
            if unused.len() > MARKDOWN_MAX_ROWS {
                out.push_str(&format!(
                    "\n_… and {} more in `{lock}`._\n",
                    unused.len() - MARKDOWN_MAX_ROWS
                ));
            }
            out.push_str("\n</details>\n\n");
        }
        if !self.unmapped.is_empty() {
            let total: usize = self.unmapped.values().sum();
            let names: Vec<String> = self
                .unmapped
                .keys()
                .map(|service| format!("`{service}`"))
                .collect();
            out.push_str(&format!(
                "> **Note:** {} ({}) had no recognizable AWS operation and {} not included in `lenslet.lock`.\n\n",
                plural(total, "call"),
                names.join(", "),
                if total == 1 { "is" } else { "are" }
            ));
        }
        out.push_str(&format!(
            "<sub>Lenslet {} · observed access from this run, not a security review</sub>\n",
            env!("CARGO_PKG_VERSION")
        ));
        out
    }
}

/// `  ✗ SQS SendMessage  400  Code ×2  message` and its cause.
fn failure_lines(failure: &FailureFinding) -> String {
    let mut out = format!(
        "  ✗ {} {}  {}  {}{}",
        failure.service.to_ascii_uppercase(),
        failure.operation,
        failure.status,
        failure.code,
        times(failure.calls.len())
    );
    if let Some(message) = failure.message.as_deref() {
        out.push_str("  ");
        out.push_str(&one_line(message, 120));
    }
    out.push('\n');
    if let Some(hint) = failure.hint.as_deref() {
        out.push_str(&format!("      → {hint}\n"));
    }
    out
}

/// `~/.lenslet/runs/x.jsonl` for a path under `home`.
pub fn tilde(path: &str, home: &Path) -> String {
    match Path::new(path).strip_prefix(home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.to_string(),
    }
}

fn times(count: usize) -> String {
    if count > 1 {
        format!(" ×{count}")
    } else {
        String::new()
    }
}

pub fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

fn seconds(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", ms as f64 / 1000.0)
    }
}

fn one_line(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let mut cut: String = flat.chars().take(max - 1).collect();
        cut.push('…');
        cut
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{access::CallAccess, problem, trace::BodyTrace};

    fn trace(id: &str, service: &str, status: u16, body: Option<&str>) -> Trace {
        Trace {
            id: id.to_string(),
            at: "2026-11-15T00:00:00+00:00".to_string(),
            method: "POST".to_string(),
            path: "/".to_string(),
            host: "127.0.0.1".to_string(),
            service: service.to_string(),
            operation: "SendMessage".to_string(),
            status,
            ms: 3,
            authorization: true,
            problem: problem::from_response(status, &[], body),
            access: vec![CallAccess {
                action: format!("{service}:SendMessage"),
                resource: format!("arn:aws:{service}:*:*:jobs"),
                new: false,
                unverified: false,
            }],
            ..Default::default()
        }
    }

    fn access(action: &str, resource: &str) -> Access {
        Access::new(action, resource)
    }

    fn lock(entries: &[Access]) -> Lock {
        Lock {
            access: entries.iter().cloned().collect(),
            ..Lock::default()
        }
    }

    fn report(summary: &Summary, locked: Option<&Lock>) -> Report {
        let mut report = Report {
            version: crate::trace::VERSION,
            run: "r2".into(),
            command: vec!["npm".into(), "test".into()],
            duration_ms: 1500,
            emulator: "moto (started by Lenslet)".into(),
            trace: "/home/u/.lenslet/runs/r2.jsonl".into(),
            log: Some("/home/u/.lenslet/runs/r2.log".into()),
            ..Report::default()
        };
        report.tally(summary, locked.map(|lock| ("lenslet.lock", lock)));
        report
    }

    #[test]
    fn tallies_calls_errors_and_access_against_the_lock() {
        let mut summary = Summary::default();
        summary.observe(&trace("a", "sqs", 200, None));
        summary.observe(&trace(
            "b",
            "sqs",
            400,
            Some(r#"{"__type":"InvalidParameterValue","message":"bad\nbody"}"#),
        ));
        let mut s3 = trace("c", "s3", 200, None);
        s3.access[0].new = true;
        summary.observe(&s3);
        assert_eq!((summary.calls, summary.errors), (3, 1));
        let sqs = &summary.access[&access("sqs:SendMessage", "arn:aws:sqs:*:*:jobs")];
        assert_eq!((sqs.calls, sqs.errors, sqs.implied), (2, 1, false));

        let locked = lock(&[
            access("sqs:SendMessage", "arn:aws:sqs:*:*:jobs"),
            access("sqs:DeleteMessage", "arn:aws:sqs:*:*:jobs"),
        ]);
        let report = report(&summary, Some(&locked));
        let added: Vec<&str> = report.added().map(|e| e.resource.as_str()).collect();
        assert_eq!(added, ["arn:aws:s3:*:*:jobs"]);
        let unused: Vec<&str> = report.unused().map(|e| e.action.as_str()).collect();
        assert_eq!(unused, ["sqs:DeleteMessage"]);
        assert_eq!(report.findings.len(), 2, "{:?}", report.findings);
        assert!(matches!(report.findings[0], Finding::Failure(_)));
        let Finding::Access(new) = &report.findings[1] else {
            panic!("{:?}", report.findings[1]);
        };
        assert_eq!(new.title, "s3:SendMessage on jobs not in lenslet.lock");
        assert_eq!(new.calls, ["c"]);
        assert_eq!(new.detail.as_deref(), Some("Needed by S3 SendMessage."));

        // The JSON has no derived duplicates of the access table.
        let json = serde_json::to_value(&report).unwrap();
        for gone in [
            "failures",
            "access_added",
            "access_unused",
            "retries",
            "policy",
        ] {
            assert!(json.get(gone).is_none(), "{gone}");
        }
        assert_eq!(json["findings"][0]["kind"], "failure");
        assert_eq!(json["findings"][1]["kind"], "access");
        assert_eq!(json["access"][2]["unused"], true);
    }

    #[test]
    fn failures_group_by_kind_and_recovered_attempts_are_not_failures() {
        let mut summary = Summary::default();
        let mut first = trace("a1", "sqs", 500, Some(r#"{"__type":"InternalError"}"#));
        first.invocation_id = Some("inv-1".into());
        let mut retry = trace("a2", "sqs", 200, None);
        retry.invocation_id = Some("inv-1".into());
        retry.attempt = Some(2);
        summary.observe(&first);
        summary.observe(&retry);
        for id in ["b1", "b2"] {
            summary.observe(&trace(
                id,
                "sqs",
                400,
                Some(r#"{"__type":"QueueDoesNotExist","message":"no queue"}"#),
            ));
        }
        let findings = summary.findings("lenslet.lock");
        assert_eq!(findings.len(), 1, "{findings:?}");
        let Finding::Failure(failure) = &findings[0] else {
            panic!()
        };
        assert_eq!(failure.title, "SQS SendMessage → QueueDoesNotExist ×2");
        assert_eq!(failure.calls, ["b1", "b2"]);
        assert_eq!(failure.status, 400);
        assert_eq!(failure.message.as_deref(), Some("no queue"));
        assert_eq!(
            summary.errors, 3,
            "every failed call still counts as an error"
        );
    }

    #[test]
    fn compares_failures_with_the_previous_run() {
        let mut summary = Summary::default();
        summary.observe(&trace(
            "a",
            "sqs",
            400,
            Some(r#"{"__type":"QueueDoesNotExist"}"#),
        ));
        let mut now = report(&summary, None);
        let mut then_summary = Summary::default();
        then_summary.observe(&trace("x", "s3", 404, Some("<Code>NoSuchKey</Code>")));
        let then = report(&then_summary, None);
        now.compare_with(&then);
        let regressions = now.regressions.clone().unwrap();
        assert_eq!(regressions.new_failures.len(), 1);
        assert_eq!(regressions.fixed[0].code, "NoSuchKey");
        let text = now.render(Path::new("/home/u"), false);
        assert!(
            text.contains("since run r2: 1 new failure, 1 fixed"),
            "{text}"
        );
        assert!(now.markdown().contains("**new**"));
    }

    #[test]
    fn the_terminal_report_says_what_answered_where_the_output_is_and_how_to_lock() {
        let mut summary = Summary::default();
        let mut unmapped = trace("u1", "custom-svc", 200, None);
        unmapped.operation = "POST".into();
        unmapped.access.clear();
        summary.observe(&trace("a", "sqs", 200, None));
        summary.observe(&unmapped);
        let report = report(&summary, None);
        let text = report.render(Path::new("/home/u"), false);
        let first = text.lines().next().unwrap();
        assert!(
            first.ends_with(" · against moto (started by Lenslet)"),
            "{first}"
        );
        assert!(text.contains("access: 1 permission observed · `lenslet run --update"));
        assert!(
            text.contains("note: 1 call (custom-svc) had no recognizable operation"),
            "{text}"
        );
        assert!(
            text.ends_with(
                "lenslet: trace ~/.lenslet/runs/r2.jsonl · output ~/.lenslet/runs/r2.log · `lenslet ui` to inspect\n"
            ),
            "{text}"
        );
        let nothing = super::tests::report(&Summary::default(), None);
        assert!(
            nothing
                .render(Path::new("/home/u"), false)
                .contains("no AWS calls reached Lenslet")
        );
    }

    #[test]
    fn findings_lead_the_summary_and_the_pr_comment_without_repeats() {
        let mut summary = Summary::default();
        for id in ["a", "b"] {
            let mut failed = trace(
                id,
                "sqs",
                400,
                Some(r#"{"__type":"QueueDoesNotExist","message":"no | queue"}"#),
            );
            failed.access[0].new = true;
            failed.access[0].unverified = true;
            failed.access[0].resource = "*".into();
            summary.observe(&failed);
        }
        let mut report = report(&summary, Some(&lock(&[access("s3:GetObject", "*")])));
        report.exit_code = Some(1);

        let text = report.render(Path::new("/home/u"), false);
        assert!(
            text.contains("✗ SQS SendMessage  400  QueueDoesNotExist ×2  no | queue"),
            "{text}"
        );
        assert_eq!(text.matches("QueueDoesNotExist").count(), 1, "{text}");
        assert!(
            text.contains("+ sqs:SendMessage                  *  (resource unknown: request body not readable)"),
            "{text}"
        );
        assert!(text.contains("unused: 1 permission in lenslet.lock not exercised by this run"));
        assert!(text.contains("- s3:GetObject"));

        let md = report.markdown();
        assert!(
            md.starts_with("### Lenslet · `npm test` **failed** (exit 1)"),
            "{md}"
        );
        let failures_at = md.find("| | Failure | Why |").expect(&md);
        assert!(
            failures_at < md.find("not in `lenslet.lock`").unwrap(),
            "{md}"
        );
        assert!(
            md.contains(
                "| ✗ | SQS SendMessage → QueueDoesNotExist ×2 | no \\| queue — The resource"
            ),
            "{md}"
        );
        assert!(
            md.contains("**1 permission not in `lenslet.lock`.** Review them; if they are intended, run `lenslet run --update`"),
            "{md}"
        );
        assert!(
            md.contains("| + | `sqs:SendMessage` | `*` | SQS SendMessage (resource unknown: request body not readable) |"),
            "{md}"
        );
        assert!(md.contains("| − | `s3:GetObject`"), "{md}");
        assert!(
            md.contains("1 AWS call") || md.contains("2 AWS calls"),
            "{md}"
        );
    }

    #[test]
    fn causal_tracker_explains_region_mismatch_and_prior_deletion() {
        let mut tracker = CausalTracker::default();
        let body = Some(BodyTrace {
            text: Some(r#"{"TableName":"orders"}"#.into()),
            bytes: 22,
            ..Default::default()
        });
        let mut create = trace("c1", "dynamodb", 200, None);
        create.operation = "CreateTable".into();
        create.region = Some("us-west-2".into());
        create.request.body = body.clone();
        let concrete = access::derive_call(&create).concrete_own;
        tracker.observe_and_enrich(&mut create, &concrete, 1);

        let mut get = trace(
            "c2",
            "dynamodb",
            400,
            Some(r#"{"__type":"ResourceNotFoundException","message":"not found"}"#),
        );
        get.operation = "GetItem".into();
        get.region = Some("us-east-1".into());
        get.request.body = body;
        let concrete = access::derive_call(&get).concrete_own;
        assert!(tracker.observe_and_enrich(&mut get, &concrete, 2));
        let hint = get.problem.as_ref().unwrap().hint.clone().unwrap();
        assert!(
            hint.contains("Region mismatch")
                && hint.contains("us-west-2")
                && hint.contains("call #1"),
            "{hint}"
        );

        let mut delete = trace("c3", "s3", 204, None);
        delete.operation = "DeleteBucket".into();
        delete.path = "/receipts".into();
        let concrete = access::derive_call(&delete).concrete_own;
        tracker.observe_and_enrich(&mut delete, &concrete, 3);
        let mut put = trace(
            "c4",
            "s3",
            404,
            Some("<Error><Code>NoSuchBucket</Code><Message>gone</Message></Error>"),
        );
        put.operation = "PutObject".into();
        put.path = "/receipts/a.txt".into();
        let concrete = access::derive_call(&put).concrete_own;
        tracker.observe_and_enrich(&mut put, &concrete, 4);
        let hint = put.problem.as_ref().unwrap().hint.clone().unwrap();
        assert!(
            hint.contains("s3://receipts")
                && hint.contains("deleted earlier")
                && hint.contains("call #3"),
            "{hint}"
        );
    }
}
