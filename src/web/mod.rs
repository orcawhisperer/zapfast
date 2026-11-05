//! What the browser build uses in place of the desktop's threads, sockets,
//! files, and native dialogs. Compiled only for wasm32; the desktop build never
//! sees it.

mod entry;
pub mod file;
pub mod http;
pub mod idb;
pub mod images;
pub mod keys;
pub mod open;
pub mod rfd;
pub mod runtime;
pub mod sfs;
pub mod sqlite;
pub mod tasks;
pub mod time;
pub mod tokio_fs;
pub mod transport;

pub use entry::start;
