//! `lenslet` end to end: the real binary, a child process that calls AWS
//! through `AWS_ENDPOINT_URL`, a stand-in emulator, and the lock workflow.

use std::{
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output},
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

    fn lenslet(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_lenslet"))
            .args(args)
            .current_dir(&self.work)
            .env("HOME", &self.home)
            .env_remove("AWS_SHARED_CREDENTIALS_FILE")
            .env_remove("AWS_CONFIG_FILE")
            .output()
            .unwrap()
    }

    /// `lenslet run [flags] -- sh -c SCRIPT`. The script is always the same
    /// (it reads its calls and exit code from the environment), so runs are
    /// runs of one command, as with a real `npm test`.
    fn run(&self, flags: &[&str], targets: &[&str], exit: i32) -> Output {
        let upstream = format!("http://127.0.0.1:{}", self.port);
        let mut args = vec!["run", "--upstream", &upstream];
        args.extend_from_slice(flags);
        self.invoke(&args, targets, exit)
    }

    /// `lenslet ARGS -- sh -c SCRIPT` with the scripted calls.
    fn invoke(&self, args: &[&str], targets: &[&str], exit: i32) -> Output {
        let script = "test -n \"$AWS_ENDPOINT_URL\" || exit 90\n\
            test -n \"$AWS_SHARED_CREDENTIALS_FILE\" || exit 91\n\
            for target in $TARGETS; do\n\
              curl -s -o /dev/null -X POST \"$AWS_ENDPOINT_URL/\" \
                -H \"x-amz-target: AmazonSQS.$target\" \
                -H 'content-type: application/x-amz-json-1.0' \
                -H 'authorization: AWS4-HMAC-SHA256 Credential=AKID/20261115/us-east-1/sqs/aws4_request, SignedHeaders=host, Signature=x' \
                -d '{\"QueueUrl\":\"http://127.0.0.1/000000000000/jobs\"}'\n\
            done\n\
            exit $EXIT\n";
        Command::new(env!("CARGO_BIN_EXE_lenslet"))
            .args(args)
            .args(["--", "sh", "-c", script])
            .current_dir(&self.work)
            .env("HOME", &self.home)
            .env("TARGETS", targets.join(" "))
            .env("EXIT", exit.to_string())
            .env_remove("AWS_SHARED_CREDENTIALS_FILE")
            .env_remove("AWS_CONFIG_FILE")
            .output()
            .unwrap()
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

#[test]
fn run_explains_failures_keeps_the_exit_code_and_writes_one_run_file() {
    if !has_curl() {
        eprintln!("skipping: curl is not installed");
        return;
    }
    let env = Env::new();
    let summary = env.home.join("summary.json");
    let output = env.run(
        &["--json", summary.to_str().unwrap()],
        &["SendMessage", "GetQueueUrl"],
        4,
    );
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

    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&summary).unwrap()).unwrap();
    assert_eq!(report["exit_code"], 4);
    assert_eq!(report["failures"][0]["code"], "QueueDoesNotExist");
    assert_eq!(report["findings"][0]["kind"], "failure");
    assert_eq!(
        report["findings"][0]["title"],
        "SQS GetQueueUrl → QueueDoesNotExist"
    );
    assert_eq!(report["access"][0]["action"], "sqs:GetQueueUrl");
    assert_eq!(report["access"][0]["resource"], "arn:aws:sqs:*:*:jobs");

    // One run file: header, calls, and the same report as the last line.
    let trace = PathBuf::from(report["trace"].as_str().unwrap());
    assert!(trace.starts_with(env.home.join(".lenslet/runs")));
    let records = lines(&trace);
    assert_eq!(records[0]["type"], "run");
    assert_eq!(records[0]["command"][0], "sh");
    assert_eq!(records[1]["type"], "call");
    assert_eq!(records[1]["access"][0]["action"], "sqs:SendMessage");
    let end = records.last().unwrap();
    assert_eq!(end["type"], "end");
    assert_eq!(end["exit_code"], 4);

    // `lenslet last --json` is that report, for an agent that did not run it.
    let last = env.lenslet(&["last", "--json"]);
    let last: serde_json::Value = serde_json::from_slice(&last.stdout).unwrap();
    assert_eq!(last["run"], report["run"]);
    assert_eq!(last["failures"], report["failures"]);
}

#[test]
fn the_lock_workflow_catches_new_access_and_regressions() {
    if !has_curl() {
        eprintln!("skipping: curl is not installed");
        return;
    }
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

    // --update writes the observed access.
    let output = env.run(&["--update"], &["SendMessage"], 0);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let lock = std::fs::read_to_string(env.work.join("lenslet.lock")).unwrap();
    assert!(
        lock.contains("sqs:SendMessage  arn:aws:sqs:*:*:jobs"),
        "{lock}"
    );

    // Same access: --check passes.
    let output = env.run(&["--check"], &["SendMessage"], 0);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stderr(&output).contains("access: within lenslet.lock"));

    // New access and a new failure: --check fails with 3 and says what changed.
    let summary = env.home.join("check.json");
    let output = env.run(
        &["--check", "--json", summary.to_str().unwrap()],
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
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&summary).unwrap()).unwrap();
    assert_eq!(report["access_added"][0]["action"], "sqs:DeleteMessage");
    assert_eq!(
        report["regressions"]["new_failures"][0]["code"],
        "QueueDoesNotExist"
    );
    let records = lines(PathBuf::from(report["trace"].as_str().unwrap()).as_path());
    let delete = records
        .iter()
        .find(|record| record["access"][0]["action"] == "sqs:DeleteMessage")
        .unwrap();
    assert_eq!(
        delete["new"], true,
        "the window can mark the call that widened access"
    );

    // A failing command keeps its own exit code even when access grew.
    let output = env.run(&["--check"], &["DeleteMessage"], 7);
    assert_eq!(output.status.code(), Some(7));

    // The lock becomes a policy.
    let output = env.lenslet(&["policy"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let policy: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(policy["Statement"][0]["Action"][0], "sqs:SendMessage");
    assert_eq!(policy["Statement"][0]["Resource"], "arn:aws:sqs:*:*:jobs");
}

#[test]
fn strict_markdown_custom_lock_and_test_marks() {
    if !has_curl() {
        eprintln!("skipping: curl is not installed");
        return;
    }
    let env = Env::new();
    let lock = env.work.join("infra/aws.lock");
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    let lock_flag = lock.to_str().unwrap();

    let output = env.run(
        &["--update", "--lock", lock_flag],
        &["SendMessage", "DeleteMessage"],
        0,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stderr(&output).contains("wrote infra/aws.lock (2 permissions)"));

    // A `# test` mark survives --update, and policy leaves the line out.
    let text = std::fs::read_to_string(&lock).unwrap();
    let marked = text.replace(
        "sqs:DeleteMessage  arn:aws:sqs:*:*:jobs",
        "sqs:DeleteMessage  arn:aws:sqs:*:*:jobs  # test",
    );
    std::fs::write(&lock, marked).unwrap();
    let output = env.run(
        &["--update", "--lock", lock_flag],
        &["SendMessage", "DeleteMessage"],
        0,
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(
        std::fs::read_to_string(&lock)
            .unwrap()
            .contains("arn:aws:sqs:*:*:jobs  # test")
    );
    let output = env.lenslet(&["policy", "--lock", lock_flag]);
    let policy: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        policy["Statement"][0]["Action"],
        serde_json::json!(["sqs:SendMessage"])
    );

    // Using less than the lock: --check passes, --strict fails and says why.
    let markdown = env.home.join("report.md");
    let output = env.run(
        &[
            "--check",
            "--lock",
            lock_flag,
            "--markdown",
            markdown.to_str().unwrap(),
        ],
        &["SendMessage"],
        0,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stderr(&output).contains("unused: 1 permission in infra/aws.lock"));
    let md = std::fs::read_to_string(&markdown).unwrap();
    assert!(md.contains("AWS access is within `infra/aws.lock`"), "{md}");
    assert!(md.contains("| − | `sqs:DeleteMessage`"), "{md}");
    let output = env.run(
        &["--check", "--strict", "--lock", lock_flag],
        &["SendMessage"],
        0,
    );
    assert_eq!(output.status.code(), Some(3));

    // New access shows in the Markdown with the call that needed it.
    let output = env.run(
        &[
            "--check",
            "--lock",
            lock_flag,
            "--markdown",
            markdown.to_str().unwrap(),
        ],
        &["SendMessage", "DeleteMessage", "PurgeQueue"],
        0,
    );
    assert_eq!(output.status.code(), Some(3));
    let md = std::fs::read_to_string(&markdown).unwrap();
    assert!(
        md.contains("| + | `sqs:PurgeQueue` | `arn:aws:sqs:*:*:jobs` | SQS PurgeQueue |"),
        "{md}"
    );
}

#[test]
fn init_writes_the_workflow_and_the_lock() {
    if !has_curl() {
        eprintln!("skipping: curl is not installed");
        return;
    }
    let env = Env::new();
    let upstream = format!("http://127.0.0.1:{}", env.port);
    let output = env.invoke(&["init", "--upstream", &upstream], &["SendMessage"], 0);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let workflow = std::fs::read_to_string(env.work.join(".github/workflows/lenslet.yml")).unwrap();
    assert!(workflow.contains(&format!(
        "uses: hxddh/lenslet@lenslet-v{}",
        env!("CARGO_PKG_VERSION")
    )));
    assert!(workflow.contains(&format!("upstream: 'http://127.0.0.1:{}'", env.port)));
    assert!(workflow.contains("command: 'sh -c "), "{workflow}");
    let lock = std::fs::read_to_string(env.work.join("lenslet.lock")).unwrap();
    assert!(lock.contains("sqs:SendMessage"));

    // Running again leaves an edited workflow alone.
    std::fs::write(env.work.join(".github/workflows/lenslet.yml"), "# mine\n").unwrap();
    let output = env.invoke(&["init", "--upstream", &upstream], &["SendMessage"], 0);
    assert_eq!(output.status.code(), Some(0));
    assert!(stderr(&output).contains("exists, left alone"));
    assert_eq!(
        std::fs::read_to_string(env.work.join(".github/workflows/lenslet.yml")).unwrap(),
        "# mine\n"
    );

    // No emulator: init says how to start one instead of guessing.
    let output = env.lenslet(&["init", "--upstream", "http://127.0.0.1:1", "--", "true"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("Start your emulator first"));
}

#[test]
fn real_keys_never_reach_the_trace() {
    if !has_curl() {
        eprintln!("skipping: curl is not installed");
        return;
    }
    let env = Env::new();
    // No emulator credentials file at all: the case 3.0 got wrong.
    std::fs::remove_dir_all(env.home.join(".aws")).unwrap();
    let summary = env.home.join("s.json");
    let script = "curl -s -o /dev/null -X POST \"$AWS_ENDPOINT_URL/\" \
        -H \"authorization: AWS4-HMAC-SHA256 Credential=$AWS_ACCESS_KEY_ID/20261115/us-east-1/sqs/aws4_request, Signature=x\" \
        -H 'x-amz-target: AmazonSQS.ListQueues' -d '{}'";
    let upstream = format!("http://127.0.0.1:{}", env.port);
    let output = Command::new(env!("CARGO_BIN_EXE_lenslet"))
        .args([
            "run",
            "--upstream",
            &upstream,
            "--json",
            summary.to_str().unwrap(),
        ])
        .args(["--", "sh", "-c", script])
        .current_dir(&env.work)
        .env("HOME", &env.home)
        .env("AWS_ACCESS_KEY_ID", "AKIAREALKEYDONOTLEAK")
        .env("AWS_SECRET_ACCESS_KEY", "real-secret")
        .env_remove("AWS_SHARED_CREDENTIALS_FILE")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&summary).unwrap()).unwrap();
    let trace = std::fs::read_to_string(report["trace"].as_str().unwrap()).unwrap();
    assert!(
        !trace.contains("AKIAREALKEYDONOTLEAK"),
        "the real key id leaked"
    );
    assert!(
        trace.contains("lenslet-local"),
        "placeholder keys were used"
    );
}

#[test]
fn usage_errors_exit_2() {
    let env = Env::new();
    for args in [
        &["run", "--check"][..],
        &["run", "--nope", "x"],
        &["frobnicate"],
    ] {
        let output = env.lenslet(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn init_detects_moto_and_run_tracks_retries() {
    if !has_curl() {
        eprintln!("skipping: curl is not installed");
        return;
    }
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
            if request.starts_with(b"GET /moto-api/") {
                let page = "<title>Moto</title>";
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nserver: Werkzeug/3.1.0 Python/3.12.0\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
                        page.len()
                    )
                    .as_bytes(),
                );
                continue;
            }
            let body = r#"{"__type":"com.amazonaws.sqs#InternalError","message":"boom"}"#;
            let response = format!(
                "HTTP/1.1 500 Internal Server Error\r\nserver: Werkzeug/3.1.0 Python/3.12.0\r\ncontent-type: application/x-amz-json-1.0\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    let env = Env::new();
    let upstream = format!("http://127.0.0.1:{port}");
    let script = "for n in 1 2; do\n\
        curl -s -o /dev/null -X POST \"$AWS_ENDPOINT_URL/\" \
          -H 'x-amz-target: AmazonSQS.SendMessage' \
          -H 'amz-sdk-invocation-id: inv-test-1' \
          -H \"amz-sdk-request: attempt=$n; max=3\" \
          -H 'content-type: application/x-amz-json-1.0' \
          -H 'authorization: AWS4-HMAC-SHA256 Credential=AKID/20261118/us-east-1/sqs/aws4_request, Signature=x' \
          -d '{\"QueueUrl\":\"http://127.0.0.1/000000000000/jobs\"}'\n\
        done";
    let summary = env.home.join("retry.json");
    let output = Command::new(env!("CARGO_BIN_EXE_lenslet"))
        .args(["init", "--upstream", &upstream, "--", "sh", "-c", script])
        .current_dir(&env.work)
        .env("HOME", &env.home)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let workflow = std::fs::read_to_string(env.work.join(".github/workflows/lenslet.yml")).unwrap();
    assert!(
        workflow.contains(&format!("emulator: 'moto_server -p {port}'")),
        "moto upstream should prefill emulator command:\n{workflow}"
    );
    let text = stderr(&output);
    assert!(text.contains("2 calls (1 retry)"), "{text}");
    assert!(text.contains("(attempt 2/3)"), "{text}");

    // Also verify JSON report via `lenslet last --json`.
    let last = env.lenslet(&["last", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&last.stdout).unwrap();
    assert_eq!(report["retries"], 1);
    assert_eq!(report["failures"][1]["attempt"], 2);
    assert_eq!(report["failures"][1]["max_attempts"], 3);
    let _ = summary;
}

#[test]
fn enforce_and_policy_gate_live_calls_end_to_end() {
    if !has_curl() {
        eprintln!("skipping: curl is not installed");
        return;
    }
    let env = Env::new();
    // Write a lock allowing only SendMessage.
    let output = env.run(&["--update"], &["SendMessage"], 0);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    // With --enforce, SendMessage gets 200 and PurgeQueue gets 400 AccessDeniedException.
    let script = "code1=$(curl -s -o /dev/null -w '%{http_code}' -X POST \"$AWS_ENDPOINT_URL/\" \
        -H 'x-amz-target: AmazonSQS.SendMessage' -H 'content-type: application/x-amz-json-1.0' \
        -H 'authorization: AWS4-HMAC-SHA256 Credential=AKID/20261118/us-east-1/sqs/aws4_request, Signature=x' \
        -d '{\"QueueUrl\":\"http://127.0.0.1/000000000000/jobs\"}'); \
      code2=$(curl -s -o /dev/null -w '%{http_code}' -X POST \"$AWS_ENDPOINT_URL/\" \
        -H 'x-amz-target: AmazonSQS.PurgeQueue' -H 'content-type: application/x-amz-json-1.0' \
        -H 'authorization: AWS4-HMAC-SHA256 Credential=AKID/20261118/us-east-1/sqs/aws4_request, Signature=x' \
        -d '{\"QueueUrl\":\"http://127.0.0.1/000000000000/jobs\"}'); \
      test \"$code1\" = 200 && test \"$code2\" = 400";
    let upstream = format!("http://127.0.0.1:{}", env.port);
    let report_path = env.home.join("enforce.json");
    let output = Command::new(env!("CARGO_BIN_EXE_lenslet"))
        .args([
            "run",
            "--enforce",
            "--upstream",
            &upstream,
            "--json",
            report_path.to_str().unwrap(),
            "--",
            "sh",
            "-c",
            script,
        ])
        .current_dir(&env.work)
        .env("HOME", &env.home)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("(1 blocked)"),
        "{}",
        stderr(&output)
    );
    assert!(stderr(&output).contains("[blocked]"), "{}", stderr(&output));
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report_path).unwrap()).unwrap();
    assert_eq!(report["blocked"], 1);
    assert_eq!(report["failures"][0]["blocked"], true);

    // Export the lock to an IAM policy JSON and enforce against `--policy` + `--check`.
    let pol = env.lenslet(&["policy"]);
    let pol_path = env.work.join("iam.json");
    std::fs::write(&pol_path, &pol.stdout).unwrap();
    let check_out = Command::new(env!("CARGO_BIN_EXE_lenslet"))
        .args([
            "run",
            "--enforce",
            "--check",
            "--policy",
            pol_path.to_str().unwrap(),
            "--upstream",
            &upstream,
            "--",
            "sh",
            "-c",
            script,
        ])
        .current_dir(&env.work)
        .env("HOME", &env.home)
        .output()
        .unwrap();
    assert_eq!(check_out.status.code(), Some(3), "{}", stderr(&check_out));
    assert!(
        stderr(&check_out).contains("not in iam.json"),
        "{}",
        stderr(&check_out)
    );

    // `lenslet runs --json` lists recorded runs newest-first, and `lenslet diff --json`
    // compares the two most recent finished runs.
    let runs_out = env.lenslet(&["runs", "--json"]);
    assert_eq!(runs_out.status.code(), Some(0), "{}", stderr(&runs_out));
    let runs_json: serde_json::Value = serde_json::from_slice(&runs_out.stdout).unwrap();
    assert!(runs_json.as_array().unwrap().len() >= 3, "{runs_json}");

    let diff_out = env.lenslet(&["diff", "--json"]);
    assert_eq!(diff_out.status.code(), Some(0), "{}", stderr(&diff_out));
    let diff_json: serde_json::Value = serde_json::from_slice(&diff_out.stdout).unwrap();
    assert!(diff_json["base_run"].is_string());
    assert!(diff_json["target_run"].is_string());

    // Explicit `prev` and `latest` aliases resolve to the same two runs.
    let alias_diff_out = env.lenslet(&["diff", "prev", "latest", "--json"]);
    assert_eq!(
        alias_diff_out.status.code(),
        Some(0),
        "{}",
        stderr(&alias_diff_out)
    );
    let alias_diff_json: serde_json::Value =
        serde_json::from_slice(&alias_diff_out.stdout).unwrap();
    assert_eq!(alias_diff_json["base_run"], diff_json["base_run"]);
    assert_eq!(alias_diff_json["target_run"], diff_json["target_run"]);
}

#[test]
fn check_re_evaluates_a_recorded_run_and_names_the_denying_statement() {
    if !has_curl() {
        eprintln!("skipping: curl is not installed");
        return;
    }
    let env = Env::new();
    let output = env.run(&["--update"], &["SendMessage", "ReceiveMessage"], 0);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let stdout = |output: &Output| String::from_utf8_lossy(&output.stdout).into_owned();

    // Against the lock it wrote: nothing denied.
    let ok = env.lenslet(&["check"]);
    assert_eq!(ok.status.code(), Some(0), "{}", stderr(&ok));
    assert!(
        stdout(&ok).contains("2 calls · all allowed"),
        "{}",
        stdout(&ok)
    );

    std::fs::write(
        env.work.join("iam.json"),
        r#"{"Statement":[
            {"Sid":"Queues","Effect":"Allow","Action":"sqs:*","Resource":"*"},
            {"Sid":"NoSend","Effect":"Deny","Action":"sqs:SendMessage","Resource":"arn:aws:sqs:*:*:jobs"}
        ]}"#,
    )
    .unwrap();
    let denied = env.lenslet(&["check", "--policy", "iam.json"]);
    assert_eq!(denied.status.code(), Some(3), "{}", stderr(&denied));
    let text = stdout(&denied);
    assert!(text.contains("1 permission denied"), "{text}");
    assert!(
        text.contains("✗ sqs:SendMessage  arn:aws:sqs:us-east-1:*:jobs  (SQS SendMessage)"),
        "{text}"
    );
    assert!(text.contains("→ denied by statement `NoSend`"), "{text}");

    let json = env.lenslet(&["check", "--policy", "iam.json", "--json", "latest"]);
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(report["calls"], 2);
    assert_eq!(report["denied"][0]["by"], "NoSend");
    assert_eq!(report["denied"][0]["reason"], "explicit_deny");
    assert_eq!(report["denied"][0]["operation"], "SendMessage");

    std::fs::write(env.work.join("empty.lock"), "# nothing yet\n").unwrap();
    let markdown = env.home.join("check.md");
    let missing = env.lenslet(&[
        "check",
        "--lock",
        "empty.lock",
        "--markdown",
        markdown.to_str().unwrap(),
    ]);
    assert_eq!(missing.status.code(), Some(3));
    assert!(
        stdout(&missing).contains("→ not in empty.lock"),
        "{}",
        stdout(&missing)
    );
    let md = std::fs::read_to_string(&markdown).unwrap();
    assert!(
        md.contains("**2 permissions denied** across 2 calls."),
        "{md}"
    );

    let usage = env.lenslet(&["check", "--policy", "iam.json", "--lock", "empty.lock"]);
    assert_eq!(usage.status.code(), Some(2));
    assert_eq!(env.lenslet(&["check", "--nope"]).status.code(), Some(2));
}

#[test]
fn env_iac_formats_and_context_simulation_work_end_to_end() {
    let env = Env::new();
    let sh_out = env.lenslet(&["env", "--shell", "sh", "--listen", "9299"]);
    assert_eq!(sh_out.status.code(), Some(0));
    let sh_text = String::from_utf8_lossy(&sh_out.stdout);
    assert!(sh_text.contains("export AWS_ENDPOINT_URL=http://127.0.0.1:9299"));
    let unset = sh_text
        .lines()
        .find(|line| line.starts_with("unset "))
        .unwrap();
    for name in [
        "AWS_PROFILE",
        "AWS_SESSION_TOKEN",
        "AWS_IGNORE_CONFIGURED_ENDPOINT_URLS",
    ] {
        assert!(unset.split(' ').any(|word| word == name), "{unset}");
    }

    let docker_out = env.lenslet(&["env", "--shell", "docker"]);
    assert_eq!(docker_out.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&docker_out.stdout)
            .contains("AWS_ENDPOINT_URL=http://host.docker.internal:9297")
    );

    if !has_curl() {
        return;
    }
    let out = env.run(&["--update"], &["SendMessage"], 0);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    let tf_out = env.lenslet(&["policy", "--format", "terraform"]);
    assert_eq!(tf_out.status.code(), Some(0), "{}", stderr(&tf_out));
    let tf_text = String::from_utf8_lossy(&tf_out.stdout);
    assert!(tf_text.contains("data \"aws_iam_policy_document\" \"lenslet\""));
    assert!(tf_text.contains("\"sqs:SendMessage\""));

    assert!(
        tf_text.contains(
            "\"arn:${data.aws_partition.current.partition}:sqs:${data.aws_region.current.region}:${data.aws_caller_identity.current.account_id}:jobs\""
        ),
        "{tf_text}"
    );

    // The lock and the run it was written from give the same statements.
    for format in ["iam", "terraform"] {
        let from_lock = env.lenslet(&["policy", "--format", format]);
        let from_run = env.lenslet(&["policy", "--run", "latest", "--format", format]);
        assert_eq!(from_lock.status.code(), Some(0), "{}", stderr(&from_lock));
        assert_eq!(from_run.status.code(), Some(0), "{}", stderr(&from_run));
        assert_eq!(
            String::from_utf8_lossy(&from_lock.stdout),
            String::from_utf8_lossy(&from_run.stdout),
            "{format}"
        );
    }
    let iam: serde_json::Value =
        serde_json::from_slice(&env.lenslet(&["policy", "--run", "latest"]).stdout)
            .expect("IAM JSON");
    assert_eq!(iam["Version"], "2012-10-17");

    for removed in ["cfn", "sam", "cdk"] {
        let out = env.lenslet(&["policy", "--format", removed]);
        assert_eq!(out.status.code(), Some(2));
        assert_eq!(
            stderr(&out).trim(),
            format!(
                "lenslet: --format {removed} was removed in 12.0; use --format iam (any IaC tool accepts IAM JSON)"
            )
        );
    }

    // Enterprise policy with cloud-runtime `aws:PrincipalOrgID`: `check` reports
    // `needs cloud context` without `--context`, and passes with `--context`.
    let corp_policy = env.work.join("corp.json");
    std::fs::write(
        &corp_policy,
        r#"{
            "Statement": [{
                "Sid": "InOrgOnly",
                "Effect": "Allow",
                "Action": "sqs:SendMessage",
                "Resource": "arn:aws:sqs:*:*:jobs",
                "Condition": { "StringEquals": { "aws:PrincipalOrgID": "o-acme" } }
            }]
        }"#,
    )
    .unwrap();
    let needs_ctx = env.lenslet(&["check", "--policy", "corp.json"]);
    assert_eq!(needs_ctx.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&needs_ctx.stdout).contains("needs cloud context"),
        "{}",
        String::from_utf8_lossy(&needs_ctx.stdout)
    );

    let with_ctx = env.lenslet(&[
        "check",
        "--policy",
        "corp.json",
        "--context",
        "aws:PrincipalOrgID=o-acme",
    ]);
    assert_eq!(
        with_ctx.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&with_ctx.stdout)
    );
}

/// Whether moto can be started here, as Lenslet looks for it.
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

/// `lenslet` with a fresh HOME, no AWS variables, in `work`.
fn lenslet_in(home: &Path, work: &Path, args: &[&str], path: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lenslet"));
    command
        .args(args)
        .current_dir(work)
        .env("HOME", home)
        .env_remove("AWS_ENDPOINT_URL")
        .env_remove("AWS_SHARED_CREDENTIALS_FILE")
        .env_remove("AWS_CONFIG_FILE");
    if let Some(path) = path {
        command.env("PATH", path);
    }
    command.output().unwrap()
}

/// SQS calls a test suite makes; moto answers them for real.
const SQS_SCRIPT: &str = "set -e\n\
    for target in CreateQueue ListQueues; do\n\
      code=$(curl -s -o /dev/null -w '%{http_code}' -X POST \"$AWS_ENDPOINT_URL/\" \
        -H \"x-amz-target: AmazonSQS.$target\" \
        -H 'content-type: application/x-amz-json-1.0' \
        -H 'authorization: AWS4-HMAC-SHA256 Credential=AKID/20261122/us-east-1/sqs/aws4_request, SignedHeaders=host, Signature=x' \
        -d '{\"QueueName\":\"jobs\"}')\n\
      test \"$code\" = 200 || { echo \"$target: HTTP $code\" >&2; exit 1; }\n\
    done\n";

/// No emulator and no moto: exit 2 with one message saying what to do; the
/// removed `--sandbox` says it was removed, then the same.
#[test]
fn no_emulator_and_no_moto_exits_2_with_what_to_do() {
    let home = temp_dir();
    let work = home.join("app");
    std::fs::create_dir_all(&work).unwrap();
    // An empty PATH: neither `moto_server` nor `python3` can be found.
    let empty = home.join("empty-bin");
    std::fs::create_dir_all(&empty).unwrap();
    let message = "lenslet: no local AWS emulator found. Start one (AWS Emulator, LocalStack, moto) \
        or install moto (pip install 'moto[server]') and Lenslet will start it for you.";
    let output = lenslet_in(&home, &work, &["run", "--", "true"], Some(&empty));
    let text = stderr(&output);
    if text.contains("lenslet: using ") {
        eprintln!("note: an emulator is running on this machine; skipping the not-found check");
        let _ = std::fs::remove_dir_all(&home);
        return;
    }
    assert_eq!(output.status.code(), Some(2), "{text}");
    assert_eq!(text.trim_end(), message);

    for alias in [
        &["run", "--sandbox"][..],
        &["run", "--upstream", "built-in"],
    ] {
        let mut args = alias.to_vec();
        args.extend(["--", "true"]);
        let output = lenslet_in(&home, &work, &args, Some(&empty));
        let text = stderr(&output);
        assert_eq!(output.status.code(), Some(2), "{text}");
        let flag = alias[1..].join(" ");
        assert_eq!(
            text.trim_end(),
            format!("lenslet: {flag} was removed in 12.0; using moto instead\n{message}")
        );
    }
    let output = lenslet_in(&home, &work, &["init", "--", "true"], Some(&empty));
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).ends_with(&format!("{message}\n")));
    let _ = std::fs::remove_dir_all(&home);
}

/// With moto installed and no emulator running: `init` starts moto for the
/// lock run and writes `emulator: moto` into CI; `--sandbox` is moto; a
/// zero-option run starts moto and says so even with --quiet; reports say
/// what answered; moto is gone after each run; `doctor --json` tells it.
#[test]
fn managed_moto_answers_init_runs_and_the_sandbox_alias() {
    if !has_curl() {
        eprintln!("skipping: curl is not installed");
        return;
    }
    if !moto_installed() {
        eprintln!(
            "note: moto is not installed (pip install 'moto[server]'); skipping the managed moto test"
        );
        return;
    }
    let home = temp_dir();
    let work = home.join("app");
    std::fs::create_dir_all(&work).unwrap();
    let run = |args: &[&str]| lenslet_in(&home, &work, args, None);

    let doctor = run(&["doctor", "--json"]);
    assert_eq!(doctor.status.code(), Some(0), "{}", stderr(&doctor));
    let doctor: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(doctor["managed"]["name"], "moto");
    assert_eq!(doctor["managed"]["available"], true);
    let command = doctor["managed"]["command"].as_array().unwrap();
    assert!(
        command == &[serde_json::json!("moto_server")]
            || command == &["python3", "-m", "moto.server"].map(serde_json::Value::from),
        "{command:?}"
    );
    assert!(doctor.get("builtin_sandbox_ready").is_none());
    let running = doctor["emulators"].as_array().unwrap();
    if !running.is_empty() {
        eprintln!("note: an emulator is running on this machine ({running:?}); skipping");
        let _ = std::fs::remove_dir_all(&home);
        return;
    }

    // A. `lenslet init` with nothing running: Lenslet starts moto.
    let output = run(&["init", "--", "sh", "-c", SQS_SCRIPT]);
    let text = stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(
        text.contains("no emulator running; Lenslet will start moto (pip package)"),
        "{text}"
    );
    let workflow = std::fs::read_to_string(work.join(".github/workflows/lenslet.yml")).unwrap();
    assert!(
        workflow.contains("          emulator: moto\n"),
        "{workflow}"
    );
    assert!(!workflow.contains("sandbox"), "{workflow}");
    let lock = std::fs::read_to_string(work.join("lenslet.lock")).unwrap();
    assert!(lock.contains("sqs:CreateQueue"), "{lock}");
    let last = run(&["last", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&last.stdout).unwrap();
    assert_eq!(report["emulator"], "moto (started by Lenslet)");
    assert_eq!(report["errors"], 0, "{report}");
    let upstream = report["upstream"].as_str().unwrap().to_string();
    assert!(upstream.starts_with("http://127.0.0.1:"), "{upstream}");
    let trace = std::fs::read_to_string(report["trace"].as_str().unwrap()).unwrap();
    let header: serde_json::Value = serde_json::from_str(trace.lines().next().unwrap()).unwrap();
    assert_eq!(header["emulator"], "moto (started by Lenslet)");
    // moto's own output went to the run's log, and moto is gone.
    let log = home
        .join(".lenslet/runs")
        .join(format!("{}.emulator.log", report["run"].as_str().unwrap()));
    assert!(log.is_file(), "{}", log.display());
    let address = upstream.trim_start_matches("http://");
    assert!(
        std::net::TcpStream::connect(address).is_err(),
        "moto still listens on {address}"
    );

    // B. Without --force an existing workflow is kept, and what would be
    //    written is shown.
    std::fs::write(work.join(".github/workflows/lenslet.yml"), "# mine\n").unwrap();
    let output = run(&["init", "--emulator", "moto", "--", "sh", "-c", SQS_SCRIPT]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stderr(&output).contains("with --force it would be replaced by:"));
    assert!(stderr(&output).contains("lenslet:   |           emulator: moto"));
    assert_eq!(
        std::fs::read_to_string(work.join(".github/workflows/lenslet.yml")).unwrap(),
        "# mine\n"
    );

    // C. The removed --sandbox is moto, and the Markdown report says so.
    let markdown = home.join("report.md");
    let output = run(&[
        "run",
        "--sandbox",
        "--markdown",
        markdown.to_str().unwrap(),
        "--",
        "sh",
        "-c",
        SQS_SCRIPT,
    ]);
    let text = stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(
        text.starts_with("lenslet: --sandbox was removed in 12.0; using moto instead\n"),
        "{text}"
    );
    let md = std::fs::read_to_string(&markdown).unwrap();
    assert!(
        md.contains("2 AWS calls · 0 errors · 1 service · against moto (started by Lenslet)"),
        "{md}"
    );
    let first = text
        .lines()
        .find(|line| line.starts_with("lenslet: 2 calls"))
        .unwrap();
    assert!(
        first.ends_with(" · against moto (started by Lenslet)"),
        "{first}"
    );

    // D. Nothing given: moto starts, and the one line about it shows even
    //    with --quiet.
    let output = run(&["run", "--quiet", "--", "sh", "-c", SQS_SCRIPT]);
    let text = stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(
        text.starts_with("lenslet: no emulator running; started moto (pip package) on 127.0.0.1:"),
        "{text}"
    );
    assert_eq!(text.lines().count(), 1, "{text}");
    let _ = std::fs::remove_dir_all(&home);
}
