//! The calls of one run, kept incrementally: rows in arrival order with an
//! id→index map, and the aggregates the window shows (counts, service
//! facets, the access table) maintained per append, so a live run costs
//! O(new lines) per batch.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use crate::model::{
    Access, CallFilter, LockView, MAX_ROWS, ServiceFacet, Trace, UsedAccess, matches_search,
    operation_label,
};

/// Counts over every row in the store.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub total: usize,
    pub errors: usize,
}

/// What one [`CallStore::ingest`] changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Delta {
    /// New rows landed only at the end, starting at this index, and nothing
    /// before them moved: a view can extend itself instead of recomputing.
    pub appended_from: Option<usize>,
    pub changed: bool,
}

#[derive(Clone, Debug, Default)]
pub struct CallStore {
    rows: Arc<Vec<Arc<Trace>>>,
    index: HashMap<String, usize>,
    stats: Stats,
    services: BTreeMap<String, (String, usize)>,
    /// Every permission seen this run, even on rows evicted past [`MAX_ROWS`].
    access: BTreeMap<Access, UsedAccess>,
    evicted_total: usize,
}

fn order_key(trace: &Trace) -> (i64, &str) {
    (trace.at_ms().unwrap_or(i64::MAX), trace.at.as_str())
}

impl CallStore {
    pub fn rows(&self) -> &Arc<Vec<Arc<Trace>>> {
        &self.rows
    }

    #[cfg(test)]
    pub fn position(&self, id: &str) -> Option<usize> {
        self.index.get(id).copied()
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn evicted_total(&self) -> usize {
        self.evicted_total
    }

    /// Services present, sorted by label.
    pub fn facets(&self) -> Vec<ServiceFacet> {
        let mut facets: Vec<ServiceFacet> = self
            .services
            .iter()
            .filter(|(_, (_, count))| *count > 0)
            .map(|(id, (label, count))| ServiceFacet {
                id: id.clone(),
                label: label.clone(),
                count: *count,
            })
            .collect();
        facets.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.id.cmp(&b.id)));
        facets
    }

    /// The permissions used so far (for a run that has no report yet).
    pub fn access(&self) -> Vec<UsedAccess> {
        self.access.values().cloned().collect()
    }

    fn count(&mut self, row: &Trace, add: bool) {
        let step = |value: &mut usize, on: bool| {
            if on {
                *value = if add {
                    *value + 1
                } else {
                    value.saturating_sub(1)
                };
            }
        };
        step(&mut self.stats.total, true);
        step(&mut self.stats.errors, row.is_error());
        let facet = self
            .services
            .entry(row.service_key().to_string())
            .or_insert_with(|| (row.service_name().to_string(), 0));
        step(&mut facet.1, true);
    }

    fn record_access(&mut self, row: &Trace) {
        let via = format!("{} {}", row.service_name(), operation_label(row));
        for (access, implied, unverified) in row.access_entries() {
            let entry = self
                .access
                .entry(access.clone())
                .or_insert_with(|| UsedAccess {
                    access,
                    implied: true,
                    ..UsedAccess::default()
                });
            entry.calls += 1;
            entry.errors += usize::from(row.is_error());
            entry.implied &= implied;
            entry.unverified |= unverified;
            if entry.via.len() < 3 && !entry.via.contains(&via) {
                entry.via.push(via.clone());
            }
        }
    }

    /// Replace everything with `rows` (a fresh load).
    pub fn reset(&mut self, rows: Vec<Trace>, prior_evicted: usize) -> Delta {
        *self = CallStore {
            evicted_total: prior_evicted,
            ..CallStore::default()
        };
        let mut delta = self.ingest(rows);
        delta.appended_from = None;
        delta.changed = true;
        delta
    }

    /// Fold new lines in, in request-arrival order; a repeated id is
    /// ignored. Only the newest [`MAX_ROWS`] rows are kept.
    pub fn ingest(&mut self, incoming: Vec<Trace>) -> Delta {
        let mut delta = Delta {
            appended_from: Some(self.rows.len()),
            ..Delta::default()
        };
        let mut reorder_from: Option<usize> = None;
        for trace in incoming {
            if self.index.contains_key(&trace.id) {
                continue;
            }
            self.count(&trace, true);
            self.record_access(&trace);
            let rows = Arc::make_mut(&mut self.rows);
            let key = order_key(&trace);
            let mut pos = rows.len();
            while pos > 0 && order_key(&rows[pos - 1]) > key {
                pos -= 1;
            }
            self.index.insert(trace.id.clone(), pos);
            if pos == rows.len() {
                rows.push(Arc::new(trace));
            } else {
                rows.insert(pos, Arc::new(trace));
                reorder_from = Some(reorder_from.map_or(pos, |from| from.min(pos)));
            }
            delta.changed = true;
        }
        let mut reindex_from = reorder_from;
        if self.rows.len() > MAX_ROWS {
            let excess = self.rows.len() - MAX_ROWS;
            self.evicted_total = self.evicted_total.saturating_add(excess);
            let evicted: Vec<Arc<Trace>> = Arc::make_mut(&mut self.rows).drain(..excess).collect();
            for row in &evicted {
                self.count(row, false);
                self.index.remove(&row.id);
            }
            reindex_from = Some(0);
        }
        if let Some(from) = reindex_from {
            for (index, row) in self.rows.iter().enumerate().skip(from) {
                self.index.insert(row.id.clone(), index);
            }
            delta.appended_from = None;
        }
        delta
    }
}

// ---------------------------------------------------------------------------
// The calls view
// ---------------------------------------------------------------------------

/// Everything a calls view filter depends on, cheap to clone into a task.
#[derive(Clone, Debug, Default)]
pub struct ViewQuery {
    pub filter: CallFilter,
    pub tokens: Vec<String>,
    pub lock: LockView,
}

impl ViewQuery {
    pub fn matches(&self, trace: &Trace) -> bool {
        self.filter.matches(trace, &self.lock) && matches_search(trace, &self.tokens)
    }
}

/// Indices of the rows that pass `query`.
pub fn filter_rows(rows: &[Arc<Trace>], query: &ViewQuery) -> Vec<u32> {
    rows.iter()
        .enumerate()
        .filter(|(_, row)| query.matches(row))
        .map(|(index, _)| index as u32)
        .collect()
}

/// Above this many rows a search runs on the background executor.
pub const BACKGROUND_SEARCH_ROWS: usize = 1_000;

// ---------------------------------------------------------------------------
// The access table
// ---------------------------------------------------------------------------

/// Where a permission stands against lenslet.lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockState {
    /// Used by this run and in the lock.
    InLock,
    /// Used by this run, not in the lock (with no lock file: every one).
    NotInLock,
    /// In the lock, not used by this run.
    Unused,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessRow {
    pub used: UsedAccess,
    pub test: bool,
    pub state: LockState,
}

impl AccessRow {
    pub fn access(&self) -> &Access {
        &self.used.access
    }

    pub fn in_lock(&self) -> bool {
        self.state != LockState::NotInLock
    }

    pub fn matches_search(&self, tokens: &[String]) -> bool {
        if tokens.is_empty() {
            return true;
        }
        let mut hay =
            format!("{} {}", self.used.access.action, self.used.access.resource).to_lowercase();
        for via in &self.used.via {
            hay.push(' ');
            hay.push_str(&via.to_lowercase());
        }
        tokens.iter().all(|token| hay.contains(token.as_str()))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AccessFilter {
    #[default]
    All,
    NotInLock,
    Unused,
    Test,
}

impl AccessFilter {
    pub fn matches(self, row: &AccessRow) -> bool {
        match self {
            AccessFilter::All => true,
            AccessFilter::NotInLock => row.state == LockState::NotInLock,
            AccessFilter::Unused => row.state == LockState::Unused,
            AccessFilter::Test => row.test,
        }
    }
}

/// This run's permissions against the lock — the one place in-lock state is
/// decided. Used permissions first (not in lock, then in lock), then the
/// lock's unused lines.
pub fn access_rows(used: &[UsedAccess], lock: &LockView) -> Vec<AccessRow> {
    let mut rows: BTreeMap<Access, AccessRow> = BTreeMap::new();
    for entry in used {
        let state = if lock.contains(&entry.access) {
            LockState::InLock
        } else {
            LockState::NotInLock
        };
        rows.insert(
            entry.access.clone(),
            AccessRow {
                test: lock.is_test(&entry.access),
                used: entry.clone(),
                state,
            },
        );
    }
    for access in lock.locked() {
        rows.entry(access.clone()).or_insert_with(|| AccessRow {
            test: lock.is_test(&access),
            used: UsedAccess {
                access,
                ..UsedAccess::default()
            },
            state: LockState::Unused,
        });
    }
    let mut rows: Vec<AccessRow> = rows.into_values().collect();
    rows.sort_by_key(|row| match row.state {
        LockState::NotInLock => 0,
        LockState::InLock => 1,
        LockState::Unused => 2,
    });
    rows
}

/// How many used permissions are not in the lock.
pub fn not_in_lock_count(rows: &[AccessRow]) -> usize {
    rows.iter()
        .filter(|row| row.state == LockState::NotInLock)
        .count()
}

/// The IAM policy for what this run used, minus `# test` lines.
pub fn policy_json(rows: &[AccessRow]) -> String {
    let inputs: Vec<lenslet_core::iac::PolicyInput> = rows
        .iter()
        .filter(|row| row.state != LockState::Unused && !row.test)
        .map(|row| lenslet_core::iac::PolicyInput {
            access: row.used.access.clone(),
            implied: row.used.implied,
            services: Vec::new(),
        })
        .collect();
    lenslet_core::iac::compile(&inputs, &lenslet_core::iac::CompileOptions::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{StatusFilter, parse_record, search_tokens};

    fn row(id: &str, at_ms: u64, status: u16, extra: &str) -> Trace {
        let secs = at_ms / 1000;
        let line = format!(
            r#"{{"type":"call","id":"{id}","at":"2026-11-15T09:{:02}:{:02}.{:03}+00:00","method":"POST","path":"/","host":"h","service":"dynamodb","operation":"GetItem","status":{status},"ms":3,"authorization":true,"request":{{"headers":[]}},"response":{{"headers":[]}}{extra}}}"#,
            (secs / 60) % 60,
            secs % 60,
            at_ms % 1000
        );
        match parse_record(&line) {
            Some(crate::model::Record::Call(call)) => *call,
            _ => panic!("bad test line {line}"),
        }
    }

    fn lock(text: &str) -> LockView {
        LockView {
            path: Some("lenslet.lock".into()),
            lock: Some(Arc::new(lenslet_core::lock::parse(text))),
        }
    }

    #[test]
    fn appends_are_incremental_and_repeats_are_ignored() {
        let mut store = CallStore::default();
        let delta = store.ingest(vec![row("a", 0, 200, ""), row("b", 10, 500, "")]);
        assert_eq!(delta.appended_from, Some(0));
        assert_eq!(store.stats().errors, 1);
        assert!(!store.ingest(vec![row("a", 0, 200, "")]).changed);
        let delta = store.ingest(vec![row("c", 20, 200, "")]);
        assert_eq!(delta.appended_from, Some(2));
        assert_eq!(store.position("c"), Some(2));
    }

    #[test]
    fn out_of_order_lines_keep_arrival_order_and_reindex() {
        let mut store = CallStore::default();
        store.ingest(vec![row("a", 0, 200, ""), row("c", 30, 200, "")]);
        let delta = store.ingest(vec![row("b", 10, 200, "")]);
        assert_eq!(delta.appended_from, None);
        let ids: Vec<&str> = store.rows().iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(store.position("c"), Some(2));
    }

    #[test]
    fn keeps_only_the_newest_rows_but_every_permission() {
        let mut store = CallStore::default();
        let batch: Vec<Trace> = (0..MAX_ROWS as u64 + 10)
            .map(|i| {
                row(
                    &format!("id{i}"),
                    i,
                    if i < 10 { 500 } else { 200 },
                    r#","access":[{"action":"dynamodb:GetItem","resource":"t"}]"#,
                )
            })
            .collect();
        store.ingest(batch);
        assert_eq!(store.rows().len(), MAX_ROWS);
        assert_eq!(store.stats().errors, 0, "the failing calls were evicted");
        assert_eq!(store.position("id10"), Some(0));
        assert_eq!(store.access()[0].calls, MAX_ROWS + 10);
    }

    #[test]
    fn filters_compose_with_the_lock() {
        let mut store = CallStore::default();
        store.ingest(vec![
            row(
                "ok",
                0,
                200,
                r#","access":[{"action":"s3:GetObject","resource":"b/*"}]"#,
            ),
            row(
                "bad",
                1,
                404,
                r#","problem":{"code":"NoSuchKey"},"access":[{"action":"s3:PutObject","resource":"b/*"}]"#,
            ),
        ]);
        let mut query = ViewQuery {
            filter: CallFilter {
                status: StatusFilter::Errors,
                ..CallFilter::default()
            },
            ..ViewQuery::default()
        };
        assert_eq!(filter_rows(store.rows(), &query), vec![1]);
        query.filter.status = StatusFilter::NotInLock;
        assert_eq!(
            filter_rows(store.rows(), &query),
            vec![0, 1],
            "no lock file"
        );
        query.lock = lock("s3:GetObject b/*\n");
        assert_eq!(filter_rows(store.rows(), &query), vec![1]);
        query.filter.status = StatusFilter::All;
        query.tokens = search_tokens("nosuchkey");
        assert_eq!(filter_rows(store.rows(), &query), vec![1]);
    }

    #[test]
    fn in_lock_state_is_consistent_with_and_without_a_lock_file() {
        let used = vec![
            UsedAccess {
                access: Access::new("s3:GetObject", "b/*"),
                calls: 2,
                ..UsedAccess::default()
            },
            UsedAccess {
                access: Access::new("s3:PutObject", "b/*"),
                calls: 1,
                ..UsedAccess::default()
            },
        ];
        // No lock file: every used permission is not in the lock.
        let rows = access_rows(&used, &LockView::default());
        assert!(rows.iter().all(|r| r.state == LockState::NotInLock));
        assert!(rows.iter().all(|r| !r.in_lock()));
        assert_eq!(not_in_lock_count(&rows), 2);

        let lock = lock("s3:GetObject b/*\nsqs:SendMessage q  # test\n");
        let rows = access_rows(&used, &lock);
        let states: Vec<(&str, LockState)> = rows
            .iter()
            .map(|r| (r.access().action.as_str(), r.state))
            .collect();
        assert_eq!(
            states,
            [
                ("s3:PutObject", LockState::NotInLock),
                ("s3:GetObject", LockState::InLock),
                ("sqs:SendMessage", LockState::Unused),
            ]
        );
        assert!(rows[2].test);
        assert_eq!(not_in_lock_count(&rows), 1);
        let policy = policy_json(&rows);
        assert!(policy.contains("s3:GetObject") && !policy.contains("sqs:SendMessage"));
    }
}
