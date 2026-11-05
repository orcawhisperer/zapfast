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

async fn run() -> Result<(), String> {
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
                cc.egui_ctx
                    .add_bytes_loader(std::sync::Arc::new(crate::web::images::PageFiles));
                let app = make_app(&cc.egui_ctx, &waker)?;
                Ok(Box::new(Page { app }))
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
}

impl eframe::App for Page {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.app.background_frame(ctx);
        self.app.window_logic(ctx);
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
