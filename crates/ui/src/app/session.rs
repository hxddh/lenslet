//! The run being shown and everything derived from it.
//!
//! `Session` is a model entity: it owns the call store, the run header and
//! report, the lock, the failure findings, the output log and the run list.
//! All IO happens on the background executor; results are applied with
//! `this.update`, guarded by a generation counter so a stale load (the user
//! already switched runs) is dropped. Views subscribe to [`SessionEvent`]s.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use futures::StreamExt;
use gpui::{Context, EventEmitter, Task};

use crate::{
    feed::{self, FeedMsg, LoadResult, Loaded, RunGroup, RunSummary, Source},
    i18n::Tr,
    lockfile,
    model::{
        self, Access, Finding, LockView, RunInfo, Trace, lock_path, project_dir, report_access,
    },
    store::{AccessRow, CallStore, access_rows, not_in_lock_count},
    tr,
};

/// Live findings are recomputed at most this often.
const FINDINGS_THROTTLE: Duration = Duration::from_millis(1_500);
/// How often a session that follows the newest run looks for a newer one,
/// and a live run's log is re-read.
const POLL: Duration = Duration::from_millis(1_000);
/// Live batches are applied at most once per frame.
const FRAME: Duration = Duration::from_millis(16);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    Loading,
    /// No run file yet.
    Missing,
    Error(Tr),
    Ready,
}

#[derive(Clone, Debug)]
pub enum SessionEvent {
    /// A different run (or the first one) is shown; everything was replaced.
    RunChanged,
    /// Rows were appended or moved.
    Rows(crate::store::Delta),
    /// The lock, access rows or findings were recomputed.
    Derived,
    /// The run list was refreshed.
    Runs,
    /// The output log changed.
    Log,
    Toast(Tr),
}

pub struct Session {
    pub source: Source,
    generation: u64,
    pub health: Health,
    pub path: Option<PathBuf>,
    pub run: Option<RunInfo>,
    pub store: CallStore,
    pub skipped: usize,
    pub evicted: usize,
    /// The folder to use when no run names one (the opened project).
    pub project: Option<PathBuf>,
    feed_task: Option<Task<()>>,
    log_task: Option<Task<()>>,
    _watch_task: Option<Task<()>>,

    pub lock: LockView,
    pub access_rows: Arc<Vec<AccessRow>>,
    pub not_in_lock: usize,
    /// The run's failure findings (the report's, or computed while live).
    pub failures: Arc<Vec<Finding>>,
    findings_running: bool,
    findings_dirty: bool,
    findings_at: Option<Instant>,

    pub log: Arc<str>,
    pub log_path: Option<PathBuf>,

    pub runs: Arc<Vec<RunSummary>>,
    pub run_groups: Arc<Vec<RunGroup>>,
}

impl EventEmitter<SessionEvent> for Session {}

impl Session {
    pub fn new(source: Source, project: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let mut session = Session {
            source,
            generation: 0,
            health: Health::Loading,
            path: None,
            run: None,
            store: CallStore::default(),
            skipped: 0,
            evicted: 0,
            project,
            feed_task: None,
            log_task: None,
            _watch_task: None,
            lock: LockView::default(),
            access_rows: Arc::new(Vec::new()),
            not_in_lock: 0,
            failures: Arc::new(Vec::new()),
            findings_running: false,
            findings_dirty: false,
            findings_at: None,
            log: Arc::from(""),
            log_path: None,
            runs: Arc::new(Vec::new()),
            run_groups: Arc::new(Vec::new()),
        };
        session.reload(cx);
        session._watch_task = Some(session.watch_newest(cx));
        session
    }

    pub fn header_endpoint(&self) -> Option<&str> {
        self.run
            .as_ref()
            .map(|run| run.header.endpoint.as_str())
            .filter(|endpoint| !endpoint.is_empty())
    }

    /// The project folder: the shown run's, else the opened one.
    pub fn project_dir(&self) -> Option<PathBuf> {
        self.run
            .as_ref()
            .and_then(|run| project_dir(&run.header))
            .or_else(|| self.project.clone())
    }

    pub fn set_project(&mut self, dir: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.project != dir {
            self.project = dir;
            self.refresh_lock(cx);
            cx.notify();
        }
    }

    // -- Loading -----------------------------------------------------------

    /// Load the source again from scratch (off the main thread).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.feed_task = None;
        self.log_task = None;
        self.health = Health::Loading;
        cx.notify();
        let source = self.source.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { feed::load_source(&source) })
                .await;
            this.update(cx, |session, cx| {
                if session.generation == generation {
                    session.apply_load(result, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Show another source (a picked run, or back to following the newest).
    pub fn switch(&mut self, source: Source, cx: &mut Context<Self>) {
        self.source = source;
        self.clear();
        cx.emit(SessionEvent::RunChanged);
        self.reload(cx);
    }

    fn clear(&mut self) {
        self.store.reset(Vec::new(), 0);
        self.run = None;
        self.path = None;
        self.failures = Arc::new(Vec::new());
        self.access_rows = Arc::new(Vec::new());
        self.not_in_lock = 0;
        self.log = Arc::from("");
        self.log_path = None;
    }

    fn apply_load(&mut self, result: LoadResult, cx: &mut Context<Self>) {
        match result {
            LoadResult::Missing => {
                self.clear();
                self.health = Health::Missing;
                self.refresh_lock(cx);
                cx.emit(SessionEvent::RunChanged);
            }
            LoadResult::Error(error) => {
                self.clear();
                self.health = Health::Error(error);
                cx.emit(SessionEvent::RunChanged);
            }
            LoadResult::Loaded(loaded) => {
                let tail = loaded.tail.clone();
                let path = loaded.path.clone();
                self.install_loaded(*loaded, cx);
                crate::fsguard::milestone_once("run loaded and merged");
                self.start_feed(path, tail, cx);
            }
        }
        self.refresh_runs(cx);
        cx.notify();
    }

    fn install_loaded(&mut self, loaded: Loaded, cx: &mut Context<Self>) {
        let path_changed = self.path.as_ref() != Some(&loaded.path);
        let run = RunInfo {
            header: loaded.header.unwrap_or_default(),
            report: loaded.report,
        };
        self.log_path = Some(model::log_path(&run, &loaded.path));
        self.path = Some(loaded.path);
        self.skipped = loaded.skipped;
        self.evicted = loaded.evicted;
        self.run = Some(run);
        self.store.reset(loaded.calls, loaded.evicted);
        self.health = Health::Ready;
        self.failures = Arc::new(Vec::new());
        self.findings_at = None;
        if path_changed {
            self.log = Arc::from("");
        }
        cx.emit(SessionEvent::RunChanged);
        self.refresh_lock(cx);
        self.refresh_log(None, cx);
        self.recompute(cx);
        self.schedule_findings(cx);
    }

    /// Keep a live run current with the file tail, and its log with a poll.
    fn start_feed(&mut self, path: PathBuf, tail: feed::TailState, cx: &mut Context<Self>) {
        if self.run.as_ref().is_none_or(|run| run.report.is_some()) {
            return;
        }
        let generation = self.generation;
        self.log_task = Some(self.poll_log(generation, cx));
        let mut rx = feed::start(path, tail);
        self.feed_task = Some(cx.spawn(async move |this, cx| {
            while let Some(first) = rx.next().await {
                let mut batch = vec![first];
                while let Ok(more) = rx.try_recv() {
                    batch.push(more);
                }
                let alive = this.update(cx, |session, cx| {
                    if session.generation != generation {
                        return false;
                    }
                    session.apply_feed(batch, cx);
                    true
                });
                if !matches!(alive, Ok(true)) {
                    break;
                }
                cx.background_executor().timer(FRAME).await;
            }
        }));
    }

    /// Apply one frame's worth of tail messages.
    fn apply_feed(&mut self, batch: Vec<FeedMsg>, cx: &mut Context<Self>) {
        let mut calls: Vec<Trace> = Vec::new();
        let mut ended = false;
        for msg in batch {
            match msg {
                FeedMsg::Calls(more) => calls.extend(more),
                FeedMsg::Header(header) => {
                    if let Some(run) = self.run.as_mut() {
                        run.header = *header;
                    }
                }
                FeedMsg::End(report) => {
                    if let Some(run) = self.run.as_mut() {
                        run.report = Some(*report);
                    }
                    ended = true;
                }
                FeedMsg::Reset(loaded) => {
                    calls.clear();
                    self.install_loaded(*loaded, cx);
                }
                FeedMsg::Error(error) => {
                    self.health = Health::Error(error);
                    cx.notify();
                }
            }
        }
        if !calls.is_empty() {
            let delta = self.store.ingest(calls);
            self.evicted = self.evicted.max(self.store.evicted_total());
            if delta.changed {
                cx.emit(SessionEvent::Rows(delta));
                self.recompute(cx);
                self.schedule_findings(cx);
            }
        }
        if ended {
            self.feed_task = None;
            self.log_task = None;
            self.refresh_log(None, cx);
            self.refresh_runs(cx);
            self.refresh_lock(cx);
            self.findings_at = None;
            self.schedule_findings(cx);
            cx.notify();
        }
    }

    // -- The output log ------------------------------------------------------

    /// Re-read the log (only when its length changed) in the background.
    fn refresh_log(&mut self, known_len: Option<u64>, cx: &mut Context<Self>) {
        let Some(path) = self.log_path.clone() else {
            return;
        };
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let read = cx
                .background_executor()
                .spawn(async move { feed::read_log(&path, known_len) })
                .await;
            this.update(cx, |session, cx| {
                if session.generation == generation
                    && let Some((_, Some(text))) = read
                    && *session.log != *text
                {
                    session.log = Arc::from(text);
                    cx.emit(SessionEvent::Log);
                }
            })
            .ok();
        })
        .detach();
    }

    fn poll_log(&self, generation: u64, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let mut known: Option<u64> = None;
            loop {
                cx.background_executor().timer(POLL).await;
                let Ok(Some(path)) = this.update(cx, |session, _| {
                    (session.generation == generation)
                        .then(|| session.log_path.clone())
                        .flatten()
                }) else {
                    break;
                };
                let read = cx
                    .background_executor()
                    .spawn(async move { feed::read_log(&path, known) })
                    .await;
                let Some((len, text)) = read else {
                    continue;
                };
                known = Some(len);
                if let Some(text) = text {
                    let gone = this
                        .update(cx, |session, cx| {
                            if session.generation == generation {
                                session.log = Arc::from(text);
                                cx.emit(SessionEvent::Log);
                            }
                        })
                        .is_err();
                    if gone {
                        break;
                    }
                }
            }
        })
    }

    // -- Runs ----------------------------------------------------------------

    /// While following the newest run, switch when a newer one appears.
    fn watch_newest(&mut self, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                let Ok(dir) = this.update(cx, |session, _| {
                    session
                        .source
                        .follows_latest()
                        .then(|| session.source.runs_dir().map(Path::to_path_buf))
                        .flatten()
                }) else {
                    break;
                };
                let Some(dir) = dir else {
                    continue;
                };
                let newest = cx
                    .background_executor()
                    .spawn(async move { feed::newest_run(&dir) })
                    .await;
                let gone = this
                    .update(cx, |session, cx| {
                        let newer = newest.is_some()
                            && newest != session.path
                            && session.source.follows_latest()
                            && session.health != Health::Loading;
                        if newer {
                            session.reload(cx);
                        }
                    })
                    .is_err();
                if gone {
                    break;
                }
            }
        })
    }

    pub fn refresh_runs(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.source.runs_dir().map(Path::to_path_buf) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let (runs, groups) = cx
                .background_executor()
                .spawn(async move {
                    let runs = feed::list_runs(&dir);
                    let groups = feed::group_runs(&runs);
                    (runs, groups)
                })
                .await;
            this.update(cx, |session, cx| {
                session.runs = Arc::new(runs);
                session.run_groups = Arc::new(groups);
                cx.emit(SessionEvent::Runs);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Step to an older (`-1`) or newer (`+1`) run in the run list.
    pub fn step_run(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(dir) = self.source.runs_dir().map(Path::to_path_buf) else {
            return;
        };
        // `runs` is newest first; step through it oldest first.
        let paths: Vec<PathBuf> = self.runs.iter().rev().map(|r| r.path.clone()).collect();
        let Some(current) = self
            .path
            .as_ref()
            .and_then(|path| paths.iter().position(|p| p == path))
        else {
            return;
        };
        let target = (current as isize + delta).clamp(0, paths.len() as isize - 1) as usize;
        if target == current {
            return;
        }
        let pinned = (target + 1 < paths.len()).then(|| paths[target].clone());
        self.switch(Source::Runs { dir, pinned }, cx);
    }

    /// Follow the newest run in the runs directory.
    pub fn follow_newest(&mut self, cx: &mut Context<Self>) {
        let dir = self
            .source
            .runs_dir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| lenslet_core::runs::dir(&super::home()));
        self.switch(Source::Runs { dir, pinned: None }, cx);
    }

    // -- The lock and what is derived from it ----------------------------------

    /// Re-read the lock file in the background.
    pub fn refresh_lock(&mut self, cx: &mut Context<Self>) {
        let path = self
            .run
            .as_ref()
            .and_then(|run| lock_path(&run.header))
            .or_else(|| self.project.as_deref().map(lenslet_core::lock::path_in));
        let Some(path) = path else {
            self.lock = LockView::default();
            self.recompute(cx);
            return;
        };
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let read_path = path.clone();
            let lock = cx
                .background_executor()
                .spawn(async move { lockfile::read(&read_path).ok().flatten() })
                .await;
            this.update(cx, |session, cx| {
                if session.generation == generation {
                    session.lock = LockView {
                        path: Some(path),
                        lock: lock.map(Arc::new),
                    };
                    session.recompute(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Recompute the access rows from what is in memory. No IO.
    pub fn recompute(&mut self, cx: &mut Context<Self>) {
        let used = match self.run.as_ref().and_then(|run| run.report.as_ref()) {
            Some(report) => report_access(report),
            None => self.store.access(),
        };
        let rows = access_rows(&used, &self.lock);
        self.not_in_lock = not_in_lock_count(&rows);
        self.access_rows = Arc::new(rows);
        cx.emit(SessionEvent::Derived);
        cx.notify();
    }

    /// Failure findings: the report's when the run ended, else computed from
    /// the calls off the main thread, at most every [`FINDINGS_THROTTLE`].
    pub fn schedule_findings(&mut self, cx: &mut Context<Self>) {
        if let Some(run) = self.run.as_ref().filter(|run| run.report.is_some()) {
            self.failures = Arc::new(run.failures().into_iter().cloned().collect());
            cx.emit(SessionEvent::Derived);
            return;
        }
        if self.findings_running {
            self.findings_dirty = true;
            return;
        }
        let wait = self
            .findings_at
            .map(|at| FINDINGS_THROTTLE.saturating_sub(at.elapsed()))
            .unwrap_or_default();
        self.findings_running = true;
        self.findings_dirty = false;
        let generation = self.generation;
        let rows = self.store.rows().clone();
        cx.spawn(async move |this, cx| {
            if !wait.is_zero() {
                cx.background_executor().timer(wait).await;
            }
            let failures = cx
                .background_executor()
                .spawn(async move {
                    // The CLI's own tally, so live findings match the report.
                    let mut summary = lenslet_core::summary::Summary::default();
                    for row in rows.iter() {
                        summary.observe(&row.core);
                    }
                    summary
                        .findings(lenslet_core::lock::FILE_NAME)
                        .into_iter()
                        .filter(|f| matches!(f, Finding::Failure(_)))
                        .collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |session, cx| {
                session.findings_running = false;
                session.findings_at = Some(Instant::now());
                if session.generation == generation
                    && session.run.as_ref().is_some_and(|r| r.report.is_none())
                {
                    session.failures = Arc::new(failures);
                    cx.emit(SessionEvent::Derived);
                    cx.notify();
                }
                if session.findings_dirty {
                    session.schedule_findings(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    // -- Lock edits ----------------------------------------------------------

    /// Where lock edits go: the run's lock, else the project's.
    pub fn lock_target(&self) -> Option<PathBuf> {
        self.lock.path.clone()
    }

    /// Accept permissions into the lock (`(access, test, implied)`; the lock
    /// itself records only `# test`).
    pub fn accept(&mut self, entries: Vec<(Access, bool, bool)>, cx: &mut Context<Self>) {
        let entries: Vec<(Access, bool)> = entries
            .into_iter()
            .map(|(access, test, _implied)| (access, test))
            .collect();
        let (Some(path), false) = (self.lock_target(), entries.is_empty()) else {
            return;
        };
        self.edit_lock(cx, move || {
            lockfile::accept(&path, &entries).map(|added| {
                tr!(
                    "Added {added} to lenslet.lock",
                    "已向 lenslet.lock 写入 {added} 项权限"
                )
            })
        });
    }

    pub fn remove_from_lock(&mut self, entries: Vec<Access>, cx: &mut Context<Self>) {
        let (Some(path), false) = (self.lock_target(), entries.is_empty()) else {
            return;
        };
        self.edit_lock(cx, move || {
            lockfile::remove(&path, &entries).map(|removed| {
                tr!(
                    "Removed {removed} from lenslet.lock",
                    "已从 lenslet.lock 移除 {removed} 项权限"
                )
            })
        });
    }

    pub fn set_test_mark(&mut self, access: Access, test: bool, cx: &mut Context<Self>) {
        let Some(path) = self.lock_target() else {
            return;
        };
        self.edit_lock(cx, move || {
            lockfile::set_test_mark(&path, &access, test).map(|_| {
                if test {
                    tr!(
                        "Marked `# test` in lenslet.lock",
                        "已在 lenslet.lock 标记 `# test`"
                    )
                } else {
                    tr!(
                        "Removed `# test` in lenslet.lock",
                        "已取消 lenslet.lock 中的 `# test`"
                    )
                }
            })
        });
    }

    fn edit_lock(
        &mut self,
        cx: &mut Context<Self>,
        edit: impl FnOnce() -> anyhow::Result<Tr> + Send + 'static,
    ) {
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { edit() }).await;
            this.update(cx, |session, cx| {
                let message = result
                    .unwrap_or_else(|error| tr!("lenslet.lock: {error}", "lenslet.lock：{error}"));
                cx.emit(SessionEvent::Toast(message));
                session.refresh_lock(cx);
            })
            .ok();
        })
        .detach();
    }
}
