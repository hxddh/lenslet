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

/// What a rewind sent again.
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
        let listener = match server::bind(port) {
            Ok(listener) => listener,
            Err(e) if !chosen && e.kind() == std::io::ErrorKind::AddrInUse => {
                server::bind(0).map_err(|e| format!("127.0.0.1: {e}"))?
            }
            Err(e) => return Err(format!("127.0.0.1:{port}: {e}")),
        };
        // Port 0 picks a free one; report the one it is.
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        // `localhost` and `[::1]` reach it too, where the machine has IPv6.
        let listener_v6 = server::bind_v6(port);
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
    let rebuild = crate::observe::Rebuild::start();
    let replayed = send_again(&built, &rebuild, log, max_gap_ms).await;
    Ok((built, replayed))
}

/// Why the account cannot be rebuilt now.
const BUSY: &str = "Lenslet is rebuilding the account; try again when it is done.";

/// The ids the first account made up, and the new account's for them.
#[derive(Default)]
struct Issued(Vec<(String, String)>);

impl Issued {
    /// `sent` naming the new account's ids instead of the first one's.
    fn apply<'a>(&self, sent: &'a Sent) -> std::borrow::Cow<'a, Sent> {
        let named = |text: &str| self.0.iter().any(|(old, _)| text.contains(old.as_str()));
        let uri = sent.uri.to_string();
        let body = std::str::from_utf8(&sent.body).ok();
        if self.0.is_empty() || !(named(&uri) || body.is_some_and(named)) {
            return std::borrow::Cow::Borrowed(sent);
        }
        let swap = |text: &str| {
            self.0.iter().fold(text.to_string(), |t, (old, new)| {
                t.replace(old.as_str(), new)
            })
        };
        let mut sent = sent.clone();
        if let Ok(local) = swap(&uri).parse() {
            sent.uri = local;
        }
        if let Some(body) = body.filter(|b| named(b)) {
            sent.body = bytes::Bytes::from(swap(body));
            sent.headers.remove(http::header::CONTENT_LENGTH);
        }
        std::borrow::Cow::Owned(sent)
    }
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
    for (i, sent) in log.iter().enumerate() {
        // Keep the session's pace, up to a few seconds a gap: what the code
        // waited for (a table, a function becoming ready) is ready again
        // before the next call.
        if let Some(previous) = i.checked_sub(1).map(|p| log[p].at_ms) {
            let gap = sent.at_ms.saturating_sub(previous).min(max_gap_ms);
            tokio::time::sleep(std::time::Duration::from_millis(gap)).await;
        }
        let sent = issued.apply(sent);
        let answer = send_one(emulator, rebuild, &sent).await;
        match answer {
            Some(response) => {
                if !sent.issued.is_empty() {
                    let (_, operation) = crate::server::operation(
                        &sent.method,
                        &sent.uri,
                        &sent.headers,
                        &sent.body,
                    );
                    let new = issued_ids(&operation, response.body());
                    issued.0.extend(sent.issued.iter().cloned().zip(new));
                }
                // A role created again gets what Lenslet allowed it.
                if let Some(role) = sent.created_role() {
                    for grant in log.iter().filter(|g| g.grant.as_deref() == Some(&role)) {
                        let _ = send_one(emulator, rebuild, grant).await;
                    }
                }
            }
            // A grant to a role not created yet waits for the role.
            None if sent.grant.is_some() => {}
            None => replayed.failed += 1,
        }
        replayed.sent += 1;
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
}
