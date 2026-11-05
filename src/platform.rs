//! The few names that differ between the desktop and the browser, so the code
//! that uses them needs no `cfg` of its own.

#[cfg(target_arch = "wasm32")]
pub use crate::web::sfs;
pub mod file {
    #[cfg(not(target_arch = "wasm32"))]
    pub use fastframe_file::*;

    #[cfg(target_arch = "wasm32")]
    pub use crate::web::file::*;
}

#[cfg(not(target_arch = "wasm32"))]
pub use open;
#[cfg(not(target_arch = "wasm32"))]
pub use std::fs as sfs;

/// `Path`'s own checks go straight to the operating system, which a page lacks.
pub trait FilePath {
    fn found(&self) -> bool;
    fn found_file(&self) -> bool;
    fn found_folder(&self) -> bool;
    fn checked_found(&self) -> std::io::Result<bool>;
    fn resolved(&self) -> std::io::Result<std::path::PathBuf>;
}

impl FilePath for std::path::Path {
    #[cfg(not(target_arch = "wasm32"))]
    fn found(&self) -> bool {
        self.exists()
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn found_file(&self) -> bool {
        self.is_file()
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn found_folder(&self) -> bool {
        self.is_dir()
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn checked_found(&self) -> std::io::Result<bool> {
        self.try_exists()
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn resolved(&self) -> std::io::Result<std::path::PathBuf> {
        self.canonicalize()
    }

    #[cfg(target_arch = "wasm32")]
    fn found(&self) -> bool {
        sfs::metadata(self).is_ok()
    }

    #[cfg(target_arch = "wasm32")]
    fn found_file(&self) -> bool {
        sfs::metadata(self).is_ok_and(|metadata| metadata.is_file())
    }

    #[cfg(target_arch = "wasm32")]
    fn found_folder(&self) -> bool {
        sfs::metadata(self).is_ok_and(|metadata| metadata.is_dir())
    }

    #[cfg(target_arch = "wasm32")]
    fn checked_found(&self) -> std::io::Result<bool> {
        Ok(self.found())
    }

    #[cfg(target_arch = "wasm32")]
    fn resolved(&self) -> std::io::Result<std::path::PathBuf> {
        sfs::canonicalize(self)
    }
}
#[cfg(not(target_arch = "wasm32"))]
pub use rfd::FileDialog;
#[cfg(not(target_arch = "wasm32"))]
pub use tokio::fs;

#[cfg(target_arch = "wasm32")]
pub use crate::web::open;
#[cfg(target_arch = "wasm32")]
pub use crate::web::rfd::FileDialog;
#[cfg(target_arch = "wasm32")]
pub use crate::web::tokio_fs as fs;

#[cfg(not(target_arch = "wasm32"))]
pub use std::time::{Instant, SystemTime, UNIX_EPOCH};
#[cfg(not(target_arch = "wasm32"))]
pub use tokio::{
    spawn,
    task::{JoinHandle, spawn_blocking},
    time,
};

#[cfg(not(target_arch = "wasm32"))]
pub use tokio::task::AbortHandle;

/// Sleeps until a moment read from [`Instant`].
#[cfg(not(target_arch = "wasm32"))]
pub async fn sleep_until(deadline: Instant) {
    time::sleep_until(time::Instant::from_std(deadline)).await;
}

/// Waits on a channel from a thread of its own.
#[cfg(not(target_arch = "wasm32"))]
pub fn recv_blocking<T>(
    channel: &async_channel::Receiver<T>,
) -> Result<T, async_channel::RecvError> {
    channel.recv_blocking()
}

#[cfg(target_arch = "wasm32")]
pub use crate::web::tasks::AbortHandle;
#[cfg(target_arch = "wasm32")]
pub use crate::web::time::sleep_until;

/// A page has no second thread to wait on, so nothing arrives.
#[cfg(target_arch = "wasm32")]
pub fn recv_blocking<T>(
    _channel: &async_channel::Receiver<T>,
) -> Result<T, async_channel::RecvError> {
    Err(async_channel::RecvError)
}

// std's clock panics in a page, Tokio's timers need a driver no page has, and
// its `spawn` asks for `Send`, which the library's client is not there.
#[cfg(target_arch = "wasm32")]
pub use crate::web::tasks::{JoinHandle, spawn, spawn_blocking};
#[cfg(target_arch = "wasm32")]
pub use crate::web::time;
#[cfg(target_arch = "wasm32")]
pub use web_time::{Instant, SystemTime, UNIX_EPOCH};
