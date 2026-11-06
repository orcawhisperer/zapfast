//! The library's HTTP through the browser's `fetch`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Request, RequestInit, RequestMode, RequestRedirect, Response};
use whatsapp_rust::wacore::net::{HttpClient, HttpRequest, HttpResponse};

/// Asks the browser. Works from the page and from a worker.
pub struct FetchClient;

/// `fetch` from whichever global scope this code runs in.
fn fetch(request: &Request) -> Result<js_sys::Promise> {
    let global = js_sys::global();
    if let Some(window) = global.dyn_ref::<web_sys::Window>() {
        return Ok(window.fetch_with_request(request));
    }
    if let Some(worker) = global.dyn_ref::<web_sys::WorkerGlobalScope>() {
        return Ok(worker.fetch_with_request(request));
    }
    Err(anyhow!("no fetch in this scope"))
}

/// What wants to hear how a transfer is going.
#[derive(Default)]
struct Watchers {
    next: u64,
    /// By the path a download's address holds.
    downloads: Vec<(u64, String, Rc<dyn Fn(u64)>)>,
    /// By the length of the file being uploaded. The callback says whether to go on.
    uploads: Vec<(u64, u64, Rc<dyn Fn(u64, u64) -> bool>)>,
}

thread_local! {
    static WATCHERS: RefCell<Watchers> = RefCell::new(Watchers::default());
}

/// How much longer than its file an encrypted upload can be: a MAC and padding.
const ENCRYPTION_OVERHEAD: u64 = 32;

/// Stops watching when the transfer's owner is done.
pub struct Watch(u64);

impl Drop for Watch {
    fn drop(&mut self) {
        WATCHERS.with_borrow_mut(|watchers| {
            watchers.downloads.retain(|(id, ..)| *id != self.0);
            watchers.uploads.retain(|(id, ..)| *id != self.0);
        });
    }
}

/// Reports how many bytes have arrived of the download whose path is `direct_path`.
/// The library builds the address itself, and the path is what stays recognisable.
pub fn watch_download(direct_path: &str, report: impl Fn(u64) + 'static) -> Watch {
    let path = direct_path
        .split('?')
        .next()
        .unwrap_or(direct_path)
        .to_owned();
    WATCHERS.with_borrow_mut(|watchers| {
        watchers.next += 1;
        watchers
            .downloads
            .push((watchers.next, path, Rc::new(report)));
        Watch(watchers.next)
    })
}

/// Reports how much has gone of the upload of a file `length` bytes long. Returning
/// `false` from `report` aborts it. The library encrypts the file itself, so its
/// length is what identifies the request.
pub fn watch_upload(length: u64, report: impl Fn(u64, u64) -> bool + 'static) -> Watch {
    WATCHERS.with_borrow_mut(|watchers| {
        watchers.next += 1;
        watchers
            .uploads
            .push((watchers.next, length, Rc::new(report)));
        Watch(watchers.next)
    })
}

/// Sends a body with `XMLHttpRequest`, the one way a page hears how much has gone.
async fn upload_watched(
    request: &HttpRequest,
    body: &[u8],
    report: Rc<dyn Fn(u64, u64) -> bool>,
) -> Result<HttpResponse> {
    use web_sys::{Event, ProgressEvent, XmlHttpRequest, XmlHttpRequestResponseType};
    let refused = || anyhow!("the browser refused the upload");
    let xhr = XmlHttpRequest::new().map_err(|_| refused())?;
    xhr.open(&request.method, &request.url)
        .map_err(|_| refused())?;
    xhr.set_response_type(XmlHttpRequestResponseType::Arraybuffer);
    for (name, value) in &request.headers {
        // The browser drops headers a page may not set.
        let _ = xhr.set_request_header(name, value);
    }
    let (answer, answered) = oneshot::channel::<bool>();
    let answer = Rc::new(Cell::new(Some(answer)));
    let settle = |done: bool| {
        let answer = answer.clone();
        Closure::<dyn FnMut(Event)>::new(move |_| {
            if let Some(answer) = answer.take() {
                let _ = answer.send(done);
            }
        })
    };
    let (loaded, failed) = (settle(true), settle(false));
    let progress = {
        let xhr = xhr.clone();
        Closure::<dyn FnMut(ProgressEvent)>::new(move |event: ProgressEvent| {
            if !report(event.loaded() as u64, event.total() as u64) {
                let _ = xhr.abort();
            }
        })
    };
    xhr.set_onload(Some(loaded.as_ref().unchecked_ref()));
    xhr.set_onerror(Some(failed.as_ref().unchecked_ref()));
    xhr.set_onabort(Some(failed.as_ref().unchecked_ref()));
    xhr.upload()
        .map_err(|_| refused())?
        .set_onprogress(Some(progress.as_ref().unchecked_ref()));
    xhr.send_with_opt_u8_array(Some(body))
        .map_err(|_| refused())?;
    let done = answered.await.unwrap_or(false);
    if !done {
        return Err(anyhow!("the upload stopped"));
    }
    // XMLHttpRequest cannot refuse a redirect as fetch does, so one is caught here:
    // the core checks a media host before asking and cannot check another.
    if xhr.response_url() != request.url {
        return Err(anyhow!("the upload was redirected"));
    }
    let status = xhr.status().map_err(|_| refused())?;
    let response = xhr.response().map_err(|_| refused())?;
    Ok(HttpResponse::new(
        status,
        js_sys::Uint8Array::new(&response).to_vec(),
    ))
}

/// Reads an answer as it arrives, telling `report` how much there is so far.
async fn read_watched(response: &Response, report: &dyn Fn(u64)) -> Result<Vec<u8>> {
    let cut = || anyhow!("could not read the response");
    let reader: web_sys::ReadableStreamDefaultReader = response
        .body()
        .ok_or_else(cut)?
        .get_reader()
        .dyn_into()
        .map_err(|_| cut())?;
    let mut body = Vec::new();
    loop {
        let part = JsFuture::from(reader.read()).await.map_err(|_| cut())?;
        let finished = js_sys::Reflect::get(&part, &"done".into()).map_err(|_| cut())?;
        if finished.is_truthy() {
            return Ok(body);
        }
        let chunk = js_sys::Reflect::get(&part, &"value".into()).map_err(|_| cut())?;
        body.extend_from_slice(&js_sys::Uint8Array::new(&chunk).to_vec());
        report(body.len() as u64);
    }
}

/// Fetches a public address, following any redirect, for what the library's checks
/// do not cover, such as GIF search.
pub async fn fetch_public(url: &str) -> Result<HttpResponse> {
    send(&HttpRequest::get(url), RequestRedirect::Follow).await
}

/// One request and its whole answer, including 4xx and 5xx. A redirect is refused,
/// since the core checks a media host before asking and cannot check another.
pub async fn execute(request: &HttpRequest) -> Result<HttpResponse> {
    send(request, RequestRedirect::Error).await
}

async fn send(request: &HttpRequest, redirect: RequestRedirect) -> Result<HttpResponse> {
    if let Some(body) = &request.body {
        let length = body.len() as u64;
        let watcher = WATCHERS.with_borrow(|watchers| {
            watchers
                .uploads
                .iter()
                .find(|(_, file, _)| (*file..=*file + ENCRYPTION_OVERHEAD).contains(&length))
                .map(|(_, _, report)| report.clone())
        });
        if let Some(report) = watcher {
            return upload_watched(request, body, report).await;
        }
    }
    let init = RequestInit::new();
    init.set_method(&request.method);
    init.set_mode(RequestMode::Cors);
    init.set_redirect(redirect);
    if let Some(body) = &request.body {
        init.set_body(&js_sys::Uint8Array::from(body.as_ref()));
    }
    let web_request = Request::new_with_str_and_init(&request.url, &init)
        .map_err(|_| anyhow!("the browser refused the request"))?;
    for (name, value) in &request.headers {
        // The browser drops headers a page may not set, such as User-Agent.
        let _ = web_request.headers().set(name, value);
    }
    let response: Response = JsFuture::from(fetch(&web_request)?)
        .await
        .map_err(|_| anyhow!("the request failed, or the server does not allow this page"))?
        .dyn_into()
        .map_err(|_| anyhow!("fetch answered something else"))?;
    let watcher = WATCHERS.with_borrow(|watchers| {
        watchers
            .downloads
            .iter()
            .find(|(_, path, _)| request.url.contains(path.as_str()))
            .map(|(_, _, report)| report.clone())
    });
    if let Some(report) = watcher {
        let body = read_watched(&response, &*report).await?;
        return Ok(HttpResponse::new(response.status(), body));
    }
    let buffer = JsFuture::from(
        response
            .array_buffer()
            .map_err(|_| anyhow!("could not read the response"))?,
    )
    .await
    .map_err(|_| anyhow!("could not read the response"))?;
    Ok(HttpResponse::new(
        response.status(),
        js_sys::Uint8Array::new(&buffer).to_vec(),
    ))
}

#[async_trait::async_trait(?Send)]
impl HttpClient for FetchClient {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
        execute(&request).await
    }
}
