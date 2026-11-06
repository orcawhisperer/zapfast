//! Starts ZapFast on the page's canvas.

use wasm_bindgen::JsCast;

use crate::app::App;
use crate::paths::AppDirs;
use crate::settings::Settings;

/// The root of the page's files, a path only ZapFast's own storage ever reads.
const ROOT: &str = "/zapfast";

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

fn make_app(ctx: &egui::Context, waker: &fastframe_shell::Waker) -> Result<App, String> {
    let dirs = AppDirs::under(std::path::Path::new(ROOT));
    dirs.ensure().map_err(|error| error.to_string())?;
    let settings = Settings::load(&dirs.settings_file());
    #[cfg(feature = "demo")]
    if wants_demo() {
        let mut app = App::headless(dirs, settings).0;
        crate::demo::populate(&mut app);
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
    let root = std::path::Path::new(ROOT);
    crate::web::sfs::hydrate(database, AppDirs::under(root).cache)
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
                cc.egui_ctx
                    .add_bytes_loader(std::sync::Arc::new(crate::web::images::PageFiles));
                let app = make_app(&cc.egui_ctx, &waker)?;
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
    let element = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id("failure"));
    if let Some(element) = element {
        element.set_text_content(Some(reason));
    }
}

/// The app in the browser's window.
struct Page {
    app: App,
    /// The unread count last shown in the tab and on the app's icon.
    shown_unread: Option<u32>,
}

impl eframe::App for Page {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.app.background_frame(ctx);
        self.app.window_logic(ctx);
        // The app's own count, which leaves out locked chats.
        let unread = self.app.unread_chat_count_everywhere();
        if self.shown_unread != Some(unread) {
            self.shown_unread = Some(unread);
            crate::web::notify::show_unread(unread);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.frame_ui(ui);
        for account in &mut self.app.accounts {
            if let Some(startup) = account.backend.take_startup() {
                let _ = startup.send(());
            }
        }
    }
}
