//! The library's executor on the page's one thread and its timers.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use whatsapp_rust::wacore::runtime::{AbortHandle, Runtime};

/// Spawns on the page's microtask queue and sleeps with `setTimeout`.
pub struct WebRuntime;

#[async_trait::async_trait(?Send)]
impl Runtime for WebRuntime {
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + 'static>>) -> AbortHandle {
        let (abortable, handle) = futures::future::abortable(future);
        wasm_bindgen_futures::spawn_local(async move {
            let _ = abortable.await;
        });
        AbortHandle::new(move || handle.abort())
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()>>> {
        Box::pin(gloo_timers::future::sleep(duration))
    }

    /// Nothing can block a page, so the work simply runs now. Long work has to
    /// yield on its own.
    fn spawn_blocking(&self, f: Box<dyn FnOnce() + 'static>) -> Pin<Box<dyn Future<Output = ()>>> {
        Box::pin(async move { f() })
    }

    fn yield_now(&self) -> Option<Pin<Box<dyn Future<Output = ()>>>> {
        Some(Box::pin(gloo_timers::future::sleep(Duration::ZERO)))
    }
}
