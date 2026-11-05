//! Stands in for the OS keyring.
//!
//! Each database's key is random, and is stored encrypted by a master key that
//! IndexedDB keeps as a `CryptoKey` marked non-extractable. Copying the browser's
//! files gives neither the master key's bytes nor the keys it wraps. It does not
//! stop code running in this page, which can ask the browser to unwrap them, so
//! the page's own integrity is what protects them in the end.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, anyhow, bail};
use js_sys::{Array, Object, Reflect, Uint8Array};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{CryptoKey, IdbDatabase, SubtleCrypto};
use zeroize::Zeroizing;

use crate::web::idb;

const MASTER: &str = "master";
const IV_LENGTH: usize = 12;

#[derive(Default)]
struct Keyring {
    database: Option<IdbDatabase>,
    master: Option<CryptoKey>,
    keys: HashMap<String, Zeroizing<[u8; 32]>>,
    /// Stored, but they would not decrypt. Never replaced: a new key would make
    /// the database they open unreadable for good.
    unreadable: HashSet<String>,
}

thread_local! {
    static KEYRING: RefCell<Keyring> = RefCell::new(Keyring::default());
}

fn subtle() -> Result<SubtleCrypto> {
    let crypto = web_sys::window()
        .context("no window")?
        .crypto()
        .map_err(|_| anyhow!("this browser has no cryptography"))?;
    Ok(crypto.subtle())
}

fn algorithm(iv: Option<&[u8]>) -> Result<Object> {
    let object = Object::new();
    let set = |name: &str, value: &JsValue| {
        Reflect::set(&object, &JsValue::from_str(name), value)
            .map_err(|_| anyhow!("could not describe the cipher"))
    };
    set("name", &JsValue::from_str("AES-GCM"))?;
    match iv {
        Some(iv) => set("iv", &Uint8Array::from(iv).into())?,
        None => set("length", &JsValue::from_f64(256.0))?,
    };
    Ok(object)
}

async fn make_master() -> Result<CryptoKey> {
    let usages = Array::of2(&JsValue::from_str("encrypt"), &JsValue::from_str("decrypt"));
    let promise = subtle()?
        .generate_key_with_object(&algorithm(None)?, false, &usages)
        .map_err(|_| anyhow!("could not ask for a master key"))?;
    JsFuture::from(promise)
        .await
        .map_err(|_| anyhow!("the browser made no master key"))?
        .dyn_into()
        .map_err(|_| anyhow!("the browser made something else"))
}

async fn unwrap_key(master: &CryptoKey, stored: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
    if stored.len() <= IV_LENGTH {
        bail!("a stored key is too short");
    }
    let (iv, sealed) = stored.split_at(IV_LENGTH);
    let promise = subtle()?
        .decrypt_with_object_and_u8_array(&algorithm(Some(iv))?, master, sealed)
        .map_err(|_| anyhow!("could not ask to open a key"))?;
    let buffer = JsFuture::from(promise)
        .await
        .map_err(|_| anyhow!("a stored key would not open"))?;
    let bytes = Zeroizing::new(Uint8Array::new(&buffer).to_vec());
    let mut key = Zeroizing::new([0; 32]);
    if bytes.len() != key.len() {
        bail!("a stored key has the wrong length");
    }
    key.copy_from_slice(&bytes);
    Ok(key)
}

async fn wrap_key(master: &CryptoKey, key: &[u8; 32]) -> Result<Vec<u8>> {
    let mut iv = [0; IV_LENGTH];
    getrandom::fill(&mut iv).map_err(|error| anyhow!("{error}"))?;
    let promise = subtle()?
        .encrypt_with_object_and_u8_array(&algorithm(Some(&iv))?, master, key)
        .map_err(|_| anyhow!("could not ask to seal a key"))?;
    let buffer = JsFuture::from(promise)
        .await
        .map_err(|_| anyhow!("the browser would not seal a key"))?;
    let mut stored = iv.to_vec();
    stored.extend(Uint8Array::new(&buffer).to_vec());
    Ok(stored)
}

/// Loads the master key, or makes one, and opens every stored key.
pub async fn load(database: IdbDatabase) -> Result<()> {
    let stored = idb::all(&database, idb::KEYS)
        .await
        .map_err(|error| anyhow!(error))?;
    let master = stored
        .iter()
        .find(|(name, _)| name == MASTER)
        .and_then(|(_, value)| value.clone().dyn_into::<CryptoKey>().ok());
    let wrapped: Vec<(String, Vec<u8>)> = stored
        .into_iter()
        .filter(|(name, _)| name != MASTER)
        .map(|(name, value)| (name, Uint8Array::new(&value).to_vec()))
        .collect();
    let master = match master {
        Some(master) => master,
        // Keys without their master can never open again, so nothing is made
        // over them.
        None if !wrapped.is_empty() => {
            bail!("the browser lost ZapFast's master key, so its archive cannot be opened")
        }
        None => {
            let master = make_master().await?;
            idb::apply(
                &database,
                idb::KEYS,
                vec![idb::Change::Put(MASTER.to_owned(), master.clone().into())],
            )
            .await
            .map_err(|error| anyhow!(error))?;
            master
        }
    };
    let mut keys = HashMap::new();
    let mut unreadable = HashSet::new();
    for (name, sealed) in wrapped {
        match unwrap_key(&master, &sealed).await {
            Ok(key) => {
                keys.insert(name, key);
            }
            Err(_) => {
                log::error!("a stored database key would not open");
                unreadable.insert(name);
            }
        }
    }
    KEYRING.with_borrow_mut(|keyring| {
        keyring.database = Some(database);
        keyring.master = Some(master);
        keyring.keys = keys;
        keyring.unreadable = unreadable;
    });
    Ok(())
}

/// Makes and stores the key for `identity` if it has none. Awaited before its
/// database is created, so a database can never exist without a saved key.
pub async fn prepare(identity: &str) -> Result<()> {
    let (database, master, known) = KEYRING.with_borrow(|keyring| {
        (
            keyring.database.clone(),
            keyring.master.clone(),
            keyring.keys.contains_key(identity) || keyring.unreadable.contains(identity),
        )
    });
    // With no storage the sample chats run, and their keys last as long as they do.
    let (Some(database), Some(master)) = (database, master) else {
        return Ok(());
    };
    if known {
        return Ok(());
    }
    let mut key = Zeroizing::new([0; 32]);
    getrandom::fill(&mut *key).map_err(|error| anyhow!("{error}"))?;
    let sealed = wrap_key(&master, &key).await?;
    idb::apply(
        &database,
        idb::KEYS,
        vec![idb::Change::Put(
            identity.to_owned(),
            idb::value_of(&sealed),
        )],
    )
    .await
    .map_err(|error| anyhow!(error))?;
    KEYRING.with_borrow_mut(|keyring| {
        keyring.keys.insert(identity.to_owned(), key);
    });
    Ok(())
}

/// The key for `identity`. It must have been prepared.
pub fn key_for(identity: &str) -> Result<Zeroizing<[u8; 32]>> {
    KEYRING.with_borrow_mut(|keyring| {
        if let Some(key) = keyring.keys.get(identity) {
            return Ok(key.clone());
        }
        if keyring.unreadable.contains(identity) {
            bail!("The browser could not open this archive's key");
        }
        if keyring.database.is_some() {
            bail!("The archive's key was not prepared");
        }
        let mut key = Zeroizing::new([0; 32]);
        getrandom::fill(&mut *key).map_err(|error| anyhow!("{error}"))?;
        keyring.keys.insert(identity.to_owned(), key.clone());
        Ok(key)
    })
}

/// Forgets a key for good, once its database is gone.
pub fn forget(identity: &str) {
    let database = KEYRING.with_borrow_mut(|keyring| {
        keyring.keys.remove(identity);
        keyring.unreadable.remove(identity);
        keyring.database.clone()
    });
    if let Some(database) = database {
        let change = idb::Change::Delete(identity.to_owned());
        wasm_bindgen_futures::spawn_local(async move {
            if idb::apply(&database, idb::KEYS, vec![change])
                .await
                .is_err()
            {
                log::warn!("a removed database's key stayed in the browser's storage");
            }
        });
    }
}
