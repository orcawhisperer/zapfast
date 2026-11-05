//! `open`'s names in a page: a link opens in a new tab, and a file has no
//! desktop app to go to.

use std::ffi::OsStr;
use std::io::{Error, ErrorKind, Result};

pub fn that_detached(target: impl AsRef<OsStr>) -> Result<()> {
    let target = target.as_ref().to_string_lossy();
    if !(target.starts_with("https://")
        || target.starts_with("http://")
        || target.starts_with("mailto:"))
    {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "a page cannot open files with other apps",
        ));
    }
    let window =
        web_sys::window().ok_or_else(|| Error::new(ErrorKind::Unsupported, "no window here"))?;
    window
        .open_with_url_and_target(&target, "_blank")
        .map(|_| ())
        .map_err(|_| Error::other("the browser refused to open the link"))
}

pub fn that(target: impl AsRef<OsStr>) -> Result<()> {
    that_detached(target)
}
