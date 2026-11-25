//! Indexes AWS Emulator's own list of what it does not do yet
//! (`issues/things-that-do-not-work/`) for the services Lenslet serves, so
//! a failed call can say when the emulator is known to fail it.

use std::{fmt::Write as _, path::Path};

/// The emulator's names for Lenslet's services, as its issues spell them.
const SERVICES: &[(&str, &str)] = &[
    ("s3", "s3"),
    ("dynamodb", "dynamodb"),
    ("sqs", "sqs"),
    ("sns", "sns"),
    ("sts", "sts"),
    ("iam", "iam"),
    ("kms", "kms"),
    ("lambda", "lambda"),
    ("secretsmanager", "secretsmanager"),
    ("secrets-manager", "secretsmanager"),
    ("ssm", "ssm"),
    ("events", "events"),
    ("eventbridge", "events"),
    ("logs", "logs"),
    ("cloudwatch-logs", "logs"),
    ("states", "states"),
    ("sfn", "states"),
    ("stepfunctions", "states"),
    ("kinesis", "kinesis"),
];

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir");
    let emulator = Path::new(&manifest).join("../../third_party/aws-emulator");
    let issues = emulator.join("issues/things-that-do-not-work");
    println!("cargo:rerun-if-changed={}", issues.display());
    let commit = std::process::Command::new("git")
        .arg("-C")
        .arg(&emulator)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "main".into());

    // (service, operation, code, severity, title, file)
    let mut rows: Vec<(String, String, String, u8, String, String)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&issues) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let file = path
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or_default();
            let mut severity = 2;
            let mut title = String::new();
            let mut keys = Vec::new();
            for line in text.lines().take(400) {
                if let Some(s) = line.strip_prefix("Issue-Severity:") {
                    severity = match s.trim() {
                        "CRITICAL" => 0,
                        "HIGH" => 1,
                        "MEDIUM" => 2,
                        _ => 3,
                    };
                } else if let Some(k) = line.strip_prefix("Issue-Key:") {
                    keys.push(k.trim().to_string());
                } else if title.is_empty()
                    && let Some(t) = line.strip_prefix("# ")
                {
                    title = t.trim().to_string();
                }
            }
            if title.chars().count() > 200 {
                title = title.chars().take(199).collect::<String>() + "…";
            }
            for key in keys {
                let mut parts = key.split('|');
                let (Some(service), Some(operation)) = (parts.next(), parts.next()) else {
                    continue;
                };
                let Some(&(_, ours)) = SERVICES.iter().find(|(name, _)| *name == service) else {
                    continue;
                };
                // `bucket.case-name|run#3|...` keys name a test, not an operation.
                if operation.is_empty() || !operation.starts_with(|c: char| c.is_ascii_uppercase())
                {
                    continue;
                }
                let code = parts.next().unwrap_or_default();
                let code = if code.contains("->") { "" } else { code };
                rows.push((
                    ours.into(),
                    operation.into(),
                    code.into(),
                    severity,
                    title.clone(),
                    file.into(),
                ));
            }
        }
    }
    rows.sort();
    rows.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1 && a.2 == b.2 && a.5 == b.5);

    let mut out = String::new();
    writeln!(out, "/// The emulator commit the gaps were listed at.").unwrap();
    writeln!(out, "pub const COMMIT: &str = {commit:?};").unwrap();
    writeln!(
        out,
        "/// (service, operation, error code or \"\", severity 0 critical..3 low, title, file), sorted."
    )
    .unwrap();
    writeln!(
        out,
        "pub static GAPS: &[(&str, &str, &str, u8, &str, &str)] = &["
    )
    .unwrap();
    for (service, operation, code, severity, title, file) in &rows {
        writeln!(
            out,
            "    ({service:?}, {operation:?}, {code:?}, {severity}, {title:?}, {file:?}),"
        )
        .unwrap();
    }
    writeln!(out, "];").unwrap();
    let dest = Path::new(&std::env::var("OUT_DIR").expect("out dir")).join("gaps.rs");
    std::fs::write(dest, out).expect("write the gaps index");
}
