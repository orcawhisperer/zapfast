//! ZapFast internals exposed for diagnostics and tests.

// In a browser, crates that cannot build there are replaced under their own
// names, so the code that uses them is the same on every platform.
#[cfg(target_arch = "wasm32")]
extern crate web_shim_fastframe_emoji as fastframe_emoji;
#[cfg(target_arch = "wasm32")]
extern crate web_shim_fastframe_shell as fastframe_shell;
#[cfg(target_arch = "wasm32")]
extern crate web_shim_openh264 as openh264;
#[cfg(target_arch = "wasm32")]
extern crate web_shim_opus as opus;
#[cfg(target_arch = "wasm32")]
extern crate web_shim_ureq as ureq;

pub mod a11y;
pub mod account;
pub mod animation;
pub mod announcer;
pub mod app;
pub mod app_lock;
#[cfg(target_os = "linux")]
pub mod appearance;
pub mod archive;
pub mod audio;
pub mod autostart;
pub mod backdrop;
pub mod backend;
pub mod bidi;
pub mod blocking;
#[cfg(not(target_arch = "wasm32"))]
pub mod call_audio;
#[cfg(target_arch = "wasm32")]
pub use web::call_audio;
pub mod call_diag;
pub mod call_tones;
pub mod calls;
pub mod camera;
#[cfg(any(test, feature = "demo"))]
pub mod demo;
pub mod diagnostics;
pub mod emoji;
pub mod geo;
pub mod group_admin;
pub mod i18n;
pub mod image_cache;
pub mod image_preview;
pub mod lottie;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod markup;
pub mod media_pause;
pub mod mentions;
pub mod model;
pub mod motion;
pub mod nav;
pub mod new_group;
pub mod notify;
pub mod opener;
pub mod paths;
pub mod pcm;
pub mod platform;
pub mod privacy;
pub mod proxy;
pub mod qr;
// Only a page reaches a call's relay through an answer; tested on every platform.
#[cfg(any(test, target_arch = "wasm32"))]
pub mod relay_answer;
pub mod safety;
pub mod settings;
pub mod single_instance;
pub mod sticker_meta;
pub mod sticker_search;
pub mod target;
pub mod theme;
pub mod timestretch;
pub mod transcript;
#[cfg(not(target_arch = "wasm32"))]
pub mod transport;
pub mod ui;
pub mod updates;
pub mod util;
pub mod video;
pub mod video_rate;
pub mod voice;
pub mod wallpaper;
#[cfg(target_arch = "wasm32")]
pub mod web;
