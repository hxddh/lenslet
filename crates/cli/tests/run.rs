//! `lenslet` end to end: the real binary, a child process that calls AWS
//! through `AWS_ENDPOINT_URL`, a stand-in emulator (or moto), the lock
//! workflow and policy checks.

use std::{
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

/// A one-response-per-connection "emulator": GetQueueUrl fails the way SQS
/// does for a missing queue; everything else succeeds.
fn fake_emulator() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&buffer[..read]),
                }
            }
            let missing = String::from_utf8_lossy(&request).contains("GetQueueUrl");
            let (status, body) = if missing {
                (
                    "400 Bad Request",
                    r#"{"__type":"com.amazonaws.sqs#QueueDoesNotExist","message":"The specified queue does not exist."}"#,
                )
            } else {
                ("200 OK", "{}")
            };
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/x-amz-json-1.0\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
}

fn temp_dir() -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "lenslet-run-test-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SEQ.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The calls a test makes: `$TARGETS` SQS actions on queue `jobs`, then
/// exit `$EXIT`. Always the same script, so runs are runs of one command.
const SCRIPT: &str = "test -n \"$AWS_ENDPOINT_URL\" || exit 90\n\
    echo \"to stdout; profile=${AWS_PROFILE:-none}\"\n\
    echo 'to stderr' >&2\n\
    for target in $TARGETS; do\n\
      curl -s -o /dev/null -X POST \"$AWS_ENDPOINT_URL/\" \
        -H \"x-amz-target: AmazonSQS.$target\" \
        -H 'content-type: application/x-amz-json-1.0' \
        -H \"authorization: AWS4-HMAC-SHA256 Credential=${AWS_ACCESS_KEY_ID:-AKID}/20261115/us-east-1/sqs/aws4_request, SignedHeaders=host, Signature=x\" \
        -d '{\"QueueUrl\":\"http://127.0.0.1/000000000000/jobs\"}'\n\
    done\n\
    exit $EXIT\n";

struct Env {
    home: PathBuf,
    work: PathBuf,
    port: u16,
}

impl Env {
    fn new() -> Env {
        let port = fake_emulator();
        let home = temp_dir();
        let emulator = home.join(format!(".aws/emulator/{port}"));
        std::fs::create_dir_all(&emulator).unwrap();
        std::fs::write(emulator.join("emulator_credentials"), "[default]\n").unwrap();
        std::fs::write(emulator.join("config"), "[default]\nregion = us-east-1\n").unwrap();
        let work = home.join("project");
        std::fs::create_dir_all(&work).unwrap();
        Env { home, work, port }
    }

    fn command(&self, dir: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lenslet"));
        command
            .current_dir(dir)
            .env("HOME", &self.home)
            .env_remove("AWS_ENDPOINT_URL")
            .env_remove("AWS_SHARED_CREDENTIALS_FILE")
            .env_remove("AWS_CONFIG_FILE");
        command
    }

    fn lenslet(&self, args: &[&str]) -> Output {
        self.command(&self.work).args(args).output().unwrap()
    }

    fn upstream(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// `lenslet run --upstream FAKE [flags] -- sh -c SCRIPT` in `work`.
    fn run(&self, flags: &[&str], targets: &[&str], exit: i32) -> Output {
        self.run_in(&self.work, flags, targets, exit)
    }

    fn run_in(&self, dir: &Path, flags: &[&str], targets: &[&str], exit: i32) -> Output {
        let upstream = self.upstream();
        let mut args = vec!["run", "--upstream", &upstream];
        args.extend_from_slice(flags);
        self.invoke(dir, &args, targets, exit)
    }

    /// `lenslet ARGS -- sh -c SCRIPT` with the scripted calls.
    fn invoke(&self, dir: &Path, args: &[&str], targets: &[&str], exit: i32) -> Output {
        self.command(dir)
            .args(args)
            .args(["--", "sh", "-c", SCRIPT])
            .env("TARGETS", targets.join(" "))
            .env("EXIT", exit.to_string())
            .output()
            .unwrap()
    }

    /// The report a run wrote with `--json PATH`.
    fn report(&self, flags: &[&str], targets: &[&str], exit: i32) -> (Output, serde_json::Value) {
        let path = self.home.join("report.json");
        let _ = std::fs::remove_file(&path);
        let mut all = vec!["--json", path.to_str().unwrap()];
        all.extend_from_slice(flags);
        let output = self.run(&all, targets, exit);
        let report = std::fs::read_to_string(&path)
            .map(|text| serde_json::from_str(&text).unwrap())
            .unwrap_or_default();
        (output, report)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn has_curl() -> bool {
    Command::new("curl").arg("--version").output().is_ok()
}

fn lines(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

macro_rules! needs_curl {
    () => {
        if !has_curl() {
            eprintln!("skipping: curl is not installed");
            return;
        }
    };
}

#[test]
fn run_explains_failures_keeps_the_exit_code_and_writes_a_format_4_run_file() {
    needs_curl!();
    let env = Env::new();
    let (output, report) = env.report(&[], &["SendMessage", "GetQueueUrl"], 4);
    let text = stderr(&output);
    assert_eq!(
        output.status.code(),
        Some(4),
        "exit code passes through:\n{text}"
    );
    assert!(text.contains("2 calls · 1 error"), "{text}");
    assert!(
        text.contains(
            "✗ SQS GetQueueUrl  400  QueueDoesNotExist  The specified queue does not exist."
        ),
        "{text}"
    );
    assert!(text.contains("access: 2 permissions observed"), "{text}");

    assert_eq!(report["version"], 4);
    assert_eq!(report["exit_code"], 4);
    assert_eq!(report["findings"][0]["kind"], "failure");
    assert_eq!(report["findings"][0]["code"], "QueueDoesNotExist");
    assert_eq!(
        report["findings"][0]["title"],
        "SQS GetQueueUrl → QueueDoesNotExist"
    );
    assert_eq!(report["access"][0]["action"], "sqs:GetQueueUrl");
    assert_eq!(report["access"][0]["resource"], "arn:aws:sqs:*:*:jobs");
    assert!(report["emulator"].is_string());
    for gone in [
        "failures",
        "access_added",
        "access_unused",
        "endpoint",
        "upstream",
        "unfinished",
    ] {
        assert!(report.get(gone).is_none(), "{gone}");
    }

    // One run file: header, one line per call, and the report as the last line.
    let trace = PathBuf::from(report["trace"].as_str().unwrap());
    assert!(trace.starts_with(env.home.join(".lenslet/runs")));
    let records = lines(&trace);
    assert_eq!(records.len(), 4);
    let header = &records[0];
    assert_eq!(header["type"], "run");
    assert_eq!(header["version"], 4);
    assert_eq!(header["command"][0], "sh");
    assert_eq!(header["emulator"], report["emulator"]);
    assert_eq!(header["log"], report["log"]);
    for gone in ["policy", "enforce", "routes", "max_body"] {
        assert!(header.get(gone).is_none(), "{gone}");
    }
    assert_eq!(records[1]["type"], "call");
    assert_eq!(records[1]["access"][0]["action"], "sqs:SendMessage");
    assert_eq!(
        records[1]["concrete"][0]["resource"],
        "arn:aws:sqs:us-east-1:*:jobs"
    );
    assert_eq!(records[3]["type"], "end");
    assert_eq!(records[3]["exit_code"], 4);
}

#[test]
fn the_test_output_reaches_the_terminal_and_the_run_log() {
    needs_curl!();
    let env = Env::new();
    let (output, report) = env.report(&[], &["SendMessage"], 0);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("to stdout; profile=none\n"));
    assert!(stderr(&output).contains("to stderr\n"));
    let log = PathBuf::from(report["log"].as_str().unwrap());
    assert_eq!(
        log,
        PathBuf::from(report["trace"].as_str().unwrap()).with_extension("log")
    );
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("to stdout; profile=none\n"), "{text}");
    assert!(text.contains("to stderr\n"), "{text}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    assert!(
        stderr(&output).contains("· output ~/.lenslet/runs/"),
        "{}",
        stderr(&output)
    );
    // The removed flag is refused.
    let output = env.run(&["--log", "x.log"], &[], 0);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn the_lock_workflow_catches_new_access_and_regressions() {
    needs_curl!();
    let env = Env::new();

    // No lock yet: --check refuses to guess.
    let output = env.run(&["--check"], &["SendMessage"], 0);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("--check needs lenslet.lock"));

    // --update never locks a failed run's partial access.
    let output = env.run(&["--update"], &["SendMessage"], 1);
    assert_eq!(output.status.code(), Some(1));
    assert!(!env.work.join("lenslet.lock").exists());
    assert!(stderr(&output).contains("lenslet.lock not written"));

    let output = env.run(&["--update"], &["SendMessage"], 0);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let lock = std::fs::read_to_string(env.work.join("lenslet.lock")).unwrap();
    assert!(
        lock.contains("sqs:SendMessage  arn:aws:sqs:*:*:jobs"),
        "{lock}"
    );

    let output = env.run(&["--check"], &["SendMessage"], 0);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stderr(&output).contains("access: within lenslet.lock"));

    // New access and a new failure: --check fails with 3 and says what changed.
    let (output, report) = env.report(
        &["--check"],
        &["SendMessage", "DeleteMessage", "GetQueueUrl"],
        0,
    );
    let text = stderr(&output);
    assert_eq!(output.status.code(), Some(3), "{text}");
    assert!(
        text.contains("access: 2 permissions not in lenslet.lock"),
        "{text}"
    );
    assert!(text.contains("+ sqs:DeleteMessage"), "{text}");
    assert!(text.contains("1 new failure, 0 fixed"), "{text}");
    let added: Vec<&str> = report["access"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["new"] == true)
        .map(|entry| entry["action"].as_str().unwrap())
        .collect();
    assert_eq!(added, ["sqs:DeleteMessage", "sqs:GetQueueUrl"]);
    assert_eq!(
        report["regressions"]["new_failures"][0]["code"],
        "QueueDoesNotExist"
    );
    let records = lines(Path::new(report["trace"].as_str().unwrap()));
    let delete = records
        .iter()
        .find(|record| record["access"][0]["action"] == "sqs:DeleteMessage")
        .unwrap();
    assert_eq!(
        delete["access"][0]["new"], true,
        "the call that widened access"
    );

    // A failing command keeps its own exit code even when access grew.
    let output = env.run(&["--check"], &["DeleteMessage"], 7);
    assert_eq!(output.status.code(), Some(7));

    // Using less than the lock: --strict fails.
    std::fs::write(
        env.work.join("lenslet.lock"),
        "sqs:SendMessage  arn:aws:sqs:*:*:jobs\nsqs:PurgeQueue  arn:aws:sqs:*:*:jobs  # test\n",
    )
    .unwrap();
    let markdown = env.home.join("report.md");
    let output = env.run(
        &["--check", "--markdown", markdown.to_str().unwrap()],
        &["SendMessage"],
        0,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let md = std::fs::read_to_string(&markdown).unwrap();
    assert!(md.contains("AWS access is within `lenslet.lock`"), "{md}");
    assert!(md.contains("| − | `sqs:PurgeQueue`"), "{md}");
    let output = env.run(&["--check", "--strict"], &["SendMessage"], 0);
    assert_eq!(output.status.code(), Some(3));

    // The lock becomes a policy, without its `# test` lines.
    let output = env.lenslet(&["policy"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let policy: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        policy["Statement"][0]["Action"],
        serde_json::json!(["sqs:SendMessage"])
    );
    assert_eq!(policy["Statement"][0]["Resource"], "arn:aws:sqs:*:*:jobs");
    for removed in [
        &["policy", "--format", "terraform"][..],
        &["policy", "--role", "x"],
    ] {
        assert_eq!(env.lenslet(removed).status.code(), Some(2), "{removed:?}");
    }
}

#[test]
fn check_evaluates_the_recorded_concrete_access_of_the_newest_run_here() {
    needs_curl!();
    let env = Env::new();
    let policy = env.work.join("role.json");
    std::fs::write(
        &policy,
        r#"{"Version":"2012-10-17","Statement":[
            {"Sid":"Send","Effect":"Allow","Action":"sqs:SendMessage","Resource":"arn:aws:sqs:us-east-1:*:jobs"},
            {"Sid":"EuOnly","Effect":"Allow","Action":"sqs:DeleteMessage","Resource":"arn:aws:sqs:eu-west-1:*:jobs"},
            {"Sid":"Tagged","Effect":"Allow","Action":"sqs:PurgeQueue","Resource":"*",
             "Condition":{"StringEquals":{"aws:ResourceTag/env":"dev"}}}
        ]}"#,
    )
    .unwrap();

    // Nothing recorded here yet.
    let output = env.lenslet(&["check", "--policy", "role.json"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("lenslet run"),
        "{}",
        stderr(&output)
    );

    let (output, report) = env.report(&[], &["SendMessage", "DeleteMessage", "PurgeQueue"], 0);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let mine = report["run"].as_str().unwrap().to_string();
    // A newer run in another directory is not the default.
    let other = env.home.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let output = env.run_in(&other, &[], &["SendMessage"], 0);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let output = env.lenslet(&["check", "--policy", "role.json", "--json"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    let checked: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(checked["run"], mine.as_str());
    assert_eq!(checked["policy"], "role.json");
    assert_eq!(checked["calls"], 3);
    let denied = checked["denied"].as_array().unwrap();
    assert_eq!(denied.len(), 2, "{checked}");
    assert_eq!(denied[0]["action"], "sqs:DeleteMessage");
    assert_eq!(denied[0]["resource"], "arn:aws:sqs:us-east-1:*:jobs");
    assert_eq!(denied[0]["reason"], "no_allow");
    assert!(denied[0].get("conditional").is_none());
    assert_eq!(denied[1]["action"], "sqs:PurgeQueue");
    assert_eq!(denied[1]["conditional"], true);
    assert_eq!(denied[1]["by"], "Tagged");

    let output = env.lenslet(&["check", "--policy", "role.json"]);
    let text = stdout(&output);
    assert!(text.contains("2 permissions denied"), "{text}");
    assert!(
        text.contains(
            "only statement `Tagged` allows it, under a condition: conditional, not evaluated"
        ),
        "{text}"
    );

    // What is checked is the access derived live, not the stored body: a
    // run file whose stored bodies say nothing checks the same.
    let trace = PathBuf::from(report["trace"].as_str().unwrap());
    let text = std::fs::read_to_string(&trace).unwrap();
    let blanked: Vec<String> = text
        .lines()
        .map(|line| {
            let mut record: serde_json::Value = serde_json::from_str(line).unwrap();
            if record["type"] == "call" {
                record["request"]["body"]["text"] = "{}".into();
                record["access"] = serde_json::json!([]);
            }
            record.to_string()
        })
        .collect();
    std::fs::write(&trace, blanked.join("\n") + "\n").unwrap();
    let output = env.lenslet(&["check", "--policy", "role.json", &mine, "--json"]);
    let again: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(again["denied"].as_array().unwrap().len(), 2, "{again}");
    assert_eq!(
        again["denied"][0]["resource"],
        "arn:aws:sqs:us-east-1:*:jobs"
    );

    // The removed flags are refused.
    for removed in [
        &["check", "--context", "a=b"][..],
        &["check", "--lock", "lenslet.lock"],
    ] {
        assert_eq!(env.lenslet(removed).status.code(), Some(2), "{removed:?}");
    }
}

#[test]
fn a_run_file_of_another_format_is_refused() {
    let env = Env::new();
    let runs = env.home.join(".lenslet/runs");
    std::fs::create_dir_all(&runs).unwrap();
    let old = runs.join("20261101-000000-1.jsonl");
    std::fs::write(
        &old,
        "{\"type\":\"run\",\"version\":3,\"run\":\"20261101-000000-1\",\"command\":[\"t\"],\"cwd\":\"/w\",\"pid\":1,\"endpoint\":\"\",\"upstream\":\"\",\"started\":\"\"}\n",
    )
    .unwrap();
    std::fs::write(env.work.join("p.json"), r#"{"Statement":[]}"#).unwrap();
    let output = env.lenslet(&["check", "--policy", "p.json", "20261101-000000-1"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        stderr(&output).trim_end(),
        format!(
            "lenslet: run file {} is format 3; this Lenslet reads format 4 — re-run your tests",
            old.display()
        )
    );
}

#[test]
fn real_credentials_never_reach_the_command_or_the_run_file() {
    needs_curl!();
    let env = Env::new();
    // No emulator credentials file at all: placeholders stand in.
    std::fs::remove_dir_all(env.home.join(".aws")).unwrap();
    let summary = env.home.join("s.json");
    let upstream = env.upstream();
    let output = env
        .command(&env.work)
        .args([
            "run",
            "--upstream",
            &upstream,
            "--json",
            summary.to_str().unwrap(),
        ])
        .args(["--", "sh", "-c", SCRIPT])
        .env("TARGETS", "ListQueues")
        .env("EXIT", "0")
        .env("AWS_ACCESS_KEY_ID", "AKIAREALKEYDONOTLEAK")
        .env("AWS_SECRET_ACCESS_KEY", "real-secret")
        .env("AWS_PROFILE", "prod")
        .env("AWS_ENDPOINT_URL_SQS", "http://127.0.0.1:1")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("profile=none"),
        "{}",
        stdout(&output)
    );
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&summary).unwrap()).unwrap();
    assert_eq!(
        report["calls"], 1,
        "the call went through Lenslet, not around it"
    );
    let trace = std::fs::read_to_string(report["trace"].as_str().unwrap()).unwrap();
    assert!(
        !trace.contains("AKIAREALKEYDONOTLEAK"),
        "the real key id leaked"
    );
    assert!(
        trace.contains("lenslet-local"),
        "placeholder keys were used"
    );
    assert!(
        stderr(&output).contains("unset AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_PROFILE")
    );
    // There is no way to keep them.
    let output = env.run(&["--no-env"], &[], 0);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn usage_errors_exit_2_and_removed_commands_are_gone() {
    let env = Env::new();
    for args in [
        &["run"][..],
        &["run", "--check"],
        &["run", "--nope", "x"],
        &["run", "--enforce", "--", "true"],
        &["frobnicate"],
        &["runs"],
        &["shell"],
        &["check"],
    ] {
        let output = env.lenslet(args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            stderr(&output)
        );
    }
    let help = env.lenslet(&["--help"]);
    let text = stdout(&help);
    for removed in [
        "--enforce",
        "--context",
        "terraform",
        "lenslet last",
        "lenslet shell",
        "--listen",
        "--max-body",
        "--no-env",
        "/_lenslet",
    ] {
        assert!(!text.contains(removed), "{removed} in usage");
    }
}

#[test]
fn doctor_reports_the_test_command_init_uses() {
    needs_curl!();
    let env = Env::new();
    std::fs::write(env.work.join("Makefile"), "test:\n\t@echo tested\n").unwrap();
    let output = env.lenslet(&["doctor", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let doctor: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(doctor["version"], env!("CARGO_PKG_VERSION"));
    assert!(doctor["cli"].as_str().unwrap().ends_with("lenslet"));
    assert!(doctor["emulators"].is_array());
    assert_eq!(doctor["managed"]["name"], "moto");
    assert_eq!(doctor["test_command"], "make test");
    assert_eq!(doctor["lock"], serde_json::Value::Null);
    assert_eq!(doctor["ci_workflow"], false);

    let output = env.lenslet(&["init", "--upstream", &env.upstream()]);
    let text = stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(text.contains("detected test command `make test`"), "{text}");
    let workflow = std::fs::read_to_string(env.work.join(".github/workflows/lenslet.yml")).unwrap();
    assert!(workflow.contains("command: 'make test'"), "{workflow}");
    assert!(
        workflow.contains(&format!("upstream: '{}'", env.upstream())),
        "{workflow}"
    );

    let doctor: serde_json::Value =
        serde_json::from_slice(&env.lenslet(&["doctor", "--json"]).stdout).unwrap();
    assert!(doctor["lock"].as_str().unwrap().ends_with("lenslet.lock"));
    assert_eq!(doctor["ci_workflow"], true);

    // An edited workflow is kept without --force.
    std::fs::write(env.work.join(".github/workflows/lenslet.yml"), "# mine\n").unwrap();
    let output = env.lenslet(&["init", "--upstream", &env.upstream()]);
    assert_eq!(output.status.code(), Some(0));
    assert!(stderr(&output).contains("exists, left alone"));
    assert_eq!(
        std::fs::read_to_string(env.work.join(".github/workflows/lenslet.yml")).unwrap(),
        "# mine\n"
    );
    let output = env.lenslet(&["init", "--force", "--upstream", &env.upstream()]);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        std::fs::read_to_string(env.work.join(".github/workflows/lenslet.yml"))
            .unwrap()
            .contains("command: 'make test'")
    );
}

fn moto_installed() -> bool {
    let on_path = std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| dir.join("moto_server").is_file())
    });
    on_path
        || Command::new("python3")
            .args(["-c", "import moto.server"])
            .output()
            .is_ok_and(|out| out.status.success())
}

/// Stopping a run (the window signals `lenslet run`'s process group; here,
/// `SIGTERM` to it) stops the test command, writes the `end` report and
/// stops the moto Lenslet started.
#[cfg(unix)]
#[test]
fn sigterm_ends_the_run_gracefully() {
    let env = Env::new();
    let managed = moto_installed();
    let upstream = env.upstream();
    let mut args = vec!["run"];
    if managed {
        args.extend(["--emulator", "moto"]);
    } else {
        eprintln!("note: moto is not installed; checking SIGTERM against the stand-in emulator");
        args.extend(["--upstream", &upstream]);
    }
    args.extend(["--", "sh", "-c", "echo started; sleep 60; echo never"]);
    let mut child = env
        .command(&env.work)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let runs = env.home.join(".lenslet/runs");
    let deadline = Instant::now() + Duration::from_secs(30);
    let (trace, log) = loop {
        let found = std::fs::read_dir(&runs).ok().and_then(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        });
        if let Some(trace) = found {
            let log = trace.with_extension("log");
            if std::fs::read_to_string(&log).is_ok_and(|text| text.contains("started")) {
                break (trace, log);
            }
        }
        assert!(Instant::now() < deadline, "the run did not start");
        std::thread::sleep(Duration::from_millis(50));
    };
    let header = &lines(&trace)[0];
    let moto = header["upstream"]
        .as_str()
        .unwrap()
        .trim_start_matches("http://")
        .to_string();
    Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "lenslet did not stop");
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert_eq!(status.code(), Some(143), "the command's signal exit: {err}");
    let records = lines(&trace);
    let end = records.last().unwrap();
    assert_eq!(end["type"], "end", "{err}");
    assert_eq!(end["exit_code"], 143);
    assert!(!std::fs::read_to_string(&log).unwrap().contains("never"));
    if managed {
        assert_eq!(header["emulator"], "moto (started by Lenslet)");
        assert!(
            std::net::TcpStream::connect(&moto).is_err(),
            "moto still listens on {moto}"
        );
    }
}

/// With moto installed and no emulator running: a run with no options
/// starts moto, says so even with --quiet, and stops it after.
#[test]
fn managed_moto_answers_when_nothing_is_running() {
    needs_curl!();
    if !moto_installed() {
        eprintln!("note: moto is not installed (pip install 'moto[server]'); skipping");
        return;
    }
    let env = Env::new();
    let doctor: serde_json::Value =
        serde_json::from_slice(&env.lenslet(&["doctor", "--json"]).stdout).unwrap();
    assert_eq!(doctor["managed"]["available"], true);
    if !doctor["emulators"].as_array().unwrap().is_empty() {
        eprintln!("note: an emulator is running on this machine; skipping");
        return;
    }
    let script = "set -e\n\
        for target in CreateQueue ListQueues; do\n\
          code=$(curl -s -o /dev/null -w '%{http_code}' -X POST \"$AWS_ENDPOINT_URL/\" \
            -H \"x-amz-target: AmazonSQS.$target\" \
            -H 'content-type: application/x-amz-json-1.0' \
            -H 'authorization: AWS4-HMAC-SHA256 Credential=AKID/20261122/us-east-1/sqs/aws4_request, SignedHeaders=host, Signature=x' \
            -d '{\"QueueName\":\"jobs\"}')\n\
          test \"$code\" = 200 || { echo \"$target: HTTP $code\" >&2; exit 1; }\n\
        done\n";
    let summary = env.home.join("moto.json");
    let output = env.lenslet(&[
        "run",
        "--quiet",
        "--json",
        summary.to_str().unwrap(),
        "--",
        "sh",
        "-c",
        script,
    ]);
    let text = stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(
        text.starts_with("lenslet: no emulator running; started moto (pip package) on 127.0.0.1:"),
        "{text}"
    );
    assert_eq!(text.lines().count(), 1, "{text}");
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&summary).unwrap()).unwrap();
    assert_eq!(report["emulator"], "moto (started by Lenslet)");
    assert_eq!(report["errors"], 0, "{report}");
    let trace = PathBuf::from(report["trace"].as_str().unwrap());
    assert!(trace.with_extension("emulator.log").is_file());
    let header = &lines(&trace)[0];
    let address = header["upstream"]
        .as_str()
        .unwrap()
        .trim_start_matches("http://");
    assert!(
        std::net::TcpStream::connect(address).is_err(),
        "moto still listens on {address}"
    );
}
