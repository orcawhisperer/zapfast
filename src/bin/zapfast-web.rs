//! The browser build's program. On the desktop it does nothing: `zapfast` is
//! the program there.

#[cfg(target_arch = "wasm32")]
fn main() {
    zapfast::web::start();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {}
