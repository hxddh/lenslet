//! Run files in `~/.lenslet/runs`: one `YYYYMMDD-HHMMSS-PID.jsonl` per run,
//! newest last by name, next to its `<run>.log` (the test command's output)
//! and, when Lenslet started moto, `<run>.emulator.log`. The first line says
//! what the run is, the last line (once it ends) what it did. The last
//! [`KEEP_RUNS`] finished runs are kept; a run that is still going is never
//! deleted.

use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::Utc;

use crate::{
    summary::Report,
    trace::{self, Record, RunHeader, Trace},
};

pub const KEEP_RUNS: usize = 20;
/// The `end` report is one line; this bounds how far back we look for it.
const MAX_LAST_LINE: u64 = 16 << 20;

/// Set Unix permission bits (no-op off Unix).
#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("set permissions on {}", path.display()))
}

#[cfg(not(unix))]
pub fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

pub fn dir(home: &Path) -> PathBuf {
    home.join(".lenslet/runs")
}

/// The test command's output (`<run>.log`) of a run file (`<run>.jsonl`).
pub fn log_path_for(run: &Path) -> PathBuf {
    run.with_extension("log")
}

/// moto's output (`<run>.emulator.log`), when Lenslet started moto.
pub fn emulator_log_path_for(run: &Path) -> PathBuf {
    run.with_extension("emulator.log")
}

/// Prepare the runs directory, drop old runs, and name the new run file.
pub fn create(dir: &Path) -> Result<PathBuf> {
    if !dir.exists() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        set_mode(dir, 0o700)?;
    }
    prune(dir, KEEP_RUNS.saturating_sub(1));
    let stamp = Utc::now().format("%Y%m%d-%H%M%S").to_string();
    Ok(unique_run_path(dir, &stamp, std::process::id()))
}

fn unique_run_path(dir: &Path, stamp: &str, pid: u32) -> PathBuf {
    let base = dir.join(format!("{stamp}-{pid}.jsonl"));
    if !base.exists() {
        return base;
    }
    // `_` (0x5f) sorts after `.` (0x2e), preserving chronological order when
    // two runs share a second and PID (e.g. PID 1 in containers).
    for seq in 2u32.. {
        let candidate = dir.join(format!("{stamp}-{pid}_{seq:02}.jsonl"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

/// `…/20261115-054628-15569.jsonl` → `20261115-054628-15569`.
pub fn run_id(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Run files, oldest first.
pub fn list(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut runs: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    runs.sort();
    runs
}

/// Keep the newest `keep` finished runs (and every run still going).
fn prune(dir: &Path, keep: usize) {
    let finished: Vec<PathBuf> = list(dir)
        .into_iter()
        .filter(|run| !is_active(run))
        .collect();
    let excess = finished.len().saturating_sub(keep);
    for run in finished.iter().take(excess) {
        let _ = fs::remove_file(log_path_for(run));
        let _ = fs::remove_file(emulator_log_path_for(run));
        let _ = fs::remove_file(run);
    }
}

/// Whether two directory paths name the same directory.
fn same_dir(a: &str, b: &Path) -> bool {
    let a = Path::new(a);
    a == b
        || matches!(
            (a.canonicalize(), b.canonicalize()),
            (Ok(a), Ok(b)) if a == b
        )
}

/// The newest finished run started in `cwd`: what `lenslet check` and
/// `lenslet policy --run latest` read by default.
pub fn newest_finished(dir: &Path, cwd: &Path) -> Result<PathBuf> {
    for run in list(dir).into_iter().rev() {
        if is_active(&run) || !has_end(&run) {
            continue;
        }
        let Ok(header) = header(&run) else { continue };
        if same_dir(&header.cwd, cwd) {
            return Ok(run);
        }
    }
    anyhow::bail!(
        "no finished run in {}; run your tests first, e.g. `lenslet run -- <tests>`",
        cwd.display()
    )
}

/// A run named on the command line: a path, `latest` (the newest finished
/// run in `cwd`), an exact run id, or a unique prefix of one.
pub fn resolve(dir: &Path, spec: &str, cwd: &Path) -> Result<PathBuf> {
    let candidate = Path::new(spec);
    if candidate.is_file() {
        return Ok(candidate.to_path_buf());
    }
    if spec.eq_ignore_ascii_case("latest") {
        return newest_finished(dir, cwd);
    }
    let all = list(dir);
    if let Some(exact) = all.iter().find(|p| run_id(p) == spec) {
        return Ok(exact.clone());
    }
    let matches: Vec<PathBuf> = all
        .into_iter()
        .filter(|p| run_id(p).starts_with(spec))
        .collect();
    match matches.as_slice() {
        [single] => Ok(single.clone()),
        [] => anyhow::bail!("no run matching `{spec}` in {}", dir.display()),
        many => {
            let names: Vec<String> = many.iter().rev().take(5).map(|p| run_id(p)).collect();
            anyhow::bail!(
                "ambiguous run `{spec}` (matches {}); specify more characters",
                names.join(", ")
            )
        }
    }
}

fn first_line(path: &Path) -> Result<String> {
    let mut line = String::new();
    BufReader::new(File::open(path).with_context(|| format!("read {}", path.display()))?)
        .read_line(&mut line)
        .with_context(|| format!("read {}", path.display()))?;
    Ok(line)
}

/// The run's header. A run file of another format is refused.
pub fn header(path: &Path) -> Result<RunHeader> {
    let line = first_line(path)?;
    let value: serde_json::Value = serde_json::from_str(&line)
        .with_context(|| format!("{} does not start with a run header", path.display()))?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("run") {
        anyhow::bail!("{} does not start with a run header", path.display());
    }
    trace::check_version(path, &value)?;
    serde_json::from_value(value).with_context(|| format!("read the header of {}", path.display()))
}

/// The run's calls, in the order they finished. A run file of another
/// format is refused.
pub fn calls(path: &Path) -> Result<Vec<Trace>> {
    header(path)?;
    let file = File::open(path).with_context(|| format!("read {}", path.display()))?;
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.with_context(|| format!("read {}", path.display()))?;
        if let Ok(Record::Call(call)) = serde_json::from_str::<Record>(&line) {
            out.push(*call);
        }
    }
    Ok(out)
}

/// The run's `end` report, or `None` while it has not ended. A run file of
/// another format is refused.
pub fn report(path: &Path) -> Result<Option<Report>> {
    let Some(line) = last_line(path) else {
        return Ok(None);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
        return Ok(None);
    };
    if value.get("type").and_then(serde_json::Value::as_str) != Some("end") {
        return Ok(None);
    }
    trace::check_version(path, &value)?;
    serde_json::from_value(value)
        .map(Some)
        .with_context(|| format!("read the report of {}", path.display()))
}

/// Whether the run file ends with an `end` line, whatever its format.
fn has_end(path: &Path) -> bool {
    last_line(path)
        .and_then(|line| serde_json::from_str::<serde_json::Value>(&line).ok())
        .is_some_and(|value| value.get("type").and_then(serde_json::Value::as_str) == Some("end"))
}

fn last_line(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut start = len;
    let mut tail: Vec<u8> = Vec::new();
    // Read backwards until the tail holds a whole last line.
    while start > 0 && len - start < MAX_LAST_LINE {
        let block = (64 * 1024u64).min(start);
        start -= block;
        let mut chunk = vec![0u8; block as usize];
        file.seek(SeekFrom::Start(start)).ok()?;
        file.read_exact(&mut chunk).ok()?;
        chunk.extend_from_slice(&tail);
        tail = chunk;
        let body = tail.strip_suffix(b"\n").unwrap_or(&tail);
        if body.contains(&b'\n') {
            break;
        }
    }
    let body = tail.strip_suffix(b"\n").unwrap_or(&tail);
    let line = match body.iter().rposition(|byte| *byte == b'\n') {
        Some(ix) => &body[ix + 1..],
        None if start == 0 => body,
        None => return None,
    };
    Some(String::from_utf8_lossy(line).into_owned())
}

/// Not ended, and its `lenslet run` process is alive.
pub fn is_active(path: &Path) -> bool {
    if has_end(path) {
        return false;
    }
    let pid = first_line(path)
        .ok()
        .and_then(|line| serde_json::from_str::<serde_json::Value>(&line).ok())
        .and_then(|value| value.get("pid").and_then(serde_json::Value::as_u64));
    pid.is_some_and(|pid| process_alive(pid as u32))
}

/// The most recent finished run of the same command in the same directory,
/// before `current`: what "did this get worse?" compares with.
pub fn previous(dir: &Path, current: &Path, command: &[String], cwd: &str) -> Option<Report> {
    list(dir)
        .into_iter()
        .rev()
        .filter(|run| run.as_path() < current)
        .filter(|run| header(run).is_ok_and(|h| h.command == command && h.cwd == cwd))
        .find_map(|run| report(&run).ok().flatten())
}

#[cfg(target_os = "linux")]
fn process_alive(pid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    if pid == 0 {
        return false;
    }
    let Ok(target) = fs::metadata(format!("/proc/{pid}")) else {
        return false;
    };
    let Ok(me) = fs::metadata("/proc/self") else {
        return true;
    };
    target.uid() == me.uid() || me.uid() == 0
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_alive(pid: u32) -> bool {
    // `kill -0` only checks that the process exists and is ours to signal.
    pid != 0
        && std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

#[cfg(not(unix))]
fn process_alive(pid: u32) -> bool {
    // No cheap liveness check: keep anything that has not recorded an end.
    pid != 0
}

pub fn now() -> String {
    Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_run(
        dir: &Path,
        name: &str,
        pid: u32,
        command: &[&str],
        cwd: &str,
        end: Option<i32>,
    ) -> PathBuf {
        let path = dir.join(format!("{name}.jsonl"));
        let header = serde_json::json!({
            "type": "run", "version": trace::VERSION, "run": name, "command": command,
            "cwd": cwd, "pid": pid, "endpoint": "http://127.0.0.1:1", "upstream": "u",
            "emulator": "moto", "started": "s", "lock": null, "log": null,
        });
        let mut text = format!("{header}\n");
        text.push_str(r#"{"type":"call","id":"a","at":"t","method":"GET","path":"/","host":"h","service":"s3","operation":"ListBuckets","status":200,"ms":1,"authorization":false,"request":{"headers":[]},"response":{"headers":[]}}"#);
        text.push('\n');
        if let Some(code) = end {
            let report = Report {
                version: trace::VERSION,
                run: name.into(),
                exit_code: Some(code),
                ..Report::default()
            };
            let mut value = serde_json::to_value(&report).unwrap();
            value["type"] = "end".into();
            text.push_str(&format!("{value}\n"));
        }
        fs::write(&path, text).unwrap();
        path
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lenslet-runs-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn keeps_the_last_finished_runs_and_every_live_one() {
        let dir = temp_dir("prune");
        for index in 0..25 {
            let run = write_run(
                &dir,
                &format!("20261115-0000{index:02}-1"),
                0,
                &["t"],
                &format!("/w{}", index % 3),
                Some(0),
            );
            fs::write(log_path_for(&run), "output").unwrap();
        }
        // The oldest run is still going (this process).
        let live = write_run(
            &dir,
            "20261114-000000-1",
            std::process::id(),
            &[],
            "/w",
            None,
        );
        assert!(is_active(&live));
        let next = create(&dir).unwrap();
        assert!(next.extension().is_some_and(|ext| ext == "jsonl"));
        let left: Vec<String> = list(&dir).iter().map(|path| run_id(path)).collect();
        assert!(live.exists(), "a running run is never pruned");
        assert_eq!(left[0], "20261114-000000-1");
        assert_eq!(left.len(), KEEP_RUNS, "19 finished runs and the live one");
        assert!(
            left[1].starts_with("20261115-000006"),
            "oldest finished runs go first, whatever their directory: {left:?}"
        );
        assert!(
            !dir.join("20261115-000005-1.log").exists(),
            "with their logs"
        );
        assert!(dir.join("20261115-000006-1.log").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_the_header_the_report_and_the_previous_run() {
        let dir = temp_dir("read");
        let one = write_run(
            &dir,
            "20261115-000001-1",
            0,
            &["npm", "test"],
            "/w",
            Some(1),
        );
        write_run(&dir, "20261115-000002-1", 0, &["pytest"], "/w", Some(0));
        let three = write_run(&dir, "20261115-000003-1", 0, &["npm", "test"], "/w", None);
        assert_eq!(header(&one).unwrap().command, ["npm", "test"]);
        assert_eq!(report(&one).unwrap().unwrap().exit_code, Some(1));
        assert!(report(&three).unwrap().is_none(), "still running");
        assert_eq!(calls(&one).unwrap().len(), 1);
        let command = vec!["npm".to_string(), "test".to_string()];
        assert_eq!(
            previous(&dir, &three, &command, "/w").unwrap().run,
            "20261115-000001-1"
        );
        assert!(previous(&dir, &three, &command, "/elsewhere").is_none());
        assert!(previous(&dir, &one, &command, "/w").is_none());

        // A long last line is still found.
        let big = dir.join("big.jsonl");
        fs::write(&big, format!("first\n{}\n", "x".repeat(300_000))).unwrap();
        assert_eq!(last_line(&big).unwrap().len(), 300_000);
        fs::write(&big, "only line\n").unwrap();
        assert_eq!(last_line(&big).unwrap(), "only line");

        // Same-second PID-1 runs never overwrite or sort out of order.
        let first = unique_run_path(&dir, "20261118-120000", 1);
        fs::write(&first, "").unwrap();
        let second = unique_run_path(&dir, "20261118-120000", 1);
        assert!(first < second, "{first:?} should sort before {second:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_run_file_of_another_format_is_refused() {
        let dir = temp_dir("format");
        let path = dir.join("20261101-000000-1.jsonl");
        fs::write(
            &path,
            "{\"type\":\"run\",\"version\":3,\"run\":\"x\",\"command\":[],\"cwd\":\"/w\",\"pid\":0,\"endpoint\":\"\",\"upstream\":\"\",\"started\":\"\"}\n\
             {\"type\":\"end\",\"version\":3,\"run\":\"x\"}\n",
        )
        .unwrap();
        let expected = format!(
            "run file {} is format 3; this Lenslet reads format 4 — re-run your tests",
            path.display()
        );
        assert_eq!(header(&path).unwrap_err().to_string(), expected);
        assert_eq!(calls(&path).unwrap_err().to_string(), expected);
        assert_eq!(report(&path).unwrap_err().to_string(), expected);
        // It is not picked as the newest run, and it can be pruned.
        assert!(newest_finished(&dir, Path::new("/w")).is_err());
        assert!(!is_active(&path));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_newest_finished_run_is_the_one_of_this_directory() {
        let dir = temp_dir("newest");
        let here = dir.join("project");
        fs::create_dir_all(&here).unwrap();
        let cwd = here.display().to_string();
        let mine = write_run(&dir, "20261115-000001-1", 0, &["t"], &cwd, Some(0));
        write_run(&dir, "20261115-000002-1", 0, &["t"], "/elsewhere", Some(0));
        write_run(
            &dir,
            "20261115-000003-1",
            std::process::id(),
            &["t"],
            &cwd,
            None,
        );
        assert_eq!(newest_finished(&dir, &here).unwrap(), mine);
        assert_eq!(resolve(&dir, "latest", &here).unwrap(), mine);
        let error = newest_finished(&dir, Path::new("/nowhere")).unwrap_err();
        assert!(error.to_string().contains("lenslet run"), "{error}");
        assert_eq!(
            resolve(&dir, "20261115-000002", &here).unwrap(),
            dir.join("20261115-000002-1.jsonl")
        );
        assert!(resolve(&dir, "20261115", &here).is_err(), "ambiguous");
        fs::remove_dir_all(&dir).unwrap();
    }
}
