//! The window's control plane, and all of it: find the `lenslet` CLI, ask it
//! `doctor --json` (the project's test command, which emulators run), start
//! `lenslet run -- <tests>` in the project and stop it with `SIGTERM` to its
//! process group, and clean up after a window that crashed.
//!
//! Every function that touches a process or the disk checks
//! [`crate::fsguard::assert_io_allowed`] and runs on the background executor.
//! Children run in their own process group, die with the window on Linux
//! (`PR_SET_PDEATHSIG`), and are recorded in `~/.lenslet/ui-children.json`.

use std::{
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{fsguard, i18n::Tr, tr};

/// The install hint shown when no emulator can be found or started.
pub const MOTO_INSTALL: &str = "pip install 'moto[server]'";
/// How long a stopped run gets to write its report before `SIGKILL`.
pub const STOP_GRACE: Duration = Duration::from_secs(10);

// -- doctor --------------------------------------------------------------------

/// A running emulator, as `lenslet doctor --json` lists it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Emulator {
    pub name: String,
    pub url: String,
}

/// The emulator Lenslet starts itself when none is running.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct ManagedEmulator {
    pub name: String,
    pub available: bool,
}

/// The parts of `lenslet doctor --json` the window uses.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Doctor {
    pub version: String,
    pub emulators: Vec<Emulator>,
    pub managed: Option<ManagedEmulator>,
    /// The project's test command, when the CLI knows it.
    pub test_command: Option<String>,
}

pub fn parse_doctor(json: &str) -> Option<Doctor> {
    serde_json::from_str(json.trim()).ok()
}

/// What the next run will talk to, in the words the window uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmulatorStatus {
    /// `doctor` has not answered yet.
    Checking,
    /// The CLI could not be run, or did not answer.
    NoCli(Tr),
    Running {
        name: String,
        url: String,
    },
    /// Nothing running, but Lenslet will start this one.
    WillStart(String),
    /// Nothing running and nothing Lenslet can start.
    Missing,
}

impl EmulatorStatus {
    pub fn from_doctor(doctor: Option<&Result<Doctor, Tr>>) -> Self {
        match doctor {
            None => EmulatorStatus::Checking,
            Some(Err(error)) => EmulatorStatus::NoCli(error.clone()),
            Some(Ok(doctor)) => {
                if let Some(emu) = doctor.emulators.first() {
                    EmulatorStatus::Running {
                        name: emu.name.clone(),
                        url: emu.url.clone(),
                    }
                } else if let Some(managed) = doctor.managed.as_ref().filter(|m| m.available) {
                    EmulatorStatus::WillStart(if managed.name.is_empty() {
                        "moto".into()
                    } else {
                        managed.name.clone()
                    })
                } else {
                    EmulatorStatus::Missing
                }
            }
        }
    }

    pub fn message(&self) -> Tr {
        match self {
            EmulatorStatus::Checking => tr!("Looking for an emulator…", "正在查找模拟器…"),
            EmulatorStatus::NoCli(error) => error.clone(),
            EmulatorStatus::Running { name, url } => {
                tr!("{name} running at {url}", "{name} 正在 {url} 运行")
            }
            EmulatorStatus::WillStart(name) => tr!(
                "No emulator running — Lenslet will start {name}",
                "未发现运行中的模拟器 — Lenslet 会自动启动 {name}"
            ),
            EmulatorStatus::Missing => tr!(
                "No emulator found — install moto:",
                "未发现模拟器 — 请安装 moto："
            ),
        }
    }

    pub fn ok(&self) -> bool {
        matches!(
            self,
            EmulatorStatus::Running { .. } | EmulatorStatus::WillStart(_)
        )
    }
}

/// Run `lenslet doctor --json` in `cwd`, bounded by `timeout`.
pub fn run_doctor(cwd: Option<&Path>, timeout: Duration) -> Result<Doctor, Tr> {
    fsguard::assert_io_allowed();
    let bin = cli_binary()?;
    let mut cmd = Command::new(&bin);
    cmd.env("PATH", augmented_path())
        .args(["doctor", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(dir) = cwd.filter(|dir| dir.is_dir()) {
        cmd.current_dir(dir);
    }
    configure_child(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| {
        tr!(
            "Cannot run lenslet doctor: {e}",
            "无法运行 lenslet doctor：{e}"
        )
    })?;
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        if let Some(pipe) = stdout.as_mut() {
            let _ = pipe.read_to_string(&mut out);
        }
        out
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                signal_group(child.id(), Signal::Kill);
                let _ = child.kill();
                let _ = child.wait();
                return Err(tr!(
                    "lenslet doctor did not answer",
                    "lenslet doctor 没有响应"
                ));
            }
        }
    }
    let out = reader.join().unwrap_or_default();
    parse_doctor(&out).ok_or_else(|| {
        tr!(
            "lenslet doctor printed no JSON — is the CLI older than the window?",
            "lenslet doctor 没有输出 JSON — 命令行工具是否比窗口旧？"
        )
    })
}

// -- finding the CLI -------------------------------------------------------------

/// A complete developer `PATH` even when `Lenslet.app` was launched from
/// Finder, where `launchd` only sets `/usr/bin:/bin`.
pub fn augmented_path() -> OsString {
    static AUGMENTED: OnceLock<OsString> = OnceLock::new();
    AUGMENTED
        .get_or_init(|| {
            let mut dirs: Vec<PathBuf> = Vec::new();
            let mut push = |p: PathBuf| {
                if !p.as_os_str().is_empty() && !dirs.contains(&p) {
                    dirs.push(p);
                }
            };
            if let Some(cur) = std::env::var_os("PATH") {
                std::env::split_paths(&cur).for_each(&mut push);
            }
            #[cfg(unix)]
            if let Some(shell_path) = login_shell_path() {
                std::env::split_paths(&shell_path).for_each(&mut push);
            }
            if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
                for rel in [
                    ".local/bin",
                    ".cargo/bin",
                    ".pyenv/shims",
                    ".asdf/shims",
                    ".volta/bin",
                    ".bun/bin",
                    "go/bin",
                ] {
                    push(home.join(rel));
                }
            }
            for sys in [
                "/opt/homebrew/bin",
                "/usr/local/bin",
                "/usr/bin",
                "/bin",
                "/usr/sbin",
                "/sbin",
            ] {
                push(PathBuf::from(sys));
            }
            std::env::join_paths(dirs.into_iter().filter(|d| d.is_dir()))
                .unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
        })
        .clone()
}

#[cfg(unix)]
fn login_shell_path() -> Option<OsString> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string());
    let mut child = Command::new(&shell)
        .args(["-lc", "printf '%s' \"$PATH\""])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_millis(300);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            if !status.success() {
                return None;
            }
            let mut out = String::new();
            child.stdout.take()?.read_to_string(&mut out).ok()?;
            let trimmed = out.trim();
            return (!trimmed.is_empty()).then(|| OsString::from(trimmed));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// The `lenslet` CLI next to `lenslet-ui` (inside `Lenslet.app` or a release
/// directory), else the one on `PATH`.
pub fn cli_binary() -> Result<PathBuf, Tr> {
    fsguard::assert_io_allowed();
    let name = if cfg!(windows) {
        "lenslet.exe"
    } else {
        "lenslet"
    };
    if let Ok(exe) = std::env::current_exe().and_then(|p| p.canonicalize())
        && let Some(candidate) = exe.parent().map(|dir| dir.join(name))
        && candidate.is_file()
    {
        return Ok(candidate);
    }
    std::env::split_paths(&augmented_path())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            tr!(
                "The lenslet CLI was not found next to the window or on PATH",
                "未在窗口旁或 PATH 中找到 lenslet 命令行工具"
            )
        })
}

// -- running the tests -----------------------------------------------------------

/// A `lenslet run` the window started.
pub struct ManagedRun {
    pub child: Child,
    pub command_line: String,
}

/// The arguments after `lenslet`: `run -- <command>`. A command with shell
/// syntax runs through `sh -c`; a plain one is split on whitespace so the
/// run header shows the command itself.
pub fn run_args(command_line: &str) -> Vec<String> {
    let command = command_line.trim();
    let mut args = vec!["run".to_string(), "--".to_string()];
    let shell = command
        .chars()
        .any(|c| "|&;<>()$`\\\"'*?[]#~=%{}!".contains(c));
    if shell {
        if cfg!(windows) {
            args.extend(["cmd".into(), "/C".into(), command.into()]);
        } else {
            args.extend(["sh".into(), "-c".into(), command.into()]);
        }
    } else {
        args.extend(command.split_whitespace().map(str::to_string));
    }
    args
}

/// Start `<bin> run -- <command>` in `cwd`. The CLI picks the run file and
/// writes `<run>.log` itself; the window follows the newest run.
pub fn spawn_run(bin: &Path, cwd: &Path, command_line: &str) -> Result<ManagedRun, Tr> {
    fsguard::assert_io_allowed();
    let command = command_line.trim();
    if command.is_empty() {
        return Err(tr!(
            "Set a test command first (for example pytest or npm test)",
            "请先设置测试命令（例如 pytest 或 npm test）"
        ));
    }
    if !cwd.is_dir() {
        let dir = cwd.display();
        return Err(tr!(
            "The project folder {dir} does not exist",
            "项目目录 {dir} 不存在"
        ));
    }
    let mut cmd = Command::new(bin);
    cmd.env("PATH", augmented_path())
        .current_dir(cwd)
        .args(run_args(command))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_child(&mut cmd);
    let child = cmd.spawn().map_err(|e| {
        tr!(
            "Could not start lenslet run: {e}",
            "无法启动 lenslet run：{e}"
        )
    })?;
    Ok(ManagedRun {
        child,
        command_line: command.to_string(),
    })
}

/// Stop a run: `SIGTERM` to its process group now (the CLI ends the run
/// gracefully and writes its report), `SIGKILL` after [`STOP_GRACE`]. Never
/// blocks the caller.
pub fn stop_run(run: ManagedRun) {
    terminate_and_reap(vec![run.child], STOP_GRACE);
}

// -- stopping children ------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
}

/// Put a child in its own process group and, on Linux, have the kernel send
/// it `SIGTERM` if the window dies first.
pub fn configure_child(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: only async-signal-safe libc calls run between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                if libc::getppid() != parent {
                    libc::raise(libc::SIGTERM);
                }
                Ok(())
            });
        }
    }
}

/// Signal a child's process group (falling back to the process itself).
#[cfg(unix)]
pub fn signal_group(pid: u32, sig: Signal) {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    if pid <= 1 {
        return;
    }
    let signo = match sig {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: plain syscalls on a pid we spawned.
    unsafe {
        if libc::kill(-pid, signo) != 0 {
            libc::kill(pid, signo);
        }
    }
}

#[cfg(not(unix))]
pub fn signal_group(_pid: u32, _sig: Signal) {}

/// `SIGTERM` every child's group now and return; a background thread reaps
/// them and sends `SIGKILL` to any still running after `grace`.
pub fn terminate_and_reap(children: Vec<Child>, grace: Duration) {
    let mut live = Vec::new();
    for mut child in children {
        if child.try_wait().ok().flatten().is_none() {
            signal_group(child.id(), Signal::Term);
            live.push(child);
        }
    }
    if live.is_empty() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("lenslet-reaper".into())
        .spawn(move || reap(live, grace));
}

fn reap(mut children: Vec<Child>, grace: Duration) {
    let deadline = Instant::now() + grace;
    while !children.is_empty() && Instant::now() < deadline {
        children.retain_mut(|c| c.try_wait().ok().flatten().is_none());
        std::thread::sleep(Duration::from_millis(20));
    }
    for mut child in children {
        signal_group(child.id(), Signal::Kill);
        let _ = child.kill();
        let _ = child.wait();
    }
}

// -- the crash-cleanup registry ------------------------------------------------------

/// One child the window started, recorded so a later window can stop it
/// after a crash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildEntry {
    pub pid: u32,
    /// The `lenslet-ui` process that started it.
    pub owner: u32,
}

fn registry_path(home: &Path) -> PathBuf {
    home.join(".lenslet/ui-children.json")
}

fn read_registry(home: &Path) -> Vec<ChildEntry> {
    fs::read_to_string(registry_path(home))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_registry(home: &Path, entries: &[ChildEntry]) {
    let path = registry_path(home);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(entries) {
        let tmp = path.with_extension(format!("json.{}", std::process::id()));
        if fs::write(&tmp, text).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }
}

pub fn record_child(home: &Path, pid: u32) {
    fsguard::assert_io_allowed();
    let mut entries = read_registry(home);
    entries.retain(|e| e.pid != pid);
    entries.push(ChildEntry {
        pid,
        owner: std::process::id(),
    });
    write_registry(home, &entries);
}

pub fn forget_child(home: &Path, pid: u32) {
    fsguard::assert_io_allowed();
    let mut entries = read_registry(home);
    let before = entries.len();
    entries.retain(|e| e.pid != pid);
    if entries.len() != before {
        write_registry(home, &entries);
    }
}

/// Split the registry into children to stop now and entries to keep: a
/// child is stopped when it is still a `lenslet run` (its pid was not
/// reused) and the window that started it is gone.
pub fn plan_cleanup(
    entries: &[ChildEntry],
    me: u32,
    is_lenslet_run: impl Fn(u32) -> bool,
    is_live_window: impl Fn(u32) -> bool,
) -> (Vec<u32>, Vec<ChildEntry>) {
    let mut stop = Vec::new();
    let mut keep = Vec::new();
    for entry in entries.iter().filter(|e| is_lenslet_run(e.pid)) {
        if entry.owner == me || is_live_window(entry.owner) {
            keep.push(entry.clone());
        } else {
            stop.push(entry.pid);
        }
    }
    (stop, keep)
}

/// The command line of a live process, or `None` when it is gone.
fn process_cmdline(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        Some(String::from_utf8_lossy(&raw).replace('\0', " "))
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let out = Command::new("ps")
            .args(["-o", "command=", "-p", &pid.to_string()])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!text.is_empty()).then_some(text)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}

fn looks_like_lenslet_run(cmdline: &str) -> bool {
    let first = cmdline.split_whitespace().next().unwrap_or("");
    let exe = first.rsplit('/').next().unwrap_or(first);
    (exe == "lenslet" || exe == "lenslet.exe") && cmdline.contains(" run")
}

/// On startup: stop runs a crashed window left behind.
pub fn cleanup_stale_children(home: &Path) -> usize {
    fsguard::assert_io_allowed();
    let entries = read_registry(home);
    if entries.is_empty() {
        return 0;
    }
    let (stop, keep) = plan_cleanup(
        &entries,
        std::process::id(),
        |pid| process_cmdline(pid).is_some_and(|c| looks_like_lenslet_run(&c)),
        |owner| process_cmdline(owner).is_some_and(|c| c.contains("lenslet-ui")),
    );
    for pid in &stop {
        signal_group(*pid, Signal::Term);
    }
    write_registry(home, &keep);
    stop.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_doctor_json_with_the_test_command_and_derives_the_status() {
        let json = r#"{"version":"13.0.0","cli":"/usr/local/bin/lenslet","emulators":[],"managed":{"name":"moto","available":true,"command":["moto_server"]},"test_command":"pytest -q","lock":"lenslet.lock","ci_workflow":null}"#;
        let doctor = parse_doctor(json).unwrap();
        assert_eq!(doctor.test_command.as_deref(), Some("pytest -q"));
        assert_eq!(
            EmulatorStatus::from_doctor(Some(&Ok(doctor.clone()))),
            EmulatorStatus::WillStart("moto".into())
        );
        let running = parse_doctor(&json.replace(
            r#""emulators":[]"#,
            r#""emulators":[{"name":"moto","url":"http://127.0.0.1:5000"}]"#,
        ))
        .unwrap();
        assert!(matches!(
            EmulatorStatus::from_doctor(Some(&Ok(running))),
            EmulatorStatus::Running { .. }
        ));
        let none = parse_doctor(
            &json
                .replace(r#""available":true"#, r#""available":false"#)
                .replace(r#""test_command":"pytest -q""#, r#""test_command":null"#),
        )
        .unwrap();
        assert_eq!(none.test_command, None);
        assert_eq!(
            EmulatorStatus::from_doctor(Some(&Ok(none))),
            EmulatorStatus::Missing
        );
        assert!(parse_doctor("not json").is_none());
        assert_eq!(EmulatorStatus::from_doctor(None), EmulatorStatus::Checking);
    }

    #[test]
    fn run_args_wrap_only_shell_syntax() {
        assert_eq!(
            run_args(" pytest -q tests/ "),
            ["run", "--", "pytest", "-q", "tests/"]
        );
        if !cfg!(windows) {
            assert_eq!(
                run_args("npm test && echo ok"),
                ["run", "--", "sh", "-c", "npm test && echo ok"]
            );
            assert_eq!(
                run_args("AWS_REGION=eu-west-1 pytest"),
                ["run", "--", "sh", "-c", "AWS_REGION=eu-west-1 pytest"]
            );
        }
    }

    /// The real wiring with a stand-in CLI: it is started in the project
    /// with `run -- <cmd>`, and Stop reaches it as `SIGTERM`.
    #[cfg(unix)]
    #[test]
    fn spawns_lenslet_run_in_the_project_and_stops_it_with_sigterm() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::testdata::scratch("daemon-spawn");
        let project = dir.join("project");
        fs::create_dir_all(&project).unwrap();
        let bin = dir.join("lenslet");
        fs::write(
            &bin,
            "#!/bin/sh\npwd > args.txt\necho \"$@\" >> args.txt\ntrap 'echo term > stopped.txt; exit 0' TERM\nwhile true; do sleep 0.05; done\n",
        )
        .unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(spawn_run(&bin, &project, "  ").is_err());
        assert!(spawn_run(&bin, &dir.join("missing"), "pytest").is_err());
        let run = spawn_run(&bin, &project, "pytest -q").unwrap();
        assert_eq!(run.command_line, "pytest -q");
        let args = project.join("args.txt");
        let deadline = Instant::now() + Duration::from_secs(5);
        while fs::read_to_string(&args).map_or(true, |s| s.lines().count() < 2) {
            assert!(Instant::now() < deadline, "the stand-in CLI never started");
            std::thread::sleep(Duration::from_millis(20));
        }
        let text = fs::read_to_string(&args).unwrap();
        let mut lines = text.lines();
        assert_eq!(
            fs::canonicalize(lines.next().unwrap()).unwrap(),
            fs::canonicalize(&project).unwrap()
        );
        assert_eq!(lines.next().unwrap(), "run -- pytest -q");

        stop_run(run);
        let stopped = project.join("stopped.txt");
        while !stopped.exists() {
            assert!(Instant::now() < deadline, "Stop did not reach the run");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn plans_cleanup_of_runs_left_by_a_crashed_window() {
        let me = 100;
        let entries = vec![
            ChildEntry { pid: 1, owner: 50 }, // owner crashed: stop
            ChildEntry { pid: 2, owner: 60 }, // owner is another live window: keep
            ChildEntry { pid: 3, owner: 50 }, // already gone: drop
            ChildEntry { pid: 5, owner: me }, // mine: keep
        ];
        let (stop, keep) = plan_cleanup(
            &entries,
            me,
            |pid| matches!(pid, 1 | 2 | 5),
            |owner| owner == 60,
        );
        assert_eq!(stop, vec![1]);
        assert_eq!(keep.iter().map(|e| e.pid).collect::<Vec<_>>(), vec![2, 5]);
        assert!(looks_like_lenslet_run(
            "/usr/local/bin/lenslet run -- pytest"
        ));
        assert!(!looks_like_lenslet_run("/usr/bin/python3 -m http.server"));
        assert!(!looks_like_lenslet_run("/opt/lenslet-ui"));
    }

    #[test]
    fn registry_round_trips() {
        let dir = crate::testdata::scratch("registry");
        record_child(&dir, 4242);
        record_child(&dir, 4343);
        assert_eq!(read_registry(&dir).len(), 2);
        forget_child(&dir, 4242);
        let left = read_registry(&dir);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].pid, 4343);
        assert_eq!(left[0].owner, std::process::id());
    }
}
