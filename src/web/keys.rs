//! Stands in for the OS keyring: the archive's key for this page.
//!
//! Keys are random and held in memory, which matches the in-memory archive they
//! open. Keeping them across reloads, wrapped by a key the page cannot read,
//! comes with persistent storage.

use std::cell::RefCell;
use std::collections::HashMap;

use anyhow::{Context, Result};
use zeroize::Zeroizing;

thread_local! {
    static KEYS: RefCell<HashMap<String, Zeroizing<[u8; 32]>>> = RefCell::new(HashMap::new());
}

/// The key for `identity`, made the first time it is asked for.
pub fn key_for(identity: &str) -> Result<Zeroizing<[u8; 32]>> {
    KEYS.with_borrow_mut(|keys| {
        if let Some(key) = keys.get(identity) {
            return Ok(key.clone());
        }
        let mut key = Zeroizing::new([0; 32]);
        getrandom::fill(&mut *key)
            .map_err(|error| anyhow::anyhow!("{error}"))
            .context("The browser gave no random bytes for the archive's key")?;
        keys.insert(identity.to_owned(), key.clone());
        Ok(key)
    })
}

pub fn forget(identity: &str) {
    KEYS.with_borrow_mut(|keys| {
        keys.remove(identity);
    });
}
