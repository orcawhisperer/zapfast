//! Starts ZapFast on the page's canvas.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use wasm_bindgen::JsCast;

use crate::app::App;
use crate::paths::AppDirs;
use crate::settings::Settings;

/// The root of the page's files, a path only ZapFast's own storage ever reads.
const ROOT: &str = "/zapfast";

/// How often a hidden tab looks for work with no wake to prompt it. Browsers slow
/// a hidden page's timers further on their own.
const HIDDEN_TICK_MS: u32 = 5_000;

/// The canvas `index.html` provides.
const CANVAS: &str = "zapfast";

/// Runs the app until the page goes away.
pub fn start() {
    eframe::WebLogger::init(log::LevelFilter::Info).ok();
    crate::web::runtime::install_clocks();
    wasm_bindgen_futures::spawn_local(async {
        if let Err(error) = run().await {
            log::error!("ZapFast could not start: {error}");
            show_failure(&error);
        }
    });
}

/// Whether the address asks for the sample chats instead of an account.
#[cfg(feature = "demo")]
fn wants_demo() -> bool {
    web_sys::window()
        .and_then(|window| window.location().search().ok())
        .is_some_and(|search| {
            search
                .trim_start_matches('?')
                .split('&')
                .any(|part| part == "demo")
        })
}

/// A short tone and clips for the sample voice messages and videos, whose files the
/// desktop's demo does not need: here they show the players work without an account.
#[cfg(feature = "demo")]
fn give_samples_a_voice(app: &mut App) {
    const RATE: u32 = 16_000;
    const SECONDS: u32 = 4;
    let samples: Vec<f32> = (0..RATE * SECONDS)
        .map(|index| {
            let at = f64::from(index) / f64::from(RATE);
            let fade = (at * std::f64::consts::PI / f64::from(SECONDS)).sin();
            ((at * 330.0 * std::f64::consts::TAU).sin() * fade * 12_000.0 / 32_767.0) as f32
        })
        .collect();
    let wav = crate::pcm::wav(&samples, RATE);
    let voice = app.dirs.media_cache_dir().join("sample-voice.wav");
    let film = app.dirs.media_cache_dir().join("sample-film.mp4");
    let note = app.dirs.media_cache_dir().join("sample-note.mp4");
    if voice
        .parent()
        .is_none_or(|folder| crate::web::sfs::create_dir_all(folder).is_ok())
        && crate::web::sfs::write(&voice, wav).is_ok()
        && crate::web::sfs::write(&film, include_bytes!("../../assets/demo/videos/launch.mp4"))
            .is_ok()
        && crate::web::sfs::write(&note, include_bytes!("../../assets/demo/videos/note.mp4"))
            .is_ok()
    {
        for conversation in app.conversations.values_mut() {
            for message in &mut conversation.messages {
                match &mut message.content {
                    crate::model::Content::Audio { media, .. } => {
                        media.path = Some(voice.clone());
                    }
                    crate::model::Content::Video {
                        media, note: round, ..
                    } => {
                        media.path = Some(if *round { note.clone() } else { film.clone() });
                    }
                    _ => {}
                }
            }
        }
    }
}

fn make_app(ctx: &egui::Context, waker: &fastframe_shell::Waker) -> Result<App, String> {
    let dirs = AppDirs::under(std::path::Path::new(ROOT));
    dirs.ensure().map_err(|error| error.to_string())?;
    let settings = Settings::load(&dirs.settings_file());
    #[cfg(feature = "demo")]
    if wants_demo() {
        let mut app = App::headless(dirs, settings).0;
        crate::demo::populate(&mut app);
        give_samples_a_voice(&mut app);
        app.attach_waker(ctx);
        app.attach(ctx);
        return Ok(app);
    }
    let mut app = App::new(
        waker,
        dirs,
        settings,
        crate::app::AppOptions { tray: false },
    )
    .map_err(|error| error.to_string())?;
    app.attach(ctx);
    Ok(app)
}

/// Holds the page's claim on the storage for as long as it lives. Two tabs on
/// the same databases would corrupt them, and split one Signal session in two.
async fn claim_storage() -> Result<(), String> {
    use wasm_bindgen::closure::Closure;
    let window = web_sys::window().ok_or("no window")?;
    if !window.is_secure_context() {
        return Err("ZapFast needs a secure page: open it over HTTPS.".to_owned());
    }
    let navigator = window.navigator();
    let locks = js_sys::Reflect::get(&navigator, &"locks".into())
        .ok()
        .filter(|locks| !locks.is_undefined())
        .ok_or("This browser cannot keep ZapFast to one tab.")?;
    let (answer, answered) = futures::channel::oneshot::channel::<bool>();
    let answer = std::cell::Cell::new(Some(answer));
    let holder = Closure::<dyn FnMut(wasm_bindgen::JsValue) -> js_sys::Promise>::new(
        move |lock: wasm_bindgen::JsValue| {
            if let Some(answer) = answer.take() {
                let _ = answer.send(!lock.is_null());
            }
            // Never settles, so the lock is held until the page goes away.
            js_sys::Promise::new(&mut |_, _| {})
        },
    );
    let options = js_sys::Object::new();
    js_sys::Reflect::set(&options, &"ifAvailable".into(), &true.into())
        .map_err(|_| "could not ask for the tab's lock")?;
    let request = js_sys::Reflect::get(&locks, &"request".into())
        .ok()
        .and_then(|request| request.dyn_into::<js_sys::Function>().ok())
        .ok_or("This browser cannot keep ZapFast to one tab.")?;
    request
        .call3(
            &locks,
            &"zapfast".into(),
            &options,
            holder.as_ref().unchecked_ref(),
        )
        .map_err(|_| "could not ask for the tab's lock")?;
    holder.forget();
    if answered.await.unwrap_or(false) {
        Ok(())
    } else {
        Err("ZapFast is already open in another tab.".to_owned())
    }
}

/// Asks the browser not to clear ZapFast's storage when space runs low. It may
/// say no, and may ask the person.
fn ask_to_persist() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(promise) = window.navigator().storage().persist() else {
        return;
    };
    wasm_bindgen_futures::spawn_local(async move {
        let granted = wasm_bindgen_futures::JsFuture::from(promise)
            .await
            .is_ok_and(|granted| granted.is_truthy());
        if !granted {
            log::warn!("the browser may clear ZapFast's storage if space runs low");
        }
    });
}

/// Registers the installed app's service worker. Only worth a warning: without it
/// the page still runs, and cannot be installed or start offline.
fn register_service_worker() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let promise = window
        .navigator()
        .service_worker()
        .register("service-worker.js");
    wasm_bindgen_futures::spawn_local(async move {
        if wasm_bindgen_futures::JsFuture::from(promise).await.is_err() {
            log::warn!("could not register the service worker");
        }
    });
}

/// Loads everything the app reads without waiting: databases, keys and settings.
async fn prepare_storage() -> Result<(), String> {
    claim_storage().await?;
    ask_to_persist();
    register_service_worker();
    crate::web::notify::ask_on_first_click();
    crate::web::sqlite::install().await?;
    let database = crate::web::idb::open().await?;
    crate::web::keys::load(database.clone())
        .await
        .map_err(|error| format!("{error:#}"))?;
    let cache = AppDirs::under(std::path::Path::new(ROOT)).cache;
    let unsaved = cache.join("incoming");
    crate::web::sfs::hydrate(database, cache, unsaved)
        .await
        .map_err(|error| error.to_string())
}

async fn run() -> Result<(), String> {
    #[cfg(feature = "demo")]
    let storage = !wants_demo();
    #[cfg(not(feature = "demo"))]
    let storage = true;
    if storage {
        prepare_storage().await?;
    }
    let document = web_sys::window()
        .and_then(|window| window.document())
        .ok_or("no document")?;
    let canvas = document
        .get_element_by_id(CANVAS)
        .ok_or("the page has no canvas")?
        .dyn_into::<web_sys::HtmlCanvasElement>()
        .map_err(|_| "the canvas is something else")?;
    let waker = fastframe_shell::Waker::default();
    eframe::WebRunner::new()
        .start(
            canvas,
            eframe::WebOptions::default(),
            Box::new(move |cc| {
                waker.attach(&cc.egui_ctx);
                crate::web::picker::listen_for_pastes(cc.egui_ctx.clone());
                let repaint = cc.egui_ctx.clone();
                crate::web::sfs::set_on_loaded(move || repaint.request_repaint());
                cc.egui_ctx
                    .add_bytes_loader(std::sync::Arc::new(crate::web::images::PageFiles));
                let app = Rc::new(RefCell::new(make_app(&cc.egui_ctx, &waker)?));
                work_while_hidden(&app, &cc.egui_ctx);
                hide_loading_note();
                Ok(Box::new(Page {
                    app,
                    shown_unread: None,
                }))
            }),
        )
        .await
        .map_err(|error| format!("{error:?}"))
}

/// Says why nothing appeared, in the page itself.
fn show_failure(reason: &str) {
    hide_loading_note();
    let element = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id("failure"));
    if let Some(element) = element {
        element.set_text_content(Some(reason));
    }
}

/// Removes what the page shows while the program downloads and starts.
fn hide_loading_note() {
    let element = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id("loading"));
    if let Some(element) = element {
        element.remove();
    }
}

/// Whether the person is looking at the page right now. eframe only counts the
/// canvas itself holding focus, which is not the same.
fn page_in_focus() -> bool {
    web_sys::window()
        .and_then(|window| window.document())
        .is_some_and(|document| !document.hidden() && document.has_focus().unwrap_or(false))
}

fn page_hidden() -> bool {
    web_sys::window()
        .and_then(|window| window.document())
        .is_some_and(|document| document.hidden())
}

/// Keeps the app working while the tab is hidden, when the browser stops drawing
/// frames: on each wake from the worker, and on a slow timer for what is due anyway.
/// Runs what the desktop runs with its window closed, which handles arriving messages
/// and shows their notifications.
fn work_while_hidden(app: &Rc<RefCell<App>>, ctx: &egui::Context) {
    fn pass(app: &Rc<RefCell<App>>, ctx: &egui::Context) {
        if !page_hidden() {
            return;
        }
        // Taken only if a frame is not using it: a hidden page has none running.
        if let Ok(mut app) = app.try_borrow_mut() {
            app.window_focused = false;
            app.background_frame(ctx);
        }
    }
    let waiting = Rc::new(Cell::new(false));
    {
        let (app, ctx) = (app.clone(), ctx.clone());
        fastframe_shell::on_wake(move || {
            // A wake can come from inside the app, so its pass waits its turn.
            if !page_hidden() || waiting.replace(true) {
                return;
            }
            let (app, ctx, waiting) = (app.clone(), ctx.clone(), waiting.clone());
            wasm_bindgen_futures::spawn_local(async move {
                waiting.set(false);
                pass(&app, &ctx);
            });
        });
    }
    let (app, ctx) = (app.clone(), ctx.clone());
    gloo_timers::callback::Interval::new(HIDDEN_TICK_MS, move || pass(&app, &ctx)).forget();
}

/// The app in the browser's window.
struct Page {
    app: Rc<RefCell<App>>,
    /// The unread count last shown in the tab and on the app's icon.
    shown_unread: Option<u32>,
}

impl eframe::App for Page {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        let focused = page_in_focus();
        raw_input.focused = focused;
        if let Some(viewport) = raw_input.viewports.get_mut(&egui::ViewportId::ROOT) {
            viewport.focused = Some(focused);
        }
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut app = self.app.borrow_mut();
        app.background_frame(ctx);
        app.window_logic(ctx);
        // The app's own count, which leaves out locked chats.
        let unread = app.unread_chat_count_everywhere();
        if self.shown_unread != Some(unread) {
            self.shown_unread = Some(unread);
            crate::web::notify::show_unread(unread);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let mut app = self.app.borrow_mut();
        app.frame_ui(ui);
        for account in &mut app.accounts {
            if let Some(startup) = account.backend.take_startup() {
                let _ = startup.send(());
            }
        }
    }
}
