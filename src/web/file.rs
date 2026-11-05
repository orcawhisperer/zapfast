//! fastframe-file's names on the page's files. Its writer keeps a thread of its
//! own, which a page lacks, and writing here never waits for a disk, so every
//! write happens at once.

use std::io::Result;
use std::path::Path;
use std::time::Duration;

use crate::web::sfs;

pub const FLUSH_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Access {
    #[default]
    Default,
    Private,
}

pub fn write(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    sfs::write(path, contents)
}

pub fn write_private(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    sfs::write(path, contents)
}

#[derive(Default)]
pub struct Writer;

impl Writer {
    pub fn new() -> Self {
        Self
    }

    pub fn write(&self, path: &Path, contents: impl Into<Vec<u8>>, _access: Access) {
        if let Err(error) = sfs::write(path, contents.into()) {
            log::warn!("could not write {}: {error}", path.display());
        }
    }

    pub fn write_or(
        &self,
        path: &Path,
        contents: impl Into<Vec<u8>>,
        _access: Access,
        on_failure: impl FnOnce() + Send + 'static,
    ) {
        if sfs::write(path, contents.into()).is_err() {
            on_failure();
        }
    }

    pub fn flush(&self, _timeout: Duration) -> bool {
        true
    }
}
