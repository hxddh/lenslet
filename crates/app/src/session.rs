//! What the window shows: the calls, the resources that exist, and the IAM
//! permissions the session used. The engine's threads write it; the window
//! reads snapshots of it.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use lenslet_core::access::Access;

/// One AWS call, as the window shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub seq: usize,
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
    /// `table/orders · id="1"`, `s3://bucket/key`, ...
    pub target: Option<String>,
    pub failure: Option<Failure>,
    /// The emulator does not serve this service or operation.
    pub unsupported: bool,
    /// The IAM permissions it needs (own, then implied).
    pub access: Vec<Access>,
    pub request: Exchange,
    pub response: Exchange,
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

/// The resources one service holds, as `name` and a short detail.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Group {
    pub service: &'static str,
    pub items: Vec<(String, Option<String>)>,
}

#[derive(Default)]
struct Inner {
    calls: Vec<Call>,
    permissions: BTreeSet<Access>,
    resources: Vec<Group>,
    generation: u64,
}

#[derive(Clone, Default)]
pub struct Session {
    inner: Arc<Mutex<Inner>>,
}

/// What changed since a reader's last look.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub calls: Vec<Call>,
    pub resources: Vec<Group>,
    pub permissions: Vec<Access>,
    pub generation: u64,
}

impl Session {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    pub fn record(&self, mut call: Call) {
        let mut inner = self.lock();
        call.seq = inner.calls.len();
        call.generation = inner.generation;
        inner.permissions.extend(call.access.iter().cloned());
        inner.calls.push(call);
    }

    pub fn set_resources(&self, resources: Vec<Group>) {
        self.lock().resources = resources;
    }

    /// A new, empty AWS: the calls stay (marked by their generation), the
    /// resources and the permissions start over.
    pub fn fresh(&self) {
        let mut inner = self.lock();
        inner.generation += 1;
        inner.permissions.clear();
        inner.resources.clear();
    }

    pub fn snapshot(&self) -> Snapshot {
        let inner = self.lock();
        Snapshot {
            calls: inner.calls.clone(),
            resources: inner.resources.clone(),
            permissions: inner.permissions.iter().cloned().collect(),
            generation: inner.generation,
        }
    }

    pub fn call_count(&self) -> usize {
        self.lock().calls.len()
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
