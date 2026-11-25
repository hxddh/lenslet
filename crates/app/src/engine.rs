//! Lenslet's engine: the embedded emulator, the endpoint in front of it, the
//! session it records into, and the resource inventory. It runs on threads
//! of its own; the window holds an [`Engine`] and reads snapshots.

use crate::{
    emulator::Emulator,
    resources::{self, Contents, Refresher},
    server,
    session::{Resource, Session, Snapshot},
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
    pub fn fresh(&self) -> impl Future<Output = ()> + use<> {
        let (emulator, session, refresher) = (
            self.emulator.clone(),
            self.session.clone(),
            self.refresher.clone(),
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.runtime.spawn(async move {
            let _ = tokio::task::spawn_blocking(move || {
                emulator.fresh();
                session.fresh();
            })
            .await;
            refresher.poke();
            let _ = tx.send(());
        });
        async move {
            let _ = rx.await;
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
        let accesses: Vec<_> = self
            .session
            .snapshot()
            .permissions
            .into_iter()
            .map(|p| p.access)
            .collect();
        lenslet_core::policy::compile(&accesses)
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
    /// before then sent to it again, in order. What the account did in
    /// response (deliveries, functions) happens again too, unrecorded.
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
                let _ = tx.send(Err("the account is already going back".into()));
                return;
            }
            crate::observe::pause(true);
            let result = replay(&emulator, &session, run).await;
            crate::observe::pause(false);
            busy.store(false, Ordering::SeqCst);
            refresher.poke();
            let _ = tx.send(result);
        });
        async move {
            rx.await
                .unwrap_or_else(|_| Err("the engine stopped".into()))
        }
    }

    /// The account as a file's text, to save it.
    pub fn save_account(&self) -> Result<String, String> {
        Ok(crate::account::to_json(&self.session.log()?))
    }

    /// Replace the account with the one saved in `text`: a new, empty
    /// engine, and the calls that made the saved one sent to it again.
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
                let _ = tx.send(Err("the account is already being rebuilt".into()));
                return;
            }
            crate::observe::pause(true);
            let fresh = (emulator.clone(), session.clone());
            let result = match tokio::task::spawn_blocking(move || {
                fresh.0.fresh();
                fresh.1.fresh();
            })
            .await
            {
                Ok(()) => {
                    session.adopt(log.clone());
                    Ok(send_again(&emulator, &log, 200).await)
                }
                Err(_) => Err("could not start a new engine".to_string()),
            };
            crate::observe::pause(false);
            busy.store(false, Ordering::SeqCst);
            refresher.poke();
            let _ = tx.send(result);
        });
        async move {
            rx.await
                .unwrap_or_else(|_| Err("the engine stopped".into()))
        }
    }

    /// Allow the role `role` what `accesses` need, as an inline policy.
    pub fn grant(
        &self,
        role: String,
        accesses: Vec<lenslet_core::access::Access>,
    ) -> impl Future<Output = Result<(), String>> + use<> {
        let emulator = self.emulator.clone();
        let policy = lenslet_core::policy::compile(&accesses);
        // One inline policy per granted action, so grants add up.
        let name = accesses
            .first()
            .map(|a| format!("lenslet-{}", a.action.replace([':', '*'], "-")))
            .unwrap_or_else(|| "lenslet-granted".into());
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.runtime.spawn(async move {
            let _ = tx.send(resources::grant(&emulator, &role, &name, &policy).await);
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

/// Whether an error answered to a call sent again means what it acts on is
/// not ready yet (a table still being created), so it is worth sending again.
fn not_ready(status: u16, body: &[u8]) -> bool {
    if matches!(status, 409 | 429 | 503) {
        return true;
    }
    let text = String::from_utf8_lossy(&body[..body.len().min(4096)]);
    [
        "NotFound", "NoSuch", "InUse", "Conflict", "Pending", "NotReady", "Throttl",
    ]
    .iter()
    .any(|code| text.contains(code))
}

async fn replay(emulator: &Emulator, session: &Session, run: u64) -> Result<Replayed, String> {
    let log = session.rewind_to(run)?;
    let fresh = emulator.clone();
    tokio::task::spawn_blocking(move || fresh.fresh())
        .await
        .map_err(|_| "could not start a new engine".to_string())?;
    Ok(send_again(emulator, &log, 3000).await)
}

/// Send `log` to the emulator again, in order, keeping the gaps between
/// the calls up to `max_gap_ms`, then let what they set off finish.
async fn send_again(
    emulator: &Emulator,
    log: &[crate::session::Sent],
    max_gap_ms: u64,
) -> Replayed {
    let mut replayed = Replayed::default();
    for (i, sent) in log.iter().enumerate() {
        // Keep the session's pace, up to a few seconds a gap: what the code
        // waited for (a table, a function becoming ready) is ready again
        // before the next call.
        if let Some(previous) = i.checked_sub(1).map(|p| log[p].at_ms) {
            let gap = sent.at_ms.saturating_sub(previous).min(max_gap_ms);
            tokio::time::sleep(std::time::Duration::from_millis(gap)).await;
        }
        // Each call succeeded the first time. Sent again sooner than it was,
        // it can find what it acts on not ready yet: wait for it, as the
        // code did. Any other error is final.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let mut request = http::Request::new(sent.body.clone());
            *request.method_mut() = sent.method.clone();
            *request.uri_mut() = sent.uri.clone();
            *request.headers_mut() = sent.headers.clone();
            let response = emulator.handle(request).await;
            let status = response.status().as_u16();
            if status < 400 {
                break;
            }
            if !not_ready(status, response.body()) || std::time::Instant::now() > deadline {
                replayed.failed += 1;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        replayed.sent += 1;
    }
    // Let what the calls set off (deliveries, functions) finish: until AWS
    // has been quiet a moment, for at most a while.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    while !crate::observe::settled(1500) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    replayed
}
