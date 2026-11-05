//! `tokio::spawn`'s shape on the page's one thread.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use futures::channel::oneshot;
pub use futures::future::AbortHandle;
use futures::future::Abortable;

/// How a task ended without its value. Nothing unwinds in a page: a panic
/// aborts the module, so the only way here is `abort`.
#[derive(Debug)]
pub struct JoinError;

impl JoinError {
    pub fn is_cancelled(&self) -> bool {
        true
    }

    pub fn is_panic(&self) -> bool {
        false
    }
}

impl std::fmt::Display for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("task was cancelled")
    }
}

impl std::error::Error for JoinError {}

/// A spawned task. Dropping it leaves the task running, as Tokio's does.
pub struct JoinHandle<T> {
    value: oneshot::Receiver<T>,
    abort: AbortHandle,
    finished: Arc<AtomicBool>,
}

impl<T> JoinHandle<T> {
    pub fn abort(&self) {
        self.abort.abort();
    }

    pub fn abort_handle(&self) -> AbortHandle {
        self.abort.clone()
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.value)
            .poll(cx)
            .map(|answer| answer.map_err(|_| JoinError))
    }
}

/// Runs `future` alongside everything else on the page's thread.
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    let (send, value) = oneshot::channel();
    let (abort, registration) = AbortHandle::new_pair();
    let finished = Arc::new(AtomicBool::new(false));
    let done = Arc::clone(&finished);
    wasm_bindgen_futures::spawn_local(async move {
        if let Ok(output) = Abortable::new(future, registration).await {
            let _ = send.send(output);
        }
        done.store(true, Ordering::Relaxed);
    });
    JoinHandle {
        value,
        abort,
        finished,
    }
}

/// Runs `work` on the same thread, on its own turn, since a page has no other.
/// It holds up the page until it returns.
pub fn spawn_blocking<F, R>(work: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + 'static,
    R: 'static,
{
    spawn(async move { work() })
}
