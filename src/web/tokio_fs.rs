//! `tokio::fs`'s names for the worker, on the browser's storage.

use std::io::{Error, ErrorKind, Result};
use std::path::Path;

fn unavailable() -> Error {
    Error::new(ErrorKind::Unsupported, "no file storage in this build yet")
}

pub async fn read(_path: impl AsRef<Path>) -> Result<Vec<u8>> {
    Err(unavailable())
}

pub async fn write(_path: impl AsRef<Path>, _bytes: impl AsRef<[u8]>) -> Result<()> {
    Err(unavailable())
}

pub async fn create_dir_all(_path: impl AsRef<Path>) -> Result<()> {
    Err(unavailable())
}

pub async fn remove_file(_path: impl AsRef<Path>) -> Result<()> {
    Err(unavailable())
}

pub async fn rename(_from: impl AsRef<Path>, _to: impl AsRef<Path>) -> Result<()> {
    Err(unavailable())
}

pub async fn metadata(_path: impl AsRef<Path>) -> Result<std::fs::Metadata> {
    Err(unavailable())
}

pub async fn copy(_from: impl AsRef<Path>, _to: impl AsRef<Path>) -> Result<u64> {
    Err(unavailable())
}
