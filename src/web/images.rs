//! Lets egui's `file://` pictures come from the page's files.

use std::sync::Arc;

use egui::load::{Bytes, BytesLoadResult, BytesLoader, BytesPoll, LoadError};

#[derive(Default)]
pub struct PageFiles;

impl BytesLoader for PageFiles {
    fn id(&self) -> &str {
        "zapfast::web::PageFiles"
    }

    fn load(&self, _ctx: &egui::Context, uri: &str) -> BytesLoadResult {
        let Some(path) = uri.strip_prefix("file://") else {
            return Err(LoadError::NotSupported);
        };
        let bytes =
            crate::web::sfs::read(path).map_err(|error| LoadError::Loading(error.to_string()))?;
        Ok(BytesPoll::Ready {
            size: None,
            bytes: Bytes::Shared(Arc::from(bytes)),
            mime: None,
        })
    }

    fn forget(&self, _uri: &str) {}

    fn forget_all(&self) {}

    fn byte_size(&self) -> usize {
        0
    }
}
