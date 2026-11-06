//! `std::fs`'s names in a page, where std's own always fails.
//!
//! Files live in memory on the page's one thread, with the behaviour the app
//! relies on: a write needs its folder, a rename replaces, a folder lists its
//! children. Once [`hydrate`] has run, changes are saved to IndexedDB in the
//! background.
//!
//! Small files, such as settings, are all read at the start. Media under the cache
//! would not fit in memory, so only an index of it is. A file's bytes are read and
//! decrypted when first asked for, and the oldest go again when too many are held. A
//! read of one not in memory fails with `WouldBlock` and starts fetching it, and
//! [`set_on_loaded`] says when to look again. Code that can wait uses
//! [`ensure_loaded`] first.
//!
//! Databases live in SQLite's own store (`web::sqlite`). They answer here too, so
//! the code that checks for or deletes one needs no special case.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::{self, Cursor, Error, ErrorKind, Read, Result, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

#[derive(Default)]
struct Store {
    files: BTreeMap<PathBuf, Rc<Vec<u8>>>,
    folders: BTreeSet<PathBuf>,
    /// Media saved in IndexedDB, with its length, whether or not it is in memory.
    kept: BTreeMap<PathBuf, u64>,
    /// Media in memory, oldest use first.
    held: Vec<PathBuf>,
    /// When each file was last written, in milliseconds since 1970, where known.
    /// Media keeps it across a reload, for what judges a file by its age.
    written: BTreeMap<PathBuf, f64>,
}

/// How much media to hold in memory before letting the oldest go.
const HOLD_BYTES: u64 = 256 * 1024 * 1024;

impl Store {
    /// Notes that `path` was just used, and lets go of what is over budget. A file
    /// only leaves once it is saved, and the one just used never does.
    fn hold(&mut self, path: &Path) {
        self.held.retain(|held| held != path);
        self.held.push(path.to_owned());
        let mut total: u64 = self
            .held
            .iter()
            .filter_map(|held| self.files.get(held))
            .map(|bytes| bytes.len() as u64)
            .sum();
        let mut index = 0;
        while total > HOLD_BYTES && index + 1 < self.held.len() {
            let candidate = &self.held[index];
            if self.kept.contains_key(candidate) {
                if let Some(bytes) = self.files.remove(candidate) {
                    total -= bytes.len() as u64;
                }
                self.held.remove(index);
            } else {
                index += 1;
            }
        }
    }

    fn forget(&mut self, path: &Path) {
        self.held.retain(|held| held != path);
        self.kept.remove(path);
        self.written.remove(path);
    }
}

thread_local! {
    static STORE: RefCell<Store> = RefCell::new(Store::default());
    static SAVING: RefCell<Saving> = RefCell::new(Saving::default());
    static LOADING: RefCell<BTreeSet<PathBuf>> = const { RefCell::new(BTreeSet::new()) };
    static ON_LOADED: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
}

/// Changes waiting to reach IndexedDB, in order.
#[derive(Default)]
struct Saving {
    database: Option<web_sys::IdbDatabase>,
    /// Where media is kept: encrypted, and read one file at a time.
    media: PathBuf,
    /// The folder that is never saved.
    unsaved: PathBuf,
    waiting: Vec<Change>,
    running: bool,
}

enum Change {
    File(PathBuf, Rc<Vec<u8>>),
    Folder(PathBuf),
    Gone(PathBuf),
}

/// How a stored entry says it is a folder. No path ends in a slash otherwise.
const FOLDER_MARK: &str = "/";

fn key_of(path: &Path, folder: bool) -> String {
    let mut key = path.to_string_lossy().into_owned();
    if folder {
        key.push_str(FOLDER_MARK);
    }
    key
}

/// Media is saved and may leave memory. What is never saved cannot leave, so it
/// stays out of the budget that decides what does.
fn is_media(path: &Path) -> bool {
    SAVING.with_borrow(|saving| {
        saving.database.is_some()
            && path.starts_with(&saving.media)
            && !path.starts_with(&saving.unsaved)
    })
}

/// Queues a change for IndexedDB and starts saving if nothing is.
fn save(change: Change) {
    let start = SAVING.with_borrow_mut(|saving| {
        let path = match &change {
            Change::File(path, _) | Change::Folder(path) | Change::Gone(path) => path,
        };
        if saving.database.is_none() || path.starts_with(&saving.unsaved) {
            return false;
        }
        saving.waiting.push(change);
        !std::mem::replace(&mut saving.running, true)
    });
    if start {
        wasm_bindgen_futures::spawn_local(save_waiting());
    }
}

/// Encrypts and stores one media file, unless it has changed or gone since.
async fn save_media(database: &web_sys::IdbDatabase, path: &Path, bytes: Rc<Vec<u8>>) {
    use crate::web::idb::{self, Change as Stored};
    let current = || {
        with(|store| {
            store
                .files
                .get(path)
                .is_some_and(|now| Rc::ptr_eq(now, &bytes))
        })
    };
    if !current() {
        return;
    }
    let Ok(sealed) = crate::web::keys::seal(&bytes).await else {
        log::warn!("could not encrypt a file for the browser's storage");
        return;
    };
    let key = key_of(path, false);
    // Its length, and when it was written once that is known.
    let length = wasm_bindgen::JsValue::from_f64(bytes.len() as f64);
    let index = match with(|store| store.written.get(path).copied()) {
        Some(written) => js_sys::Array::of2(&length, &written.into()).into(),
        None => length,
    };
    let saved = idb::apply(
        database,
        idb::MEDIA,
        vec![Stored::Put(key.clone(), idb::value_of(&sealed))],
    )
    .await
    .and(idb::apply(database, idb::MEDIA_INDEX, vec![Stored::Put(key, index)]).await);
    match saved {
        // Now it can leave memory, and be found again after a reload.
        Ok(()) if current() => with(|store| {
            store.kept.insert(path.to_owned(), bytes.len() as u64);
            store.hold(path);
        }),
        Ok(()) => {}
        // Most likely the browser's quota. The file lasts as long as the page.
        Err(_) => log::warn!("a file did not fit in the browser's storage"),
    }
}

async fn save_waiting() {
    use crate::web::idb::{self, Change as Stored};
    loop {
        let (database, batch) = SAVING.with_borrow_mut(|saving| {
            (saving.database.clone(), std::mem::take(&mut saving.waiting))
        });
        let Some(database) = database.filter(|_| !batch.is_empty()) else {
            SAVING.with_borrow_mut(|saving| saving.running = false);
            return;
        };
        let mut small = Vec::new();
        let mut media = Vec::new();
        let mut gone = Vec::new();
        for change in batch {
            match change {
                Change::File(path, bytes) if is_media(&path) => media.push((path, bytes)),
                Change::File(path, bytes) => {
                    small.push(Stored::Put(key_of(&path, false), idb::value_of(&bytes)));
                }
                Change::Folder(path) => small.push(Stored::Put(
                    key_of(&path, true),
                    wasm_bindgen::JsValue::TRUE,
                )),
                Change::Gone(path) => {
                    let key = key_of(&path, false);
                    gone.push(key.clone());
                    small.push(Stored::Delete(key));
                    small.push(Stored::Delete(key_of(&path, true)));
                }
            }
        }
        if !small.is_empty() && idb::apply(&database, idb::FILES, small).await.is_err() {
            log::warn!("a change did not reach the browser's storage");
        }
        if !gone.is_empty() {
            for store in [idb::MEDIA, idb::MEDIA_INDEX] {
                let deletions = gone.iter().cloned().map(Stored::Delete).collect();
                if idb::apply(&database, store, deletions).await.is_err() {
                    log::warn!("a removed file stayed in the browser's storage");
                }
            }
        }
        for (path, bytes) in media {
            save_media(&database, &path, bytes).await;
        }
    }
}

/// Fills the store from IndexedDB, then starts saving changes to it. Files under
/// `media` are read as they are needed, and everything under `unsaved` stays in
/// memory only.
pub async fn hydrate(
    database: web_sys::IdbDatabase,
    media: PathBuf,
    unsaved: PathBuf,
) -> Result<()> {
    use crate::web::idb;
    use wasm_bindgen::JsCast as _;
    let stored = idb::all(&database, idb::FILES)
        .await
        .map_err(Error::other)?;
    let index = idb::all(&database, idb::MEDIA_INDEX)
        .await
        .map_err(Error::other)?;
    with(|store| {
        for (key, value) in stored {
            match key.strip_suffix(FOLDER_MARK) {
                Some(folder) => {
                    store.folders.insert(PathBuf::from(folder));
                }
                None => {
                    let bytes = js_sys::Uint8Array::new(&value).to_vec();
                    store.files.insert(PathBuf::from(key), Rc::new(bytes));
                }
            }
        }
        for (key, value) in index {
            // Older entries hold the length alone.
            let (length, written) = match value.dyn_ref::<js_sys::Array>() {
                Some(pair) => (pair.get(0).as_f64(), pair.get(1).as_f64()),
                None => (value.as_f64(), None),
            };
            let path = PathBuf::from(key);
            if let Some(written) = written {
                store.written.insert(path.clone(), written);
            }
            store.kept.insert(path, length.unwrap_or(0.0) as u64);
        }
    });
    SAVING.with_borrow_mut(|saving| {
        saving.database = Some(database);
        saving.media = media;
        saving.unsaved = unsaved;
    });
    Ok(())
}

/// Sets what to call when a file fetched in the background has arrived.
pub fn set_on_loaded(hook: impl Fn() + 'static) {
    ON_LOADED.with_borrow_mut(|slot| *slot = Some(Box::new(hook)));
}

/// Whether reading `path` would answer at once.
pub fn is_loaded(path: impl AsRef<Path>) -> bool {
    let path = clean(path.as_ref());
    with(|store| store.files.contains_key(&path) || !store.kept.contains_key(&path))
}

/// Brings a kept file into memory. A file that will not open is forgotten, so it
/// reads as missing and the app offers to download it again.
pub async fn ensure_loaded(path: impl AsRef<Path>) {
    use crate::web::idb;
    let path = clean(path.as_ref());
    if with(|store| store.files.contains_key(&path) || !store.kept.contains_key(&path)) {
        return;
    }
    let Some(database) = SAVING.with_borrow(|saving| saving.database.clone()) else {
        return;
    };
    let key = key_of(&path, false);
    let opened = match idb::get(&database, idb::MEDIA, &key).await {
        Ok(stored) if !stored.is_undefined() => {
            crate::web::keys::open(&js_sys::Uint8Array::new(&stored).to_vec())
                .await
                .ok()
        }
        _ => None,
    };
    with(|store| match opened {
        Some(bytes) => {
            store.files.entry(path.clone()).or_insert(Rc::new(bytes));
            store.hold(&path);
        }
        None => {
            log::warn!("a stored file could not be read back");
            store.forget(&path);
        }
    });
}

/// Fetches a kept file in the background, once however often it is asked for.
pub fn request_load(path: impl AsRef<Path>) {
    let path = clean(path.as_ref());
    if !LOADING.with_borrow_mut(|loading| loading.insert(path.clone())) {
        return;
    }
    wasm_bindgen_futures::spawn_local(async move {
        ensure_loaded(&path).await;
        LOADING.with_borrow_mut(|loading| {
            loading.remove(&path);
        });
        ON_LOADED.with_borrow(|hook| {
            if let Some(hook) = hook {
                hook();
            }
        });
    });
}

fn not_here_yet(path: &Path) -> Error {
    request_load(path);
    Error::new(ErrorKind::WouldBlock, "the file is on its way from storage")
}

fn is_database(path: &Path) -> bool {
    crate::web::sqlite::exists(&path.to_string_lossy())
}

/// The path with `.` and `..` resolved, the same however it was spelled.
fn clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for part in path.components() {
        match part {
            Component::Normal(name) => out.push(name),
            Component::ParentDir => {
                out.pop();
            }
            _ => {}
        }
    }
    out
}

fn missing() -> Error {
    Error::new(ErrorKind::NotFound, "no such file or folder")
}

impl Store {
    fn is_folder(&self, path: &Path) -> bool {
        path == Path::new("/") || self.folders.contains(path)
    }

    fn parent_exists(&self, path: &Path) -> bool {
        path.parent().is_none_or(|parent| self.is_folder(parent))
    }

    /// Everything beneath `path`, but not `path` itself.
    fn beneath(&self, path: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let under = |candidate: &&PathBuf| candidate.starts_with(path) && *candidate != path;
        (
            self.files
                .keys()
                .chain(self.kept.keys())
                .filter(under)
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            self.folders.iter().filter(under).cloned().collect(),
        )
    }
}

fn with<T>(act: impl FnOnce(&mut Store) -> T) -> T {
    STORE.with_borrow_mut(act)
}

pub fn read(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    let path = clean(path.as_ref());
    let found = with(|store| {
        let found = store.files.get(&path).map(|bytes| bytes.to_vec());
        if found.is_some() && store.kept.contains_key(&path) {
            store.hold(&path);
        }
        found
    });
    match found {
        Some(bytes) => Ok(bytes),
        None if with(|store| store.kept.contains_key(&path)) => Err(not_here_yet(&path)),
        None => Err(missing()),
    }
}

pub fn read_to_string(path: impl AsRef<Path>) -> Result<String> {
    String::from_utf8(read(path)?).map_err(|_| Error::new(ErrorKind::InvalidData, "not UTF-8"))
}

fn put(path: &Path, bytes: Vec<u8>) -> Result<()> {
    with(|store| {
        if store.is_folder(path) {
            return Err(Error::new(ErrorKind::IsADirectory, "that is a folder"));
        }
        if !store.parent_exists(path) {
            return Err(missing());
        }
        let bytes = Rc::new(bytes);
        store.files.insert(path.to_owned(), bytes.clone());
        store.written.insert(path.to_owned(), js_sys::Date::now());
        // Held until saved, under what the store knows of it.
        if is_media(path) {
            store.kept.remove(path);
            store.hold(path);
        }
        save(Change::File(path.to_owned(), bytes));
        Ok(())
    })
}

pub fn write(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> Result<()> {
    put(&clean(path.as_ref()), bytes.as_ref().to_vec())
}

pub fn create_dir_all(path: impl AsRef<Path>) -> Result<()> {
    let path = clean(path.as_ref());
    with(|store| {
        for folder in path.ancestors() {
            if store.files.contains_key(folder) {
                return Err(Error::new(ErrorKind::NotADirectory, "a file is in the way"));
            }
            if folder != Path::new("/") && store.folders.insert(folder.to_owned()) {
                save(Change::Folder(folder.to_owned()));
            }
        }
        Ok(())
    })
}

pub fn create_dir(path: impl AsRef<Path>) -> Result<()> {
    let path = clean(path.as_ref());
    with(|store| {
        if store.is_folder(&path) || store.files.contains_key(&path) {
            return Err(Error::new(ErrorKind::AlreadyExists, "already there"));
        }
        if !store.parent_exists(&path) {
            return Err(missing());
        }
        store.folders.insert(path.clone());
        save(Change::Folder(path));
        Ok(())
    })
}

pub fn remove_file(path: impl AsRef<Path>) -> Result<()> {
    let path = clean(path.as_ref());
    if is_database(&path) {
        return crate::web::sqlite::delete(&path.to_string_lossy());
    }
    let found = with(|store| {
        let kept = store.kept.contains_key(&path);
        let held = store.files.remove(&path).is_some();
        store.forget(&path);
        kept || held
    });
    if !found {
        return Err(missing());
    }
    save(Change::Gone(path));
    Ok(())
}

pub fn remove_dir_all(path: impl AsRef<Path>) -> Result<()> {
    let path = clean(path.as_ref());
    with(|store| {
        if !store.is_folder(&path) {
            return Err(missing());
        }
        let (files, folders) = store.beneath(&path);
        for file in files {
            store.files.remove(&file);
            store.forget(&file);
            save(Change::Gone(file));
        }
        for folder in folders {
            store.folders.remove(&folder);
            save(Change::Gone(folder));
        }
        store.folders.remove(&path);
        save(Change::Gone(path.clone()));
        Ok(())
    })?;
    for database in crate::web::sqlite::beneath(&path) {
        crate::web::sqlite::delete(&database)?;
    }
    Ok(())
}

pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<()> {
    let (from, to) = (clean(from.as_ref()), clean(to.as_ref()));
    if is_database(&from) {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "a database cannot be moved in a page",
        ));
    }
    with(|store| {
        if !store.parent_exists(&to) {
            return Err(missing());
        }
        if store.kept.contains_key(&from) && !store.files.contains_key(&from) {
            return Err(not_here_yet(&from));
        }
        if let Some(bytes) = store.files.remove(&from) {
            let written = store.written.get(&from).copied();
            store.forget(&from);
            store.files.insert(to.clone(), bytes.clone());
            if let Some(written) = written {
                store.written.insert(to.clone(), written);
            }
            save(Change::Gone(from));
            save(Change::File(to, bytes));
            return Ok(());
        }
        if !store.folders.contains(&from) {
            return Err(missing());
        }
        let (files, folders) = store.beneath(&from);
        // As for one file: what is only in storage moves once it is here, or it
        // would be deleted without arriving.
        let waiting: Vec<&PathBuf> = files
            .iter()
            .filter(|file| store.kept.contains_key(*file) && !store.files.contains_key(*file))
            .collect();
        if let Some((first, rest)) = waiting.split_first() {
            for file in rest {
                request_load(file);
            }
            return Err(not_here_yet(first));
        }
        for file in files {
            if let (Some(bytes), Ok(rest)) = (store.files.remove(&file), file.strip_prefix(&from)) {
                let written = store.written.get(&file).copied();
                store.forget(&file);
                store.files.insert(to.join(rest), bytes.clone());
                if let Some(written) = written {
                    store.written.insert(to.join(rest), written);
                }
                save(Change::File(to.join(rest), bytes));
            }
            save(Change::Gone(file));
        }
        for folder in folders {
            store.folders.remove(&folder);
            if let Ok(rest) = folder.strip_prefix(&from) {
                store.folders.insert(to.join(rest));
                save(Change::Folder(to.join(rest)));
            }
            save(Change::Gone(folder));
        }
        store.folders.remove(&from);
        save(Change::Gone(from));
        store.folders.insert(to.clone());
        save(Change::Folder(to));
        Ok(())
    })
}

pub fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<u64> {
    let bytes = read(from)?;
    let length = bytes.len() as u64;
    write(to, bytes)?;
    Ok(length)
}

/// Stored once: two names are the same bytes until one is replaced.
pub fn hard_link(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<()> {
    let to = clean(to.as_ref());
    if with(|store| store.files.contains_key(&to)) {
        return Err(Error::new(ErrorKind::AlreadyExists, "already there"));
    }
    copy(from, to).map(|_| ())
}

/// A page's files have no owners, so there is nothing to change.
pub fn set_permissions(_path: impl AsRef<Path>, _permissions: Permissions) -> Result<()> {
    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct Permissions;

#[derive(Clone, Copy, Debug)]
pub struct FileType {
    folder: bool,
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.folder
    }

    pub fn is_file(&self) -> bool {
        !self.folder
    }

    pub fn is_symlink(&self) -> bool {
        false
    }
}

#[derive(Clone, Debug)]
pub struct Metadata {
    length: u64,
    folder: bool,
    /// When it was written, in milliseconds since 1970, where known.
    written: Option<f64>,
}

impl Metadata {
    pub fn len(&self) -> u64 {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn is_file(&self) -> bool {
        !self.folder
    }

    pub fn is_dir(&self) -> bool {
        self.folder
    }

    pub fn is_symlink(&self) -> bool {
        false
    }

    pub fn file_type(&self) -> FileType {
        FileType {
            folder: self.folder,
        }
    }

    /// When the file was written, read off the browser's clock at the time. A
    /// file read back after a reload knows it only if it is media. std's own
    /// clock does not run in a page, so its age is not `elapsed()`: measure it
    /// against `crate::platform::SystemTime`.
    pub fn modified(&self) -> Result<std::time::SystemTime> {
        self.written
            .map(|written| std::time::UNIX_EPOCH + std::time::Duration::from_millis(written as u64))
            .ok_or_else(|| Error::new(ErrorKind::Unsupported, "no time is known for this file"))
    }

    pub fn permissions(&self) -> Permissions {
        Permissions
    }
}

pub fn metadata(path: impl AsRef<Path>) -> Result<Metadata> {
    let path = clean(path.as_ref());
    // SQLite's store does not say how long a database is; nothing here asks.
    if is_database(&path) {
        return Ok(Metadata {
            length: 1,
            folder: false,
            written: None,
        });
    }
    with(|store| {
        if store.is_folder(&path) {
            return Ok(Metadata {
                length: 0,
                folder: true,
                written: None,
            });
        }
        store
            .files
            .get(&path)
            .map(|bytes| bytes.len() as u64)
            .or_else(|| store.kept.get(&path).copied())
            .map(|length| Metadata {
                length,
                folder: false,
                written: store.written.get(&path).copied(),
            })
            .ok_or_else(missing)
    })
}

pub fn symlink_metadata(path: impl AsRef<Path>) -> Result<Metadata> {
    metadata(path)
}

pub struct DirEntry {
    path: PathBuf,
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    pub fn file_name(&self) -> OsString {
        self.path.file_name().unwrap_or_default().to_owned()
    }

    pub fn metadata(&self) -> Result<Metadata> {
        metadata(&self.path)
    }

    pub fn file_type(&self) -> Result<FileType> {
        Ok(self.metadata()?.file_type())
    }
}

pub struct ReadDir(std::vec::IntoIter<PathBuf>);

impl Iterator for ReadDir {
    type Item = Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|path| Ok(DirEntry { path }))
    }
}

pub fn read_dir(path: impl AsRef<Path>) -> Result<ReadDir> {
    let path = clean(path.as_ref());
    with(|store| {
        if !store.is_folder(&path) {
            return Err(missing());
        }
        let (files, folders) = store.beneath(&path);
        let children: Vec<PathBuf> = files
            .into_iter()
            .chain(folders)
            .filter(|child| child.parent() == Some(path.as_path()))
            .collect();
        Ok(ReadDir(children.into_iter()))
    })
}

#[derive(Default)]
pub struct DirBuilder {
    recursive: bool,
}

impl DirBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn recursive(&mut self, recursive: bool) -> &mut Self {
        self.recursive = recursive;
        self
    }

    pub fn create(&self, path: impl AsRef<Path>) -> Result<()> {
        if self.recursive {
            create_dir_all(path)
        } else {
            create_dir(path)
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

impl OpenOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read(&mut self, on: bool) -> &mut Self {
        self.read = on;
        self
    }

    pub fn write(&mut self, on: bool) -> &mut Self {
        self.write = on;
        self
    }

    pub fn append(&mut self, on: bool) -> &mut Self {
        self.append = on;
        self
    }

    pub fn truncate(&mut self, on: bool) -> &mut Self {
        self.truncate = on;
        self
    }

    pub fn create(&mut self, on: bool) -> &mut Self {
        self.create = on;
        self
    }

    pub fn create_new(&mut self, on: bool) -> &mut Self {
        self.create_new = on;
        self
    }

    pub fn open(&self, path: impl AsRef<Path>) -> Result<File> {
        let path = clean(path.as_ref());
        let existing = with(|store| store.files.get(&path).cloned());
        let is_folder = with(|store| store.is_folder(&path));
        // Replacing a file needs nothing of the old one; anything else does.
        if existing.is_none()
            && !(self.truncate && (self.write || self.append))
            && with(|store| store.kept.contains_key(&path))
        {
            if self.create_new {
                return Err(Error::new(ErrorKind::AlreadyExists, "already there"));
            }
            return Err(not_here_yet(&path));
        }
        if is_folder {
            return Ok(File {
                path,
                data: Cursor::new(Vec::new()),
                writable: false,
                dirty: false,
            });
        }
        let writes = self.write || self.append;
        let data = match existing {
            Some(_) if self.create_new => {
                return Err(Error::new(ErrorKind::AlreadyExists, "already there"));
            }
            Some(_) if self.truncate && writes => Vec::new(),
            Some(bytes) => bytes.to_vec(),
            None if self.create || self.create_new => Vec::new(),
            None => return Err(missing()),
        };
        let mut file = File {
            path,
            data: Cursor::new(data),
            writable: writes,
            dirty: self.truncate || self.create || self.create_new,
        };
        if self.append {
            file.data.seek(SeekFrom::End(0))?;
        }
        if file.dirty {
            file.save()?;
        }
        Ok(file)
    }
}

/// An open file: its bytes in hand, put back when written and closed.
#[derive(Debug)]
pub struct File {
    path: PathBuf,
    data: Cursor<Vec<u8>>,
    writable: bool,
    dirty: bool,
}

impl File {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        OpenOptions::new().read(true).open(path)
    }

    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
    }

    pub fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            length: self.data.get_ref().len() as u64,
            folder: false,
            written: with(|store| store.written.get(&self.path).copied()),
        })
    }

    pub fn set_len(&mut self, length: u64) -> Result<()> {
        self.data.get_mut().resize(length as usize, 0);
        self.dirty = true;
        Ok(())
    }

    pub fn sync_all(&self) -> Result<()> {
        Ok(())
    }

    pub fn sync_data(&self) -> Result<()> {
        Ok(())
    }

    fn save(&mut self) -> Result<()> {
        self.dirty = false;
        put(&self.path, self.data.get_ref().clone())
    }
}

impl Read for File {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.data.read(buffer)
    }
}

impl Seek for File {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.data.seek(position)
    }
}

impl Write for File {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if !self.writable {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "opened for reading",
            ));
        }
        self.dirty = true;
        self.data.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.dirty {
            self.save()?;
        }
        Ok(())
    }
}

impl Drop for File {
    fn drop(&mut self) {
        if self.dirty {
            let _ = self.save();
        }
    }
}

impl whatsapp_rust::wacore::download::DownloadWriter for File {
    fn truncate(&mut self, len: u64) -> Result<()> {
        self.set_len(len)
    }
}

/// The path as the store files it, once it is known to exist.
pub fn canonicalize(path: impl AsRef<Path>) -> Result<PathBuf> {
    metadata(path.as_ref())?;
    Ok(clean(path.as_ref()))
}
