//! `std::fs`'s names in a page, where std's own always fails.
//!
//! Files live in memory on the page's one thread, with the behaviour the app
//! relies on: a write needs its folder, a rename replaces, a folder lists its
//! children. Nothing here outlives the page yet.

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
}

thread_local! {
    static STORE: RefCell<Store> = RefCell::new(Store::default());
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
            self.files.keys().filter(under).cloned().collect(),
            self.folders.iter().filter(under).cloned().collect(),
        )
    }
}

fn with<T>(act: impl FnOnce(&mut Store) -> T) -> T {
    STORE.with_borrow_mut(act)
}

pub fn read(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    let path = clean(path.as_ref());
    with(|store| store.files.get(&path).map(|bytes| bytes.to_vec())).ok_or_else(missing)
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
        store.files.insert(path.to_owned(), Rc::new(bytes));
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
            if folder != Path::new("/") {
                store.folders.insert(folder.to_owned());
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
        store.folders.insert(path);
        Ok(())
    })
}

pub fn remove_file(path: impl AsRef<Path>) -> Result<()> {
    let path = clean(path.as_ref());
    with(|store| store.files.remove(&path).map(|_| ())).ok_or_else(missing)
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
        }
        for folder in folders {
            store.folders.remove(&folder);
        }
        store.folders.remove(&path);
        Ok(())
    })
}

pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<()> {
    let (from, to) = (clean(from.as_ref()), clean(to.as_ref()));
    with(|store| {
        if !store.parent_exists(&to) {
            return Err(missing());
        }
        if let Some(bytes) = store.files.remove(&from) {
            store.files.insert(to, bytes);
            return Ok(());
        }
        if !store.folders.contains(&from) {
            return Err(missing());
        }
        let (files, folders) = store.beneath(&from);
        for file in files {
            if let (Some(bytes), Ok(rest)) = (store.files.remove(&file), file.strip_prefix(&from)) {
                store.files.insert(to.join(rest), bytes);
            }
        }
        for folder in folders {
            store.folders.remove(&folder);
            if let Ok(rest) = folder.strip_prefix(&from) {
                store.folders.insert(to.join(rest));
            }
        }
        store.folders.remove(&from);
        store.folders.insert(to);
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

    /// No clock is kept for files, so anything that compares ages sees none.
    pub fn modified(&self) -> Result<std::time::SystemTime> {
        Err(Error::new(ErrorKind::Unsupported, "no file times here"))
    }

    pub fn permissions(&self) -> Permissions {
        Permissions
    }
}

pub fn metadata(path: impl AsRef<Path>) -> Result<Metadata> {
    let path = clean(path.as_ref());
    with(|store| {
        if store.is_folder(&path) {
            return Ok(Metadata {
                length: 0,
                folder: true,
            });
        }
        store
            .files
            .get(&path)
            .map(|bytes| Metadata {
                length: bytes.len() as u64,
                folder: false,
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
