//! Files coming into the page: the browser's picker, drops, and pasted pictures.
//!
//! A browser hands over a file's bytes, never its path, so each is copied into the
//! page's files and the app carries on with a path as it does on the desktop.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use futures::channel::oneshot;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{ClipboardEvent, Event, File, FileList, HtmlInputElement};

use crate::web::sfs;

/// Where incoming files are kept. Under the cache, so they are never saved.
const INCOMING: &str = "/zapfast/cache/incoming";

/// The largest notification sound kept: about a minute of good MP3.
const LARGEST_SOUND: usize = 8 * 1024 * 1024;

/// The hidden input the picker opens from. Some browsers only open an attached one.
const INPUT: &str = "zapfast-picker";

thread_local! {
    static NEXT: Cell<u64> = const { Cell::new(0) };
    /// Dropped and pasted files, waiting for the next frame to attach them.
    static ARRIVED: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
}

/// A file's name with no way to climb out of its folder.
fn safe_name(name: &str) -> String {
    let name = name.rsplit(['/', '\\']).next().unwrap_or_default();
    match name.trim() {
        "" | "." | ".." => "file".to_owned(),
        name => name.to_owned(),
    }
}

/// Files `bytes` under `name`, in a folder of its own so names cannot collide.
fn keep(name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    let turn = NEXT.get();
    NEXT.set(turn + 1);
    let folder = Path::new(INCOMING).join(turn.to_string());
    sfs::create_dir_all(&folder)?;
    let path = folder.join(safe_name(name));
    sfs::write(&path, bytes)?;
    Ok(path)
}

async fn keep_file(file: &File) -> Option<PathBuf> {
    let buffer = JsFuture::from(file.array_buffer()).await.ok()?;
    let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
    match keep(&file.name(), &bytes) {
        Ok(path) => Some(path),
        Err(error) => {
            log::warn!("could not keep an incoming file: {error}");
            None
        }
    }
}

async fn keep_files(files: Option<FileList>) -> Vec<PathBuf> {
    let Some(files) = files else {
        return Vec::new();
    };
    let mut kept = Vec::new();
    for index in 0..files.length() {
        if let Some(file) = files.get(index)
            && let Some(path) = keep_file(&file).await
        {
            kept.push(path);
        }
    }
    kept
}

fn input() -> Option<HtmlInputElement> {
    let document = web_sys::window()?.document()?;
    let input: HtmlInputElement = match document.get_element_by_id(INPUT) {
        Some(input) => input.dyn_into().ok()?,
        None => {
            let input: HtmlInputElement = document.create_element("input").ok()?.dyn_into().ok()?;
            input.set_id(INPUT);
            input.set_type("file");
            input.style().set_property("display", "none").ok()?;
            document.body()?.append_child(&input).ok()?;
            input
        }
    };
    Some(input)
}

/// Opens the browser's picker, for the extensions in `accept` or for anything when it
/// is empty. Nothing is chosen when it is cancelled. A browser opens it only soon
/// after a click.
async fn ask(multiple: bool, accept: &str) -> Option<FileList> {
    let input = input()?;
    input.set_multiple(multiple);
    input.set_accept(accept);
    // Choosing the same file again must still count as a change.
    input.set_value("");
    let (answer, answered) = oneshot::channel::<()>();
    let answer = Rc::new(Cell::new(Some(answer)));
    let settled = Closure::<dyn FnMut(Event)>::new(move |_| {
        if let Some(answer) = answer.take() {
            let _ = answer.send(());
        }
    });
    input.set_onchange(Some(settled.as_ref().unchecked_ref()));
    let _ = input.add_event_listener_with_callback("cancel", settled.as_ref().unchecked_ref());
    input.click();
    let _ = answered.await;
    input.set_onchange(None);
    let _ = input.remove_event_listener_with_callback("cancel", settled.as_ref().unchecked_ref());
    input.files()
}

/// Opens the browser's picker and returns what was chosen, once copied in.
pub async fn pick_files() -> Vec<PathBuf> {
    keep_files(ask(true, "").await).await
}

/// Asks for one file, among the extensions in `accept`, and returns it once copied in.
pub async fn pick_file(accept: &str) -> Option<PathBuf> {
    keep_files(ask(false, accept).await)
        .await
        .into_iter()
        .next()
}

/// Keeps a picked notification sound among the files that outlive a reload, as the
/// ones just picked do not. It is read back at every start, so it has to stay small.
pub fn keep_sound(path: &Path, state: &Path) -> Result<PathBuf, String> {
    let bytes = sfs::read(path).map_err(|error| error.to_string())?;
    if bytes.len() > LARGEST_SOUND {
        return Err("This file is too long for a notification sound".to_owned());
    }
    let folder = state.join("sounds");
    sfs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let kept = folder.join(safe_name(&name));
    sfs::write(&kept, bytes).map_err(|error| error.to_string())?;
    Ok(kept)
}

/// The files dropped or pasted since the last frame.
pub fn take_arrived() -> Vec<PathBuf> {
    ARRIVED.with_borrow_mut(std::mem::take)
}

/// Copies in a file dropped on the window, for a later frame to attach.
pub fn receive_drop(file: Arc<dyn egui::DroppedFile>, ctx: egui::Context) {
    wasm_bindgen_futures::spawn_local(async move {
        let name = file.path().to_string_lossy().into_owned();
        match file.bytes_async().await {
            Ok(bytes) => match keep(&name, &bytes) {
                Ok(path) => ARRIVED.with_borrow_mut(|arrived| arrived.push(path)),
                Err(error) => log::warn!("could not keep a dropped file: {error}"),
            },
            Err(_) => log::warn!("the browser could not read a dropped file"),
        }
        ctx.request_repaint();
    });
}

/// Listens for files and pictures pasted anywhere on the page. Pasted text is
/// left to egui, which has its own listener.
pub fn listen_for_pastes(ctx: egui::Context) {
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return;
    };
    let listener = Closure::<dyn FnMut(ClipboardEvent)>::new(move |event: ClipboardEvent| {
        let files = event.clipboard_data().and_then(|data| data.files());
        if files.as_ref().is_none_or(|files| files.length() == 0) {
            return;
        }
        let ctx = ctx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let kept = keep_files(files).await;
            ARRIVED.with_borrow_mut(|arrived| arrived.extend(kept));
            ctx.request_repaint();
        });
    });
    let target: &JsValue = listener.as_ref();
    let _ = document.add_event_listener_with_callback("paste", target.unchecked_ref());
    // Lives as long as the page.
    listener.forget();
}
