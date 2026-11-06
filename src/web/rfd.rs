//! rfd's `FileDialog`, which rfd leaves out of wasm builds. A browser opens its
//! picker only soon after a click and answers later, so a choice arrives through
//! [`pick_file_then`], and a page has no way to ask where to save: nothing is ever
//! chosen from [`FileDialog::save_file`].

use std::path::PathBuf;

#[derive(Default)]
pub struct FileDialog {
    /// The extensions offered, as the picker's `accept` attribute spells them.
    accept: Vec<String>,
}

impl FileDialog {
    pub fn new() -> Self {
        Self::default()
    }

    /// A browser's picker has no title.
    pub fn set_title(self, _title: impl Into<String>) -> Self {
        self
    }

    pub fn set_file_name(self, _name: impl Into<String>) -> Self {
        self
    }

    pub fn add_filter(mut self, _name: impl Into<String>, extensions: &[impl ToString]) -> Self {
        self.accept.extend(
            extensions
                .iter()
                .map(|extension| format!(".{}", extension.to_string())),
        );
        self
    }

    pub fn save_file(self) -> Option<PathBuf> {
        None
    }
}

/// Asks for one file, and runs `then` with the choice once it is in the page's files.
pub fn pick_file_then(dialog: FileDialog, then: impl FnOnce(Option<PathBuf>) + 'static) {
    crate::web::tasks::spawn(async move {
        then(crate::web::picker::pick_file(&dialog.accept.join(",")).await);
    });
}
