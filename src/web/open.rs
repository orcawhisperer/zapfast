//! `open`'s names in a page. A link opens in a new tab. A file goes through the
//! browser's own download, with its usual checks, since a page cannot hand one to
//! another app.

use std::ffi::OsStr;
use std::io::{Error, ErrorKind, Result};
use std::path::Path;

use wasm_bindgen::JsCast;

fn unsupported(what: &str) -> Error {
    Error::new(ErrorKind::Unsupported, what.to_owned())
}

/// Offers the page's file at `path` as a download.
fn download(path: &Path) -> Result<()> {
    let bytes = crate::web::sfs::read(path)?;
    let document = web_sys::window()
        .and_then(|window| window.document())
        .ok_or_else(|| unsupported("no document here"))?;
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes.as_slice()));
    let blob = web_sys::Blob::new_with_u8_array_sequence(&parts)
        .map_err(|_| Error::other("the browser would not hold the file"))?;
    let url = web_sys::Url::create_object_url_with_blob(&blob)
        .map_err(|_| Error::other("the browser would not offer the file"))?;
    let anchor: web_sys::HtmlAnchorElement = document
        .create_element("a")
        .map_err(|_| Error::other("the browser would not offer the file"))?
        .dyn_into()
        .map_err(|_| Error::other("the browser would not offer the file"))?;
    anchor.set_href(&url);
    anchor.set_download(
        &path
            .file_name()
            .map_or_else(|| "file".into(), |name| name.to_string_lossy()),
    );
    anchor.click();
    // Released after the browser has started on it.
    let _ = gloo_timers::callback::Timeout::new(60_000, move || {
        let _ = web_sys::Url::revoke_object_url(&url);
    })
    .forget();
    Ok(())
}

pub fn that_detached(target: impl AsRef<OsStr>) -> Result<()> {
    let target = target.as_ref();
    let text = target.to_string_lossy();
    if text.starts_with("https://") || text.starts_with("http://") || text.starts_with("mailto:") {
        let window = web_sys::window().ok_or_else(|| unsupported("no window here"))?;
        return window
            .open_with_url_and_target(&text, "_blank")
            .map(|_| ())
            .map_err(|_| Error::other("the browser refused to open the link"));
    }
    let path = Path::new(target);
    if crate::web::sfs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return download(path);
    }
    Err(unsupported("a page cannot open folders"))
}

pub fn that(target: impl AsRef<OsStr>) -> Result<()> {
    that_detached(target)
}
