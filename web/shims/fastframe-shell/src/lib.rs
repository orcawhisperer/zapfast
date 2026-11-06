//! The browser's stand-in for fastframe-shell, chosen by target in ZapFast's
//! manifest. The real crate needs winit's window; a page has none, and the
//! browser keeps the tab alive. Only `Waker` and the types the app names exist.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// What the app wants when its window closes. A page never reopens itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closed {
    Quit,
    Hide,
    Reopen,
}

/// What a headless tick decided. Never asked for in a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Headless {
    Wait,
    WaitFor(Duration),
    Show,
    Quit,
}

/// The app's half of the shell's contract, kept so the desktop code compiles.
pub trait Resident {
    fn closed(&self) -> Closed;
    fn window_gone(&mut self);
    fn headless_frame(&mut self, ctx: &egui::Context) -> Headless;
    fn start_hidden(&mut self) -> bool {
        false
    }
    fn shutdown(&mut self);
}

thread_local! {
    static ON_WAKE: std::cell::RefCell<Option<Box<dyn Fn()>>> = const { std::cell::RefCell::new(None) };
}

/// Calls `hook` on every wake, for what a repaint alone cannot reach.
pub fn on_wake(hook: impl Fn() + 'static) {
    ON_WAKE.with_borrow_mut(|slot| *slot = Some(Box::new(hook)));
}

/// Wakes the window from other tasks.
#[derive(Clone, Default)]
pub struct Waker {
    window: Arc<Mutex<Option<egui::Context>>>,
}

impl std::fmt::Debug for Waker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Waker")
            .field("attached", &self.context().is_some())
            .finish()
    }
}

impl Waker {
    pub fn attach(&self, ctx: &egui::Context) {
        *self.lock() = Some(ctx.clone());
    }

    pub fn detach(&self) {
        *self.lock() = None;
    }

    pub fn wake(&self) {
        if let Some(ctx) = self.context() {
            ctx.request_repaint();
        }
        // A repaint never comes while the tab is hidden.
        ON_WAKE.with_borrow(|hook| {
            if let Some(hook) = hook {
                hook();
            }
        });
    }

    pub fn wake_after(&self, delay: Duration) {
        if let Some(ctx) = self.context() {
            ctx.request_repaint_after(delay);
        }
    }

    fn context(&self) -> Option<egui::Context> {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<egui::Context>> {
        self.window.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub mod window {
    /// The saved window, unused in a page: the browser places its tab.
    #[derive(Clone, Copy, Debug, Default, PartialEq)]
    pub struct Geometry {
        pub size: Option<[f32; 2]>,
        pub position: Option<[f32; 2]>,
        pub maximized: bool,
    }

    impl Geometry {
        /// Nothing to remember: the browser places the page.
        pub fn remember(&mut self, _ctx: &egui::Context) -> bool {
            false
        }
    }

    /// A page cannot raise itself.
    pub fn raise_on_compositor() -> bool {
        false
    }
}
