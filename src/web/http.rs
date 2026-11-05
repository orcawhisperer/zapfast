//! The library's HTTP through the browser's `fetch`.

use anyhow::{Result, anyhow};
use wasm_bindgen::JsCast;
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

/// One request and its whole answer, including 4xx and 5xx.
pub async fn execute(request: &HttpRequest) -> Result<HttpResponse> {
    let init = RequestInit::new();
    init.set_method(&request.method);
    init.set_mode(RequestMode::Cors);
    // The core checks a media host before asking and cannot check a redirect.
    init.set_redirect(RequestRedirect::Error);
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
