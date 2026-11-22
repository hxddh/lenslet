//! The run file and its on-disk sink.
//!
//! A run file is an event stream. The first line is the `run` header, then
//! one `call` line per proxied call, written when the call finishes, and the
//! last line is the `end` report. Every fact a reader shows (the error, the
//! IAM access, whether it is in the lock) is computed here once and written
//! down, so the window, `--json` and agents all see the same thing. The run
//! file keeps raw headers and text bodies and is written owner-only.

use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::{
    access::{Access, Derived},
    summary::{Report, Summary},
};

pub use lenslet_core::trace::*;

struct SinkState {
    file: File,
    summary: Summary,
    /// Set by [`TraceSink::seal`]: nothing but the `end` line is written after.
    sealed: bool,
}

/// Append-only run file. Writes are serialized through one open file
/// handle; the sink also keeps the [`Summary`] that becomes the `end` report.
pub struct TraceSink {
    state: Mutex<SinkState>,
    /// The lock's lines, when there is a lock: what is `new`.
    locked: Option<BTreeSet<Access>>,
}

impl TraceSink {
    /// Create the parent directory and the run file (replacing one that
    /// exists) and write the header, so failures surface at startup rather
    /// than on the first proxied call. On Unix the file is owner-only (0600).
    pub fn new(
        path: PathBuf,
        header: &RunHeader,
        locked: Option<BTreeSet<Access>>,
    ) -> Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("create trace directory {}", parent.display()))?;
        }
        let file = open_owner_only(&path)
            .with_context(|| format!("open trace file {}", path.display()))?;
        let sink = TraceSink {
            state: Mutex::new(SinkState {
                file,
                summary: Summary::default(),
                sealed: false,
            }),
            locked,
        };
        if let Some(line) = encode("run", header) {
            write_line(&mut sink.lock(), &line);
        }
        Ok(sink)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SinkState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Write one finished call with what its complete request needs. The
    /// stored request body is capped at [`MAX_BODY_TEXT`] after derivation.
    /// Never fails the caller: trace problems are logged to stderr and the
    /// proxy keeps forwarding. After [`TraceSink::seal`] the call is dropped.
    pub fn record(&self, mut trace: Trace, derived: &Derived) {
        derived.record(&mut trace, self.locked.as_ref());
        if let Some(body) = trace.request.body.as_mut() {
            body.cap(MAX_BODY_TEXT);
        }
        let mut state = self.lock();
        if state.sealed {
            return;
        }
        let call_no = state.summary.calls + 1;
        state
            .summary
            .causal
            .observe_and_enrich(&mut trace, &derived.concrete_own, call_no);
        state.summary.observe(&trace);
        if let Some(line) = encode("call", &trace) {
            write_line(&mut state, &line);
        }
    }

    /// Stop recording: calls that finish from now on are dropped. Returns
    /// the final tally.
    pub fn seal(&self) -> Summary {
        let mut state = self.lock();
        state.sealed = true;
        state.summary.clone()
    }

    /// Seal the run and append the `end` report: nothing is written after it.
    pub fn end(&self, report: &Report) {
        let line = encode("end", report);
        let mut state = self.lock();
        state.sealed = true;
        if let Some(line) = line {
            write_line(&mut state, &line);
        }
    }
}

/// One `write_all` per line, so a reader tailing the file never sees half a
/// record followed by another line.
fn write_line(state: &mut SinkState, line: &str) {
    if let Err(error) = state.file.write_all(line.as_bytes()) {
        eprintln!("lenslet: cannot write trace: {error}");
    }
}

/// `{"type": kind, ...value}` and a newline.
fn encode<T: Serialize>(kind: &'static str, value: &T) -> Option<String> {
    match serde_json::to_string(&Tagged { kind, value }) {
        Ok(line) => Some(line + "\n"),
        Err(error) => {
            eprintln!("lenslet: cannot encode trace: {error}");
            None
        }
    }
}

/// A fresh, owner-only (0600) file, replacing what was there.
pub fn open_owner_only(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lenslet-sink-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn header() -> RunHeader {
        RunHeader {
            version: VERSION,
            run: "r".into(),
            command: vec!["t".into()],
            cwd: "/w".into(),
            emulator: "moto".into(),
            ..RunHeader::default()
        }
    }

    fn put(table: &str, body_pad: usize) -> Trace {
        let text = format!(
            r#"{{"TableName":"{table}","Item":{{"pad":{{"S":"{}"}}}}}}"#,
            "x".repeat(body_pad)
        );
        Trace {
            id: table.into(),
            method: "POST".into(),
            path: "/".into(),
            service: "dynamodb".into(),
            operation: "PutItem".into(),
            status: 200,
            region: Some("us-east-1".into()),
            request: Message {
                body: Some(BodyTrace {
                    bytes: text.len() as u64,
                    text: Some(text),
                    ..BodyTrace::default()
                }),
                ..Message::default()
            },
            ..Trace::default()
        }
    }

    #[test]
    fn writes_one_line_per_finished_call_with_concrete_access_and_lock_marks() {
        let dir = temp_dir("record");
        let path = dir.join("r.jsonl");
        let locked: BTreeSet<Access> = [Access::new(
            "dynamodb:PutItem",
            "arn:aws:dynamodb:*:*:table/orders",
        )]
        .into();
        let sink = TraceSink::new(path.clone(), &header(), Some(locked)).unwrap();
        for table in ["orders", "audit"] {
            let call = put(table, 0);
            let derived = access::derive_call(&call);
            sink.record(call, &derived);
        }
        // A body over the stored limit is derived from in full, stored capped.
        let big = put("big", MAX_BODY_TEXT + 10);
        let derived = access::derive_call(&big);
        sink.record(big, &derived);
        let summary = sink.seal();
        assert_eq!(summary.calls, 3);
        sink.end(&Report::default());
        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0]["type"], "run");
        assert_eq!(lines[1]["access"][0].get("new"), None);
        assert_eq!(lines[2]["access"][0]["new"], true);
        assert_eq!(
            lines[2]["concrete"][0]["resource"],
            "arn:aws:dynamodb:us-east-1:*:table/audit"
        );
        assert_eq!(
            lines[3]["concrete"][0]["resource"],
            "arn:aws:dynamodb:us-east-1:*:table/big"
        );
        assert_eq!(lines[3]["request"]["body"]["truncated"], true);
        assert_eq!(
            lines[3]["request"]["body"]["text"].as_str().unwrap().len(),
            MAX_BODY_TEXT
        );
        assert_eq!(lines[4]["type"], "end");
        for gone in ["pending", "new", "blocked", "target"] {
            assert!(lines[2].get(gone).is_none(), "{gone}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // Nothing is written after the end.
        let late = put("late", 0);
        let derived = access::derive_call(&late);
        sink.record(late, &derived);
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        let _ = fs::remove_dir_all(&dir);
    }
}
