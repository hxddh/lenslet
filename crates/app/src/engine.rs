//! Lenslet's engine: the embedded emulator, the endpoint in front of it, the
//! session it records into, and the resource inventory. It runs on threads
//! of its own; the window holds an [`Engine`] and reads snapshots.

use crate::{
    emulator::Emulator,
    resources::{self, Contents, Refresher},
    server,
    replay::{Replayed, rebuild},
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
    /// How far the account being rebuilt is.
    progress: crate::replay::Progress,
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
        crate::observe::attach(emulator.identity(), session.clone(), refresher.clone());
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
                    tokio::spawn(crate::functions::watch(emulator.clone(), session.clone()));
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
            progress: Default::default(),
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
                Ok(router) => {
                    let (old, new) = (emulator.identity(), router.identity());
                    emulator
                        .swap_with(router, || {
                            session.fresh();
                            crate::observe::attach(new, session.clone(), refresher.clone());
                            Ok(())
                        })
                        .inspect(|()| crate::observe::detach(old))
                }
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

    /// How far the account being rebuilt (a rewind, an opened account) is:
    /// the calls sent again so far, of how many.
    pub fn rebuild_progress(&self) -> Option<(usize, usize)> {
        *self.progress.lock().unwrap_or_else(|e| e.into_inner())
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
        let progress = self.progress.clone();
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
                let (built, replayed) = rebuild(&emulator, &log, &progress).await?;
                let (old, new) = (emulator.identity(), built.identity());
                emulator.adopt(&built, || {
                    session.rewind_to(run, log)?;
                    crate::observe::attach(new, session.clone(), refresher.clone());
                    Ok(())
                })?;
                crate::observe::detach(old);
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
        let progress = self.progress.clone();
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
                let (built, replayed) = rebuild(&emulator, &log, &progress).await?;
                // The engine, the session and its history change over
                // together: what arrives after belongs to the opened one.
                let (old, new) = (emulator.identity(), built.identity());
                emulator.adopt(&built, || {
                    session.fresh();
                    session.adopt(log);
                    crate::observe::attach(new, session.clone(), refresher.clone());
                    Ok(())
                })?;
                crate::observe::detach(old);
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

    /// What the function a call invoked did in that invocation (what it
    /// printed, how it ended), read on the engine's threads; `None` for a call
    /// that invoked none, or a run not logged yet.
    pub fn function_run(
        &self,
        call: &crate::session::Call,
    ) -> impl Future<Output = Option<crate::functions::Run>> + use<> {
        let emulator = self.emulator.clone();
        let invoked = crate::functions::invoked(call);
        let at_ms = call.at_ms;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.runtime.spawn(async move {
            let run = match invoked {
                Some((function, request_id)) => {
                    crate::functions::run(&emulator, &function, &request_id, at_ms).await
                }
                None => None,
            };
            let _ = tx.send(run);
        });
        async move { rx.await.ok().flatten() }
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

/// Why the account cannot be rebuilt now.
const BUSY: &str = "Lenslet is rebuilding the account; try again when it is done.";
