//! Where the databases live in a page: blocks in IndexedDB, held in memory.
//!
//! The VFS keeps each database in memory and writes changed blocks to IndexedDB
//! after a commit returns, so a tab killed in between loses that commit. SQLite's
//! durable browser VFS needs a dedicated worker, which this build does not run in
//! yet. The cost is known and bounded: a message comes back with the next history
//! sync, and a Signal session re-establishes.
//!
//! Encryption wraps the VFS. SQLite3 Multiple Ciphers wraps only the default VFS
//! at the moment SQLite starts, so a VFS registered later has to be wrapped by
//! name, or a `PRAGMA hexkey` would silently leave everything in plaintext.

use std::cell::OnceCell;
use std::ffi::CString;

use sqlite_wasm_rs::{SQLITE_OK, WasmOsCallback, sqlite3_initialize, sqlite3mc_vfs_create};
use sqlite_wasm_vfs::relaxed_idb::{
    Preload, RelaxedIdbCfg, RelaxedIdbUtil, install as install_vfs,
};

const VFS: &str = "relaxed-idb";

thread_local! {
    static STORE: OnceCell<RelaxedIdbUtil> = const { OnceCell::new() };
}

/// Loads every database and makes the encrypted VFS the default. Until this has
/// run a database opens in memory, as the sample chats' does.
pub async fn install() -> Result<(), String> {
    // SAFETY: called once, before any database opens, on the page's one thread.
    if unsafe { sqlite3_initialize() } != SQLITE_OK {
        return Err("SQLite could not start".to_owned());
    }
    let store = install_vfs::<WasmOsCallback>(
        &RelaxedIdbCfg {
            vfs_name: VFS.to_owned(),
            clear_on_init: false,
            preload: Preload::All,
        },
        false,
    )
    .await
    .map_err(|error| format!("could not load the browser's databases: {error}"))?;
    let name = CString::new(VFS).map_err(|error| error.to_string())?;
    // SAFETY: the name is NUL-terminated, and the VFS it names was just registered.
    if unsafe { sqlite3mc_vfs_create(name.as_ptr(), 1) } != SQLITE_OK {
        return Err("could not encrypt the browser's databases".to_owned());
    }
    STORE.with(|cell| {
        let _ = cell.set(store);
    });
    Ok(())
}

/// Whether the database at `path` exists.
pub fn exists(path: &str) -> bool {
    STORE.with(|cell| cell.get().is_some_and(|store| store.exists(path)))
}

/// Every database at or beneath `path`.
pub fn beneath(path: &std::path::Path) -> Vec<String> {
    STORE.with(|cell| {
        cell.get().map_or_else(Vec::new, |store| {
            store
                .list()
                .into_iter()
                .filter(|name| std::path::Path::new(name).starts_with(path))
                .collect()
        })
    })
}

/// Deletes a closed database. The deletion reaches IndexedDB shortly after.
pub fn delete(path: &str) -> std::io::Result<()> {
    STORE.with(|cell| {
        let Some(store) = cell.get() else {
            return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        };
        let waiting = store
            .delete_db(path)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        wasm_bindgen_futures::spawn_local(async move {
            if waiting.await.is_err() {
                log::warn!("a deleted database did not leave the browser's storage");
            }
        });
        Ok(())
    })
}
