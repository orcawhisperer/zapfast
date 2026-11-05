//! The library's executor on the page's one thread and its timers.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use whatsapp_rust::wacore::runtime::{AbortHandle, Runtime};
use whatsapp_rust::wacore::time;

/// The protocol's clocks. The library has no way to read the time in a page and
/// would otherwise count from 1970, which WhatsApp would see in its timestamps.
struct WallClock;

impl time::TimeProvider for WallClock {
    fn now_millis(&self) -> i64 {
        js_sys::Date::now() as i64
    }
}

struct SteadyClock;

impl time::MonotonicProvider for SteadyClock {
    fn now_nanos(&self) -> u64 {
        static START: std::sync::OnceLock<web_time::Instant> = std::sync::OnceLock::new();
        START
            .get_or_init(web_time::Instant::now)
            .elapsed()
            .as_nanos() as u64
    }
}

/// Sets the clocks once, before anything reads them.
pub fn install_clocks() {
    let _ = time::set_time_provider(WallClock);
    let _ = time::set_monotonic_provider(SteadyClock);
}

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
