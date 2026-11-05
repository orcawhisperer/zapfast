//! rfd's blocking `FileDialog`, which rfd leaves out of wasm builds. A browser
//! opens its picker only from a click and answers asynchronously, so until the
//! composer drives that, no dialog ever returns a file.

use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct FileDialog;

impl FileDialog {
    pub fn new() -> Self {
        Self
    }

    pub fn set_title(self, _title: impl Into<String>) -> Self {
        self
    }

    pub fn set_file_name(self, _name: impl Into<String>) -> Self {
        self
    }

    pub fn set_directory(self, _path: impl AsRef<Path>) -> Self {
        self
    }

    pub fn add_filter(self, _name: impl Into<String>, _extensions: &[impl ToString]) -> Self {
        self
    }

    pub fn pick_file(self) -> Option<PathBuf> {
        None
    }

    pub fn pick_files(self) -> Option<Vec<PathBuf>> {
        None
    }

    pub fn pick_folder(self) -> Option<PathBuf> {
        None
    }

    pub fn save_file(self) -> Option<PathBuf> {
        None
    }
}
