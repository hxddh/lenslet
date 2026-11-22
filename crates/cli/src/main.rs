//! `lenslet`: see, in every pull request, what AWS access your code needs,
//! and why its AWS calls failed.
//!
//! ```text
//! lenslet pytest              run the tests against a local emulator; report at the end
//! lenslet run --check -- …    the same, failing when access is not in lenslet.lock
//! lenslet check --policy P    would the newest run pass under an IAM policy?
//! lenslet policy              an IAM policy from lenslet.lock
//! lenslet ui                  the window
//! ```

mod capture;
mod emulator;
mod proxy;
mod trace;

pub(crate) use lenslet_core::{access, lock, policy, problem, runs, summary, wire};

use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::Write as _,
    path::{Path, PathBuf},
    process::{ExitCode, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::{
    proxy::{Options, Server},
    summary::{Report, plural},
    trace::RunHeader,
};

/// Exit code when the command passed but `--check` found access outside the
/// lock, or `lenslet check` found a denied permission.
const EXIT_ACCESS_CHANGED: i32 = 3;

fn usage() -> String {
    format!(
        "lenslet {version} — see what AWS access your code needs, and why its AWS calls failed

Usage:
  lenslet COMMAND [ARGS...]
      Shorthand for `lenslet run -- COMMAND`: run your tests (`lenslet pytest`,
      `lenslet npm test`) with AWS_ENDPOINT_URL pointing at Lenslet, in front
      of the local emulator that is running (or moto, started for the run).
  lenslet run [options] [--] COMMAND [ARGS...]
      The same, with options.
  lenslet check --policy PATH [RUN] [--json] [--markdown PATH]
      Would a recorded run (default: the newest finished run in this
      directory) pass under an IAM policy? Each denied permission and the
      statement that denies it. Exit {changed} if anything is denied.
  lenslet policy [--lock PATH | --run RUN]
      An IAM policy (JSON) from lenslet.lock (without `# test` lines) or a run.
  lenslet init [--emulator moto | --upstream ADDRESS] [--force] [-- COMMAND]
      Detect your test command, write lenslet.lock from it, and add
      .github/workflows/lenslet.yml (an existing one is kept without --force).
  lenslet doctor [--json]
      The emulators running, managed moto, your test command, lock and CI.
  lenslet ui
      Open the window (same as `lenslet` with no arguments).

Run options:
  --check              exit {changed} if the run used access not in the lock
  --strict             with --check: also exit {changed} for lock lines the run did not use
  --update             write the lock from this run (keeps `# test` marks)
  --lock PATH          the lock file (default ./lenslet.lock)
  --json PATH          also write the report as JSON (`-` for stdout)
  --markdown PATH      also write the report as Markdown (PR comments, job summaries)
  --quiet              no banner or report on stderr
  --ui                 open the window on this run
  --emulator moto      start moto (pip install 'moto[server]') for this run and stop it after
  --upstream ADDRESS   the emulator: an AWS Emulator port or an http(s) URL. Without it
                       (or --emulator), Lenslet uses a loopback AWS_ENDPOINT_URL, AWS
                       Emulator on 9298, LocalStack on 4566 or moto on 5000 (checked by
                       protocol), else starts moto when it is installed
  --allow-docker       listen on all interfaces and also serve calls from Docker
                       networks (172.16.0.0/12, 192.168.65.0/24)
  --emulator-ca PATH   emulator CA (default ~/.aws/emulator/PORT/ca.pem)
  --trace PATH         run file (default: a new file in ~/.lenslet/runs)

The command never sees real AWS credentials. Its output goes to the terminal
and to <run>.log next to the run file.

Exit codes: COMMAND's own code when it fails; {changed} when it passed but --check
found a change in access; 2 for Lenslet's own errors; otherwise 0.",
        version = env!("CARGO_PKG_VERSION"),
        changed = EXIT_ACCESS_CHANGED,
    )
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `~/.lenslet/runs/x.jsonl` for display.
fn tilde(path: &Path, home: &Path) -> String {
    summary::tilde(&path.display().to_string(), home)
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let command = if args.is_empty() {
        "ui".to_string()
    } else {
        args.remove(0)
    };
    let result = match command.as_str() {
        "run" => runtime().and_then(|runtime| runtime.block_on(run(args))),
        "init" => runtime().and_then(|runtime| runtime.block_on(init(args))),
        "doctor" => runtime().and_then(|runtime| runtime.block_on(doctor(args))),
        "check" => check(args),
        "policy" => policy_cmd(args),
        "ui" => open_ui(None).map(|()| 0),
        "-V" | "--version" | "version" => {
            println!("lenslet {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        "-h" | "--help" | "help" => {
            println!("{}", usage());
            Ok(0)
        }
        other if other != "lenslet" && !other.starts_with('-') && command_exists_on_path(other) => {
            let mut forwarded = Vec::with_capacity(args.len() + 2);
            forwarded.push("--".to_string());
            forwarded.push(other.to_string());
            forwarded.extend(args);
            runtime().and_then(|runtime| runtime.block_on(run(forwarded)))
        }
        other => Err(anyhow::anyhow!(
            "unknown command `{other}`; try `lenslet --help`"
        )),
    };
    match result {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(error) => {
            eprintln!("lenslet: {error:#}");
            ExitCode::from(2)
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("start the async runtime")
}

struct RunArgs {
    options: Options,
    command: Vec<String>,
    json: Option<String>,
    markdown: Option<String>,
    quiet: bool,
    ui: bool,
    check: bool,
    strict: bool,
    update: bool,
    lock: Option<PathBuf>,
}

fn parse_run(args: Vec<String>, home: &Path) -> Result<RunArgs> {
    let mut parsed = RunArgs {
        options: Options::new(home),
        command: Vec::new(),
        json: None,
        markdown: None,
        quiet: false,
        ui: false,
        check: false,
        strict: false,
        update: false,
        lock: None,
    };
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().with_context(|| format!("{arg} requires a value"));
        match arg.as_str() {
            "--" => {
                parsed.command.extend(it.by_ref());
                break;
            }
            "--json" => parsed.json = Some(value()?),
            "--markdown" => parsed.markdown = Some(value()?),
            "--lock" => parsed.lock = Some(PathBuf::from(value()?)),
            "--strict" => parsed.strict = true,
            "--quiet" | "-q" => parsed.quiet = true,
            "--ui" => parsed.ui = true,
            "--check" => parsed.check = true,
            "--update" => parsed.update = true,
            "-h" | "--help" => bail!("{}", usage()),
            flag if flag.starts_with('-') => {
                if !parsed.options.accept(flag, &mut value)? {
                    bail!("unknown option `{flag}`; try `lenslet --help`");
                }
            }
            _ => {
                parsed.command.push(arg);
                parsed.command.extend(it.by_ref());
                break;
            }
        }
    }
    if parsed.command.is_empty() {
        bail!("lenslet run needs a command, e.g. `lenslet run -- npm test`");
    }
    if parsed.check && parsed.update {
        bail!("use --check or --update, not both");
    }
    if parsed.strict && !parsed.check {
        bail!("--strict goes with --check");
    }
    Ok(parsed)
}

/// What the child process gets: variables to set and variables to remove,
/// so the banner can say exactly what Lenslet did.
#[derive(Debug, Default, PartialEq)]
struct ChildEnv {
    set: Vec<(String, String)>,
    remove: Vec<String>,
}

/// Placeholder keys for emulators that accept any credentials (moto,
/// LocalStack) when AWS Emulator's own credentials file is not there.
const LOCAL_KEYS: (&str, &str) = ("lenslet-local", "lenslet-local");

/// Real credentials never reach the emulator or the run file: keys,
/// profiles, OIDC web-identity tokens, container credential URIs and bearer
/// tokens are always removed from the child's environment.
const CREDENTIAL_VARS: &[&str] = &[
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_PROFILE",
    "AWS_DEFAULT_PROFILE",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_ROLE_ARN",
    "AWS_ROLE_SESSION_NAME",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
    "AWS_BEARER_TOKEN_BEDROCK",
];

/// Variables that would send calls around the proxy: service-specific
/// endpoints (`AWS_ENDPOINT_URL_S3`, ...) and the switch that makes SDKs
/// ignore `AWS_ENDPOINT_URL`.
fn bypasses_proxy(name: &str) -> bool {
    name.starts_with("AWS_ENDPOINT_URL_") || name == "AWS_IGNORE_CONFIGURED_ENDPOINT_URLS"
}

/// This process's environment, by name.
fn current_env() -> BTreeMap<String, OsString> {
    std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value)))
        .collect()
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (a.canonicalize(), b.canonicalize()),
            (Ok(a), Ok(b)) if a == b
        )
}

fn child_env(
    endpoint: &str,
    emulator_dir: &Path,
    current: &BTreeMap<String, OsString>,
) -> ChildEnv {
    let mut env = ChildEnv {
        set: vec![("AWS_ENDPOINT_URL".to_string(), endpoint.to_string())],
        remove: CREDENTIAL_VARS
            .iter()
            .filter(|name| current.contains_key(**name))
            .map(|name| name.to_string())
            .collect(),
    };
    env.remove
        .extend(current.keys().filter(|name| bypasses_proxy(name)).cloned());
    let credentials = emulator_dir.join("emulator_credentials");
    let config = emulator_dir.join("config");
    // A credentials file stays only when it is the emulator's own: any other
    // (say ~/.aws/credentials) could hand real keys to the SDK.
    let keep_file = current
        .get("AWS_SHARED_CREDENTIALS_FILE")
        .is_some_and(|file| same_file(Path::new(file), &credentials));
    if !keep_file {
        if current.contains_key("AWS_SHARED_CREDENTIALS_FILE") {
            env.remove.push("AWS_SHARED_CREDENTIALS_FILE".to_string());
        }
        if credentials.is_file() {
            env.set.push((
                "AWS_SHARED_CREDENTIALS_FILE".to_string(),
                credentials.display().to_string(),
            ));
        } else {
            // Without these, the SDK would fall back to ~/.aws/credentials.
            env.set
                .push(("AWS_ACCESS_KEY_ID".to_string(), LOCAL_KEYS.0.to_string()));
            env.set.push((
                "AWS_SECRET_ACCESS_KEY".to_string(),
                LOCAL_KEYS.1.to_string(),
            ));
        }
    }
    if !current.contains_key("AWS_CONFIG_FILE") && config.is_file() {
        env.set
            .push(("AWS_CONFIG_FILE".to_string(), config.display().to_string()));
    }
    env
}

/// What a run's header names its emulator when Lenslet started moto.
const STARTED_MOTO: &str = "moto (started by Lenslet)";

/// `SIGINT` and `SIGTERM`, handled for the whole run so that stopping it
/// (Ctrl-C, or the window signalling the run's process group) always ends
/// with the `end` report and moto stopped.
struct Signals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl Signals {
    fn new() -> Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Signals {
                interrupt: signal(SignalKind::interrupt()).context("handle SIGINT")?,
                terminate: signal(SignalKind::terminate()).context("handle SIGTERM")?,
            })
        }
        #[cfg(not(unix))]
        Ok(Signals {})
    }

    /// The next signal.
    async fn next(&mut self) -> Signal {
        #[cfg(unix)]
        tokio::select! {
            _ = self.interrupt.recv() => Signal::Interrupt,
            _ = self.terminate.recv() => Signal::Terminate,
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            Signal::Interrupt
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Signal {
    Interrupt,
    Terminate,
}

/// Decide what answers this run's calls and point `options.upstream` at it:
/// `--emulator moto` starts moto; an explicit `--upstream` is used as given;
/// otherwise a running emulator is discovered, and when there is none moto
/// is started if it is installed. Returns the run's `emulator` label and the
/// moto Lenslet started, if any.
async fn choose_upstream(
    options: &mut Options,
    home: &Path,
    moto_log: &Path,
    quiet: bool,
    signals: &mut Signals,
) -> Result<(String, Option<emulator::Managed>)> {
    if options.managed {
        let Some(command) = emulator::moto_command() else {
            bail!("--emulator moto: moto is not installed (pip install 'moto[server]')");
        };
        let moto = start_moto(options, &command, moto_log, signals).await?;
        if !quiet {
            eprintln!(
                "lenslet: started moto on 127.0.0.1:{} (log {})",
                moto.port,
                tilde(&moto.log, home)
            );
        }
        return Ok((STARTED_MOTO.to_string(), Some(moto)));
    }
    if options.upstream_given {
        let ca = options.emulator_ca();
        let label = match emulator::identify(&options.upstream, Some(&ca)).await {
            Some(Some(kind)) => kind.name().to_string(),
            _ => options.upstream.base.clone(),
        };
        if !options.upstream.reachable().await {
            eprintln!(
                "lenslet: warning: nothing is listening on {}; start your emulator first",
                options.upstream.base
            );
        }
        return Ok((label, None));
    }
    let endpoint_url = std::env::var("AWS_ENDPOINT_URL").ok();
    if let Some(found) = emulator::discover(endpoint_url.as_deref(), home).await {
        if !quiet {
            eprintln!(
                "lenslet: using {} at {} (found via {})",
                found.label(),
                found.upstream.base,
                found.source
            );
        }
        options.upstream = found.upstream.clone();
        return Ok((found.label(), None));
    }
    let Some(command) = emulator::moto_command() else {
        bail!("{}", emulator::NOT_FOUND);
    };
    let moto = start_moto(options, &command, moto_log, signals).await?;
    // Shown even with --quiet: it says what answers the calls.
    eprintln!(
        "lenslet: no emulator running; started moto (pip package) on 127.0.0.1:{}",
        moto.port
    );
    Ok((STARTED_MOTO.to_string(), Some(moto)))
}

/// Start moto and wait for it; a signal while waiting stops it.
async fn start_moto(
    options: &mut Options,
    command: &[String],
    log: &Path,
    signals: &mut Signals,
) -> Result<emulator::Managed> {
    let mut moto = emulator::Managed::spawn(command, log)?;
    tokio::select! {
        ready = moto.ready() => ready?,
        _ = signals.next() => bail!("stopped while starting moto"),
    }
    options.upstream = moto.upstream.clone();
    Ok(moto)
}

async fn run(args: Vec<String>) -> Result<i32> {
    let home = home();
    let RunArgs {
        mut options,
        command,
        json,
        markdown,
        quiet,
        ui,
        check,
        strict,
        update,
        lock,
    } = parse_run(args, &home)?;
    let mut signals = Signals::new()?;
    let cwd = std::env::current_dir().context("read the current directory")?;
    let lock_path = lock.unwrap_or_else(|| lock::path_in(&cwd));
    // How the lock is named in reports: relative to the run's directory.
    let lock_label = relative_to(&lock_path, &cwd);
    let locked = lock::read(&lock_path)?;
    if check && locked.is_none() {
        bail!("--check needs {lock_label}; create it with `lenslet run --update -- <tests>`");
    }
    let runs_dir = runs::dir(&home);
    let trace_path = match options.trace.clone() {
        Some(path) => path,
        None => runs::create(&runs_dir)?,
    };
    let log_path = runs::log_path_for(&trace_path);
    // Held until the run ends: dropping it stops moto (on errors too).
    let (emulator_label, mut moto) = choose_upstream(
        &mut options,
        &home,
        &runs::emulator_log_path_for(&trace_path),
        quiet,
        &mut signals,
    )
    .await?;
    if options.allow_docker {
        eprintln!(
            "lenslet: warning: --allow-docker: the proxy listens on all interfaces; it serves calls only from loopback and Docker networks (172.16.0.0/12, 192.168.65.0/24)"
        );
    }
    let lock_name = locked.as_ref().map(|_| lock_label.clone());
    let header = RunHeader {
        version: trace::VERSION,
        run: runs::run_id(&trace_path),
        command: command.clone(),
        cwd: cwd.display().to_string(),
        pid: std::process::id(),
        endpoint: String::new(),
        upstream: String::new(),
        emulator: emulator_label.clone(),
        started: runs::now(),
        lock: lock_name.clone(),
        log: Some(log_path.display().to_string()),
        allow_docker: options.allow_docker,
    };
    let server = Server::start(
        &options,
        trace_path.clone(),
        header,
        locked.as_ref().map(|lock| lock.access.clone()),
    )
    .await?;
    let endpoint = server.endpoint();
    let started = Instant::now();

    let env = child_env(&endpoint, &options.emulator_dir(), &current_env());
    if !quiet {
        print_banner(&endpoint, &options, &env, &trace_path, &home);
    }
    if ui && let Err(error) = open_ui(Some(&trace_path)) {
        eprintln!("lenslet: {error:#}");
    }
    let code = run_child(&command, &env, &log_path, &mut signals).await?;

    let state = server.state.clone();
    server.stop(Duration::from_secs(2)).await;
    // From here on, a call that finishes late is not recorded, and nothing
    // is written after the `end` line.
    let summary = state.sink.seal();
    let mut report = Report {
        version: trace::VERSION,
        run: runs::run_id(&trace_path),
        command: command.clone(),
        exit_code: Some(code),
        ended: runs::now(),
        duration_ms: started.elapsed().as_millis() as u64,
        emulator: emulator_label,
        trace: trace_path.display().to_string(),
        log: Some(log_path.display().to_string()),
        ..Report::default()
    };
    report.tally(
        &summary,
        locked.as_ref().map(|lock| (lock_label.as_str(), lock)),
    );
    let runs_dir = trace_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or(runs_dir);
    if let Some(previous) =
        runs::previous(&runs_dir, &trace_path, &command, &cwd.display().to_string())
    {
        report.compare_with(&previous);
    }
    state.sink.end(&report);
    server.join(Duration::from_millis(500)).await;
    if let Some(moto) = moto.as_mut() {
        moto.stop();
    }

    if !quiet {
        eprint!("{}", report.render(&home));
    }
    if update {
        update_lock(
            &lock_path,
            &lock_label,
            locked.as_ref(),
            &summary,
            code,
            quiet,
        )?;
    }
    if let Some(target) = json {
        write_json(&report, &target)?;
    }
    if let Some(target) = markdown {
        write_text(&report.markdown(), &target)?;
    }
    let changed = report.added().next().is_some() || (strict && report.unused().next().is_some());
    Ok(match code {
        0 if check && changed => EXIT_ACCESS_CHANGED,
        code => code,
    })
}

/// What `lenslet run` says before the command starts.
fn print_banner(endpoint: &str, options: &Options, env: &ChildEnv, trace_path: &Path, home: &Path) {
    eprintln!(
        "lenslet: {} → {} · trace {}",
        endpoint,
        options.upstream.base,
        tilde(trace_path, home)
    );
    for (name, value) in &env.set {
        eprintln!("lenslet:   {name}={}", tilde(Path::new(value), home));
    }
    if !env.remove.is_empty() {
        eprintln!(
            "lenslet:   unset {} (real credentials and other endpoints never reach the command)",
            env.remove.join(", ")
        );
    }
    if options.upstream.tls && env.set.iter().any(|(name, _)| name == "AWS_ACCESS_KEY_ID") {
        eprintln!(
            "lenslet: note: no emulator credentials in {}; using placeholder keys",
            tilde(&options.emulator_dir(), home)
        );
    }
}

/// `--update`: write the lock from this run, unless the command failed.
fn update_lock(
    path: &Path,
    label: &str,
    locked: Option<&lock::Lock>,
    summary: &summary::Summary,
    code: i32,
    quiet: bool,
) -> Result<()> {
    // A failed run stops early, so its access is incomplete: keep the old lock.
    if code != 0 {
        eprintln!(
            "lenslet: {label} not written: the command failed, so its access may be incomplete"
        );
        return Ok(());
    }
    let test = locked.map(|lock| lock.test.clone()).unwrap_or_default();
    lock::write(path, summary.access.keys(), &test)?;
    if !quiet {
        eprintln!(
            "lenslet: wrote {label} ({})",
            plural(summary.access.len(), "permission")
        );
    }
    Ok(())
}

/// Run the test command in its own process group, its stdout and stderr
/// passed through to ours unchanged and also written to `log` (0600).
///
/// Ctrl-C (`SIGINT`) is forwarded to the command's process group; a second
/// one, or `SIGTERM`, stops the whole group (`SIGTERM`, then `SIGKILL`), so
/// no descendant outlives the run.
async fn run_child(
    command: &[String],
    env: &ChildEnv,
    log: &Path,
    signals: &mut Signals,
) -> Result<i32> {
    let log_file = Arc::new(Mutex::new(
        trace::open_owner_only(log).with_context(|| format!("open {}", log.display()))?,
    ));
    let mut child = tokio::process::Command::new(&command[0]);
    child
        .args(&command[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    child.process_group(0);
    // Remove first: a placeholder key replaces a real one of the same name.
    for name in &env.remove {
        child.env_remove(name);
    }
    for (name, value) in &env.set {
        child.env(name, value);
    }
    let mut child = match child.spawn() {
        Ok(child) => child,
        Err(error) => {
            let message = format!("lenslet: cannot start `{}`: {error}\n", command[0]);
            eprint!("{message}");
            if let Ok(mut file) = log_file.lock() {
                let _ = file.write_all(message.as_bytes());
            }
            return Ok(127);
        }
    };
    let mut tees = Vec::new();
    if let Some(out) = child.stdout.take() {
        tees.push(tokio::spawn(tee(out, std::io::stdout, log_file.clone())));
    }
    if let Some(err) = child.stderr.take() {
        tees.push(tokio::spawn(tee(err, std::io::stderr, log_file.clone())));
    }
    let pid = child.id();
    let mut interrupted = false;
    let status = loop {
        tokio::select! {
            status = child.wait() => break status.context("wait for the command")?,
            signal = signals.next() => {
                if signal == Signal::Terminate || interrupted {
                    break terminate_child_tree(&mut child).await?;
                }
                interrupted = true;
                if let Some(pid) = pid {
                    signal_process_group(pid, "INT");
                }
            }
        }
    };
    for task in tees {
        let _ = tokio::time::timeout(Duration::from_millis(500), task).await;
    }
    Ok(exit_code(status))
}

/// Copy `from` to the terminal stream `to` and to `log`, chunk by chunk.
async fn tee<R, W>(mut from: R, to: fn() -> W, log: Arc<Mutex<std::fs::File>>)
where
    R: tokio::io::AsyncRead + Unpin,
    W: std::io::Write,
{
    use tokio::io::AsyncReadExt as _;
    let mut buf = [0u8; 8192];
    while let Ok(n) = from.read(&mut buf).await {
        if n == 0 {
            break;
        }
        let mut out = to();
        let _ = out.write_all(&buf[..n]);
        let _ = out.flush();
        if let Ok(mut file) = log.lock() {
            let _ = file.write_all(&buf[..n]);
        }
    }
}

async fn terminate_child_tree(
    child: &mut tokio::process::Child,
) -> Result<std::process::ExitStatus> {
    let pid = child.id();
    if let Some(pid) = pid {
        signal_process_group(pid, "TERM");
    }
    if let Ok(status) = tokio::time::timeout(Duration::from_millis(1500), child.wait()).await {
        if let Some(pid) = pid {
            signal_process_group(pid, "KILL");
        }
        return status.context("wait for the stopped command");
    }
    if let Some(pid) = pid {
        signal_process_group(pid, "KILL");
    }
    let _ = child.start_kill();
    child.wait().await.context("wait for the killed command")
}

#[cfg(unix)]
fn signal_process_group(pgid: u32, sig: &str) {
    if pgid > 0 {
        let _ = std::process::Command::new("sh")
            .args([
                "-c",
                &format!(
                    "kill -{sig} -{pgid} 2>/dev/null || kill -{sig} {pgid} 2>/dev/null || true"
                ),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(not(unix))]
fn signal_process_group(_pgid: u32, _sig: &str) {}

fn write_json<T: Serialize>(value: &T, target: &str) -> Result<()> {
    let text = serde_json::to_string_pretty(value).context("encode report")?;
    write_text(&format!("{text}\n"), target)
}

/// Write to a file, or to stdout for `-`.
fn write_text(text: &str, target: &str) -> Result<()> {
    if target == "-" {
        print!("{text}");
        Ok(())
    } else {
        std::fs::write(target, text).with_context(|| format!("write {target}"))
    }
}

/// The workflow `lenslet init` writes. `{version}`, `{command}` and
/// `{emulator}` (the `with:` lines that pick the emulator) are filled in;
/// everything else is plain YAML a user can edit.
const WORKFLOW: &str = r#"# Lenslet: show the AWS access each pull request needs, and fail when it grows.
# Docs: https://github.com/hxddh/lenslet#in-ci
name: Lenslet
on:
  pull_request:
  push:
    branches: [main]
permissions:
  contents: read
  pull-requests: write # the access summary is posted as one PR comment
jobs:
  aws-access:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v5
      # Install what your tests need here (language runtime, dependencies).
      - uses: hxddh/lenslet@lenslet-v{version}
        with:
          command: {command}
{emulator}"#;

/// The `with:` lines when Lenslet starts moto itself in CI.
const WORKFLOW_MOTO: &str = r#"          # The action installs moto (pip install 'moto[server]') if needed and
          # Lenslet starts it for the run.
          emulator: moto
"#;

/// The `with:` lines for an emulator the job starts itself.
const WORKFLOW_UPSTREAM: &str = r#"          upstream: {upstream}
          # A command that starts your local AWS emulator inside this job, for
          # example `aws-emulator --telemetry off --port 9298` once AWS Emulator
          # is installed (see its docs), or `localstack start -d`.
          emulator: {emulator}
"#;

/// One shell word: left as is when it is plain, single-quoted otherwise, so
/// `bash -c` in the action runs exactly the argv `init` ran.
fn shell_quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|word| shell_quote(word))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A YAML single-quoted scalar.
fn yaml_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// What CI runs the tests against.
#[derive(Debug, PartialEq)]
enum CiEmulator {
    /// Lenslet starts moto (the action's `emulator: moto`).
    Moto,
    /// The job starts this emulator (`emulator:` command, maybe none) and
    /// Lenslet uses `upstream`.
    Upstream {
        upstream: String,
        start: Option<String>,
    },
}

fn render_workflow(version: &str, command: &[String], emulator: &CiEmulator) -> String {
    let with = match emulator {
        CiEmulator::Moto => WORKFLOW_MOTO.to_string(),
        CiEmulator::Upstream { upstream, start } => WORKFLOW_UPSTREAM
            .replace("{upstream}", &yaml_quote(upstream))
            .replace(
                "{emulator}",
                &start
                    .as_deref()
                    .map(yaml_quote)
                    .unwrap_or_else(|| "\"\"".to_string()),
            ),
    };
    WORKFLOW
        .replace("{version}", version)
        .replace("{command}", &yaml_quote(&shell_join(command)))
        .replace("{emulator}", &with)
}

/// The command that starts a discovered emulator in CI, when Lenslet knows it.
fn start_command(found: &emulator::Found) -> Option<String> {
    let port = found.upstream.port;
    match found.kind? {
        emulator::Kind::Moto => Some(format!("moto_server -p {port}")),
        emulator::Kind::LocalStack => Some("localstack start -d".to_string()),
        emulator::Kind::AwsEmulator => Some(format!("aws-emulator --telemetry off --port {port}")),
    }
}

/// The project's test command, from the files in `cwd`: `npm`/`pnpm`/
/// `yarn`/`bun test` (a `test` script in package.json), `cargo test`,
/// `go test ./...`, `pytest` (or `uv run pytest`), `make test`. The one
/// detection `lenslet init` and `lenslet doctor` (and so the window) use.
fn detect_test_command(cwd: &Path) -> Option<Vec<String>> {
    let words = |words: &[&str]| Some(words.iter().map(|word| word.to_string()).collect());
    if let Ok(text) = std::fs::read_to_string(cwd.join("package.json"))
        && let Ok(package) = serde_json::from_str::<serde_json::Value>(&text)
        && package
            .get("scripts")
            .and_then(|scripts| scripts.get("test"))
            .and_then(|test| test.as_str())
            .is_some_and(|test| !test.trim().is_empty())
    {
        if cwd.join("pnpm-lock.yaml").is_file() {
            return words(&["pnpm", "test"]);
        } else if cwd.join("yarn.lock").is_file() {
            return words(&["yarn", "test"]);
        } else if cwd.join("bun.lockb").is_file() || cwd.join("bun.lock").is_file() {
            return words(&["bun", "test"]);
        }
        return words(&["npm", "test"]);
    }
    if cwd.join("Cargo.toml").is_file() {
        return words(&["cargo", "test"]);
    }
    if cwd.join("go.mod").is_file() {
        return words(&["go", "test", "./..."]);
    }
    if ["pyproject.toml", "pytest.ini", "requirements.txt"]
        .iter()
        .any(|file| cwd.join(file).is_file())
    {
        if cwd.join("uv.lock").is_file() && command_exists_on_path("uv") {
            return words(&["uv", "run", "pytest"]);
        }
        return words(&["pytest"]);
    }
    if cwd.join("Makefile").is_file() {
        return words(&["make", "test"]);
    }
    None
}

/// `lenslet init [--emulator moto | --upstream ADDRESS] [--force] [-- COMMAND]`:
/// from nothing to a lock and a GitHub Actions workflow in one step.
async fn init(args: Vec<String>) -> Result<i32> {
    let home = home();
    let mut options = Options::new(&home);
    let mut force = false;
    let mut command = Vec::new();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().with_context(|| format!("{arg} requires a value"));
        match arg.as_str() {
            "--force" => force = true,
            "--" => {
                command.extend(it.by_ref());
                break;
            }
            "--emulator" | "--upstream" => {
                options.accept(&arg, &mut value)?;
            }
            flag if flag.starts_with('-') => {
                bail!("unknown option `{flag}`; try `lenslet --help`")
            }
            _ => {
                command.push(arg);
                command.extend(it.by_ref());
                break;
            }
        }
    }
    let cwd = std::env::current_dir().context("read the current directory")?;
    if command.is_empty() {
        let Some(detected) = detect_test_command(&cwd) else {
            bail!(
                "could not detect a test command in {}; pass one after `--`, e.g. `lenslet init -- pytest`",
                cwd.display()
            );
        };
        eprintln!("lenslet: detected test command `{}`", shell_join(&detected));
        command = detected;
    }

    // What answers: as `lenslet run` decides, without starting anything yet
    // (the lock run below starts moto when it is needed).
    let ci = if options.managed {
        if emulator::moto_command().is_none() {
            bail!("{}", emulator::NOT_FOUND);
        }
        eprintln!("lenslet: 1/3 Lenslet will start moto (pip package), here and in CI");
        CiEmulator::Moto
    } else if options.upstream_given {
        let target = &options.upstream;
        eprintln!("lenslet: 1/3 emulator at {}", target.base);
        if !target.reachable().await {
            bail!(
                "nothing is listening on {}. Start your emulator first, or run `lenslet init` without --upstream",
                target.base
            );
        }
        let ca = options.emulator_ca();
        let found = emulator::Found {
            upstream: target.clone(),
            kind: emulator::identify(target, Some(&ca)).await.flatten(),
            source: "--upstream",
        };
        if found.kind.is_some() {
            eprintln!("lenslet:   it is {}", found.label());
        }
        CiEmulator::Upstream {
            upstream: target.base.clone(),
            start: start_command(&found),
        }
    } else {
        let endpoint_url = std::env::var("AWS_ENDPOINT_URL").ok();
        if let Some(found) = emulator::discover(endpoint_url.as_deref(), &home).await {
            eprintln!(
                "lenslet: 1/3 {} at {} (found via {})",
                found.label(),
                found.upstream.base,
                found.source
            );
            CiEmulator::Upstream {
                upstream: found.upstream.base.clone(),
                start: start_command(&found),
            }
        } else if emulator::moto_command().is_some() {
            eprintln!(
                "lenslet: 1/3 no emulator running; Lenslet will start moto (pip package), here and in CI"
            );
            CiEmulator::Moto
        } else {
            bail!("{}", emulator::NOT_FOUND);
        }
    };

    let workflow = cwd.join(".github/workflows/lenslet.yml");
    eprintln!("lenslet: 2/3 workflow {}", workflow.display());
    let text = render_workflow(env!("CARGO_PKG_VERSION"), &command, &ci);
    if workflow.exists() && !force {
        eprintln!("lenslet:   exists, left alone; with --force it would be replaced by:");
        for line in text.lines() {
            eprintln!("lenslet:   | {line}");
        }
    } else {
        std::fs::create_dir_all(workflow.parent().unwrap_or(&cwd))
            .with_context(|| format!("create {}", workflow.display()))?;
        std::fs::write(&workflow, text).with_context(|| format!("write {}", workflow.display()))?;
    }

    eprintln!("lenslet: 3/3 lenslet.lock from `{}`", shell_join(&command));
    let mut run_args = vec!["--update".to_string()];
    match &ci {
        CiEmulator::Moto => run_args.extend(["--emulator".to_string(), "moto".to_string()]),
        CiEmulator::Upstream { upstream, .. } => {
            run_args.extend(["--upstream".to_string(), upstream.clone()]);
        }
    }
    run_args.push("--".to_string());
    run_args.extend(command);
    let code = Box::pin(run(run_args)).await?;
    if code == 0 {
        let emulator_note = match &ci {
            CiEmulator::Moto => "In CI, the action installs moto and Lenslet starts it.",
            CiEmulator::Upstream { start: Some(_), .. } => {
                "Make sure the workflow installs your emulator and test dependencies."
            }
            CiEmulator::Upstream { start: None, .. } => {
                "Set `emulator:` in the workflow to start your emulator in CI."
            }
        };
        eprintln!(
            "\nlenslet: done. Review lenslet.lock (mark fixture-only lines `# test`), then\n  \
             git add lenslet.lock .github/workflows/lenslet.yml\n\
             {emulator_note}"
        );
    }
    Ok(code)
}

/// What `lenslet doctor --json` prints: what the window reads to offer the
/// test command and to say which emulator would answer.
#[derive(Serialize)]
struct Doctor {
    version: String,
    /// This `lenslet` binary.
    cli: String,
    /// Emulators running now, recognized by their protocol.
    emulators: Vec<DoctorEmulator>,
    /// The emulator Lenslet can start when none is running.
    managed: Managed,
    /// The project's test command (the detection `lenslet init` uses).
    test_command: Option<String>,
    /// The lock file in this directory, when there is one.
    lock: Option<String>,
    /// `.github/workflows/lenslet.yml` exists.
    ci_workflow: bool,
}

#[derive(Serialize)]
struct DoctorEmulator {
    name: String,
    url: String,
}

#[derive(Serialize)]
struct Managed {
    name: &'static str,
    available: bool,
    command: Option<Vec<String>>,
}

/// `lenslet doctor [--json]`.
async fn doctor(args: Vec<String>) -> Result<i32> {
    let json = match args.as_slice() {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => bail!("usage: lenslet doctor [--json]"),
    };
    let home = home();
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .unwrap_or_else(|_| PathBuf::from("lenslet"));
    let endpoint_url = std::env::var("AWS_ENDPOINT_URL").ok();
    let running = emulator::running(endpoint_url.as_deref(), &home).await;
    let moto = emulator::moto_command();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let lock_path = lock::path_in(&cwd);
    let report = Doctor {
        version: env!("CARGO_PKG_VERSION").to_string(),
        cli: exe.display().to_string(),
        emulators: running
            .iter()
            .filter_map(|found| {
                Some(DoctorEmulator {
                    name: found.kind?.name().to_string(),
                    url: found.upstream.base.clone(),
                })
            })
            .collect(),
        managed: Managed {
            name: "moto",
            available: moto.is_some(),
            command: moto,
        },
        test_command: detect_test_command(&cwd).map(|command| shell_join(&command)),
        lock: lock_path.is_file().then(|| lock_path.display().to_string()),
        ci_workflow: cwd.join(".github/workflows/lenslet.yml").is_file(),
    };
    if json {
        write_json(&report, "-")?;
        return Ok(0);
    }
    let ui_installed = find_ui_binary(&exe).is_some();
    println!("lenslet doctor · v{}", report.version);
    println!(
        "  ✓ CLI                 {} ({})",
        report.cli,
        if command_exists_on_path("lenslet") {
            "on PATH"
        } else {
            "not on PATH"
        }
    );
    println!(
        "  {} Window              {}",
        if ui_installed { "✓" } else { "·" },
        if ui_installed {
            "installed (`lenslet ui`)"
        } else {
            "not installed (install.sh --app installs it)"
        }
    );
    if running.is_empty() {
        println!("  · Emulators running   none");
    } else {
        let list: Vec<String> = running
            .iter()
            .map(|found| format!("{} ({})", found.label(), found.upstream.base))
            .collect();
        println!("  ✓ Emulators running   {}", list.join(", "));
    }
    match report.managed.command.as_ref() {
        Some(command) => println!(
            "  ✓ Managed moto        available (`{}`); used when no emulator is running",
            command.join(" ")
        ),
        None => println!(
            "  · Managed moto        not installed (pip install 'moto[server]' to let Lenslet start it)"
        ),
    }
    match report.test_command.as_deref() {
        Some(command) => {
            println!("  ✓ Test command        `{command}` (run it with `lenslet {command}`)")
        }
        None => println!("  · Test command        none detected in {}", cwd.display()),
    }
    match lock::read(&lock_path).ok().flatten() {
        Some(lock) => println!(
            "  ✓ Lock                lenslet.lock ({})",
            plural(lock.access.len(), "permission")
        ),
        None => println!("  · Lock                no lenslet.lock yet (`lenslet init` creates it)"),
    }
    println!(
        "  {} GitHub Actions      {}",
        if report.ci_workflow { "✓" } else { "·" },
        if report.ci_workflow {
            ".github/workflows/lenslet.yml"
        } else {
            "not configured yet (`lenslet init` adds it)"
        }
    );
    Ok(0)
}

fn command_exists_on_path(cmd: &str) -> bool {
    if cmd.is_empty() {
        return false;
    }
    if cmd.contains('/') {
        return is_executable_file(Path::new(cmd));
    }
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| is_executable_file(&dir.join(cmd)))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn find_ui_binary(cli_exe: &Path) -> Option<PathBuf> {
    let dir = cli_exe.parent()?;
    if dir.join("lenslet-ui").is_file() {
        return Some(dir.join("lenslet-ui"));
    }
    if cfg!(target_os = "macos") {
        for app in [
            dir.ancestors()
                .nth(2)
                .filter(|p| p.extension().is_some_and(|e| e == "app"))
                .map(Path::to_path_buf),
            Some(PathBuf::from("/Applications/Lenslet.app")),
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Applications/Lenslet.app")),
        ]
        .into_iter()
        .flatten()
        {
            if app.is_dir() {
                return Some(app);
            }
        }
    }
    None
}

const POLICY_USAGE: &str = "usage: lenslet policy [--lock PATH | --run RUN]";

/// `lenslet policy`: an IAM policy JSON document from `lenslet.lock`
/// (without `# test` lines) or from a recorded run's access.
fn policy_cmd(args: Vec<String>) -> Result<i32> {
    let mut lock_path: Option<PathBuf> = None;
    let mut run_spec: Option<String> = None;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--lock" => lock_path = Some(PathBuf::from(it.next().context(POLICY_USAGE)?)),
            "--run" => run_spec = Some(it.next().context(POLICY_USAGE)?),
            _ => bail!("{POLICY_USAGE}"),
        }
    }
    if lock_path.is_some() && run_spec.is_some() {
        bail!("use --lock or --run, not both\n{POLICY_USAGE}");
    }
    let cwd = std::env::current_dir().context("read the current directory")?;
    if let Some(spec) = run_spec.as_deref() {
        let run_path = runs::resolve(&runs::dir(&home()), spec, &cwd)?;
        let report = runs::report(&run_path)?
            .with_context(|| format!("run {} has not finished yet", runs::run_id(&run_path)))?;
        let access = policy::from_entries(&report.access);
        print!("{}", policy::compile(&access));
        eprintln!(
            "lenslet: {} from run {}. Review before attaching it to a role.",
            plural(access.len(), "permission"),
            report.run
        );
        return Ok(0);
    }
    let path = lock_path.unwrap_or_else(|| lock::path_in(&cwd));
    let Some(locked) = lock::read(&path)? else {
        bail!(
            "no {}; create it with `lenslet run --update -- <tests>`",
            path.display()
        );
    };
    print!("{}", policy::compile(&policy::from_lock(&locked)));
    eprintln!(
        "lenslet: observed access from {} ({}, {} marked `# test` left out). \
         Review before attaching it to a role.",
        path.display(),
        plural(locked.access.len() - locked.test.len(), "permission"),
        locked.test.len()
    );
    Ok(0)
}

const CHECK_USAGE: &str = "usage: lenslet check --policy PATH [RUN] [--json] [--markdown PATH]";

/// What `lenslet check` found.
#[derive(Serialize)]
struct CheckReport {
    run: String,
    /// The policy checked against.
    policy: String,
    calls: usize,
    denied: Vec<policy::Denial>,
}

impl CheckReport {
    /// Why a permission is not allowed: the denying statement, a conditional
    /// statement Lenslet did not evaluate, or that nothing allows it.
    fn reason(denial: &policy::Denial) -> String {
        let decision = &denial.decision;
        let mut text = match (
            decision.reason,
            decision.by.as_deref(),
            decision.conditional,
        ) {
            (policy::Reason::ExplicitDeny, Some(statement), false) => {
                format!("denied by statement `{statement}`")
            }
            (policy::Reason::ExplicitDeny, Some(statement), true) => {
                format!("statement `{statement}` may deny it: conditional, not evaluated")
            }
            (_, Some(statement), true) => format!(
                "only statement `{statement}` allows it, under a condition: conditional, not evaluated"
            ),
            _ => "no statement allows it".to_string(),
        };
        if denial.implied {
            text.push_str(" (implied)");
        }
        if denial.unverified {
            text.push_str(" (resource unknown: request body not readable)");
        }
        text
    }

    /// Denials grouped by permission and reason, in first-call order, with
    /// how many calls hit each: a loop of the same call is one line.
    fn grouped(&self) -> Vec<(&policy::Denial, usize)> {
        let mut groups: Vec<(&policy::Denial, usize)> = Vec::new();
        for item in &self.denied {
            let key = |d: &policy::Denial| (d.action.clone(), d.resource.clone(), Self::reason(d));
            match groups.iter_mut().find(|(first, _)| key(first) == key(item)) {
                Some((_, calls)) => *calls += 1,
                None => groups.push((item, 1)),
            }
        }
        groups
    }

    fn render(&self) -> String {
        let mut out = format!(
            "lenslet check: run {} against {} · {} · ",
            self.run,
            self.policy,
            plural(self.calls, "call")
        );
        if self.denied.is_empty() {
            out.push_str("all allowed\n");
            return out;
        }
        out.push_str(&format!(
            "{} denied\n",
            plural(self.grouped().len(), "permission")
        ));
        for (item, calls) in self.grouped() {
            let times = if calls > 1 {
                format!(" ×{calls}")
            } else {
                String::new()
            };
            out.push_str(&format!(
                "  ✗ {}  {}  ({} {}{times})\n      → {}\n",
                item.action,
                item.resource,
                item.service.to_ascii_uppercase(),
                item.operation,
                Self::reason(item)
            ));
        }
        out
    }

    fn markdown(&self) -> String {
        let mut out = format!(
            "### Lenslet check · run `{}` against `{}`\n\n",
            self.run, self.policy
        );
        if self.denied.is_empty() {
            out.push_str(&format!("All {} allowed.\n", plural(self.calls, "call")));
            return out;
        }
        out.push_str(&format!(
            "**{} denied** across {}. Grant what is intended in `{}`, or change the code that needs it.\n\n| Call | Permission | Resource | Why |\n|---|---|---|---|\n",
            plural(self.grouped().len(), "permission"),
            plural(self.calls, "call"),
            self.policy
        ));
        for (item, calls) in self.grouped() {
            let times = if calls > 1 {
                format!(" ×{calls}")
            } else {
                String::new()
            };
            out.push_str(&format!(
                "| {} {}{times} | `{}` | `{}` | {} |\n",
                item.service.to_ascii_uppercase(),
                item.operation,
                item.action,
                item.resource,
                Self::reason(item).replace('|', "\\|")
            ));
        }
        out
    }
}

/// `lenslet check --policy PATH [RUN] [--json] [--markdown PATH]`: check a
/// recorded run's calls (their concrete access, derived live) against an
/// IAM policy, without re-running anything.
fn check(args: Vec<String>) -> Result<i32> {
    let mut json = false;
    let mut markdown: Option<String> = None;
    let mut policy_path: Option<PathBuf> = None;
    let mut run: Option<String> = None;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--markdown" => markdown = Some(it.next().context(CHECK_USAGE)?),
            "--policy" => policy_path = Some(PathBuf::from(it.next().context(CHECK_USAGE)?)),
            flag if flag.starts_with('-') => bail!("{CHECK_USAGE}"),
            _ if run.is_none() => run = Some(arg),
            _ => bail!("{CHECK_USAGE}"),
        }
    }
    let Some(policy_path) = policy_path else {
        bail!("{CHECK_USAGE}");
    };
    let cwd = std::env::current_dir().context("read the current directory")?;
    let checked = policy::Policy::from_file(&policy_path, relative_to(&policy_path, &cwd))?;
    let dir = runs::dir(&home());
    let path = match run.as_deref() {
        Some(spec) => runs::resolve(&dir, spec, &cwd)?,
        None => runs::newest_finished(&dir, &cwd)?,
    };
    let calls = runs::calls(&path)?;
    let report = CheckReport {
        run: runs::run_id(&path),
        policy: checked.label.clone(),
        calls: calls.len(),
        denied: policy::check(&calls, &checked),
    };
    if let Some(target) = markdown.as_deref() {
        write_text(&report.markdown(), target)?;
    }
    if json {
        write_json(&report, "-")?;
    } else {
        print!("{}", report.render());
    }
    Ok(if report.denied.is_empty() {
        0
    } else {
        EXIT_ACCESS_CHANGED
    })
}

/// `path` relative to `dir` when it is inside it, also through symlinks
/// (macOS temp dirs live under `/var`, which is `/private/var`).
fn relative_to(path: &Path, dir: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(dir) {
        return relative.display().to_string();
    }
    let canonical = |path: &Path| -> Option<PathBuf> {
        // The file may not exist yet: resolve its directory instead.
        let parent = path.parent()?.canonicalize().ok()?;
        Some(parent.join(path.file_name()?))
    };
    match (canonical(path), dir.canonicalize().ok()) {
        (Some(path), Some(dir)) => match path.strip_prefix(&dir) {
            Ok(relative) => relative.display().to_string(),
            Err(_) => path.display().to_string(),
        },
        _ => path.display().to_string(),
    }
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

/// Start the window next to this binary: `lenslet-ui`, or `Lenslet.app` on
/// macOS, on run file `trace` when given. The window is a separate program
/// so `lenslet run` never needs graphics libraries (CI, containers, SSH).
fn open_ui(trace: Option<&Path>) -> Result<()> {
    let exe = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .context("locate the lenslet binary")?;
    let dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    // The CLI ships inside Lenslet.app on macOS (Contents/MacOS/lenslet, next
    // to the window's `lenslet-ui`), next to `lenslet-ui` in the Linux
    // archive, or on its own via install.sh (then /Applications is tried).
    let bundle = dir
        .ancestors()
        .nth(2)
        .filter(|app| app.extension().is_some_and(|ext| ext == "app"))
        .map(Path::to_path_buf);
    let app = bundle
        .or_else(|| Some(dir.join("Lenslet.app")).filter(|app| app.is_dir()))
        .or_else(|| Some(PathBuf::from("/Applications/Lenslet.app")).filter(|app| app.is_dir()))
        .filter(|_| cfg!(target_os = "macos"));
    let mut command = if let Some(app) = app {
        let mut open = std::process::Command::new("open");
        open.arg("-a").arg(app);
        if let Some(trace) = trace {
            open.arg("--env")
                .arg(format!("LENSLET_TRACE={}", trace.display()));
            open.arg(trace);
        }
        open
    } else if dir.join("lenslet-ui").is_file() {
        let mut bin = std::process::Command::new(dir.join("lenslet-ui"));
        if let Some(trace) = trace {
            bin.arg(trace);
        }
        bin
    } else {
        bail!(
            "the Lenslet window is not installed next to {} (expected lenslet-ui{}). \
             `lenslet run` works without it.",
            exe.display(),
            if cfg!(target_os = "macos") {
                " or Lenslet.app"
            } else {
                ""
            }
        );
    };
    if let Some(trace) = trace {
        command.env("LENSLET_TRACE", trace);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("open the Lenslet window")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn run_takes_options_then_the_command() {
        let home = Path::new("/home/tester");
        let parsed = parse_run(
            strings(&[
                "--upstream",
                "9400",
                "--json",
                "-",
                "--",
                "npm",
                "test",
                "--x",
            ]),
            home,
        )
        .unwrap();
        assert_eq!(parsed.command, ["npm", "test", "--x"]);
        assert_eq!(parsed.json.as_deref(), Some("-"));
        assert_eq!(parsed.options.upstream.port, 9400);

        // Without `--`, the first non-option starts the command.
        let parsed = parse_run(strings(&["--quiet", "pytest", "-k", "s3"]), home).unwrap();
        assert!(parsed.quiet);
        assert_eq!(parsed.command, ["pytest", "-k", "s3"]);

        let parsed = parse_run(strings(&["--check", "--strict", "npm", "test"]), home).unwrap();
        assert!(parsed.check && parsed.strict);
        assert!(
            parse_run(strings(&[]), home).is_err(),
            "a command is required"
        );
        assert!(parse_run(strings(&["--check"]), home).is_err());
        assert!(parse_run(strings(&["--check", "--update", "x"]), home).is_err());
        assert!(parse_run(strings(&["--strict", "x"]), home).is_err());
        for removed in [
            "--enforce",
            "--policy",
            "--no-env",
            "--listen",
            "--route",
            "--max-body",
            "--log",
            "--sandbox",
        ] {
            assert!(
                parse_run(strings(&[removed, "v", "x"]), home).is_err(),
                "{removed}"
            );
        }
    }

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, OsString> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), OsString::from(value)))
            .collect()
    }

    #[test]
    fn credentials_are_always_stripped_and_sdks_point_at_lenslet() {
        let dir = std::env::temp_dir().join(format!("lenslet-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("emulator_credentials"), "[default]").unwrap();
        std::fs::write(dir.join("config"), "[default]").unwrap();
        let names = |env: &ChildEnv| -> Vec<String> {
            env.set.iter().map(|(name, _)| name.clone()).collect()
        };
        let real = vars(&[
            ("AWS_PROFILE", "real"),
            ("AWS_ACCESS_KEY_ID", "real"),
            ("AWS_SESSION_TOKEN", "real"),
            ("AWS_WEB_IDENTITY_TOKEN_FILE", "real"),
            ("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "real"),
            ("AWS_BEARER_TOKEN_BEDROCK", "real"),
            ("AWS_ENDPOINT_URL_S3", "http://localhost:9000"),
            ("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", "true"),
            ("HOME", "/home/tester"),
        ]);
        let env = child_env("http://127.0.0.1:5000", &dir, &real);
        assert_eq!(
            names(&env),
            [
                "AWS_ENDPOINT_URL",
                "AWS_SHARED_CREDENTIALS_FILE",
                "AWS_CONFIG_FILE"
            ]
        );
        assert_eq!(
            env.remove,
            [
                "AWS_ACCESS_KEY_ID",
                "AWS_SESSION_TOKEN",
                "AWS_PROFILE",
                "AWS_WEB_IDENTITY_TOKEN_FILE",
                "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
                "AWS_BEARER_TOKEN_BEDROCK",
                "AWS_ENDPOINT_URL_S3",
                "AWS_IGNORE_CONFIGURED_ENDPOINT_URLS",
            ]
        );

        // No emulator credentials: real keys still go, placeholders come in,
        // so the SDK cannot fall back to ~/.aws/credentials.
        let empty = dir.join("nothing");
        let env = child_env("http://x", &empty, &real);
        assert_eq!(
            names(&env),
            [
                "AWS_ENDPOINT_URL",
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY"
            ]
        );
        assert_eq!(env.set[1].1, LOCAL_KEYS.0);

        // Another credentials file (say ~/.aws/credentials) goes too...
        let mine = vars(&[(
            "AWS_SHARED_CREDENTIALS_FILE",
            "/home/tester/.aws/credentials",
        )]);
        let env = child_env("http://x", &empty, &mine);
        assert_eq!(env.remove, ["AWS_SHARED_CREDENTIALS_FILE"]);
        // ...but the emulator's own stays.
        let emulators = dir.join("emulator_credentials");
        let own = vars(&[("AWS_SHARED_CREDENTIALS_FILE", emulators.to_str().unwrap())]);
        let env = child_env("http://x", &dir, &own);
        assert_eq!(names(&env), ["AWS_ENDPOINT_URL", "AWS_CONFIG_FILE"]);
        assert!(env.remove.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn init_keeps_argument_boundaries_for_bash() {
        let argv = |words: &[&str]| strings(words);
        assert_eq!(shell_join(&argv(&["npm", "test"])), "npm test");
        assert_eq!(
            shell_join(&argv(&["pytest", "-k", "not slow"])),
            "pytest -k 'not slow'"
        );
        assert_eq!(shell_join(&argv(&["echo", ""])), "echo ''");
        let joined = shell_join(&argv(&["printf", "%s|", "a b", "it's", "$HOME"]));
        let out = std::process::Command::new("bash")
            .args(["-c", &joined])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "a b|it's|$HOME|");
    }

    #[test]
    fn the_init_workflow_quotes_the_command() {
        assert_eq!(
            yaml_quote("pytest -k 'not slow'"),
            "'pytest -k ''not slow'''"
        );
        let text = render_workflow(
            "13.0.0",
            &strings(&["npm", "test"]),
            &CiEmulator::Upstream {
                upstream: "https://127.0.0.1:9298".to_string(),
                start: Some("aws-emulator --telemetry off --port 9298".to_string()),
            },
        );
        assert!(text.contains("uses: hxddh/lenslet@lenslet-v13.0.0"));
        assert!(text.contains("command: 'npm test'"));
        assert!(text.contains("upstream: 'https://127.0.0.1:9298'"));
        assert!(text.contains("emulator: 'aws-emulator --telemetry off --port 9298'"));
        let moto = render_workflow("13.0.0", &strings(&["pytest"]), &CiEmulator::Moto);
        assert!(moto.contains("          emulator: moto\n"), "{moto}");
        assert!(!moto.contains("upstream:"), "{moto}");
        assert!(
            !moto.contains('{') && !text.contains('{'),
            "every placeholder is filled"
        );
    }

    #[test]
    fn detects_the_test_command_from_the_project_files() {
        let dir = std::env::temp_dir().join(format!("lenslet-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(detect_test_command(&dir), None);
        std::fs::write(dir.join("Makefile"), "test:\n").unwrap();
        assert_eq!(detect_test_command(&dir), Some(strings(&["make", "test"])));
        std::fs::write(dir.join("go.mod"), "module x\n").unwrap();
        assert_eq!(
            detect_test_command(&dir),
            Some(strings(&["go", "test", "./..."]))
        );
        std::fs::write(dir.join("package.json"), r#"{"scripts":{"test":"jest"}}"#).unwrap();
        std::fs::write(dir.join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(detect_test_command(&dir), Some(strings(&["pnpm", "test"])));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn exit_codes_pass_through() {
        let status = std::process::Command::new("sh")
            .args(["-c", "exit 7"])
            .status()
            .unwrap();
        assert_eq!(exit_code(status), 7);
    }

    #[test]
    fn lock_labels_are_relative_even_through_symlinks() {
        let base = std::env::temp_dir().join(format!("lenslet-rel-{}", std::process::id()));
        let real = base.join("real");
        std::fs::create_dir_all(real.join("infra")).unwrap();
        assert_eq!(
            relative_to(&real.join("lenslet.lock"), &real),
            "lenslet.lock"
        );
        #[cfg(unix)]
        {
            let link = base.join("link");
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert_eq!(
                relative_to(&link.join("infra/aws.lock"), &real.canonicalize().unwrap()),
                "infra/aws.lock"
            );
        }
        assert_eq!(
            relative_to(Path::new("/elsewhere/x.lock"), &real),
            "/elsewhere/x.lock"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn check_reasons_name_the_statement_and_say_when_it_is_conditional() {
        let denial = |reason, by: Option<&str>, conditional| policy::Denial {
            call: "c".into(),
            service: "s3".into(),
            operation: "GetObject".into(),
            action: "s3:GetObject".into(),
            resource: "*".into(),
            implied: false,
            unverified: false,
            decision: policy::Decision {
                allowed: false,
                reason,
                by: by.map(str::to_string),
                conditional,
            },
        };
        assert_eq!(
            CheckReport::reason(&denial(policy::Reason::ExplicitDeny, Some("A"), false)),
            "denied by statement `A`"
        );
        assert!(
            CheckReport::reason(&denial(policy::Reason::ExplicitDeny, Some("B"), true))
                .contains("conditional, not evaluated")
        );
        assert!(
            CheckReport::reason(&denial(policy::Reason::NoAllow, Some("C"), true))
                .starts_with("only statement `C` allows it")
        );
        assert_eq!(
            CheckReport::reason(&denial(policy::Reason::NoAllow, None, false)),
            "no statement allows it"
        );
    }
}
