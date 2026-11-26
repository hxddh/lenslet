//! Lenslet's engine: the embedded emulator, the endpoint in front of it, the
//! session it records into, and the resource inventory. It runs on threads
//! of its own; the window holds an [`Engine`] and reads snapshots.

use crate::{
    emulator::Emulator,
    resources::{self, Contents, Refresher},
    server,
    session::{Resource, Sent, Session, Snapshot},
};

/// The port Lenslet listens on unless `LENSLET_PORT` says otherwise; when it
/// is taken, Lenslet takes a free one.
pub const DEFAULT_PORT: u16 = 4510;
pub const ACCOUNT: &str = "000000000000";
pub const REGION: &str = "us-east-1";

/// Request handling runs the emulator's generated deserializers, which need
/// a deeper stack than tokio's default (AWS Emulator's binary uses the same).
const WORKER_STACK: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct Engine {
    pub port: u16,
    emulator: Emulator,
    session: Session,
    refresher: Refresher,
    runtime: tokio::runtime::Handle,
    /// A rewind is under way.
    rewinding: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// What a rewind sent again: the calls that made the account (Lenslet's own
/// grants, sent with their roles, are not among them), and how many failed.
#[derive(Clone, Copy, Debug, Default)]
pub struct Replayed {
    pub sent: usize,
    pub failed: usize,
}

impl Engine {
    /// Start the emulator and the endpoint on `port`; on the default port,
    /// a free one when it is taken.
    pub fn start(port: u16) -> Result<Engine, String> {
        let chosen = std::env::var_os("LENSLET_PORT").is_some();
        // `127.0.0.1`, and `[::1]` where the machine has IPv6: `localhost`
        // is either.
        let (listener, listener_v6) = server::bind_both(port, chosen)?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let endpoint = format!("http://127.0.0.1:{port}");
        let emulator = Emulator::new(&endpoint);
        let session = Session::default();
        let refresher = Refresher::default();
        crate::observe::install(session.clone(), refresher.clone());
        let (handle_tx, handle_rx) = std::sync::mpsc::channel();
        let background = (emulator.clone(), session.clone(), refresher.clone());
        std::thread::Builder::new()
            .name("lenslet-engine".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(4)
                    .thread_stack_size(WORKER_STACK)
                    .enable_all()
                    .build()
                    .expect("start the engine runtime");
                let _ = handle_tx.send(runtime.handle().clone());
                let (emulator, session, refresher) = background;
                runtime.block_on(async move {
                    let listener = tokio::net::TcpListener::from_std(listener).expect("listen");
                    tokio::spawn(refresher.clone().run(emulator.clone(), session.clone()));
                    if let Some(v6) =
                        listener_v6.and_then(|l| tokio::net::TcpListener::from_std(l).ok())
                    {
                        tokio::spawn(server::serve(
                            v6,
                            emulator.clone(),
                            session.clone(),
                            refresher.clone(),
                        ));
                    }
                    server::serve(listener, emulator, session, refresher).await;
                });
            })
            .map_err(|e| e.to_string())?;
        let runtime = handle_rx.recv().map_err(|e| e.to_string())?;
        Ok(Engine {
            port,
            emulator,
            session,
            refresher,
            runtime,
            rewinding: Default::default(),
        })
    }

    /// The port from `LENSLET_PORT`, else [`DEFAULT_PORT`].
    pub fn port_from_env() -> u16 {
        std::env::var("LENSLET_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(DEFAULT_PORT)
    }

    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn snapshot(&self) -> Snapshot {
        self.session.snapshot()
    }

    /// The session, when it changed since `version`.
    pub fn snapshot_since(&self, version: Option<u64>) -> Option<Snapshot> {
        self.session.snapshot_since(version)
    }

    pub fn call_count(&self) -> usize {
        self.session.call_count()
    }

    /// A new, empty AWS account. Building the engine takes a moment, on the
    /// engine's threads; calls until it is ready go to the old account, and
    /// the session starts over once the new one serves. The future says when.
    pub fn fresh(&self) -> impl Future<Output = Result<(), String>> + use<> {
        let (emulator, session, refresher, busy) = (
            self.emulator.clone(),
            self.session.clone(),
            self.refresher.clone(),
            self.rewinding.clone(),
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.runtime.spawn(async move {
            use std::sync::atomic::Ordering;
            // A rewind or an opening account would go on filling the new one.
            if busy.swap(true, Ordering::SeqCst) {
                let _ = tx.send(Err(BUSY.to_string()));
                return;
            }
            let result = match tokio::task::spawn_blocking(Emulator::build).await {
                // The engine and the session change over together.
                Ok(router) => emulator.swap_with(router, || {
                    session.fresh();
                    Ok(())
                }),
                Err(_) => Err("could not start a new engine".to_string()),
            };
            busy.store(false, Ordering::SeqCst);
            refresher.poke();
            let _ = tx.send(result);
        });
        async move {
            rx.await
                .unwrap_or_else(|_| Err("the engine stopped".into()))
        }
    }

    /// What the terminal's shell says about its commands: each one a run.
    pub fn on_mark(&self) -> crate::shell::OnMark {
        let session = self.session.clone();
        std::sync::Arc::new(move |mark| match mark {
            crate::shell::Mark::Start(command) => {
                session.start_run(command);
            }
            crate::shell::Mark::End(exit) => session.end_run(exit),
        })
    }

    /// Clear the list of calls; the resources stay.
    pub fn clear(&self) {
        self.session.clear();
    }

    /// Look at what exists again now.
    pub fn refresh(&self) {
        self.refresher.poke();
    }

    /// The IAM policy for every permission this session used.
    pub fn policy(&self) -> String {
        let accesses: Vec<_> = self.permissions().into_iter().map(|p| p.access).collect();
        lenslet_core::policy::compile(&accesses)
    }

    /// Every permission the session's calls needed, with those calls.
    pub fn permissions(&self) -> Vec<crate::session::Permission> {
        self.session.permissions()
    }

    /// The variables that point SDKs and the AWS CLI at Lenslet, from any
    /// shell.
    pub fn env_vars(&self) -> Vec<(&'static str, String)> {
        self.vars("lenslet")
    }

    /// The variables of the built-in terminal: its own access key, so its
    /// commands' calls are told from everyone else's.
    pub fn terminal_vars(&self) -> Vec<(&'static str, String)> {
        self.vars(crate::session::TERMINAL_KEY)
    }

    fn vars(&self, key: &str) -> Vec<(&'static str, String)> {
        vec![
            ("AWS_ENDPOINT_URL", self.endpoint()),
            ("AWS_REGION", REGION.into()),
            ("AWS_DEFAULT_REGION", REGION.into()),
            ("AWS_ACCESS_KEY_ID", key.into()),
            ("AWS_SECRET_ACCESS_KEY", "lenslet".into()),
            // A profile's `ignore_configured_endpoint_urls` would send the
            // calls to AWS itself.
            ("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", "false".into()),
        ]
    }

    /// What to paste into a shell so SDKs and the AWS CLI talk to Lenslet.
    pub fn env(&self) -> String {
        self.env_vars()
            .into_iter()
            .map(|(name, value)| format!("export {name}={value}\n"))
            .collect()
    }

    /// What `resource` holds, fetched on the engine's threads.
    pub fn contents(
        &self,
        service: &'static str,
        resource: Resource,
    ) -> impl Future<Output = Contents> + use<> {
        let emulator = self.emulator.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.runtime.spawn(async move {
            let _ = tx.send(resources::contents(&emulator, service, &resource).await);
        });
        async move { rx.await.unwrap_or_default() }
    }

    /// Put the account back as it was before the run `run` started: a new,
    /// empty engine, and every call from outside that changed the account
    /// before then sent to it again, in order, and what Lenslet allowed roles
    /// since. What the account did in response (deliveries, functions)
    /// happens again too, unrecorded. The engine is built on its own and
    /// serves once it is done: until then, calls go to the account as it was.
    /// Answers how many calls were sent again, and how many of them failed.
    pub fn rewind(&self, run: u64) -> impl Future<Output = Result<Replayed, String>> + use<> {
        let (emulator, session, refresher, busy) = (
            self.emulator.clone(),
            self.session.clone(),
            self.refresher.clone(),
            self.rewinding.clone(),
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.runtime.spawn(async move {
            use std::sync::atomic::Ordering;
            if busy.swap(true, Ordering::SeqCst) {
                let _ = tx.send(Err(BUSY.to_string()));
                return;
            }
            let result = async {
                let log = session.rewind_log(run)?;
                let (built, replayed) = rebuild(&emulator, &log, 3000).await?;
                emulator.adopt(&built, || session.rewind_to(run, log))?;
                Ok(replayed)
            }
            .await;
            busy.store(false, Ordering::SeqCst);
            refresher.poke();
            let _ = tx.send(result);
        });
        async move {
            rx.await
                .unwrap_or_else(|_| Err("the engine stopped".into()))
        }
    }

    /// The account as a file's text, to save it: made on the engine's
    /// threads (it can be large).
    pub fn save_account(&self) -> impl Future<Output = Result<String, String>> + use<> {
        self.save_account_to(None)
    }

    /// Save the account to `path` (or only make its text), on the engine's
    /// threads.
    pub fn save_account_to(
        &self,
        path: Option<std::path::PathBuf>,
    ) -> impl Future<Output = Result<String, String>> + use<> {
        let session = self.session.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.runtime.spawn_blocking(move || {
            let saved = session.log().map(|log| crate::account::to_json(&log));
            let _ = tx.send(match (saved, path) {
                (Ok(text), Some(path)) => std::fs::write(&path, &text)
                    .map(|()| String::new())
                    .map_err(|e| e.to_string()),
                (saved, _) => saved,
            });
        });
        async move {
            rx.await
                .unwrap_or_else(|_| Err("the engine stopped".into()))
        }
    }

    /// Replace the account with the one saved in `text`: a new, empty
    /// engine, and the calls that made the saved one sent to it again; it
    /// serves once it is built.
    pub fn open_account(
        &self,
        text: String,
    ) -> impl Future<Output = Result<Replayed, String>> + use<> {
        let (emulator, session, refresher, busy) = (
            self.emulator.clone(),
            self.session.clone(),
            self.refresher.clone(),
            self.rewinding.clone(),
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.runtime.spawn(async move {
            use std::sync::atomic::Ordering;
            let log = match crate::account::from_json(&text) {
                Ok(log) => log,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            };
            if busy.swap(true, Ordering::SeqCst) {
                let _ = tx.send(Err(BUSY.to_string()));
                return;
            }
            let result = async {
                let (built, replayed) = rebuild(&emulator, &log, 200).await?;
                // The engine, the session and its history change over
                // together: what arrives after belongs to the opened one.
                emulator.adopt(&built, || {
                    session.fresh();
                    session.adopt(log);
                    Ok(())
                })?;
                Ok(replayed)
            }
            .await;
            busy.store(false, Ordering::SeqCst);
            refresher.poke();
            let _ = tx.send(result);
        });
        async move {
            rx.await
                .unwrap_or_else(|_| Err("the engine stopped".into()))
        }
    }

    /// Allow the role `role` what `accesses` need, as an inline policy (one
    /// per action, which each grant adds to). The change is part of the
    /// account: it is saved with it, stands through a rewind, and comes back
    /// when the role is created again.
    pub fn grant(
        &self,
        role: String,
        accesses: Vec<lenslet_core::access::Access>,
    ) -> impl Future<Output = Result<(), String>> + use<> {
        let (emulator, session, busy) = (
            self.emulator.clone(),
            self.session.clone(),
            self.rewinding.clone(),
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.runtime.spawn(async move {
            // The account being rebuilt would not have it.
            if busy.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = tx.send(Err(BUSY.to_string()));
                return;
            }
            let generation = session.generation();
            let result = resources::grant(&emulator, &role, &accesses).await;
            let _ = tx.send(result.map(|sent| session.keep(sent, generation)));
        });
        async move {
            rx.await
                .unwrap_or_else(|_| Err("the engine stopped".into()))
        }
    }

    /// An S3 object's text, fetched on the engine's threads.
    pub fn object_text(
        &self,
        bucket: String,
        key: String,
    ) -> impl Future<Output = Option<String>> + use<> {
        let emulator = self.emulator.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.runtime.spawn(async move {
            let _ = tx.send(resources::object_text(&emulator, &bucket, &key).await);
        });
        async move { rx.await.ok().flatten() }
    }
}

/// Why a call sent again failed, and whether to send it once more.
#[derive(PartialEq)]
enum Retry {
    /// What it acts on is busy (a function being created): it will be ready.
    Busy,
    /// What it names is not there yet: it may be on its way (a table being
    /// created), unless it was a handle the first account made up.
    Missing,
    /// Any other error is final.
    Never,
}

fn retry(status: u16, code: &str) -> Retry {
    let has = |words: &[&str]| words.iter().any(|w| code.contains(w));
    if matches!(status, 409 | 429 | 503)
        || has(&[
            "InUse",
            "Conflict",
            "Pending",
            "NotReady",
            "Throttl",
            "TooManyRequests",
        ])
    {
        return Retry::Busy;
    }
    // A handle the first account made up (a message's receipt, an upload's
    // id) names nothing in the new one, however long it waits.
    if has(&["ReceiptHandle", "NoSuchUpload", "Invalid"]) {
        return Retry::Never;
    }
    if has(&["NotFound", "NoSuch", "NonExistent", "DoesNotExist"]) {
        return Retry::Missing;
    }
    Retry::Never
}

/// How long a call sent again waits for what it acts on to be ready.
const READY_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
/// How long it waits for what it names to appear, once nothing else is
/// going on (the emulator's resources settle in a few seconds).
const APPEAR_WAIT: std::time::Duration = std::time::Duration::from_millis(3500);

/// Build an account from `log` on a new engine of its own, not yet serving:
/// the engine, and what sending the calls again did.
async fn rebuild(
    emulator: &Emulator,
    log: &[Sent],
    max_gap_ms: u64,
) -> Result<(Emulator, Replayed), String> {
    let router = tokio::task::spawn_blocking(Emulator::build)
        .await
        .map_err(|_| "could not start a new engine".to_string())?;
    let built = emulator.offline(router);
    let rebuild = crate::observe::Rebuild::start(built.identity());
    let replayed = send_again(&built, &rebuild, log, max_gap_ms).await;
    Ok((built, replayed))
}

/// Why the account cannot be rebuilt now.
const BUSY: &str = "Lenslet is rebuilding the account; try again when it is done.";

/// The ids the first account made up, and the new account's for them.
#[derive(Default)]
struct Issued(Vec<(String, String)>);

impl Issued {
    /// `text` naming the new ids, if it named any of the old ones.
    fn swap(&self, text: &str) -> Option<String> {
        self.0
            .iter()
            .any(|(old, _)| text.contains(old.as_str()))
            .then(|| {
                self.0.iter().fold(text.to_string(), |t, (old, new)| {
                    t.replace(old.as_str(), new)
                })
            })
    }

    /// `sent` naming the new account's ids instead of the first one's: in its
    /// address, and in its body as its protocol writes it.
    fn apply<'a>(&self, sent: &'a Sent) -> std::borrow::Cow<'a, Sent> {
        if self.0.is_empty() {
            return std::borrow::Cow::Borrowed(sent);
        }
        let uri = self.uri(&sent.uri);
        let body = self.body(sent);
        if uri.is_none() && body.is_none() {
            return std::borrow::Cow::Borrowed(sent);
        }
        let mut sent = sent.clone();
        if let Some(uri) = uri {
            sent.uri = uri;
        }
        if let Some(body) = body {
            plain(&mut sent.headers, body.len());
            sent.body = bytes::Bytes::from(body);
        }
        std::borrow::Cow::Owned(sent)
    }

    /// The address with the new ids, each part of it read percent-decoded
    /// (a receipt handle's `+`, `/` and `=` travel as `%2B`, `%2F`, `%3D`).
    fn uri(&self, uri: &http::Uri) -> Option<http::Uri> {
        let mut changed = false;
        let mut swap = |part: &str| match self.swap(&lenslet_core::wire::percent_decode_path(part))
        {
            Some(new) => {
                changed = true;
                encode(&new)
            }
            None => part.to_string(),
        };
        let path: Vec<String> = uri.path().split('/').map(&mut swap).collect();
        let mut local = path.join("/");
        if let Some(query) = uri.query() {
            let pairs: Vec<String> = query
                .split('&')
                .map(|pair| match pair.split_once('=') {
                    Some((key, value)) => format!("{key}={}", swap(value)),
                    None => pair.to_string(),
                })
                .collect();
            local = format!("{local}?{}", pairs.join("&"));
        }
        changed.then(|| local.parse().ok()).flatten()
    }

    /// The body with the new ids, unframed and inflated: CBOR rewritten item
    /// for item, a Query form field by field, JSON and XML as text. An S3
    /// body is an object's data (S3's ids are in the address): never touched.
    fn body(&self, sent: &Sent) -> Option<Vec<u8>> {
        if sent.body.is_empty() {
            return None;
        }
        let path = sent.uri.path_and_query().map_or("/", |p| p.as_str());
        if lenslet_core::wire::wire_info(&sent.headers, path).service == "s3" {
            return None;
        }
        let payload = lenslet_core::body::decode(&sent.headers, &sent.body).bytes;
        let kind = sent
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if kind.contains("cbor") {
            let out = lenslet_core::cbor::map_text(&payload, &|text| self.swap(text))?;
            return (out != *payload).then_some(out);
        }
        let text = std::str::from_utf8(&payload).ok()?;
        if kind.starts_with("application/x-www-form-urlencoded")
            || lenslet_core::wire::form_action(&payload).is_some()
        {
            let mut changed = false;
            let form: Vec<String> = text
                .split('&')
                .map(|pair| {
                    let swapped = pair.split_once('=').and_then(|(key, value)| {
                        let new = self.swap(&lenslet_core::wire::percent_decode_query(value))?;
                        Some(format!("{key}={}", encode(&new)))
                    });
                    changed |= swapped.is_some();
                    swapped.unwrap_or_else(|| pair.to_string())
                })
                .collect();
            return changed.then(|| form.join("&").into_bytes());
        }
        self.swap(text).map(String::into_bytes)
    }
}

/// Percent-encoding for a URI part or a form value: all but the characters
/// that never need it.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The headers of a request whose body is now its plain payload of `len`
/// bytes: no framing or compression said of it, no checksum of what it was.
fn plain(headers: &mut http::HeaderMap, len: usize) {
    let kept: Vec<String> = headers
        .get_all(http::header::CONTENT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|e| e.trim().to_string())
        .filter(|e| {
            !["aws-chunked", "gzip", "x-gzip", ""].contains(&e.to_ascii_lowercase().as_str())
        })
        .collect();
    headers.remove(http::header::CONTENT_ENCODING);
    if !kept.is_empty()
        && let Ok(value) = http::HeaderValue::from_str(&kept.join(", "))
    {
        headers.insert(http::header::CONTENT_ENCODING, value);
    }
    if headers
        .get("x-amz-content-sha256")
        .is_some_and(|v| v.as_bytes().starts_with(b"STREAMING-"))
    {
        headers.insert(
            "x-amz-content-sha256",
            http::HeaderValue::from_static("UNSIGNED-PAYLOAD"),
        );
    }
    let stale: Vec<http::HeaderName> = headers
        .keys()
        .filter(|name| {
            let name = name.as_str();
            name.starts_with("x-amz-checksum-")
                || matches!(
                    name,
                    "x-amz-decoded-content-length"
                        | "x-amz-trailer"
                        | "x-amz-sdk-checksum-algorithm"
                        | "content-md5"
                )
        })
        .cloned()
        .collect();
    for name in stale {
        headers.remove(name);
    }
    headers.insert(http::header::CONTENT_LENGTH, http::HeaderValue::from(len));
}

/// The ids a call's answer made up, for the calls that make them up: an S3
/// multipart upload's, a KMS key's, a Lambda event source mapping's, and the
/// receipt handles of the messages an SQS receive took.
pub fn issued_ids(operation: &str, body: &[u8]) -> Vec<String> {
    let text = match std::str::from_utf8(body) {
        Ok(text) => std::borrow::Cow::Borrowed(text),
        Err(_) => match lenslet_core::cbor::decode_cbor_json(body) {
            Some(json) => std::borrow::Cow::Owned(json),
            None => return Vec::new(),
        },
    };
    let tags = |tag: &str| -> Vec<String> {
        let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
        text.split(open.as_str())
            .skip(1)
            .filter_map(|rest| {
                rest.split_once(close.as_str())
                    .map(|(id, _)| id.to_string())
            })
            .collect()
    };
    let json = || serde_json::from_str::<serde_json::Value>(&text).unwrap_or_default();
    let ids = match operation {
        "CreateMultipartUpload" => tags("UploadId"),
        "CreateKey" => json()["KeyMetadata"]["KeyId"]
            .as_str()
            .map(str::to_string)
            .into_iter()
            .collect(),
        "CreateEventSourceMapping" => json()["UUID"]
            .as_str()
            .map(str::to_string)
            .into_iter()
            .collect(),
        "ReceiveMessage" if text.starts_with('<') => tags("ReceiptHandle"),
        "ReceiveMessage" => json()["Messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["ReceiptHandle"].as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    // Short ids would match more than themselves.
    ids.into_iter().filter(|id| id.len() >= 8).collect()
}

/// Send `log` to the engine being built, in order, keeping the gaps between
/// the calls up to `max_gap_ms`, then let what they set off finish.
async fn send_again(
    emulator: &Emulator,
    rebuild: &crate::observe::Rebuild,
    log: &[Sent],
    max_gap_ms: u64,
) -> Replayed {
    let mut replayed = Replayed::default();
    let mut issued = Issued::default();
    // What Lenslet allowed roles, and the roles the new account has: a grant
    // is sent once its role exists, and once more each time it is created.
    let mut standing: Vec<((String, String), &Sent)> = Vec::new();
    let mut roles = std::collections::HashSet::new();
    let mut previous = None;
    for sent in log {
        if let Some(key) = sent.granted() {
            standing.retain(|(k, _)| *k != key);
            if roles.contains(&key.0) {
                let _ = send_one(emulator, rebuild, sent).await;
            }
            standing.push((key, sent));
            continue;
        }
        // Keep the session's pace, up to a few seconds a gap: what the code
        // waited for (a table, a function becoming ready) is ready again
        // before the next call.
        if let Some(previous) = previous.replace(sent.at_ms) {
            let gap = sent.at_ms.saturating_sub(previous).min(max_gap_ms);
            tokio::time::sleep(std::time::Duration::from_millis(gap)).await;
        }
        replayed.sent += 1;
        let sent = issued.apply(sent);
        let Some(response) = send_one(emulator, rebuild, &sent).await else {
            replayed.failed += 1;
            continue;
        };
        if !sent.issued.is_empty() {
            let (_, operation) =
                crate::server::operation(&sent.method, &sent.uri, &sent.headers, &sent.body);
            let new = issued_ids(&operation, response.body());
            issued.0.extend(sent.issued.iter().cloned().zip(new));
        }
        if let Some(role) = sent.created_role() {
            for (_, grant) in standing.iter().filter(|((r, _), _)| *r == role) {
                let _ = send_one(emulator, rebuild, grant).await;
            }
            roles.insert(role);
        } else if let Some(role) = sent.deleted_role() {
            roles.remove(&role);
        } else if let Some(key) = sent.deleted_policy() {
            standing.retain(|(k, _)| *k != key);
        }
    }
    // Let what the calls set off (deliveries, functions) finish: until AWS
    // has been quiet a moment, for at most a while.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    while !rebuild.settled(1500) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    replayed
}

/// Send one call again: its answer when it succeeds. Each call succeeded the
/// first time; sent again sooner than it was, it can find what it acts on
/// not ready yet, and waits for it as the code did.
async fn send_one(
    emulator: &Emulator,
    rebuild: &crate::observe::Rebuild,
    sent: &Sent,
) -> Option<http::Response<bytes::Bytes>> {
    let started = std::time::Instant::now();
    loop {
        let id = rebuild.call();
        let mut request = sent.request();
        // What the call sets off is told apart by this call's tracing header.
        if let Ok(value) = http::HeaderValue::from_str(&crate::observe::trace_header(id)) {
            request.headers_mut().insert("x-amzn-trace-id", value);
        }
        crate::observe::chain_started(request.headers(), id);
        let (response, ()) = emulator.handle_traced(request, id, || ()).await;
        let status = response.status().as_u16();
        if status < 400 {
            return Some(response);
        }
        let headers = lenslet_core::trace::headers(response.headers());
        let text = String::from_utf8_lossy(&response.body()[..response.body().len().min(4096)]);
        let code = lenslet_core::problem::from_response(status, &headers, Some(&text))
            .map(|p| p.code)
            .unwrap_or_default();
        let waited = started.elapsed();
        // A grant to a role not there yet is sent again with the role.
        let again = sent.grant.is_none()
            && match retry(status, &code) {
                Retry::Busy => waited < READY_WAIT,
                // Something still under way may be making it; else it has a few
                // seconds to appear.
                Retry::Missing => {
                    waited < READY_WAIT && (!rebuild.settled(500) || waited < APPEAR_WAIT)
                }
                Retry::Never => false,
            };
        if !again {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_only_for_what_can_become_ready() {
        assert!(retry(409, "ResourceConflictException") == Retry::Busy);
        assert!(retry(400, "ResourceInUseException") == Retry::Busy);
        assert!(retry(400, "ResourceNotFoundException") == Retry::Missing);
        assert!(retry(404, "NoSuchBucket") == Retry::Missing);
        // A handle the first account made up never comes.
        assert!(retry(404, "ReceiptHandleIsInvalid") == Retry::Never);
        assert!(retry(404, "NoSuchUpload") == Retry::Never);
        assert!(retry(400, "ValidationException") == Retry::Never);
    }

    #[test]
    fn later_calls_name_the_new_accounts_ids() {
        let sent = |uri: &str, body: &'static str| Sent {
            at_ms: 0,
            method: http::Method::PUT,
            uri: uri.parse().unwrap(),
            headers: http::HeaderMap::new(),
            body: bytes::Bytes::from_static(body.as_bytes()),
            issued: Vec::new(),
            grant: None,
        };
        assert_eq!(
            issued_ids(
                "CreateMultipartUpload",
                b"<InitiateMultipartUploadResult><UploadId>old-upload-1</UploadId></InitiateMultipartUploadResult>"
            ),
            vec!["old-upload-1"]
        );
        assert_eq!(
            issued_ids(
                "ReceiveMessage",
                br#"{"Messages":[{"ReceiptHandle":"handle-one-1"},{"ReceiptHandle":"handle-two-2"}]}"#
            ),
            vec!["handle-one-1", "handle-two-2"]
        );
        assert!(issued_ids("ReceiveMessage", b"{}").is_empty());
        let issued = Issued(vec![("old-upload-1".into(), "new-upload-9".into())]);
        let part = sent("/b/k?partNumber=1&uploadId=old-upload-1", "data");
        assert_eq!(
            issued.apply(&part).uri.to_string(),
            "/b/k?partNumber=1&uploadId=new-upload-9"
        );
        let key = sent("/", r#"{"KeyId":"old-upload-1"}"#);
        assert_eq!(&issued.apply(&key).body[..], br#"{"KeyId":"new-upload-9"}"#);
        let other = sent("/b/other", "x");
        assert!(matches!(
            issued.apply(&other),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    /// A message's receipt handle, with the characters encodings change.
    const OLD: &str = "AQEB+old/handle==";
    const NEW: &str = "AQEB+new/handle/9=";

    fn request(headers: &[(&'static str, &str)], uri: &str, body: Vec<u8>) -> Sent {
        let mut map = http::HeaderMap::new();
        for (name, value) in headers {
            map.append(*name, value.parse().unwrap());
        }
        Sent {
            at_ms: 0,
            method: http::Method::POST,
            uri: uri.parse().unwrap(),
            headers: map,
            body: bytes::Bytes::from(body),
            issued: Vec::new(),
            grant: None,
        }
    }

    fn scope(service: &str) -> String {
        format!(
            "AWS4-HMAC-SHA256 Credential=k/20260101/us-east-1/{service}/aws4_request, SignedHeaders=host, Signature=0"
        )
    }

    #[test]
    fn new_ids_are_written_as_each_encoding_writes_them() {
        let issued = Issued(vec![(OLD.into(), NEW.into())]);
        let sqs = scope("sqs");

        // Smithy RPCv2 CBOR: valid CBOR, the new handle at its own length.
        let mut cbor = vec![0xa2, 0x68];
        cbor.extend_from_slice(b"QueueUrl");
        cbor.extend_from_slice(&[0x61, b'q', 0x6d]);
        cbor.extend_from_slice(b"ReceiptHandle");
        cbor.push(0x60 | OLD.len() as u8);
        cbor.extend_from_slice(OLD.as_bytes());
        let sent = request(
            &[
                ("authorization", &sqs),
                ("content-type", "application/cbor"),
                ("smithy-protocol", "rpc-v2-cbor"),
            ],
            "/service/AmazonSQS/operation/DeleteMessage",
            cbor,
        );
        let swapped = issued.apply(&sent);
        let value = lenslet_core::cbor::decode_cbor(&swapped.body).expect("valid CBOR");
        assert_eq!(value["ReceiptHandle"], NEW);
        assert_eq!(value["QueueUrl"], "q");

        // A gzip'd Query form, its handle percent-encoded: sent plain.
        let form = format!(
            "Action=DeleteMessage&QueueUrl=q&ReceiptHandle={}",
            encode(OLD)
        );
        assert!(form.contains("%2Bold%2Fhandle%3D%3D"));
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, form.as_bytes()).unwrap();
        let sent = request(
            &[
                ("authorization", &sqs),
                ("content-type", "application/x-www-form-urlencoded"),
                ("content-encoding", "gzip"),
                ("content-length", "999"),
            ],
            "/",
            gz.finish().unwrap(),
        );
        let swapped = issued.apply(&sent);
        let body = std::str::from_utf8(&swapped.body).unwrap();
        assert_eq!(
            lenslet_core::wire::form_field(body, "ReceiptHandle").as_deref(),
            Some(NEW)
        );
        assert!(body.starts_with("Action=DeleteMessage&QueueUrl=q&"));
        assert!(!swapped.headers.contains_key("content-encoding"));
        assert_eq!(swapped.headers["content-length"], body.len().to_string());

        // aws-chunked JSON with a trailing checksum: unframed, its framing
        // and checksum headers gone.
        let json = format!(r#"{{"QueueUrl":"q","ReceiptHandle":"{OLD}"}}"#);
        let framed = format!(
            "{:x}\r\n{json}\r\n0\r\nx-amz-checksum-crc32:AAAAAA==\r\n\r\n",
            json.len()
        );
        let sent = request(
            &[
                ("authorization", &sqs),
                ("content-type", "application/x-amz-json-1.0"),
                ("content-encoding", "aws-chunked"),
                ("x-amz-content-sha256", "STREAMING-UNSIGNED-PAYLOAD-TRAILER"),
                ("x-amz-decoded-content-length", &json.len().to_string()),
                ("x-amz-trailer", "x-amz-checksum-crc32"),
            ],
            "/",
            framed.into_bytes(),
        );
        let swapped = issued.apply(&sent);
        assert_eq!(&swapped.body[..], json.replace(OLD, NEW).as_bytes());
        for gone in [
            "content-encoding",
            "x-amz-decoded-content-length",
            "x-amz-trailer",
        ] {
            assert!(!swapped.headers.contains_key(gone), "{gone}");
        }
        assert_eq!(swapped.headers["x-amz-content-sha256"], "UNSIGNED-PAYLOAD");

        // In the address, percent-encoded; an object's data is never changed.
        let sent = request(
            &[("authorization", &scope("s3"))],
            &format!("/b/k?partNumber=1&uploadId={}", encode(OLD)),
            format!("part data naming {OLD}").into_bytes(),
        );
        let swapped = issued.apply(&sent);
        assert_eq!(
            lenslet_core::wire::query_param(&swapped.uri.to_string(), "uploadId").as_deref(),
            Some(NEW)
        );
        assert_eq!(swapped.body, sent.body);
    }
}
