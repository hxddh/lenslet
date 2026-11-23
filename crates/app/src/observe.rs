//! The calls AWS makes inside the emulator: a bucket's notification invoking
//! a function, the function's own SDK calls, a topic delivering to a queue, a
//! rule reaching its target. The router tells the observer about every call
//! it serves; the ones made in-process (callback depth 1 and more) are
//! recorded as internal calls, under the call they happened inside.
//!
//! The parent is found two ways. On the same task: a call from outside is
//! served inside [`within`], which holds a stack of the calls in progress on
//! that task. Across tasks: every call from outside carries an X-Ray tracing
//! header (Lenslet adds one when the caller did not), which AWS carries onto
//! everything the call causes, deliveries made later included; a call on a
//! chain hangs under the chain's innermost call still in progress, else under
//! the call from outside that started the chain.

use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use bytes::Bytes;
use smithy_server_runtime::observer::{CallObserver, ObservedCall, set_call_observer};

use crate::{
    resources::Refresher,
    server::{Answered, Sent, changes_resources, record},
    session::{Session, now_ms},
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A fresh call id.
pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

tokio::task_local! {
    static STACK: RefCell<Vec<u64>>;
}

/// Run `work` as the call `id`: the calls it causes on this task name it.
pub async fn within<F: Future>(id: u64, work: F) -> F::Output {
    STACK.scope(RefCell::new(vec![id]), work).await
}

struct Open {
    parent: Option<u64>,
    started: Instant,
    at_ms: u64,
}

struct Observer {
    session: Session,
    refresher: Refresher,
    open: Mutex<HashMap<u64, Open>>,
    chains: Mutex<Chains>,
}

/// The calls on each tracing chain.
#[derive(Default)]
struct Chains {
    /// The call from outside each chain started with, by the chain's root.
    started: HashMap<String, u64>,
    /// The calls on each chain still in progress, innermost last.
    open: HashMap<String, Vec<u64>>,
    /// The chain of each call in progress.
    chain_of: HashMap<u64, String>,
    /// The latest function invocation on each chain: a function runs after
    /// an asynchronous invocation returned, and its own calls hang under it.
    invoked: HashMap<String, u64>,
}

/// The X-Ray root of a tracing header: `Root=1-...;Parent=...` gives `1-...`.
pub fn trace_root(headers: &http::HeaderMap) -> Option<String> {
    let value = headers.get("x-amzn-trace-id")?.to_str().ok()?;
    value
        .split(';')
        .find_map(|part| part.trim().strip_prefix("Root="))
        .map(str::to_string)
}

/// A tracing header for the call `id`, in X-Ray's form.
pub fn trace_header(id: u64) -> String {
    format!("Root=1-{:08x}-{:024x};Sampled=0", now_ms() / 1000, id)
}

/// Remember that the call from outside `id` starts the chain its request names.
pub fn chain_started(headers: &http::HeaderMap, id: u64) {
    if let (Some(root), Some(observer)) = (trace_root(headers), OBSERVER.get()) {
        let mut chains = observer.chains.lock().unwrap_or_else(|e| e.into_inner());
        if chains.started.len() > 100_000 {
            chains.started.clear();
        }
        chains.started.insert(root, id);
    }
}

static OBSERVER: std::sync::OnceLock<Arc<Observer>> = std::sync::OnceLock::new();

impl CallObserver for Observer {
    fn begin(&self, depth: u8, headers: &http::HeaderMap) -> u64 {
        if depth == 0 {
            // Calls from outside are recorded where they arrive.
            return 0;
        }
        let id = next_id();
        let on_task = STACK
            .try_with(|stack| {
                let mut stack = stack.borrow_mut();
                let parent = stack.last().copied();
                stack.push(id);
                parent
            })
            .ok()
            .flatten();
        let parent = {
            let mut chains = self.chains.lock().unwrap_or_else(|e| e.into_inner());
            let root = trace_root(headers);
            let on_chain = root.as_ref().and_then(|root| {
                chains
                    .open
                    .get(root)
                    .and_then(|open| open.last().copied())
                    .or_else(|| chains.invoked.get(root).copied())
                    .or_else(|| chains.started.get(root).copied())
            });
            if let Some(root) = root {
                chains.open.entry(root.clone()).or_default().push(id);
                chains.chain_of.insert(id, root);
            }
            on_task.or(on_chain)
        };
        self.open.lock().unwrap_or_else(|e| e.into_inner()).insert(
            id,
            Open {
                parent,
                started: Instant::now(),
                at_ms: now_ms(),
            },
        );
        id
    }

    fn end(&self, token: u64, depth: u8, call: &ObservedCall<'_>) {
        if depth == 0 || token == 0 {
            return;
        }
        let _ = STACK.try_with(|stack| {
            let mut stack = stack.borrow_mut();
            if stack.last() == Some(&token) {
                stack.pop();
            }
        });
        {
            let mut chains = self.chains.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(root) = chains.chain_of.remove(&token) {
                if let Some(open) = chains.open.get_mut(&root) {
                    open.retain(|&id| id != token);
                    if open.is_empty() {
                        chains.open.remove(&root);
                    }
                }
                if call.uri.path().ends_with("/invocations") {
                    chains.invoked.insert(root, token);
                }
            }
        }
        let Some(open) = self
            .open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&token)
        else {
            return;
        };
        let body = Bytes::copy_from_slice(call.body);
        let mut recorded = record(
            Sent {
                method: call.method,
                uri: call.uri,
                headers: call.headers,
            },
            &body,
            Answered {
                status: call.response.status(),
                headers: call.response.headers(),
            },
            call.response.body(),
            open.at_ms,
            open.started.elapsed().as_millis() as u64,
        );
        // AWS's own checks with nothing to hang under (S3 reading a function's
        // policy before it delivers) are plumbing, not part of any story.
        if open.parent.is_none() && !changes_resources(&recorded) {
            return;
        }
        recorded.id = token;
        recorded.parent = open.parent;
        recorded.internal = true;
        // The emulator's own requests carry no client of yours.
        recorded.client = None;
        if changes_resources(&recorded) {
            self.refresher.poke();
        }
        self.session.record(recorded);
    }
}

/// Record the calls AWS makes inside the emulator into `session`.
pub fn install(session: Session, refresher: Refresher) {
    let observer = Arc::new(Observer {
        session,
        refresher,
        open: Mutex::new(HashMap::new()),
        chains: Mutex::new(Chains::default()),
    });
    // One observer per process: the first engine's (the window has one).
    if OBSERVER.set(observer.clone()).is_ok() {
        set_call_observer(Some(observer));
    }
}
