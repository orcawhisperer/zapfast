//! What ZapFast keeps in IndexedDB besides SQLite's own blocks: small files and
//! wrapped keys. A thin layer, only what the two callers ask of it.

use futures::channel::oneshot;
use js_sys::{Array, Uint8Array};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{
    Event, IdbDatabase, IdbObjectStore, IdbOpenDbRequest, IdbRequest, IdbTransaction,
    IdbTransactionMode,
};

const NAME: &str = "zapfast";
const VERSION: u32 = 2;
pub const FILES: &str = "files";
pub const KEYS: &str = "keys";
/// Encrypted media, read one file at a time.
pub const MEDIA: &str = "media";
/// Each stored media file's length, read whole at the start.
pub const MEDIA_INDEX: &str = "media-index";

fn failed(what: &str) -> String {
    format!("the browser's storage refused to {what}")
}

/// Waits for a request's answer.
async fn answer(request: &IdbRequest) -> Result<JsValue, String> {
    let (send, receive) = oneshot::channel::<bool>();
    let send = std::rc::Rc::new(std::cell::Cell::new(Some(send)));
    let succeed = {
        let send = send.clone();
        Closure::<dyn FnMut(Event)>::new(move |_| {
            if let Some(send) = send.take() {
                let _ = send.send(true);
            }
        })
    };
    let fail = Closure::<dyn FnMut(Event)>::new(move |_| {
        if let Some(send) = send.take() {
            let _ = send.send(false);
        }
    });
    request.set_onsuccess(Some(succeed.as_ref().unchecked_ref()));
    request.set_onerror(Some(fail.as_ref().unchecked_ref()));
    let done = receive.await.unwrap_or(false);
    request.set_onsuccess(None);
    request.set_onerror(None);
    if !done {
        return Err(failed("answer"));
    }
    request.result().map_err(|_| failed("answer"))
}

/// Waits until a transaction's writes are on their way to the disk.
async fn committed(transaction: &IdbTransaction) -> Result<(), String> {
    let (send, receive) = oneshot::channel::<bool>();
    let send = std::rc::Rc::new(std::cell::Cell::new(Some(send)));
    let complete = {
        let send = send.clone();
        Closure::<dyn FnMut(Event)>::new(move |_| {
            if let Some(send) = send.take() {
                let _ = send.send(true);
            }
        })
    };
    let fail = {
        let send = send.clone();
        Closure::<dyn FnMut(Event)>::new(move |_| {
            if let Some(send) = send.take() {
                let _ = send.send(false);
            }
        })
    };
    transaction.set_oncomplete(Some(complete.as_ref().unchecked_ref()));
    transaction.set_onerror(Some(fail.as_ref().unchecked_ref()));
    transaction.set_onabort(Some(fail.as_ref().unchecked_ref()));
    let done = receive.await.unwrap_or(false);
    transaction.set_oncomplete(None);
    transaction.set_onerror(None);
    transaction.set_onabort(None);
    done.then_some(()).ok_or_else(|| failed("save"))
}

/// Opens the database, creating its two stores the first time.
pub async fn open() -> Result<IdbDatabase, String> {
    let factory = web_sys::window()
        .ok_or("no window")?
        .indexed_db()
        .map_err(|_| failed("open"))?
        .ok_or_else(|| failed("open"))?;
    let request: IdbOpenDbRequest = factory
        .open_with_u32(NAME, VERSION)
        .map_err(|_| failed("open"))?;
    let upgrade = Closure::<dyn FnMut(Event)>::new(|event: Event| {
        let database = event
            .target()
            .and_then(|target| target.dyn_into::<IdbRequest>().ok())
            .and_then(|request| request.result().ok())
            .and_then(|result| result.dyn_into::<IdbDatabase>().ok());
        if let Some(database) = database {
            for store in [FILES, KEYS, MEDIA, MEDIA_INDEX] {
                if database.object_store_names().contains(store) {
                    continue;
                }
                if let Err(error) = database.create_object_store(store) {
                    log::error!("could not create browser storage: {error:?}");
                }
            }
        }
    });
    request.set_onupgradeneeded(Some(upgrade.as_ref().unchecked_ref()));
    let result = answer(&request).await;
    request.set_onupgradeneeded(None);
    result?.dyn_into().map_err(|_| failed("open"))
}

fn store(
    database: &IdbDatabase,
    name: &str,
    mode: IdbTransactionMode,
) -> Result<(IdbTransaction, IdbObjectStore), String> {
    let transaction = database
        .transaction_with_str_and_mode(name, mode)
        .map_err(|_| failed("start"))?;
    let store = transaction
        .object_store(name)
        .map_err(|_| failed("start"))?;
    Ok((transaction, store))
}

/// Everything in a store, as keys beside their values.
pub async fn all(database: &IdbDatabase, name: &str) -> Result<Vec<(String, JsValue)>, String> {
    let (_transaction, store) = store(database, name, IdbTransactionMode::Readonly)?;
    let keys = store.get_all_keys().map_err(|_| failed("read"))?;
    let values = store.get_all().map_err(|_| failed("read"))?;
    let keys: Array = answer(&keys)
        .await?
        .dyn_into()
        .map_err(|_| failed("read"))?;
    let values: Array = answer(&values)
        .await?
        .dyn_into()
        .map_err(|_| failed("read"))?;
    Ok(keys
        .iter()
        .zip(values.iter())
        .filter_map(|(key, value)| Some((key.as_string()?, value)))
        .collect())
}

/// One entry of a store, without reading the rest.
pub async fn get(database: &IdbDatabase, name: &str, key: &str) -> Result<JsValue, String> {
    let (_transaction, store) = store(database, name, IdbTransactionMode::Readonly)?;
    let request = store
        .get(&JsValue::from_str(key))
        .map_err(|_| failed("read"))?;
    answer(&request).await
}

/// One change to a store.
pub enum Change {
    Put(String, JsValue),
    Delete(String),
}

/// Applies `changes` together, and returns once they are committed.
pub async fn apply(database: &IdbDatabase, name: &str, changes: Vec<Change>) -> Result<(), String> {
    let (transaction, store) = store(database, name, IdbTransactionMode::Readwrite)?;
    for change in changes {
        match change {
            Change::Put(key, value) => store.put_with_key(&value, &JsValue::from_str(&key)),
            Change::Delete(key) => store.delete(&JsValue::from_str(&key)),
        }
        .map_err(|_| failed("save"))?;
    }
    committed(&transaction).await
}

/// `bytes` as IndexedDB holds them.
pub fn value_of(bytes: &[u8]) -> JsValue {
    Uint8Array::from(bytes).into()
}
