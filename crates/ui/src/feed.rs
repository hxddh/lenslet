//! Getting a run into the window, off the main thread.
//!
//! [`load`] reads the newest [`MAX_ROWS`] lines of a run file once (one
//! backwards scan). [`start`] then keeps a live run current from a background
//! thread with an incremental tail of the file: only complete lines are
//! parsed, there, and the window only merges.

use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};

use crate::{
    fsguard,
    i18n::Tr,
    model::{MAX_ROWS, Record, Report, RunHeader, Trace, parse_records_at, version_error},
    tr,
};

/// A reload reads backwards in blocks of this size until it has enough lines.
const TAIL_BLOCK_BYTES: u64 = 1 << 20;
/// Upper bound on what a reload reads, whatever the line sizes.
const MAX_TAIL_BYTES: u64 = 256 << 20;
/// Never let a partially written line grow without bound.
const MAX_CARRY_BYTES: usize = 16 << 20;
/// How often the tail checks the file.
pub const TAIL_INTERVAL: Duration = Duration::from_millis(250);

/// What the window follows: one fixed file, or a run in the runs directory
/// (the newest, unless the viewer picked one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    File(PathBuf),
    Runs {
        dir: PathBuf,
        pinned: Option<PathBuf>,
    },
}

impl Source {
    /// The file to show now. Lists the runs directory: background only.
    pub fn resolve(&self) -> Option<PathBuf> {
        fsguard::assert_io_allowed();
        match self {
            Source::File(path) => Some(path.clone()),
            Source::Runs { dir, pinned } => pinned
                .clone()
                .filter(|path| path.exists())
                .or_else(|| lenslet_core::runs::list(dir).pop()),
        }
    }

    pub fn runs_dir(&self) -> Option<&Path> {
        match self {
            Source::Runs { dir, .. } => Some(dir),
            Source::File(path) => path.parent(),
        }
    }

    pub fn follows_latest(&self) -> bool {
        matches!(self, Source::Runs { pinned: None, .. })
    }
}

/// Where an incremental tail resumes.
#[derive(Clone, Debug, Default)]
pub struct TailState {
    pub offset: u64,
    pub file_key: u64,
    pub carry: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct Loaded {
    pub path: PathBuf,
    pub header: Option<RunHeader>,
    pub report: Option<Report>,
    pub calls: Vec<Trace>,
    pub skipped: usize,
    pub evicted: usize,
    pub tail: TailState,
}

#[derive(Debug)]
pub enum LoadResult {
    /// No run file (yet).
    Missing,
    Error(Tr),
    Loaded(Box<Loaded>),
}

/// Resolve `source` and read its run. Blocking IO: background only.
pub fn load_source(source: &Source) -> LoadResult {
    match source.resolve() {
        Some(path) => load(&path),
        None => LoadResult::Missing,
    }
}

/// Read the newest [`MAX_ROWS`] calls of a run file, its header and report.
/// A run file of another format is an error, never half-read.
pub fn load(path: &Path) -> LoadResult {
    fsguard::assert_io_allowed();
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return LoadResult::Missing,
        Err(error) => return LoadResult::Error(io_error(&error)),
    };
    let metadata = match file.metadata() {
        Ok(metadata) if metadata.is_dir() => {
            return LoadResult::Error(tr!(
                "That is a folder, not a run file",
                "这是一个目录，不是运行文件"
            ));
        }
        Ok(metadata) => metadata,
        Err(error) => return LoadResult::Error(io_error(&error)),
    };
    let len = metadata.len();

    // Read backwards a block at a time, joining the blocks once at the end.
    let mut blocks: Vec<Vec<u8>> = Vec::new();
    let mut start = len;
    let mut newlines = 0usize;
    while start > 0 && len - start < MAX_TAIL_BYTES {
        let block = TAIL_BLOCK_BYTES.min(start);
        start -= block;
        let mut chunk = vec![0u8; block as usize];
        if let Err(error) = file
            .seek(SeekFrom::Start(start))
            .and_then(|_| file.read_exact(&mut chunk))
        {
            return LoadResult::Error(io_error(&error));
        }
        newlines += chunk.iter().filter(|byte| **byte == b'\n').count();
        blocks.push(chunk);
        if newlines > MAX_ROWS + 2 {
            break;
        }
    }
    let mut bytes = Vec::with_capacity((len - start) as usize);
    for block in blocks.iter().rev() {
        bytes.extend_from_slice(block);
    }
    drop(blocks);

    // Drop a leading partial line, keep an unterminated tail for later.
    let mut body: &[u8] = &bytes;
    let mut body_offset = start;
    if start > 0 {
        match body.iter().position(|byte| *byte == b'\n') {
            Some(ix) => {
                body = &body[ix + 1..];
                body_offset += (ix + 1) as u64;
            }
            None => body = &[],
        }
    }
    let (complete, carry) = match body.iter().rposition(|byte| *byte == b'\n') {
        Some(ix) => (&body[..=ix], body[ix + 1..].to_vec()),
        None => (&[][..], body.to_vec()),
    };
    let source = Arc::new(path.to_path_buf());
    let mut parsed = parse_records_at(complete, body_offset, Some(&source));
    // A long run's header is far before the tail that was read.
    let header = parsed
        .header
        .take()
        .or_else(|| crate::model::read_header(path));
    let version = header
        .as_ref()
        .map(|h| h.version)
        .or_else(|| parsed.report.as_ref().map(|r| r.version));
    match version {
        Some(version) => {
            if let Some(error) = version_error(version) {
                return LoadResult::Error(error);
            }
        }
        None if len > 0 => {
            return LoadResult::Error(tr!(
                "Not a Lenslet run file (or one this version cannot read) — re-run your tests",
                "不是 Lenslet 运行文件（或此版本无法读取）— 请重新运行测试"
            ));
        }
        None => {}
    }
    let mut evicted = 0usize;
    if parsed.calls.len() > MAX_ROWS {
        let drop_n = parsed.calls.len() - MAX_ROWS;
        parsed.calls.drain(..drop_n);
        evicted += drop_n;
    }
    if start > 0 {
        evicted = evicted.max(1);
    }
    LoadResult::Loaded(Box::new(Loaded {
        path: path.to_path_buf(),
        header,
        report: parsed.report,
        calls: parsed.calls,
        skipped: parsed.bad,
        evicted,
        tail: TailState {
            offset: len,
            file_key: file_key(&metadata),
            carry,
        },
    }))
}

/// What the tail thread sends the window.
#[derive(Debug)]
pub enum FeedMsg {
    Calls(Vec<Trace>),
    Header(Box<RunHeader>),
    End(Box<Report>),
    /// The file was truncated or replaced: here is all of it again.
    Reset(Box<Loaded>),
    Error(Tr),
}

/// Keep `path` current from a background thread until its report arrives.
/// Dropping the receiver stops the thread.
pub fn start(path: PathBuf, tail: TailState) -> UnboundedReceiver<FeedMsg> {
    let (tx, rx) = unbounded();
    std::thread::Builder::new()
        .name("lenslet-tail".into())
        .spawn(move || tail_loop(&path, tail, &tx))
        .expect("spawn tail thread");
    rx
}

fn tail_loop(path: &Path, mut state: TailState, tx: &UnboundedSender<FeedMsg>) {
    loop {
        match poll_tail(path, &mut state) {
            TailPoll::Idle => {}
            TailPoll::Records(records) => {
                let mut calls = Vec::new();
                let mut ended = false;
                for record in records {
                    match record {
                        Record::Call(trace) => calls.push(*trace),
                        Record::Run(header) => {
                            let _ = tx.unbounded_send(FeedMsg::Header(header));
                        }
                        Record::End(report) => {
                            if !calls.is_empty() {
                                let _ =
                                    tx.unbounded_send(FeedMsg::Calls(std::mem::take(&mut calls)));
                            }
                            let _ = tx.unbounded_send(FeedMsg::End(report));
                            ended = true;
                        }
                    }
                }
                if !calls.is_empty() && tx.unbounded_send(FeedMsg::Calls(calls)).is_err() {
                    return;
                }
                if ended {
                    return;
                }
            }
            TailPoll::Replaced => match load(path) {
                LoadResult::Loaded(loaded) => {
                    state = loaded.tail.clone();
                    let done = loaded.report.is_some();
                    if tx.unbounded_send(FeedMsg::Reset(loaded)).is_err() || done {
                        return;
                    }
                }
                LoadResult::Missing => {}
                LoadResult::Error(error) => {
                    let _ = tx.unbounded_send(FeedMsg::Error(error));
                    return;
                }
            },
            TailPoll::Error(error) => {
                if tx.unbounded_send(FeedMsg::Error(error)).is_err() {
                    return;
                }
            }
        }
        std::thread::sleep(TAIL_INTERVAL);
        if tx.is_closed() {
            return;
        }
    }
}

enum TailPoll {
    Idle,
    Records(Vec<Record>),
    Replaced,
    Error(Tr),
}

/// Read what was appended since `state`, parsing only complete lines.
fn poll_tail(path: &Path, state: &mut TailState) -> TailPoll {
    fsguard::assert_io_allowed();
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => return TailPoll::Error(io_error(&error)),
    };
    let len = metadata.len();
    let key = file_key(&metadata);
    if len < state.offset || (state.file_key != 0 && key != 0 && state.file_key != key) {
        return TailPoll::Replaced;
    }
    if len == state.offset {
        return TailPoll::Idle;
    }
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) => return TailPoll::Error(io_error(&error)),
    };
    let mut bytes = Vec::with_capacity((len - state.offset) as usize);
    if let Err(error) = file
        .seek(SeekFrom::Start(state.offset))
        .and_then(|_| file.take(len - state.offset).read_to_end(&mut bytes))
    {
        return TailPoll::Error(io_error(&error));
    }
    let base_offset = state.offset - state.carry.len() as u64;
    state.offset += bytes.len() as u64;
    state.carry.extend_from_slice(&bytes);
    let Some(newline) = state.carry.iter().rposition(|byte| *byte == b'\n') else {
        if state.carry.len() > MAX_CARRY_BYTES {
            state.carry.clear();
        }
        return TailPoll::Idle;
    };
    let complete: Vec<u8> = state.carry.drain(..=newline).collect();
    let source = Arc::new(path.to_path_buf());
    let parsed = parse_records_at(&complete, base_offset, Some(&source));
    let mut records: Vec<Record> = Vec::new();
    if let Some(header) = parsed.header {
        records.push(Record::Run(Box::new(header)));
    }
    records.extend(
        parsed
            .calls
            .into_iter()
            .map(|call| Record::Call(Box::new(call))),
    );
    if let Some(report) = parsed.report {
        records.push(Record::End(Box::new(report)));
    }
    TailPoll::Records(records)
}

fn io_error(error: &io::Error) -> Tr {
    match error.kind() {
        io::ErrorKind::PermissionDenied => tr!(
            "Permission denied reading the run file",
            "没有权限读取运行文件"
        ),
        io::ErrorKind::NotFound => tr!("The run file is gone", "运行文件已不存在"),
        _ => tr!(
            "Cannot read the run file: {error}",
            "无法读取运行文件：{error}"
        ),
    }
}

#[cfg(unix)]
fn file_key(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.ino()
}

#[cfg(not(unix))]
fn file_key(_metadata: &fs::Metadata) -> u64 {
    0
}

// ---------------------------------------------------------------------------
// The output log
// ---------------------------------------------------------------------------

/// The most of a test command's output the window keeps.
pub const MAX_LOG_BYTES: usize = 512 * 1024;

/// The last [`MAX_LOG_BYTES`] of a log file, with its length (to skip
/// re-reading an unchanged file). Background only.
pub fn read_log(path: &Path, known_len: Option<u64>) -> Option<(u64, Option<String>)> {
    fsguard::assert_io_allowed();
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if Some(len) == known_len {
        return Some((len, None));
    }
    if len > MAX_LOG_BYTES as u64 {
        file.seek(SeekFrom::Start(len - MAX_LOG_BYTES as u64))
            .ok()?;
    }
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if len > MAX_LOG_BYTES as u64
        && let Some(ix) = text.find('\n')
    {
        text.drain(..=ix);
    }
    Some((len, Some(text)))
}

// ---------------------------------------------------------------------------
// The run list
// ---------------------------------------------------------------------------

/// A run in the runs directory, for the run picker.
#[derive(Clone, Debug)]
pub struct RunSummary {
    pub path: PathBuf,
    pub id: String,
    pub header: Option<RunHeader>,
    pub report: Option<Report>,
}

/// Runs of one project, as indices into the run list (newest first).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunGroup {
    pub project: String,
    pub runs: Vec<usize>,
}

/// Group runs by project, keeping the order projects first appear in.
pub fn group_runs(runs: &[RunSummary]) -> Vec<RunGroup> {
    let mut groups: Vec<RunGroup> = Vec::new();
    for (index, run) in runs.iter().enumerate() {
        let project = run.project_name();
        match groups.iter_mut().find(|g| g.project == project) {
            Some(group) => group.runs.push(index),
            None => groups.push(RunGroup {
                project,
                runs: vec![index],
            }),
        }
    }
    groups
}

impl RunSummary {
    pub fn project_name(&self) -> String {
        let Some(cwd) = self
            .header
            .as_ref()
            .map(|h| h.cwd.as_str())
            .filter(|s| !s.is_empty())
        else {
            return "—".into();
        };
        Path::new(cwd)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| cwd.to_string())
    }

    /// An older run file the window refuses to read.
    pub fn outdated(&self) -> bool {
        self.header
            .as_ref()
            .is_some_and(|h| version_error(h.version).is_some())
    }
}

/// Every run in `dir`, newest first, with its header and report. Background only.
pub fn list_runs(dir: &Path) -> Vec<RunSummary> {
    fsguard::assert_io_allowed();
    lenslet_core::runs::list(dir)
        .into_iter()
        .rev()
        .map(|path| RunSummary {
            id: lenslet_core::runs::run_id(&path),
            header: crate::model::read_header(&path),
            report: lenslet_core::runs::report(&path),
            path,
        })
        .collect()
}

/// Just the newest run file's path (cheap: one directory listing).
pub fn newest_run(dir: &Path) -> Option<PathBuf> {
    fsguard::assert_io_allowed();
    lenslet_core::runs::list(dir).pop()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdata::{RUN_V4, header_line, scratch, synthetic_run};
    use std::io::Write;

    fn loaded(path: &Path) -> Box<Loaded> {
        match load(path) {
            LoadResult::Loaded(loaded) => loaded,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn loads_the_tail_of_a_long_run_with_its_header_and_hydrates_bodies() {
        let path = scratch("feed-long").join("run.jsonl");
        let big = "x".repeat(20_000);
        let text = synthetic_run(MAX_ROWS + 300, Some(&big), true);
        fs::write(&path, &text).unwrap();
        let loaded = loaded(&path);
        assert_eq!(loaded.calls.len(), MAX_ROWS);
        assert!(loaded.header.is_some(), "header read separately");
        assert!(loaded.report.is_some());
        let last = loaded.calls.last().unwrap();
        assert!(last.is_dehydrated());
        let full = last.hydrated().unwrap();
        let text = full.request.body.as_ref().unwrap().text.as_ref().unwrap();
        assert_eq!(text.len(), big.len());
    }

    #[test]
    fn reads_the_format_4_fixture() {
        let path = scratch("feed-fixture").join("run.jsonl");
        fs::write(&path, RUN_V4).unwrap();
        let loaded = loaded(&path);
        assert_eq!(loaded.calls.len(), 15);
        assert_eq!(loaded.skipped, 0);
        let report = loaded.report.unwrap();
        assert_eq!(report.findings.len(), 3);
        assert_eq!(loaded.header.unwrap().version, 4);
    }

    #[test]
    fn refuses_a_run_from_an_older_lenslet() {
        let path = scratch("feed-old").join("run.jsonl");
        fs::write(&path, RUN_V4.replace("\"version\":4", "\"version\":3")).unwrap();
        match load(&path) {
            LoadResult::Error(error) => {
                assert!(error.en.contains("older Lenslet"), "{error}");
                assert!(error.zh.contains("旧版"));
            }
            other => panic!("expected the version error, got {other:?}"),
        }
        fs::write(&path, "{\"type\":\"run\",\"version\":2}\n").unwrap();
        assert!(matches!(load(&path), LoadResult::Error(_)));
    }

    #[test]
    fn tails_appends_holds_partial_lines_and_detects_truncation() {
        let path = scratch("feed-tail").join("run.jsonl");
        let text = synthetic_run(3, None, false);
        fs::write(&path, &text).unwrap();
        let mut state = loaded(&path).tail;
        assert!(matches!(poll_tail(&path, &mut state), TailPoll::Idle));

        let more = synthetic_run(5, None, false);
        let extra: Vec<&str> = more.lines().skip(4).collect();
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        // A partial line (split inside a UTF-8 character) is held back.
        let line = extra[0].replace("\"operation\":\"", "\"operation\":\"é");
        let split = line.find('é').unwrap() + 1;
        file.write_all(&line.as_bytes()[..split]).unwrap();
        file.flush().unwrap();
        assert!(matches!(poll_tail(&path, &mut state), TailPoll::Idle));
        file.write_all(&line.as_bytes()[split..]).unwrap();
        file.write_all(b"\n").unwrap();
        file.flush().unwrap();
        match poll_tail(&path, &mut state) {
            TailPoll::Records(records) => {
                assert_eq!(records.len(), 1);
                let Record::Call(call) = &records[0] else {
                    panic!()
                };
                assert!(call.operation.starts_with('é'));
                let full = call.hydrated().unwrap();
                assert_eq!(full.id, call.id, "offset points at the line");
            }
            _ => panic!("expected the appended call"),
        }
        fs::write(&path, header_line()).unwrap();
        assert!(matches!(poll_tail(&path, &mut state), TailPoll::Replaced));
    }

    #[test]
    fn the_tail_thread_follows_a_live_run_until_its_report() {
        use futures::StreamExt;
        let path = scratch("feed-live").join("run.jsonl");
        let partial = synthetic_run(2, None, false);
        fs::write(&path, &partial).unwrap();
        let state = loaded(&path).tail;
        let mut rx = start(path.clone(), state);
        let full = synthetic_run(4, None, true);
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&full.as_bytes()[partial.len()..])
            .unwrap();
        let msgs: Vec<FeedMsg> = futures::executor::block_on(async {
            let mut out = Vec::new();
            while let Some(msg) = rx.next().await {
                out.push(msg);
            }
            out
        });
        let calls: usize = msgs
            .iter()
            .map(|m| match m {
                FeedMsg::Calls(c) => c.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(calls, 2);
        assert!(matches!(msgs.last(), Some(FeedMsg::End(_))));
    }

    #[test]
    fn reads_the_end_of_a_long_log() {
        let path = scratch("feed-log").join("run.log");
        let line = "0123456789".repeat(10);
        let text: String = (0..10_000).map(|i| format!("{i} {line}\n")).collect();
        fs::write(&path, &text).unwrap();
        let (len, tail) = read_log(&path, None).unwrap();
        let tail = tail.unwrap();
        assert!(tail.len() <= MAX_LOG_BYTES);
        assert!(tail.ends_with(&format!("9999 {line}\n")));
        assert!(
            tail.chars().next().unwrap().is_ascii_digit(),
            "starts on a line"
        );
        assert_eq!(read_log(&path, Some(len)).unwrap().1, None, "unchanged");
    }
}
