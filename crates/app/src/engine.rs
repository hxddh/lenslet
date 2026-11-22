//! Lenslet's engine: the embedded emulator, the endpoint in front of it, the
//! session it records into, and the resource inventory. It runs on threads
//! of its own; the window holds an [`Engine`] and reads snapshots.

use crate::{
    emulator::Emulator,
    resources::Refresher,
    server,
    session::{Session, Snapshot},
};

/// The port Lenslet listens on unless `LENSLET_PORT` says otherwise.
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
}

impl Engine {
    /// Start the emulator and the endpoint on `port`.
    pub fn start(port: u16) -> Result<Engine, String> {
        let listener = server::bind(port).map_err(|e| format!("127.0.0.1:{port}: {e}"))?;
        // Port 0 picks a free one; report the one it is.
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let endpoint = format!("http://127.0.0.1:{port}");
        let emulator = Emulator::new(&endpoint);
        let session = Session::default();
        let refresher = Refresher::default();
        let engine = Engine { port, emulator, session, refresher };
        let background = engine.clone();
        std::thread::Builder::new()
            .name("lenslet-engine".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(4)
                    .thread_stack_size(WORKER_STACK)
                    .enable_all()
                    .build()
                    .expect("start the engine runtime");
                runtime.block_on(async move {
                    let listener = tokio::net::TcpListener::from_std(listener).expect("listen");
                    tokio::spawn(
                        background
                            .refresher
                            .clone()
                            .run(background.emulator.clone(), background.session.clone()),
                    );
                    server::serve(listener, background.emulator, background.session, background.refresher)
                        .await;
                });
            })
            .map_err(|e| e.to_string())?;
        Ok(engine)
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

    pub fn call_count(&self) -> usize {
        self.session.call_count()
    }

    /// A new, empty AWS account, at once.
    pub fn fresh(&self) {
        self.emulator.fresh();
        self.session.fresh();
        self.refresher.poke();
    }

    /// The IAM policy for every permission this session used.
    pub fn policy(&self) -> String {
        lenslet_core::policy::compile(&self.session.snapshot().permissions)
    }

    /// What to paste into a shell so SDKs and the AWS CLI talk to Lenslet.
    pub fn env(&self) -> String {
        format!(
            "export AWS_ENDPOINT_URL={}\nexport AWS_REGION={REGION}\nexport AWS_ACCESS_KEY_ID=lenslet\nexport AWS_SECRET_ACCESS_KEY=lenslet\n",
            self.endpoint()
        )
    }
}
