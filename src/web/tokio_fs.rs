//! `tokio::fs`'s names on the page's files, which never wait for a disk.

use std::io::Result;
use std::path::Path;

use crate::web::sfs;

pub async fn read(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    sfs::read(path)
}

pub async fn write(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> Result<()> {
    sfs::write(path, bytes)
}

pub async fn create_dir_all(path: impl AsRef<Path>) -> Result<()> {
    sfs::create_dir_all(path)
}

pub async fn remove_file(path: impl AsRef<Path>) -> Result<()> {
    sfs::remove_file(path)
}

pub async fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<()> {
    sfs::rename(from, to)
}

pub async fn metadata(path: impl AsRef<Path>) -> Result<sfs::Metadata> {
    sfs::metadata(path)
}

pub async fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<u64> {
    sfs::copy(from, to)
}
