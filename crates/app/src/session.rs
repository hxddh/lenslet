//! What the window shows: the calls, the resources that exist, and the IAM
//! permissions the session used, each with the calls that needed it. The
//! engine's threads write it; the window takes a snapshot when the version
//! changed, and calls are shared (`Arc`), so a snapshot copies pointers, not
//! bodies.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use lenslet_core::access::Access;

/// One AWS call, as the window shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub seq: usize,
    /// The call's id while it runs: the calls it causes name it as `parent`.
    pub id: u64,
    /// The call this one happened inside: the S3 put whose notification
    /// invoked a function, the function whose code made this call.
    pub parent: Option<u64>,
    /// AWS made it, not your code: a delivery, a trigger, a function's own call.
    pub internal: bool,
    /// The terminal command it belongs to.
    pub run: Option<u64>,
    /// Which fresh start it belongs to (0 is the first).
    pub generation: u64,
    /// Milliseconds since the Unix epoch, when the request arrived.
    pub at_ms: u64,
    pub service: String,
    pub operation: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub ms: u64,
    pub region: Option<String>,
    /// `table/orders · id="1"`, `s3://bucket/key`, ...
    pub target: Option<String>,
    pub failure: Option<Failure>,
    /// The emulator does not serve this service or operation.
    pub unsupported: bool,
    /// The IAM permissions it needs (own, then implied).
    pub access: Vec<Access>,
    /// What sent it: `boto3 1.35.0`, `AWS CLI 2.17.0`, ...
    pub client: Option<String>,
    /// The SDK's attempt number (1 for the first try) and its maximum.
    pub attempt: Option<u32>,
    pub max_attempts: Option<u32>,
    /// The SDK's id of the invocation, the same on every retry of it.
    pub invocation: Option<String>,
    /// The id the service gave the request.
    pub request_id: Option<String>,
    pub request: Exchange,
    pub response: Exchange,
}

impl Call {
    pub fn failed(&self) -> bool {
        self.status >= 400
    }

    /// A retry of an earlier attempt of the same SDK call.
    pub fn is_retry(&self) -> bool {
        self.attempt.is_some_and(|a| a > 1)
    }

    /// Whether the call touched the resource named `name` of `service`.
    pub fn touches(&self, service: &str, name: &str) -> bool {
        if !service.eq_ignore_ascii_case(&self.service) || name.is_empty() {
            return false;
        }
        let named = |s: &str| {
            s == name
                || s.ends_with(&format!("/{name}"))
                || s.ends_with(&format!(":{name}"))
                || s.contains(&format!("/{name}/"))
                || s.contains(&format!("/{name} "))
                || s.contains(&format!("//{name}"))
        };
        self.target
            .as_deref()
            .is_some_and(|t| t.split(" · ").any(named))
            || self.access.iter().any(|a| named(&a.resource))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub code: String,
    pub message: Option<String>,
    pub hint: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Exchange {
    pub headers: Vec<(String, String)>,
    pub content_type: Option<String>,
    pub body: String,
    pub bytes: usize,
}

/// One resource the local AWS holds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Resource {
    /// What the window shows: a bucket's, table's or queue's name.
    pub name: String,
    /// How the service addresses it, when that is not the name (a queue's
    /// URL, a topic's or state machine's ARN).
    pub id: String,
    /// A count: objects, items, messages.
    pub count: Option<u64>,
}

/// The resources one service holds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Group {
    /// The signing name (`s3`, `dynamodb`).
    pub service: &'static str,
    pub items: Vec<Resource>,
}

/// A permission and the calls that needed it.
#[derive(Clone, Debug, PartialEq)]
pub struct Permission {
    pub access: Access,
    pub calls: Vec<usize>,
}

#[derive(Default)]
/// One command run in the terminal, and the calls it made.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub id: u64,
    pub command: String,
    pub started_ms: u64,
    /// When it ended, and with what status; `None` while it runs.
    pub ended_ms: Option<u64>,
    pub exit: Option<i32>,
    /// Which fresh start it ran in.
    pub generation: u64,
}

/// A call from outside that changed the account, kept as it was sent, so
/// the account can be rebuilt by sending it again.
#[derive(Clone, Debug)]
pub struct Sent {
    pub at_ms: u64,
    pub method: http::Method,
    pub uri: http::Uri,
    pub headers: http::HeaderMap,
    pub body: bytes::Bytes,
}

#[derive(Default)]
struct Inner {
    /// What changed the account, in order, since it was last empty.
    log: Vec<Sent>,
    calls: Vec<Arc<Call>>,
    /// Each call's place in `calls`, by its id.
    by_id: std::collections::HashMap<u64, usize>,
    runs: Vec<Run>,
    /// The run of the command running now.
    current: Option<u64>,
    permissions: BTreeMap<Access, Vec<usize>>,
    resources: Arc<Vec<Group>>,
    generation: u64,
    /// The calls before this index were cleared from the list.
    cleared: usize,
    version: u64,
}

#[derive(Clone, Default)]
pub struct Session {
    inner: Arc<Mutex<Inner>>,
}

/// The session at one version.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub version: u64,
    pub calls: Vec<Arc<Call>>,
    pub runs: Vec<Run>,
    pub resources: Arc<Vec<Group>>,
    pub permissions: Vec<Permission>,
    pub generation: u64,
}

impl Session {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    pub fn version(&self) -> u64 {
        self.lock().version
    }

    pub fn record(&self, mut call: Call) {
        let mut inner = self.lock();
        call.seq = inner.calls.len();
        call.generation = inner.generation;
        // A call belongs to the run of the call it happened inside, else to
        // the command running now.
        if call.run.is_none() {
            call.run = call
                .parent
                .and_then(|p| inner.by_id.get(&p))
                .and_then(|&i| inner.calls[i].run)
                .or(inner.current);
        }
        if call.id != 0 {
            let seq = call.seq;
            inner.by_id.insert(call.id, seq);
        }
        for access in &call.access {
            let calls = inner.permissions.entry(access.clone()).or_default();
            if calls.last() != Some(&call.seq) {
                calls.push(call.seq);
            }
        }
        inner.calls.push(Arc::new(call));
        inner.version += 1;
    }

    /// A command starts in the terminal.
    pub fn start_run(&self, command: String) -> u64 {
        let mut inner = self.lock();
        let id = inner.runs.last().map_or(1, |r| r.id + 1);
        let generation = inner.generation;
        inner.runs.push(Run {
            id,
            command,
            started_ms: now_ms(),
            ended_ms: None,
            exit: None,
            generation,
        });
        inner.current = Some(id);
        inner.version += 1;
        id
    }

    /// The command running now ended with `exit`.
    pub fn end_run(&self, exit: i32) {
        let mut inner = self.lock();
        let Some(id) = inner.current.take() else {
            return;
        };
        if let Some(run) = inner.runs.iter_mut().find(|r| r.id == id) {
            run.ended_ms = Some(now_ms());
            run.exit = Some(exit);
        }
        inner.version += 1;
    }

    pub fn set_resources(&self, resources: Vec<Group>) {
        let mut inner = self.lock();
        if *inner.resources != resources {
            inner.resources = Arc::new(resources);
            inner.version += 1;
        }
    }

    /// A new, empty AWS: the calls stay (marked by their generation), the
    /// resources and the permissions start over.
    pub fn fresh(&self) {
        let mut inner = self.lock();
        inner.log.clear();
        inner.generation += 1;
        inner.permissions.clear();
        inner.resources = Arc::default();
        inner.version += 1;
    }

    /// Keep a call that changed the account, to send it again on a rewind.
    pub fn keep(&self, sent: Sent) {
        self.lock().log.push(sent);
    }

    /// What changed the account before `at_ms`; what came after is dropped,
    /// as the account goes back to that moment.
    pub fn rewind_to(&self, at_ms: u64) -> Vec<Sent> {
        let mut inner = self.lock();
        inner.log.retain(|s| s.at_ms < at_ms);
        inner.generation += 1;
        inner.resources = Arc::default();
        inner.version += 1;
        inner.log.clone()
    }

    pub fn run(&self, id: u64) -> Option<Run> {
        self.lock().runs.iter().find(|r| r.id == id).cloned()
    }

    /// Clear the list of calls, and the permissions they needed; the
    /// resources stay.
    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.cleared = inner.calls.len();
        inner.permissions.clear();
        let current = inner.current;
        inner.runs.retain(|r| Some(r.id) == current);
        inner.version += 1;
    }

    /// The session, when it changed since `version`.
    pub fn snapshot_since(&self, version: Option<u64>) -> Option<Snapshot> {
        let inner = self.lock();
        if version == Some(inner.version) {
            return None;
        }
        Some(Snapshot {
            version: inner.version,
            calls: inner.calls[inner.cleared..].to_vec(),
            runs: inner.runs.clone(),
            resources: inner.resources.clone(),
            permissions: inner
                .permissions
                .iter()
                .map(|(access, calls)| Permission {
                    access: access.clone(),
                    calls: calls.clone(),
                })
                .collect(),
            generation: inner.generation,
        })
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot_since(None).unwrap_or_default()
    }

    /// One call by its sequence number.
    pub fn call(&self, seq: usize) -> Option<Arc<Call>> {
        self.lock().calls.get(seq).cloned()
    }

    pub fn call_count(&self) -> usize {
        let inner = self.lock();
        inner.calls.len() - inner.cleared
    }
}

/// What sent a request, from its `User-Agent`: `boto3 1.35.0`.
pub fn client_of(user_agent: &str) -> Option<String> {
    const KNOWN: &[(&str, &str)] = &[
        ("aws-cli/", "AWS CLI"),
        ("Boto3/", "boto3"),
        ("Botocore/", "botocore"),
        ("aws-sdk-js/", "AWS SDK for JavaScript"),
        ("aws-sdk-java/", "AWS SDK for Java"),
        ("aws-sdk-go-v2/", "AWS SDK for Go"),
        ("aws-sdk-go/", "AWS SDK for Go"),
        ("aws-sdk-rust/", "AWS SDK for Rust"),
        ("aws-sdk-dotnet-coreclr/", "AWS SDK for .NET"),
        ("aws-sdk-dotnet", "AWS SDK for .NET"),
        ("aws-sdk-ruby3/", "AWS SDK for Ruby"),
        ("aws-sdk-php/", "AWS SDK for PHP"),
        ("aws-sdk-kotlin/", "AWS SDK for Kotlin"),
        ("aws-sdk-swift/", "AWS SDK for Swift"),
        ("aws-sdk-cpp/", "AWS SDK for C++"),
        ("terraform-provider-aws/", "Terraform"),
        ("Terraform/", "Terraform"),
        ("aws-cdk/", "AWS CDK"),
    ];
    let first = user_agent.split_whitespace().collect::<Vec<_>>();
    for (prefix, name) in KNOWN {
        if let Some(token) = first.iter().find(|t| t.starts_with(prefix)) {
            let version = token[prefix.len()..]
                .split(['#', ' '])
                .next()
                .unwrap_or_default();
            return Some(if version.is_empty() {
                name.to_string()
            } else {
                format!("{name} {version}")
            });
        }
    }
    let token = first.first()?;
    Some(token.replace('/', " "))
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_client_from_its_user_agent() {
        assert_eq!(
            client_of("Boto3/1.35.0 md/Botocore#1.35.0 ua/2.0 os/linux#6.1 lang/python#3.12.1")
                .as_deref(),
            Some("boto3 1.35.0")
        );
        assert_eq!(
            client_of("aws-cli/2.17.0 md/awscrt#0.20.11 ua/2.0 os/macos#23.5.0").as_deref(),
            Some("AWS CLI 2.17.0")
        );
        assert_eq!(
            client_of("aws-sdk-js/3.600.0 ua/2.1 os/darwin#23.5.0 lang/js md/nodejs#20.14.0")
                .as_deref(),
            Some("AWS SDK for JavaScript 3.600.0")
        );
        assert_eq!(client_of("curl/8.5.0").as_deref(), Some("curl 8.5.0"));
        assert_eq!(client_of(""), None);
    }

    #[test]
    fn snapshots_only_when_changed_and_clear_keeps_resources() {
        let session = Session::default();
        let first = session.snapshot_since(None).unwrap();
        assert!(session.snapshot_since(Some(first.version)).is_none());
        session.set_resources(vec![Group {
            service: "s3",
            items: vec![Resource::default()],
        }]);
        session.record(call("s3", Some("bucket/a")));
        let s = session.snapshot_since(Some(first.version)).unwrap();
        assert_eq!(s.calls.len(), 1);
        assert_eq!(s.permissions.len(), 1);
        assert_eq!(s.permissions[0].calls, vec![0]);
        session.clear();
        let s = session.snapshot();
        assert!(s.calls.is_empty() && s.permissions.is_empty());
        assert_eq!(s.resources.len(), 1);
        session.record(call("s3", None));
        assert_eq!(session.snapshot().calls[0].seq, 1);
    }

    #[test]
    fn matches_calls_to_the_resources_they_touched() {
        let mut c = call("sqs", None);
        c.target = Some("queue/orders".into());
        assert!(c.touches("sqs", "orders"));
        assert!(!c.touches("sqs", "order"));
        assert!(!c.touches("sns", "orders"));
        let mut c = call("dynamodb", None);
        c.access = vec![Access::new(
            "dynamodb:PutItem",
            "arn:aws:dynamodb:*:*:table/orders",
        )];
        assert!(c.touches("dynamodb", "orders"));
    }

    fn call(service: &str, resource: Option<&str>) -> Call {
        Call {
            seq: 0,
            id: 0,
            parent: None,
            internal: false,
            run: None,
            generation: 0,
            at_ms: 0,
            service: service.into(),
            operation: "Op".into(),
            method: "POST".into(),
            path: "/".into(),
            status: 200,
            ms: 1,
            region: None,
            target: None,
            failure: None,
            unsupported: false,
            access: resource
                .map(|r| vec![Access::new(format!("{service}:Op"), r)])
                .unwrap_or_default(),
            client: None,
            attempt: None,
            max_attempts: None,
            invocation: None,
            request_id: None,
            request: Exchange::default(),
            response: Exchange::default(),
        }
    }
}
