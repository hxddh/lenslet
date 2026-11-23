//! The calls AWS makes inside the emulator: a bucket's notification invoking
//! a function, the function's own SDK calls, a topic delivering to a queue, a
//! rule reaching its target. The router tells the observer about every call
//! it serves; the ones made in-process (callback depth 1 and more) are
//! recorded as internal calls, under the call they happened inside.
//!
//! The parent is found through the task: a call from outside is served inside
//! [`within`], which holds a stack of the calls in progress on that task. A
//! delivery the emulator makes later, on a task of its own, has no stack; it
//! is recorded with no parent.

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
}

impl CallObserver for Observer {
    fn begin(&self, depth: u8) -> u64 {
        if depth == 0 {
            // Calls from outside are recorded where they arrive.
            return 0;
        }
        let id = next_id();
        let parent = STACK
            .try_with(|stack| {
                let mut stack = stack.borrow_mut();
                let parent = stack.last().copied();
                stack.push(id);
                parent
            })
            .ok()
            .flatten();
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, Open { parent, started: Instant::now(), at_ms: now_ms() });
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
        let Some(open) = self.open.lock().unwrap_or_else(|e| e.into_inner()).remove(&token) else {
            return;
        };
        let body = Bytes::copy_from_slice(call.body);
        let mut recorded = record(
            Sent { method: call.method, uri: call.uri, headers: call.headers },
            &body,
            Answered { status: call.response.status(), headers: call.response.headers() },
            call.response.body(),
            open.at_ms,
            open.started.elapsed().as_millis() as u64,
        );
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
    set_call_observer(Some(Arc::new(Observer { session, refresher, open: Mutex::new(HashMap::new()) })));
}
