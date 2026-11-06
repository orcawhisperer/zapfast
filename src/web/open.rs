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

/// An address for `bytes`, released a minute later, by when anything asking for
/// them has them.
pub(crate) fn temporary_url(bytes: &[u8]) -> Result<String> {
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
    let blob = web_sys::Blob::new_with_u8_array_sequence(&parts)
        .map_err(|_| Error::other("the browser would not hold the file"))?;
    let url = web_sys::Url::create_object_url_with_blob(&blob)
        .map_err(|_| Error::other("the browser would not offer the file"))?;
    let release = url.clone();
    gloo_timers::callback::Timeout::new(60_000, move || {
        let _ = web_sys::Url::revoke_object_url(&release);
    })
    .forget();
    Ok(url)
}

/// Offers the page's file at `path` as a download.
fn download(path: &Path) -> Result<()> {
    let bytes = crate::web::sfs::read(path)?;
    let document = web_sys::window()
        .and_then(|window| window.document())
        .ok_or_else(|| unsupported("no document here"))?;
    let url = temporary_url(&bytes)?;
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
        if crate::web::sfs::is_loaded(path) {
            return download(path);
        }
        // On its way from storage, and offered once it is here.
        let path = path.to_owned();
        wasm_bindgen_futures::spawn_local(async move {
            crate::web::sfs::ensure_loaded(&path).await;
            if download(&path).is_err() {
                log::warn!("could not offer a stored file as a download");
            }
        });
        return Ok(());
    }
    Err(unsupported("a page cannot open folders"))
}

pub fn that(target: impl AsRef<OsStr>) -> Result<()> {
    that_detached(target)
}
