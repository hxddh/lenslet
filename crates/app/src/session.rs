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
    /// The access key it was signed with, if any.
    pub key: Option<String>,
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
        let s3 = self.service.eq_ignore_ascii_case("s3");
        let named = |s: &str| {
            if s3 {
                // A bucket: the first part of its ARN or URL, never a key.
                let rest = s
                    .strip_prefix("arn:aws:s3:::")
                    .or_else(|| s.strip_prefix("s3://"))
                    .unwrap_or(s);
                return rest.split('/').next() == Some(name);
            }
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
    /// What the window shows: the body's text (decoded from CBOR, unframed,
    /// inflated), or a note for data.
    pub body: String,
    pub bytes: usize,
    /// `body` is all of what the body says, decoded (unframed, inflated,
    /// CBOR as JSON): the input to copy as a CLI command or boto3.
    pub complete: bool,
    /// `body` is exactly the bytes sent: what curl sends as it is.
    pub verbatim: bool,
    /// The bytes sent, when `body` is not them and they are small.
    pub raw: Option<bytes::Bytes>,
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
    /// What it did is no longer in the account (a rewind or a new account
    /// undid it): it cannot be rewound to.
    pub undone: bool,
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
    /// The ids the emulator made up in its answer (an S3 multipart upload's,
    /// the receipt handles of the messages a receive took): sent again, the
    /// call gets new ones, and the later calls that name these are sent with
    /// the new ones.
    pub issued: Vec<String>,
    /// Lenslet allowed this role what a denied call needed: the change stands
    /// through rewinds, and comes back when the role is created again.
    pub grant: Option<String>,
}

impl Sent {
    /// `request`, as it arrived at `at_ms`.
    pub fn of(request: &http::Request<bytes::Bytes>, at_ms: u64) -> Sent {
        Sent {
            at_ms,
            method: request.method().clone(),
            uri: request.uri().clone(),
            headers: request.headers().clone(),
            body: request.body().clone(),
            issued: Vec::new(),
            grant: None,
        }
    }

    /// The request, to send again.
    pub fn request(&self) -> http::Request<bytes::Bytes> {
        let mut request = http::Request::new(self.body.clone());
        *request.method_mut() = self.method.clone();
        *request.uri_mut() = self.uri.clone();
        *request.headers_mut() = self.headers.clone();
        request
    }

    /// The role this call created, if it is IAM's `CreateRole`.
    pub fn created_role(&self) -> Option<String> {
        self.iam("CreateRole").map(|(role, _)| role)
    }

    /// The role this call deleted, if it is IAM's `DeleteRole`.
    pub fn deleted_role(&self) -> Option<String> {
        self.iam("DeleteRole").map(|(role, _)| role)
    }

    /// The role and inline policy Lenslet allowed with this call.
    pub fn granted(&self) -> Option<(String, String)> {
        self.grant.as_ref()?;
        self.iam("PutRolePolicy")
    }

    /// The role and inline policy this call deleted, if it is IAM's
    /// `DeleteRolePolicy`.
    pub fn deleted_policy(&self) -> Option<(String, String)> {
        self.iam("DeleteRolePolicy")
    }

    /// The role (and policy) an IAM Query call with `action` names, read
    /// under its wire encodings.
    fn iam(&self, action: &str) -> Option<(String, String)> {
        let form = lenslet_core::body::decode(&self.headers, &self.body).bytes;
        if lenslet_core::wire::form_action(&form).as_deref() != Some(action) {
            return None;
        }
        let form = std::str::from_utf8(&form).ok()?;
        let field = |name| lenslet_core::wire::form_field(form, name).unwrap_or_default();
        let role = field("RoleName");
        (!role.is_empty()).then(|| (role, field("PolicyName")))
    }

    /// About how much memory it takes: its body and headers.
    fn size(&self) -> usize {
        self.body.len()
            + self.uri.to_string().len()
            + self
                .headers
                .iter()
                .map(|(n, v)| n.as_str().len() + v.len() + 8)
                .sum::<usize>()
    }
}

/// The most calls the account's history keeps, whatever their size.
const MAX_LOG_CALLS: usize = 200_000;

/// The access key the built-in terminal's programs sign with: their calls,
/// and only theirs, make up a run.
pub const TERMINAL_KEY: &str = "lenslet-terminal";

/// The grants that stand at the end of `log`, in order: the latest for each
/// role's policy, unless the policy was deleted after it.
pub fn standing_grants(log: &[Sent]) -> Vec<&Sent> {
    let mut standing: Vec<((String, String), &Sent)> = Vec::new();
    for sent in log {
        if let Some(key) = sent.granted() {
            standing.retain(|(k, _)| *k != key);
            standing.push((key, sent));
        } else if let Some(key) = sent.deleted_policy() {
            standing.retain(|(k, _)| *k != key);
        }
    }
    standing.into_iter().map(|(_, sent)| sent).collect()
}

/// Whether a call's answer hands out credentials: STS's (`AssumeRole`,
/// `GetSessionToken`, …) and a new IAM user's access key. Only these name
/// keys the terminal's programs then sign with; an `AccessKeyId` anywhere
/// else (an object, an item, a function's answer) is data.
fn vends_keys(call: &Call) -> bool {
    match call.service.as_str() {
        "sts" => matches!(
            call.operation.as_str(),
            "AssumeRole"
                | "AssumeRoleWithWebIdentity"
                | "AssumeRoleWithSAML"
                | "AssumeRoot"
                | "GetSessionToken"
                | "GetFederationToken"
        ),
        "iam" => call.operation == "CreateAccessKey",
        _ => false,
    }
}

/// The access key ids in a response (STS's AssumeRole and the like, in XML
/// or JSON).
fn vended_keys(body: &str) -> Vec<String> {
    let mut keys = Vec::new();
    for (open, close) in [
        ("<AccessKeyId>", "</AccessKeyId>"),
        ("\"AccessKeyId\":\"", "\""),
        ("\"AccessKeyId\": \"", "\""),
    ] {
        let mut rest = body;
        while let Some(at) = rest.find(open) {
            rest = &rest[at + open.len()..];
            let Some(end) = rest.find(close) else { break };
            let key = rest[..end].trim();
            if !key.is_empty() && key.len() <= 128 {
                keys.push(key.to_string());
            }
            rest = &rest[end..];
        }
    }
    keys
}

/// The most calls the list keeps; older ones go first.
const MAX_CALLS: usize = 20_000;
/// The most request bytes kept to rebuild the account; past it, rewinding is
/// off until the next new account.
const MAX_LOG_BYTES: usize = 512 * 1024 * 1024;

#[derive(Default)]
struct Inner {
    /// What changed the account, in order, since it was last empty.
    log: Vec<Sent>,
    log_bytes: usize,
    /// The log no longer holds everything (it grew past its limit).
    log_broken: bool,
    /// The calls, in order of `seq`.
    calls: Vec<Arc<Call>>,
    next_seq: usize,
    /// Each call's `seq`, by its id.
    by_id: std::collections::HashMap<u64, usize>,
    runs: Vec<Run>,
    next_run: u64,
    /// The run of the command running now.
    current: Option<u64>,
    /// The access keys the terminal's programs sign with: its own, and the
    /// ones AWS handed them (an assumed role's).
    terminal_keys: std::collections::HashSet<String>,
    /// The run of each call from outside still being served, decided as it
    /// arrived: the calls AWS makes inside it end, and are recorded, first.
    arriving: std::collections::HashMap<u64, Option<u64>>,
    /// The caller of each call AWS makes, while its caller is not recorded.
    callers: std::collections::HashMap<u64, u64>,
    permissions: BTreeMap<Access, Vec<usize>>,
    stats: Arc<Stats>,
    resources: Arc<Vec<Group>>,
    generation: u64,
    version: u64,
}

impl Inner {
    fn find(&self, seq: usize) -> Option<&Arc<Call>> {
        self.calls
            .binary_search_by_key(&seq, |c| c.seq)
            .ok()
            .map(|i| &self.calls[i])
    }

    /// Whether a call signed with `key`, sent by `client` (named from its
    /// `User-Agent`), came from the terminal: signed with one of its keys.
    /// An unsigned call could be anyone's: the terminal's only when a program
    /// that says what it is sent it (`curl`, an SDK), never a browser or a
    /// client that does not say.
    fn terminal(&self, key: Option<&str>, client: Option<&str>) -> bool {
        match key {
            Some(key) => key == TERMINAL_KEY || self.terminal_keys.contains(key),
            None => client.is_some_and(|client| !client.starts_with("Mozilla")),
        }
    }

    /// When the run `id` started, if the account can go back to then.
    fn rewind_start(&self, id: u64) -> Result<u64, String> {
        if self.log_broken {
            return Err(
                "the session sent too much data to rewind; start a new account to rewind again"
                    .into(),
            );
        }
        let run = self.runs.iter().find(|r| r.id == id).ok_or("no such run")?;
        if run.undone {
            return Err("what this command did is no longer in the account".into());
        }
        if run.ended_ms.is_none() {
            return Err("this command is still running".into());
        }
        Ok(run.started_ms)
    }

    /// The run of the call `id`: recorded, still arriving, or inside one of
    /// those.
    fn run_of(&self, mut id: u64) -> Option<u64> {
        for _ in 0..64 {
            if let Some(call) = self.by_id.get(&id).and_then(|&seq| self.find(seq)) {
                return call.run;
            }
            if let Some(&run) = self.arriving.get(&id) {
                return run;
            }
            id = *self.callers.get(&id)?;
        }
        None
    }
}

#[derive(Clone, Default)]
pub struct Session {
    inner: Arc<Mutex<Inner>>,
}

/// How many calls, failed calls and calls the emulator does not serve.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Tally {
    pub calls: usize,
    pub failed: usize,
    pub unsupported: usize,
}

impl Tally {
    fn add(&mut self, call: &Call) {
        self.calls += 1;
        if call.unsupported {
            self.unsupported += 1;
        } else if call.failed() {
            self.failed += 1;
        }
    }
}

/// Counts kept as calls arrive, so the window never counts them itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub all: Tally,
    pub runs: std::collections::HashMap<u64, Tally>,
    /// The services the calls went to.
    pub services: std::collections::BTreeSet<String>,
}

impl Stats {
    fn add(&mut self, call: &Call) {
        self.all.add(call);
        if let Some(run) = call.run {
            self.runs.entry(run).or_default().add(call);
        }
        if !self.services.contains(&call.service) {
            self.services.insert(call.service.clone());
        }
    }

    fn of(calls: &[Arc<Call>]) -> Stats {
        let mut stats = Stats::default();
        for call in calls {
            stats.add(call);
        }
        stats
    }

    /// The tally of the run `run`, or of every call.
    pub fn of_run(&self, run: Option<u64>) -> Tally {
        match run {
            Some(run) => self.runs.get(&run).copied().unwrap_or_default(),
            None => self.all,
        }
    }
}

/// The session at one version.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub version: u64,
    pub calls: Vec<Arc<Call>>,
    pub runs: Vec<Run>,
    pub resources: Arc<Vec<Group>>,
    pub stats: Arc<Stats>,
    pub generation: u64,
}

impl Snapshot {
    /// Whether the account can go back to before the run `id` (as far as
    /// the window can tell: the engine has the last word).
    pub fn can_rewind(&self, id: u64) -> bool {
        self.runs
            .iter()
            .any(|r| r.id == id && !r.undone && r.ended_ms.is_some())
    }
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

    /// A call from outside arrives, signed with `key` and sent by `client`
    /// (named from its `User-Agent`): its run is the command running now
    /// when the terminal sent it.
    pub fn arrive(&self, id: u64, key: Option<&str>, client: Option<&str>) {
        let mut inner = self.lock();
        let run = if inner.terminal(key, client) {
            inner.current
        } else {
            None
        };
        if inner.arriving.len() > 100_000 {
            inner.arriving.clear();
        }
        inner.arriving.insert(id, run);
    }

    /// AWS began the call `id` inside the call `caller`.
    pub fn link(&self, id: u64, caller: u64) {
        let mut inner = self.lock();
        if inner.callers.len() > 100_000 {
            inner.callers.clear();
        }
        inner.callers.insert(id, caller);
    }

    pub fn record(&self, mut call: Call) {
        let mut inner = self.lock();
        call.seq = inner.next_seq;
        inner.next_seq += 1;
        call.generation = inner.generation;
        // A call belongs to the run of the call it happened inside, else to
        // the command running now when the terminal sent it.
        let terminal = inner.terminal(call.key.as_deref(), call.client.as_deref());
        if call.run.is_none() {
            call.run = match (call.internal, call.parent) {
                // Inside another call: that call's run, recorded or not yet.
                (true, Some(parent)) => inner.run_of(parent),
                (true, None) => None,
                // From outside: as it was when it arrived.
                (false, _) => match inner.arriving.get(&call.id) {
                    Some(&run) => run,
                    None if terminal => inner.current,
                    None => None,
                },
            };
        }
        inner.arriving.remove(&call.id);
        inner.callers.remove(&call.id);
        // Keys AWS hands the terminal's programs are the terminal's too.
        if call.key.is_some() && terminal && call.status < 400 && vends_keys(&call) {
            for key in vended_keys(&call.response.body) {
                if inner.terminal_keys.len() < 10_000 {
                    inner.terminal_keys.insert(key);
                }
            }
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
        Arc::make_mut(&mut inner.stats).add(&call);
        inner.calls.push(Arc::new(call));
        if inner.calls.len() > MAX_CALLS {
            let gone: Vec<u64> = inner.calls.drain(..MAX_CALLS / 10).map(|c| c.id).collect();
            for id in gone {
                inner.by_id.remove(&id);
            }
            inner.stats = Arc::new(Stats::of(&inner.calls));
            // The permissions keep only calls that are still listed.
            let oldest = inner.calls.first().map_or(0, |c| c.seq);
            for seqs in inner.permissions.values_mut() {
                seqs.retain(|&seq| seq >= oldest);
            }
        }
        inner.version += 1;
    }

    /// A command starts in the terminal.
    pub fn start_run(&self, command: String) -> u64 {
        let mut inner = self.lock();
        // A run the shell never said the end of is over now.
        if let Some(open) = inner.current.take()
            && let Some(run) = inner.runs.iter_mut().find(|r| r.id == open)
        {
            run.ended_ms = Some(now_ms());
            run.exit = Some(-1);
        }
        inner.next_run += 1;
        let id = inner.next_run;
        let generation = inner.generation;
        inner.runs.push(Run {
            id,
            command,
            started_ms: now_ms(),
            ended_ms: None,
            exit: None,
            generation,
            undone: false,
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
        // The keys the old account handed out name nothing in the new one.
        inner.terminal_keys.clear();
        inner.log.clear();
        inner.log_bytes = 0;
        inner.log_broken = false;
        for run in &mut inner.runs {
            run.undone = true;
        }
        inner.generation += 1;
        inner.permissions.clear();
        inner.resources = Arc::default();
        inner.version += 1;
    }

    /// Keep a call that changed the account, to send it again on a rewind;
    /// `generation` is the account it was sent to (a call that raced a new
    /// account belongs to the old one).
    pub fn keep(&self, sent: Sent, generation: u64) {
        let mut inner = self.lock();
        if generation != inner.generation || inner.log_broken {
            return;
        }
        // A grant holds all its policy allows: the one it adds to goes.
        if let Some(key) = sent.granted() {
            inner.log.retain(|s| s.granted().as_ref() != Some(&key));
            inner.log_bytes = inner.log.iter().map(Sent::size).sum();
        }
        inner.log_bytes += sent.size();
        if inner.log_bytes > MAX_LOG_BYTES || inner.log.len() >= MAX_LOG_CALLS {
            inner.log.clear();
            inner.log_bytes = 0;
            inner.log_broken = true;
            for run in &mut inner.runs {
                run.undone = true;
            }
            inner.version += 1;
            return;
        }
        inner.log.push(sent);
    }

    /// What changed the account since it was empty, to save it.
    pub fn log(&self) -> Result<Vec<Sent>, String> {
        let inner = self.lock();
        if inner.log_broken {
            return Err("the session sent too much data to keep the account; start a new account to save one".into());
        }
        Ok(inner.log.clone())
    }

    /// The account, newly empty, is being built from `log`: it is what
    /// made it.
    pub fn adopt(&self, log: Vec<Sent>) {
        let mut inner = self.lock();
        inner.log_bytes = log.iter().map(Sent::size).sum();
        inner.log = log;
        inner.version += 1;
    }

    /// Whether the account can go back to before the run `id`.
    pub fn can_rewind(&self, id: u64) -> bool {
        let inner = self.lock();
        !inner.log_broken
            && inner
                .runs
                .iter()
                .any(|r| r.id == id && !r.undone && r.ended_ms.is_some())
    }

    /// What made the account as it was before the run `id` started, to
    /// build it again: the calls before then, and every role Lenslet allowed
    /// something since (a fix stands through the rewind).
    pub fn rewind_log(&self, id: u64) -> Result<Vec<Sent>, String> {
        let inner = self.lock();
        let at_ms = inner.rewind_start(id)?;
        Ok(inner
            .log
            .iter()
            .filter(|s| s.at_ms < at_ms || s.grant.is_some())
            .cloned()
            .collect())
    }

    /// The account went back to before the run `id`, built from `log` (what
    /// [`Self::rewind_log`] gave): the run and the ones after it are undone.
    pub fn rewind_to(&self, id: u64, log: Vec<Sent>) -> Result<(), String> {
        let mut inner = self.lock();
        let at_ms = inner.rewind_start(id)?;
        inner.log_bytes = log.iter().map(Sent::size).sum();
        inner.log = log;
        for run in &mut inner.runs {
            if run.started_ms >= at_ms {
                run.undone = true;
            }
        }
        inner.generation += 1;
        inner.resources = Arc::default();
        inner.version += 1;
        Ok(())
    }

    /// What Lenslet allowed the role `role` that stands (not since deleted),
    /// in order: sent again when the role is created again.
    pub fn grants_for(&self, role: &str) -> Vec<Sent> {
        standing_grants(&self.lock().log)
            .into_iter()
            .filter(|s| s.grant.as_deref() == Some(role))
            .cloned()
            .collect()
    }

    pub fn run(&self, id: u64) -> Option<Run> {
        self.lock().runs.iter().find(|r| r.id == id).cloned()
    }

    /// Clear the list of calls, and the permissions they needed; the
    /// resources stay.
    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.calls.clear();
        inner.by_id.clear();
        inner.permissions.clear();
        inner.stats = Arc::default();
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
            calls: inner.calls.clone(),
            runs: inner.runs.clone(),
            resources: inner.resources.clone(),
            stats: inner.stats.clone(),
            generation: inner.generation,
        })
    }

    /// Every permission the session's calls needed, with those calls.
    pub fn permissions(&self) -> Vec<Permission> {
        self.lock()
            .permissions
            .iter()
            .map(|(access, calls)| Permission {
                access: access.clone(),
                calls: calls.clone(),
            })
            .collect()
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot_since(None).unwrap_or_default()
    }

    /// One call by its sequence number.
    pub fn call(&self, seq: usize) -> Option<Arc<Call>> {
        self.lock().find(seq).cloned()
    }

    pub fn call_count(&self) -> usize {
        self.lock().calls.len()
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
        let permissions = session.permissions();
        assert_eq!(permissions.len(), 1);
        assert_eq!(permissions[0].calls, vec![0]);
        session.clear();
        let s = session.snapshot();
        assert!(s.calls.is_empty() && session.permissions().is_empty());
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
        // A bucket is the first part of an S3 name, never a key.
        let mut c = call("s3", None);
        c.target = Some("s3://shop/data/1.json".into());
        assert!(c.touches("s3", "shop"));
        assert!(!c.touches("s3", "data"));
        assert!(!c.touches("s3", "1.json"));
        c.access = vec![Access::new("s3:GetObject", "arn:aws:s3:::other/shop")];
        assert!(c.touches("s3", "other"));
    }

    #[test]
    fn a_rewind_undoes_the_run_and_the_ones_after_it() {
        let session = Session::default();
        let sent = |at_ms| Sent {
            at_ms,
            method: http::Method::POST,
            uri: http::Uri::from_static("/"),
            headers: http::HeaderMap::new(),
            body: bytes::Bytes::from_static(b"x"),
            issued: Vec::new(),
            grant: None,
        };
        let rewind = |id| {
            let log = session.rewind_log(id)?;
            session.rewind_to(id, log.clone()).map(|()| log)
        };
        let one = session.start_run("one".into());
        session.keep(sent(now_ms()), session.generation());
        session.end_run(0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let two = session.start_run("two".into());
        // Still running: not yet.
        assert!(rewind(two).is_err());
        session.keep(sent(now_ms()), session.generation());
        session.end_run(1);
        // Lenslet allows a role what run two was denied: it stands.
        let mut grant = sent(now_ms());
        grant.grant = Some("fn-role".into());
        grant.body = bytes::Bytes::from_static(
            b"Action=PutRolePolicy&RoleName=fn-role&PolicyName=p&PolicyDocument=d",
        );
        session.keep(grant, session.generation());
        assert!(session.can_rewind(two));
        let log = rewind(two).expect("rewind");
        assert_eq!(log.len(), 2, "what came before run two, and the grant");
        assert_eq!(log[1].grant.as_deref(), Some("fn-role"));
        assert_eq!(session.grants_for("fn-role").len(), 1);
        assert!(!session.can_rewind(two), "run two is undone");
        assert!(session.can_rewind(one));
        // A call sent to the account before the rewind is not kept for the new one.
        session.keep(sent(now_ms()), session.generation() - 1);
        let log = rewind(one).expect("rewind");
        assert_eq!(log.len(), 1, "only the grant");
        // A new account has none.
        session.fresh();
        assert!(session.grants_for("fn-role").is_empty());
    }

    fn iam(form: &str, grant: bool, at_ms: u64) -> Sent {
        Sent {
            at_ms,
            method: http::Method::POST,
            uri: http::Uri::from_static("/"),
            headers: http::HeaderMap::new(),
            body: bytes::Bytes::from(form.to_string()),
            issued: Vec::new(),
            grant: grant.then(|| "fn-role".into()),
        }
    }

    #[test]
    fn grants_keep_only_their_latest_and_not_what_was_deleted() {
        let session = Session::default();
        let g = session.generation();
        let put = |table: &str| {
            format!(
                "Action=PutRolePolicy&RoleName=fn-role&PolicyName=lenslet-dynamodb-PutItem&PolicyDocument={table}"
            )
        };
        session.keep(iam("Action=CreateRole&RoleName=fn-role", false, 1), g);
        session.keep(iam(&put("orders"), true, 2), g);
        session.keep(iam(&put("orders+carts"), true, 3), g);
        let other = "Action=PutRolePolicy&RoleName=fn-role&PolicyName=lenslet-sqs-SendMessage&PolicyDocument=q";
        session.keep(iam(other, true, 4), g);
        // The second grant of a policy holds the first: one each.
        let log = session.log().unwrap();
        assert_eq!(log.iter().filter(|s| s.grant.is_some()).count(), 2);
        assert!(log.iter().any(|s| s.body.ends_with(b"orders+carts")));
        assert!(!log.iter().any(|s| s.body.ends_with(b"=orders")));
        assert_eq!(session.grants_for("fn-role").len(), 2);
        // Deleted by the code since: no longer sent with the role.
        session.keep(
            iam(
                "Action=DeleteRolePolicy&RoleName=fn-role&PolicyName=lenslet-sqs-SendMessage",
                false,
                5,
            ),
            g,
        );
        let standing = session.grants_for("fn-role");
        assert_eq!(standing.len(), 1);
        assert!(standing[0].body.ends_with(b"orders+carts"));
        // Read under its wire encodings: a gzip'd CreateRole is one too.
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, b"Action=CreateRole&RoleName=fn-role").unwrap();
        let mut created = iam("", false, 6);
        created.body = bytes::Bytes::from(gz.finish().unwrap());
        created
            .headers
            .insert("content-encoding", http::HeaderValue::from_static("gzip"));
        assert_eq!(created.created_role().as_deref(), Some("fn-role"));
    }

    #[test]
    fn a_run_holds_the_terminals_calls_only() {
        let session = Session::default();
        let run = session.start_run("pytest".into());
        let signed = |key: &str| {
            let mut c = call("sqs", None);
            c.key = Some(key.into());
            c
        };
        session.record(signed(TERMINAL_KEY));
        // Another program, at the same time: not the run's.
        session.record(signed("lenslet"));
        // The terminal assumes a role: its keys are the terminal's too.
        let mut assumed = signed(TERMINAL_KEY);
        assumed.service = "sts".into();
        assumed.operation = "AssumeRole".into();
        assumed.response.body =
            "<Credentials><AccessKeyId>ASIAROLE</AccessKeyId></Credentials>".into();
        session.record(assumed);
        session.record(signed("ASIAROLE"));
        // A key named in an object's or a table's data is only data.
        let mut object = signed(TERMINAL_KEY);
        object.service = "s3".into();
        object.operation = "GetObject".into();
        object.response.body = r#"{"AccessKeyId":"AKIADATA"}"#.into();
        session.record(object);
        session.record(signed("AKIADATA"));
        // No key at all: the command's while it runs when a program sent it
        // (a curl), never a browser or a client that does not say.
        let mut curl = call("s3", None);
        curl.client = Some("curl 8.5.0".into());
        session.record(curl);
        let mut browser = call("s3", None);
        browser.client = Some("Mozilla 5.0".into());
        session.record(browser);
        session.record(call("s3", None));
        let runs: Vec<_> = session.snapshot().calls.iter().map(|c| c.run).collect();
        assert_eq!(
            runs,
            vec![
                Some(run),
                None,
                Some(run),
                Some(run),
                Some(run),
                None,
                Some(run),
                None,
                None
            ]
        );
        // A new account forgets the keys the old one handed out.
        session.fresh();
        session.record(signed("ASIAROLE"));
        assert_eq!(session.snapshot().calls.last().unwrap().run, None);
    }

    #[test]
    fn aws_own_calls_join_the_run_of_their_cause_before_it_is_recorded() {
        let session = Session::default();
        let run = session.start_run("aws lambda invoke".into());
        // The Invoke arrives; inside it the function's call, inside that a
        // delivery: both end, and are recorded, before the Invoke.
        session.arrive(10, Some(TERMINAL_KEY), Some("AWS CLI 2.17.0"));
        session.link(11, 10);
        session.link(12, 11);
        let inner = |id, parent| {
            let mut c = call("dynamodb", None);
            c.id = id;
            c.parent = Some(parent);
            c.internal = true;
            c
        };
        session.record(inner(12, 11));
        session.record(inner(11, 10));
        let mut outer = call("lambda", None);
        outer.id = 10;
        outer.key = Some(TERMINAL_KEY.into());
        session.record(outer);
        let runs: Vec<_> = session.snapshot().calls.iter().map(|c| c.run).collect();
        assert_eq!(runs, vec![Some(run); 3]);
    }

    #[test]
    fn a_run_left_open_ends_when_the_next_starts() {
        let session = Session::default();
        let one = session.start_run("exit".into());
        session.start_run("ls".into());
        let run = session.run(one).unwrap();
        assert_eq!(run.exit, Some(-1));
        assert!(run.ended_ms.is_some());
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
            key: None,
            request_id: None,
            request: Exchange::default(),
            response: Exchange::default(),
        }
    }
}
