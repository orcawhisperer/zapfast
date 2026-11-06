//! The browser's notifications, the sound beside them, and the unread count.

use std::path::Path;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{Event, HtmlAudioElement, Notification, NotificationOptions, NotificationPermission};

/// Shows a notification, or `None` when the person has not allowed them or the
/// browser only offers them through a service worker, as on Android.
pub fn show(
    title: &str,
    body: &str,
    picture: Option<&Path>,
    silent: bool,
    tag: &str,
    clicked: impl FnOnce() + 'static,
) -> Option<Notification> {
    if Notification::permission() != NotificationPermission::Granted {
        return None;
    }
    let options = NotificationOptions::new();
    options.set_body(body);
    options.set_tag(tag);
    options.set_silent(Some(silent));
    if let Some(url) = picture
        .and_then(|path| crate::web::sfs::read(path).ok())
        .and_then(|bytes| crate::web::open::temporary_url(&bytes).ok())
    {
        options.set_icon(&url);
    }
    let notification = Notification::new_with_options(title, &options).ok()?;
    let on_click = Closure::once_into_js(move |_: Event| {
        if let Some(window) = web_sys::window() {
            let _ = window.focus();
        }
        clicked();
    });
    notification.set_onclick(Some(on_click.unchecked_ref()));
    Some(notification)
}

/// Asks for permission on the person's first click: browsers refuse to ask any
/// other way, and a prompt out of nowhere is what people learn to block.
pub fn ask_on_first_click() {
    if Notification::permission() != NotificationPermission::Default {
        return;
    }
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return;
    };
    let ask = Closure::once_into_js(move |_: Event| {
        if let Ok(asked) = Notification::request_permission() {
            wasm_bindgen_futures::spawn_local(async move {
                let _ = wasm_bindgen_futures::JsFuture::from(asked).await;
            });
        }
    });
    let options = web_sys::AddEventListenerOptions::new();
    options.set_once(true);
    let _ = document.add_event_listener_with_callback_and_add_event_listener_options(
        "pointerup",
        ask.unchecked_ref(),
        &options,
    );
}

/// Plays a short sound. The browser may refuse until the person has clicked.
pub fn play(bytes: &[u8]) {
    let Ok(url) = crate::web::open::temporary_url(bytes) else {
        return;
    };
    let Ok(audio) = HtmlAudioElement::new_with_src(&url) else {
        return;
    };
    if let Ok(playing) = audio.play() {
        wasm_bindgen_futures::spawn_local(async move {
            if wasm_bindgen_futures::JsFuture::from(playing).await.is_err() {
                log::debug!("the browser did not play a notification sound");
            }
        });
    }
}

/// Shows the unread count in the tab's title, and on the icon once installed.
pub fn show_unread(count: u32) {
    let Some(window) = web_sys::window() else {
        return;
    };
    if let Some(document) = window.document() {
        document.set_title(&match count {
            0 => "ZapFast".to_owned(),
            count => format!("({count}) ZapFast"),
        });
    }
    // Not in every browser, and not in web-sys's stable set.
    let navigator: JsValue = window.navigator().into();
    if let Ok(set) = js_sys::Reflect::get(&navigator, &"setAppBadge".into())
        && let Ok(set) = set.dyn_into::<js_sys::Function>()
    {
        let _ = set.call1(&navigator, &JsValue::from_f64(f64::from(count)));
    }
}
