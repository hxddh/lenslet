//! The window's control plane: the managed proxy and command runs, the CLI's
//! `doctor --json` (which emulators are running, whether Lenslet can start
//! moto), CI workflow generation, shell snippets and CLI installation.
//!
//! Every function that touches the disk, the network or a process checks
//! [`crate::fsguard::assert_io_allowed`] and runs on GPUI's background
//! executor. Children run in their own process group, die with the window on
//! Linux (`PR_SET_PDEATHSIG`), and are recorded in `~/.lenslet/ui-children.json`
//! so a window that crashed can clean up after itself on its next start.

use std::{
    ffi::OsString,
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{fsguard, i18n::Tr, tr};

pub const DEFAULT_PROXY_PORT: u16 = 9297;
/// The most stdout/stderr the window keeps for a run.
pub const MAX_LOG_BYTES: usize = 256 * 1024;
/// The install hint shown when no emulator can be found or started.
pub const MOTO_INSTALL: &str = "pip install 'moto[server]'";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShellFormat {
    #[default]
    Sh,
    Fish,
    Pwsh,
    Dotenv,
    Docker,
}

impl ShellFormat {
    pub const ALL: [ShellFormat; 5] = [
        ShellFormat::Sh,
        ShellFormat::Fish,
        ShellFormat::Pwsh,
        ShellFormat::Dotenv,
        ShellFormat::Docker,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ShellFormat::Sh => "sh / zsh",
            ShellFormat::Fish => "fish",
            ShellFormat::Pwsh => "PowerShell",
            ShellFormat::Dotenv => ".env",
            ShellFormat::Docker => "docker",
        }
    }
}

/// What `/_lenslet/health` says about the proxy on the managed port.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProxyHealth {
    pub running: bool,
    pub port: u16,
    pub pid: Option<u32>,
    pub upstream: Option<String>,
    pub enforce: bool,
    pub calls: usize,
    pub errors: usize,
}

// -- doctor --------------------------------------------------------------------

/// A running, protocol-verified emulator, as `lenslet doctor --json` lists it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Emulator {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: String,
}

/// The emulator Lenslet starts itself when none is running.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct ManagedEmulator {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub command: Option<Vec<String>>,
}

/// The parts of `lenslet doctor --json` the window shows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Doctor {
    #[serde(default)]
    pub emulators: Vec<Emulator>,
    #[serde(default)]
    pub managed: Option<ManagedEmulator>,
}

pub fn parse_doctor(json: &str) -> Option<Doctor> {
    serde_json::from_str(json.trim()).ok()
}

/// What the next run will talk to, in the words the window uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmulatorStatus {
    /// `doctor` has not answered yet.
    Checking,
    /// The CLI could not be run.
    NoCli,
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
            Some(Err(_)) => EmulatorStatus::NoCli,
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
            EmulatorStatus::NoCli => tr!(
                "The lenslet CLI was not found next to the window or on PATH",
                "未在窗口旁或 PATH 中找到 lenslet 命令行工具"
            ),
            EmulatorStatus::Running { name, url } => {
                tr!("{name} running at {url}", "{name} 正在 {url} 运行")
            }
            EmulatorStatus::WillStart(name) => tr!(
                "No emulator — Lenslet will start {name}",
                "未发现模拟器 — Lenslet 会自动启动 {name}"
            ),
            EmulatorStatus::Missing => tr!(
                "No emulator found — install moto: {MOTO_INSTALL}",
                "未发现模拟器 — 请安装 moto：{MOTO_INSTALL}"
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

/// Run `lenslet doctor --json`, bounded by `timeout`.
pub fn run_doctor(timeout: Duration) -> Result<Doctor, Tr> {
    fsguard::assert_io_allowed();
    let bin = sibling_cli_binary()?;
    let mut cmd = Command::new(&bin);
    cmd.env("PATH", augmented_path())
        .args(["doctor", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
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
            "lenslet doctor printed no JSON (is the CLI older than the window?)",
            "lenslet doctor 没有输出 JSON（命令行工具版本是否过旧？）"
        )
    })
}

// -- paths ---------------------------------------------------------------------

/// A complete developer `PATH` even when `Lenslet.app` was launched from
/// Finder, where `launchd` only sets `/usr/bin:/bin`.
pub fn augmented_path() -> OsString {
    static AUGMENTED: OnceLock<OsString> = OnceLock::new();
    AUGMENTED
        .get_or_init(|| {
            let mut dirs: Vec<PathBuf> = Vec::new();
            let mut push_unique = |p: PathBuf| {
                if !p.as_os_str().is_empty() && !dirs.contains(&p) {
                    dirs.push(p);
                }
            };
            if let Some(cur) = std::env::var_os("PATH") {
                for p in std::env::split_paths(&cur) {
                    push_unique(p);
                }
            }
            #[cfg(unix)]
            if let Some(shell_path) = probe_login_shell_path() {
                for p in std::env::split_paths(&shell_path) {
                    push_unique(p);
                }
            }
            if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
                for rel in [
                    ".local/bin",
                    ".cargo/bin",
                    ".orbstack/bin",
                    ".rd/bin",
                    ".volta/bin",
                    ".bun/bin",
                    ".pyenv/shims",
                    ".asdf/shims",
                    "go/bin",
                ] {
                    let candidate = home.join(rel);
                    if candidate.is_dir() {
                        push_unique(candidate);
                    }
                }
            }
            for sys in [
                "/opt/homebrew/bin",
                "/opt/homebrew/sbin",
                "/usr/local/bin",
                "/usr/local/sbin",
                "/usr/bin",
                "/bin",
                "/usr/sbin",
                "/sbin",
            ] {
                let p = PathBuf::from(sys);
                if p.is_dir() {
                    push_unique(p);
                }
            }
            std::env::join_paths(dirs)
                .unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
        })
        .clone()
}

#[cfg(unix)]
fn probe_login_shell_path() -> Option<OsString> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/bin/zsh".to_string());
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

pub fn find_in_path(bin_name: &str) -> Option<PathBuf> {
    let exe_name = if cfg!(windows) && !bin_name.ends_with(".exe") {
        format!("{bin_name}.exe")
    } else {
        bin_name.to_string()
    };
    std::env::split_paths(&augmented_path())
        .map(|dir| dir.join(&exe_name))
        .find(|candidate| candidate.is_file())
}

/// The `lenslet` CLI next to `lenslet-ui` (inside `Lenslet.app` or a release
/// directory), else the one on `PATH`.
pub fn sibling_cli_binary() -> Result<PathBuf, Tr> {
    fsguard::assert_io_allowed();
    let name = if cfg!(windows) {
        "lenslet.exe"
    } else {
        "lenslet"
    };
    if let Ok(exe) = std::env::current_exe().and_then(|p| p.canonicalize())
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    find_in_path("lenslet").ok_or_else(|| {
        tr!(
            "The lenslet CLI was not found next to the window or on PATH",
            "未在窗口旁或 PATH 中找到 lenslet 命令行工具"
        )
    })
}

// -- the managed proxy ------------------------------------------------------------

pub fn probe_proxy(port: u16) -> ProxyHealth {
    fsguard::assert_io_allowed();
    let mut health = ProxyHealth {
        port,
        ..ProxyHealth::default()
    };
    if let Some(json) = http_get_json(port, "/_lenslet/health") {
        health.running = true;
        health.pid = json.get("pid").and_then(|v| v.as_u64()).map(|n| n as u32);
        health.upstream = json
            .get("upstream")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        health.enforce = json
            .get("enforce")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        health.calls = json.get("calls").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        health.errors = json.get("errors").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    }
    health
}

fn http_get_json(port: u16, path: &str) -> Option<serde_json::Value> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(120)).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(180)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(180)));
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::with_capacity(1024);
    stream.read_to_end(&mut buf).ok()?;
    let text = std::str::from_utf8(&buf).ok()?;
    if !text.starts_with("HTTP/1.1 200") && !text.starts_with("HTTP/1.0 200") {
        return None;
    }
    let (_, body) = text.split_once("\r\n\r\n")?;
    serde_json::from_str(body.trim()).ok()
}

#[derive(Clone, Debug)]
pub struct ProxyLaunchOpts {
    pub port: u16,
    pub upstream: Option<String>,
    pub routes: Vec<String>,
    pub enforce: bool,
    pub allow_docker: bool,
    pub cwd: Option<PathBuf>,
}

/// Whether a health reply belongs to the proxy we spawned (and not to one
/// that already held the port).
pub fn health_is_ours(health_pid: Option<u32>, child_pid: u32) -> bool {
    health_pid == Some(child_pid)
}

/// Start `lenslet run` as a managed proxy and wait until *it* answers
/// `/_lenslet/health`. Fails with the proxy's first stderr line when it
/// exits, and when another process answers or nothing answers in time.
pub fn spawn_proxy_verified(opts: &ProxyLaunchOpts) -> Result<Child, Tr> {
    fsguard::assert_io_allowed();
    let bin = sibling_cli_binary()?;
    let err_path = std::env::temp_dir().join(format!(
        "lenslet-proxy-err-{}-{}.log",
        std::process::id(),
        opts.port
    ));
    let err_file = fs::File::create(&err_path)
        .map_err(|e| tr!("Cannot open the proxy log: {e}", "无法打开代理日志：{e}"))?;

    let mut cmd = Command::new(&bin);
    cmd.env("PATH", augmented_path())
        .arg("run")
        .arg("--listen")
        .arg(opts.port.to_string())
        .arg("--quiet")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err_file));
    push_network_args(
        &mut cmd,
        opts.upstream.as_deref(),
        &opts.routes,
        opts.allow_docker,
    );
    if opts.enforce {
        cmd.arg("--enforce");
    }
    if let Some(dir) = opts.cwd.as_deref().filter(|d| d.is_dir()) {
        cmd.current_dir(dir);
    }
    configure_child(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| tr!("Could not start the proxy: {e}", "无法启动本地代理：{e}"))?;
    let pid = child.id();

    let deadline = Instant::now() + Duration::from_secs(5);
    let result = loop {
        if let Ok(Some(status)) = child.try_wait() {
            let detail = fs::read_to_string(&err_path).unwrap_or_default();
            let line = detail
                .lines()
                .find(|l| !l.trim().is_empty())
                .map(str::trim)
                .unwrap_or_default()
                .to_string();
            let code = status.code().unwrap_or(1);
            break Err(if line.is_empty() {
                tr!(
                    "The proxy exited right away (exit {code})",
                    "本地代理启动后立即退出 (exit {code})"
                )
            } else {
                tr!("{line} (exit {code})", "{line} (exit {code})")
            });
        }
        if let Some(json) = http_get_json(opts.port, "/_lenslet/health") {
            let health_pid = json.get("pid").and_then(|v| v.as_u64()).map(|n| n as u32);
            if health_is_ours(health_pid, pid) {
                break Ok(());
            }
        }
        if Instant::now() >= deadline {
            let port = opts.port;
            break Err(tr!(
                "The proxy did not answer on port {port} (is another one already using it?)",
                "本地代理未在端口 {port} 上响应（是否已有其他进程占用？）"
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let _ = fs::remove_file(&err_path);
    match result {
        Ok(()) => Ok(child),
        Err(err) => {
            terminate_and_reap(vec![child], Duration::from_millis(500));
            Err(err)
        }
    }
}

fn push_network_args(
    cmd: &mut Command,
    upstream: Option<&str>,
    routes: &[String],
    allow_docker: bool,
) {
    if let Some(up) = upstream.map(str::trim).filter(|s| !s.is_empty()) {
        cmd.arg("--upstream").arg(up);
    }
    for route in routes.iter().map(|r| r.trim()).filter(|r| !r.is_empty()) {
        cmd.arg("--route").arg(route);
    }
    if allow_docker {
        cmd.arg("--allow-docker");
    }
}

// -- command runs --------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct CommandRunOpts {
    pub command_line: String,
    pub cwd: PathBuf,
    pub upstream: Option<String>,
    pub routes: Vec<String>,
    /// Block calls outside the lock (`--enforce`).
    pub enforce: bool,
    /// Accept calls from Docker containers (`--allow-docker`).
    pub allow_docker: bool,
    pub runs_dir: PathBuf,
}

pub struct ManagedRun {
    pub child: Child,
    pub trace_path: PathBuf,
    pub command_line: String,
}

/// Launch a command under `lenslet run`, writing the trace to a new run file
/// and stdout/stderr to its companion `.log`.
pub fn spawn_command_run(opts: &CommandRunOpts) -> Result<ManagedRun, Tr> {
    fsguard::assert_io_allowed();
    let command = opts.command_line.trim();
    if command.is_empty() {
        return Err(tr!(
            "Set a test command first (for example pytest or npm test)",
            "请先设置测试命令（例如 pytest 或 npm test）"
        ));
    }
    if !opts.cwd.is_dir() {
        let dir = opts.cwd.display();
        return Err(tr!(
            "The project folder {dir} does not exist",
            "项目目录 {dir} 不存在"
        ));
    }
    let bin = sibling_cli_binary()?;
    let trace_path = lenslet_core::runs::create(&opts.runs_dir)
        .map_err(|e| tr!("Cannot create the run file: {e}", "无法创建运行文件：{e}"))?;
    let log_path = lenslet_core::runs::log_path_for(&trace_path);

    let mut cmd = Command::new(&bin);
    cmd.env("PATH", augmented_path())
        .current_dir(&opts.cwd)
        .arg("run")
        .arg("--trace")
        .arg(&trace_path)
        .arg("--log")
        .arg(&log_path);
    push_network_args(
        &mut cmd,
        opts.upstream.as_deref(),
        &opts.routes,
        opts.allow_docker,
    );
    if opts.enforce {
        cmd.arg("--enforce");
    }
    cmd.arg("--");
    if cfg!(windows) {
        cmd.args(["cmd", "/C", command]);
    } else {
        cmd.args(["sh", "-c", command]);
    }
    cmd.stdin(Stdio::null())
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
        trace_path,
        command_line: command.to_string(),
    })
}

/// The proxy port a run file's header names (`endpoint`), if it has one yet.
pub fn run_endpoint_port(trace_path: &Path) -> Option<u16> {
    fsguard::assert_io_allowed();
    let header = lenslet_core::runs::header(trace_path)?;
    endpoint_port(&header.endpoint)
}

pub fn endpoint_port(endpoint: &str) -> Option<u16> {
    endpoint
        .trim_end_matches('/')
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .filter(|p| *p > 0)
}

/// Discover test commands from files in `cwd`. Empty when nothing matches:
/// the window then asks for a command instead of guessing one.
pub fn detect_project_commands(cwd: &Path) -> Vec<String> {
    fsguard::assert_io_allowed();
    let mut out: Vec<String> = Vec::new();
    let mut push = |cmd: String| {
        if !out.contains(&cmd) {
            out.push(cmd);
        }
    };
    if let Ok(text) = fs::read_to_string(cwd.join("package.json"))
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(scripts) = val.get("scripts").and_then(|s| s.as_object())
    {
        let runner = if cwd.join("pnpm-lock.yaml").is_file() {
            "pnpm"
        } else if cwd.join("yarn.lock").is_file() {
            "yarn"
        } else if cwd.join("bun.lockb").is_file() || cwd.join("bun.lock").is_file() {
            "bun run"
        } else {
            "npm run"
        };
        for key in ["test", "test:integration", "test:e2e"] {
            if scripts.contains_key(key) {
                push(if key == "test" && runner == "npm run" {
                    "npm test".to_string()
                } else {
                    format!("{runner} {key}")
                });
            }
        }
    }
    if cwd.join("pyproject.toml").is_file()
        || cwd.join("pytest.ini").is_file()
        || cwd.join("requirements.txt").is_file()
    {
        if cwd.join("uv.lock").is_file() {
            push("uv run pytest".into());
        }
        push("pytest".into());
    }
    if cwd.join("Cargo.toml").is_file() {
        push("cargo test".into());
    }
    if cwd.join("go.mod").is_file() {
        push("go test ./...".into());
    }
    if cwd.join("Makefile").is_file() {
        push("make test".into());
    }
    out
}

// -- CI ------------------------------------------------------------------------

pub enum CiError {
    /// The workflow file is already there; ask before replacing it.
    Exists(PathBuf),
    Failed(Tr),
}

pub fn ci_workflow_path(cwd: &Path) -> PathBuf {
    cwd.join(".github/workflows/lenslet.yml")
}

/// The workflow text. With no upstream the action's `lenslet run` finds or
/// starts moto, so the job installs it first.
pub fn ci_workflow_yaml(command_line: &str, upstream: Option<&str>) -> String {
    let yaml_quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let command = match command_line.trim() {
        "" => "npm test",
        other => other,
    };
    let version = env!("CARGO_PKG_VERSION");
    let mut out = String::from(
        "# Lenslet: show the AWS access each pull request needs, and fail when it grows.\n\
         # Docs: https://github.com/hxddh/lenslet#ci\n\
         name: Lenslet\n\
         on:\n  pull_request:\n  push:\n    branches: [main]\n\
         permissions:\n  contents: read\n  pull-requests: write\n\
         jobs:\n  aws-access:\n    runs-on: ubuntu-latest\n    steps:\n\
         \x20     - uses: actions/checkout@v5\n",
    );
    let upstream = upstream.map(str::trim).filter(|u| !u.is_empty());
    if upstream.is_none() {
        out.push_str(&format!("      - run: {MOTO_INSTALL}\n"));
    }
    out.push_str(&format!(
        "      - uses: hxddh/lenslet@lenslet-v{version}\n        with:\n          command: {}\n",
        yaml_quote(command)
    ));
    if let Some(up) = upstream {
        out.push_str(&format!("          upstream: {}\n", yaml_quote(up)));
    }
    out
}

/// Write `.github/workflows/lenslet.yml`; an existing file is only replaced
/// with `overwrite`.
pub fn generate_ci_workflow(
    cwd: &Path,
    command_line: &str,
    upstream: Option<&str>,
    overwrite: bool,
) -> Result<PathBuf, CiError> {
    fsguard::assert_io_allowed();
    if !cwd.is_dir() {
        let dir = cwd.display();
        return Err(CiError::Failed(tr!(
            "The project folder {dir} does not exist",
            "项目目录 {dir} 不存在"
        )));
    }
    let path = ci_workflow_path(cwd);
    if path.exists() && !overwrite {
        return Err(CiError::Exists(path));
    }
    let write = || -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, ci_workflow_yaml(command_line, upstream))
    };
    write().map_err(|e| {
        let p = path.display();
        CiError::Failed(tr!("Cannot write {p}: {e}", "无法写入 {p}：{e}"))
    })?;
    Ok(path)
}

/// The last `max_bytes` (at most [`MAX_LOG_BYTES`]) of a run's stdout/stderr log.
pub fn read_run_log(log_path: &Path, max_bytes: usize) -> Option<String> {
    fsguard::assert_io_allowed();
    let max_bytes = max_bytes.min(MAX_LOG_BYTES);
    let mut file = fs::File::open(log_path).ok()?;
    let len = file.metadata().ok()?.len() as usize;
    if len == 0 {
        return Some(String::new());
    }
    let mut buf = Vec::new();
    if len > max_bytes {
        use std::io::{Seek, SeekFrom};
        let _ = file.seek(SeekFrom::Start((len - max_bytes) as u64));
    }
    file.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
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
/// them and sends `SIGKILL` to any still running after `grace`. Safe to call
/// from the main thread (quit, window close): it never waits.
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

/// Ask `lenslet run` on `port` to seal its trace and exit.
fn request_shutdown(port: u16) {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    if let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(100)) {
        let _ = stream.set_read_timeout(Some(Duration::from_millis(150)));
        let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
        let req = format!(
            "POST /_lenslet/shutdown HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        if stream.write_all(req.as_bytes()).is_ok() {
            let mut drain = [0u8; 256];
            let _ = stream.read(&mut drain);
        }
    }
}

/// Stop a managed `lenslet run` cleanly (shutdown request on its own port,
/// then `SIGTERM`, then `SIGKILL` after `grace`). Blocks: background only.
pub fn stop_gracefully(mut child: Child, port: Option<u16>, grace: Duration) {
    fsguard::assert_io_allowed();
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    if let Some(port) = port {
        request_shutdown(port);
    }
    signal_group(child.id(), Signal::Term);
    reap(vec![child], grace);
}

/// Stop a command run using *its* proxy port, read from its own run file.
pub fn stop_managed_run(run: ManagedRun) {
    let port = run_endpoint_port(&run.trace_path);
    stop_gracefully(run.child, port, Duration::from_millis(2_500));
}

// -- the child registry -------------------------------------------------------------

/// One child the window started, recorded so a later window can clean up
/// after a crash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildEntry {
    pub pid: u32,
    /// The `lenslet-ui` process that started it.
    pub owner: u32,
}

pub fn registry_path(home: &Path) -> PathBuf {
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

pub fn forget_children(home: &Path, pids: &[u32]) {
    fsguard::assert_io_allowed();
    let mut entries = read_registry(home);
    let before = entries.len();
    entries.retain(|e| !pids.contains(&e.pid));
    if entries.len() != before {
        write_registry(home, &entries);
    }
}

/// Split the registry into children to stop now and entries to keep.
///
/// A child is stopped when it is still alive, still looks like a `lenslet`
/// process (its pid was not reused), and the window that started it is gone.
/// Children of another live window are kept; dead ones are dropped.
pub fn plan_cleanup(
    entries: &[ChildEntry],
    me: u32,
    is_lenslet_child: impl Fn(u32) -> bool,
    is_live_window: impl Fn(u32) -> bool,
) -> (Vec<u32>, Vec<ChildEntry>) {
    let mut stop = Vec::new();
    let mut keep = Vec::new();
    for entry in entries {
        if !is_lenslet_child(entry.pid) {
            continue;
        }
        if entry.owner != me && is_live_window(entry.owner) {
            keep.push(entry.clone());
        } else if entry.owner != me {
            stop.push(entry.pid);
        } else {
            keep.push(entry.clone());
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

fn looks_like_lenslet_child(cmdline: &str) -> bool {
    let first = cmdline.split_whitespace().next().unwrap_or("");
    let exe = first.rsplit('/').next().unwrap_or(first);
    (exe == "lenslet" || exe == "lenslet.exe") && cmdline.contains(" run")
        || (exe == "lenslet" && cmdline.contains(" doctor"))
}

/// On startup: stop children a crashed window left behind.
pub fn cleanup_stale_children(home: &Path) -> usize {
    fsguard::assert_io_allowed();
    let entries = read_registry(home);
    if entries.is_empty() {
        return 0;
    }
    let (stop, keep) = plan_cleanup(
        &entries,
        std::process::id(),
        |pid| process_cmdline(pid).is_some_and(|c| looks_like_lenslet_child(&c)),
        |owner| process_cmdline(owner).is_some_and(|c| c.contains("lenslet-ui")),
    );
    for pid in &stop {
        signal_group(*pid, Signal::Term);
    }
    write_registry(home, &keep);
    stop.len()
}

// -- install & shells ------------------------------------------------------------

/// Symlink the bundled `lenslet` CLI into `/usr/local/bin` when writable, else
/// `~/.local/bin`.
pub fn install_cli_to_path(home: &Path) -> Result<PathBuf, Tr> {
    fsguard::assert_io_allowed();
    let src = sibling_cli_binary()?;
    let usr_local = Path::new("/usr/local/bin/lenslet");
    if cfg!(unix)
        && let Some(parent) = usr_local.parent()
        && parent.is_dir()
        && try_link_binary(&src, usr_local).is_ok()
    {
        return Ok(usr_local.to_path_buf());
    }
    let local_bin = home.join(".local/bin");
    fs::create_dir_all(&local_bin).map_err(|e| {
        let d = local_bin.display();
        tr!("Cannot create {d}: {e}", "无法创建 {d}：{e}")
    })?;
    let dest = local_bin.join(if cfg!(windows) {
        "lenslet.exe"
    } else {
        "lenslet"
    });
    try_link_binary(&src, &dest)?;
    Ok(dest)
}

fn try_link_binary(src: &Path, dest: &Path) -> Result<(), Tr> {
    let tmp = dest.with_extension(format!("tmp.{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let d = dest.display();
    #[cfg(unix)]
    std::os::unix::fs::symlink(src, &tmp)
        .map_err(|e| tr!("Cannot link {d}: {e}", "无法链接 {d}：{e}"))?;
    #[cfg(not(unix))]
    fs::copy(src, &tmp).map_err(|e| tr!("Cannot copy {d}: {e}", "无法复制 {d}：{e}"))?;
    fs::rename(&tmp, dest).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        tr!("Cannot install {d}: {e}", "无法安装 {d}：{e}")
    })
}

pub fn shell_env_snippet_for(port: u16, fmt: ShellFormat) -> String {
    match fmt {
        ShellFormat::Sh => format!(
            "unset AWS_PROFILE AWS_DEFAULT_PROFILE AWS_SESSION_TOKEN\nexport AWS_ENDPOINT_URL=http://127.0.0.1:{port}\nexport AWS_ACCESS_KEY_ID=lenslet-local\nexport AWS_SECRET_ACCESS_KEY=lenslet-local\nexport AWS_DEFAULT_REGION=us-east-1"
        ),
        ShellFormat::Fish => format!(
            "set -e AWS_PROFILE AWS_DEFAULT_PROFILE AWS_SESSION_TOKEN\nset -gx AWS_ENDPOINT_URL http://127.0.0.1:{port}\nset -gx AWS_ACCESS_KEY_ID lenslet-local\nset -gx AWS_SECRET_ACCESS_KEY lenslet-local\nset -gx AWS_DEFAULT_REGION us-east-1"
        ),
        ShellFormat::Pwsh => format!(
            "Remove-Item Env:AWS_PROFILE, Env:AWS_DEFAULT_PROFILE, Env:AWS_SESSION_TOKEN -ErrorAction SilentlyContinue\n$env:AWS_ENDPOINT_URL = \"http://127.0.0.1:{port}\"\n$env:AWS_ACCESS_KEY_ID = \"lenslet-local\"\n$env:AWS_SECRET_ACCESS_KEY = \"lenslet-local\"\n$env:AWS_DEFAULT_REGION = \"us-east-1\""
        ),
        ShellFormat::Dotenv => format!(
            "AWS_ENDPOINT_URL=http://127.0.0.1:{port}\nAWS_ACCESS_KEY_ID=lenslet-local\nAWS_SECRET_ACCESS_KEY=lenslet-local\nAWS_DEFAULT_REGION=us-east-1"
        ),
        ShellFormat::Docker => format!(
            "-e AWS_ENDPOINT_URL=http://host.docker.internal:{port} -e AWS_ACCESS_KEY_ID=lenslet-local -e AWS_SECRET_ACCESS_KEY=lenslet-local -e AWS_DEFAULT_REGION=us-east-1"
        ),
    }
}

/// Write this window's terminal launcher (pre-configured with
/// `AWS_ENDPOINT_URL`) and open the platform terminal in `cwd`.
pub fn open_configured_terminal(home: &Path, cwd: &Path, port: u16) -> Result<(), Tr> {
    fsguard::assert_io_allowed();
    let script_path = write_terminal_launcher_script(home, cwd, port)?;

    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .args(["-a", "Terminal"])
            .arg(&script_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| tr!("Cannot open Terminal: {e}", "无法打开终端：{e}"))?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    {
        let script = script_path.to_string_lossy().into_owned();
        let candidates: &[(&str, &[&str])] = &[
            ("x-terminal-emulator", &["-e"]),
            ("gnome-terminal", &["--"]),
            ("kgx", &["--"]),
            ("konsole", &["-e"]),
            ("xfce4-terminal", &["-x"]),
            ("kitty", &[]),
            ("alacritty", &["-e"]),
            ("wezterm", &["start", "--"]),
        ];
        for (bin, prefix_args) in candidates {
            if let Some(exe) = find_in_path(bin) {
                let spawned = Command::new(exe)
                    .env("PATH", augmented_path())
                    .args(*prefix_args)
                    .arg(&script)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
                if spawned.is_ok() {
                    return Ok(());
                }
            }
        }
        Err(tr!(
            "No terminal app found; run lenslet shell in your terminal",
            "未找到终端程序；请在终端中运行 lenslet shell"
        ))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = script_path;
        Err(tr!(
            "Run lenslet shell in your terminal",
            "请在终端中运行 lenslet shell"
        ))
    }
}

/// This window's own launcher script, so two windows never overwrite each
/// other's.
pub fn terminal_script_path(home: &Path) -> PathBuf {
    home.join(".lenslet")
        .join(format!("lenslet-terminal-{}.command", std::process::id()))
}

pub fn write_terminal_launcher_script(home: &Path, cwd: &Path, port: u16) -> Result<PathBuf, Tr> {
    let script_path = terminal_script_path(home);
    let fail = |e: std::io::Error| {
        let p = script_path.display();
        tr!("Cannot write {p}: {e}", "无法写入 {p}：{e}")
    };
    if let Some(dir) = script_path.parent() {
        fs::create_dir_all(dir).map_err(fail)?;
    }
    let escaped_cwd = cwd.to_string_lossy().replace('\'', r"'\''");
    let content = format!(
        "#!/bin/sh\n\
         cd '{escaped_cwd}' 2>/dev/null || cd \"$HOME\"\n\
         unset AWS_PROFILE AWS_DEFAULT_PROFILE AWS_SESSION_TOKEN AWS_WEB_IDENTITY_TOKEN_FILE AWS_ROLE_ARN\n\
         export AWS_ENDPOINT_URL=\"http://127.0.0.1:{port}\"\n\
         export AWS_ACCESS_KEY_ID=\"lenslet-local\"\n\
         export AWS_SECRET_ACCESS_KEY=\"lenslet-local\"\n\
         export AWS_DEFAULT_REGION=\"us-east-1\"\n\
         export LENSLET_ACTIVE=\"1\"\n\
         printf '\\033[1;36m● Lenslet terminal ready\\033[0m · AWS_ENDPOINT_URL=%s\\n' \"$AWS_ENDPOINT_URL\"\n\
         printf '  Every AWS CLI / SDK / test command here is captured live in Lenslet.\\n\\n'\n\
         exec \"${{SHELL:-/bin/sh}}\" -i\n"
    );
    let mut opts = fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o700);
    }
    let mut file = opts.open(&script_path).map_err(fail)?;
    file.write_all(content.as_bytes()).map_err(fail)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&script_path, fs::Permissions::from_mode(0o700));
    }
    Ok(script_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_snippets_cover_all_supported_formats() {
        for fmt in ShellFormat::ALL {
            let text = shell_env_snippet_for(9297, fmt);
            assert!(text.contains("9297"), "missing port in {fmt:?}");
            assert!(text.contains("AWS_ENDPOINT_URL"), "{fmt:?}");
        }
        assert!(shell_env_snippet_for(9297, ShellFormat::Docker).contains("host.docker.internal"));
    }

    #[test]
    fn parses_doctor_json_and_derives_the_status() {
        let running = parse_doctor(
            r#"{"cli":"12.0.0","emulators":[{"name":"moto","url":"http://127.0.0.1:5000"}],
                "managed":{"name":"moto","available":true,"command":["moto_server","-p","0"]}}"#,
        )
        .unwrap();
        assert_eq!(running.emulators[0].url, "http://127.0.0.1:5000");
        assert_eq!(
            EmulatorStatus::from_doctor(Some(&Ok(running))),
            EmulatorStatus::Running {
                name: "moto".into(),
                url: "http://127.0.0.1:5000".into()
            }
        );

        let startable =
            parse_doctor(r#"{"emulators":[],"managed":{"name":"moto","available":true}}"#).unwrap();
        assert_eq!(
            EmulatorStatus::from_doctor(Some(&Ok(startable))),
            EmulatorStatus::WillStart("moto".into())
        );

        let missing = parse_doctor(
            r#"{"emulators":[],"managed":{"name":"moto","available":false,"command":null}}"#,
        )
        .unwrap();
        let status = EmulatorStatus::from_doctor(Some(&Ok(missing)));
        assert_eq!(status, EmulatorStatus::Missing);
        assert!(status.message().en.contains(MOTO_INSTALL));
        assert!(!status.ok());

        // Older CLIs without the fields, and garbage.
        assert_eq!(parse_doctor("{}"), Some(Doctor::default()));
        assert_eq!(parse_doctor("not json"), None);
        assert_eq!(EmulatorStatus::from_doctor(None), EmulatorStatus::Checking);
        assert_eq!(
            EmulatorStatus::from_doctor(Some(&Err(Tr::default()))),
            EmulatorStatus::NoCli
        );
    }

    #[test]
    fn plans_cleanup_of_children_left_by_a_crashed_window() {
        let me = 100;
        let entries = vec![
            ChildEntry { pid: 1, owner: 50 }, // owner crashed: stop
            ChildEntry { pid: 2, owner: 60 }, // owner is another live window: keep
            ChildEntry { pid: 3, owner: 50 }, // already gone: drop
            ChildEntry { pid: 4, owner: 50 }, // pid reused by something else: drop
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
        assert!(looks_like_lenslet_child(
            "/usr/local/bin/lenslet run --trace x -- sh -c pytest"
        ));
        assert!(!looks_like_lenslet_child("/usr/bin/python3 -m http.server"));
        assert!(!looks_like_lenslet_child("/opt/lenslet-ui"));
    }

    #[test]
    fn registry_round_trips() {
        let dir = std::env::temp_dir().join(format!("lenslet-registry-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        record_child(&dir, 4242);
        record_child(&dir, 4343);
        assert_eq!(read_registry(&dir).len(), 2);
        forget_children(&dir, &[4242]);
        let left = read_registry(&dir);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].pid, 4343);
        assert_eq!(left[0].owner, std::process::id());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn health_must_come_from_the_spawned_child() {
        assert!(health_is_ours(Some(7), 7));
        assert!(!health_is_ours(Some(8), 7));
        assert!(!health_is_ours(None, 7));
        assert_eq!(endpoint_port("http://127.0.0.1:43761"), Some(43761));
        assert_eq!(endpoint_port(""), None);
    }

    #[test]
    fn detects_commands_without_guessing_and_guards_ci_overwrite() {
        let dir = std::env::temp_dir().join(format!("lenslet-daemon-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(detect_project_commands(&dir).is_empty());
        fs::write(
            dir.join("package.json"),
            r#"{"scripts":{"test":"vitest","test:e2e":"playwright test"}}"#,
        )
        .unwrap();
        fs::write(dir.join("pnpm-lock.yaml"), "").unwrap();
        let cmds = detect_project_commands(&dir);
        assert_eq!(cmds, vec!["pnpm test", "pnpm test:e2e"]);

        let wf = generate_ci_workflow(&dir, "pnpm test", None, false)
            .ok()
            .unwrap();
        let yaml = fs::read_to_string(&wf).unwrap();
        assert!(yaml.contains("command: 'pnpm test'"));
        assert!(yaml.contains(MOTO_INSTALL));
        assert!(!yaml.contains("sandbox"));
        assert!(matches!(
            generate_ci_workflow(&dir, "pnpm test", Some("http://127.0.0.1:4566"), false),
            Err(CiError::Exists(_))
        ));
        let wf = generate_ci_workflow(&dir, "pnpm test", Some("http://127.0.0.1:4566"), true)
            .ok()
            .unwrap();
        let yaml = fs::read_to_string(&wf).unwrap();
        assert!(yaml.contains("upstream: 'http://127.0.0.1:4566'"));
        assert!(!yaml.contains("pip install"));

        let script = write_terminal_launcher_script(&dir, &dir, 9297).unwrap();
        assert!(
            script
                .to_string_lossy()
                .contains(&std::process::id().to_string())
        );
        let body = fs::read_to_string(&script).unwrap();
        assert!(body.contains("AWS_ENDPOINT_URL=\"http://127.0.0.1:9297\""));

        let log_path = dir.join("sample.log");
        fs::write(&log_path, "line1\nline2\n").unwrap();
        assert_eq!(
            read_run_log(&log_path, 64).as_deref(),
            Some("line1\nline2\n")
        );
        assert_eq!(read_run_log(&log_path, 6).as_deref(), Some("line2\n"));
        let _ = fs::remove_dir_all(&dir);
    }
}
