//! Video through the browser's `<video>` element, which decodes and sounds it, with
//! each picture copied to a texture so it draws in its place in the window.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use egui::{ColorImage, TextureHandle, TextureOptions};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{CanvasRenderingContext2d, Event, HtmlCanvasElement, HtmlVideoElement};

use crate::audio::{next_cycled_speed, supported_speed};
use crate::backend::Waker;
use crate::platform::Instant;
use crate::video::{Notice, State, Status};

const SIDE: u32 = 480;
const EXPANDED_SIDE: u32 = 1280;

/// How long a video goes undrawn before it pauses, as it does on the desktop.
const UNSEEN: Duration = Duration::from_secs(1);

/// How often to look for a new picture while one plays.
const FRAME_STEP: Duration = Duration::from_millis(33);

/// A `<video>` holding `bytes`, and the address to release with it.
pub(crate) fn element(bytes: &[u8]) -> Option<(HtmlVideoElement, String)> {
    let video: HtmlVideoElement = web_sys::window()?
        .document()?
        .create_element("video")
        .ok()?
        .dyn_into()
        .ok()?;
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
    let options = web_sys::BlobPropertyBag::new();
    options.set_type("video/mp4");
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options).ok()?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).ok()?;
    video.set_src(&url);
    video.set_preload("auto");
    let _ = video.set_attribute("playsinline", "");
    Some((video, url))
}

/// A canvas to copy pictures out on.
pub(crate) struct Copier {
    canvas: HtmlCanvasElement,
    context: CanvasRenderingContext2d,
}

impl Copier {
    pub(crate) fn new() -> Option<Self> {
        let canvas: HtmlCanvasElement = web_sys::window()?
            .document()?
            .create_element("canvas")
            .ok()?
            .dyn_into()
            .ok()?;
        let options = js_sys::Object::new();
        js_sys::Reflect::set(&options, &"willReadFrequently".into(), &true.into()).ok()?;
        let context = canvas
            .get_context_with_context_options("2d", &options)
            .ok()??
            .dyn_into()
            .ok()?;
        Some(Self { canvas, context })
    }

    /// The current picture, with its longest side at most `side`.
    pub(crate) fn picture(&self, video: &HtmlVideoElement, side: u32) -> Option<ColorImage> {
        let (width, height) = (video.video_width(), video.video_height());
        if width == 0 || height == 0 {
            return None;
        }
        let scale = (f64::from(side) / f64::from(width.max(height))).min(1.0);
        let (out_width, out_height) = (
            ((f64::from(width) * scale).round() as u32).max(1),
            ((f64::from(height) * scale).round() as u32).max(1),
        );
        if self.canvas.width() != out_width || self.canvas.height() != out_height {
            self.canvas.set_width(out_width);
            self.canvas.set_height(out_height);
        }
        let (dw, dh) = (f64::from(out_width), f64::from(out_height));
        self.context
            .draw_image_with_html_video_element_and_dw_and_dh(video, 0.0, 0.0, dw, dh)
            .ok()?;
        let data = self.context.get_image_data(0.0, 0.0, dw, dh).ok()?.data();
        Some(ColorImage::from_rgba_unmultiplied(
            [out_width as usize, out_height as usize],
            &data,
        ))
    }
}

fn seconds(value: f64) -> Duration {
    if value.is_finite() && value > 0.0 {
        Duration::from_secs_f64(value)
    } else {
        Duration::ZERO
    }
}

struct Session {
    message: String,
    path: PathBuf,
    video: HtmlVideoElement,
    url: String,
    copier: Copier,
    texture: Option<TextureHandle>,
    drawn_at: f64,
    told_unsupported: bool,
    /// Kept for as long as the element can call it.
    _wake: Closure<dyn FnMut(Event)>,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.video.pause();
        self.video.set_onerror(None);
        self.video.set_onloadeddata(None);
        self.video.set_onended(None);
        self.video.remove_attribute("src").ok();
        self.video.load();
        let _ = web_sys::Url::revoke_object_url(&self.url);
    }
}

/// The same interface as the desktop's player.
pub struct Player {
    waker: Waker,
    session: Option<Session>,
    muted: bool,
    seen: Cell<Instant>,
    side: u32,
    speed: f32,
    /// A video still on its way from storage, which starts when it lands.
    fetching: Option<(String, PathBuf)>,
}

impl Player {
    pub fn attach(&mut self, _ctx: &egui::Context) {
        if let Some(session) = &mut self.session {
            session.texture = None;
            session.drawn_at = -1.0;
        }
    }

    pub fn new(waker: Waker) -> Self {
        Self {
            waker,
            session: None,
            muted: false,
            seen: Cell::new(Instant::now()),
            side: SIDE,
            speed: 1.0,
            fetching: None,
        }
    }

    pub fn speed(&self) -> f32 {
        self.speed
    }

    pub fn cycle_speed(&mut self) {
        self.set_speed(next_cycled_speed(self.speed));
    }

    pub fn set_speed(&mut self, speed: f32) {
        self.speed = supported_speed(speed);
        if let Some(session) = &self.session {
            session.video.set_playback_rate(f64::from(self.speed));
        }
    }

    #[cfg(any(test, feature = "demo"))]
    pub fn silence(&mut self) {
        self.muted = true;
    }

    fn start_session(&mut self, message: &str, path: &Path) -> bool {
        let Ok(bytes) = crate::web::sfs::read(path) else {
            return false;
        };
        let Some((video, url)) = element(&bytes) else {
            return false;
        };
        let Some(copier) = Copier::new() else {
            return false;
        };
        video.set_muted(self.muted);
        video.set_playback_rate(f64::from(self.speed));
        let waker = self.waker.clone();
        let wake = Closure::<dyn FnMut(Event)>::new(move |_| waker.wake());
        let callback = wake.as_ref().unchecked_ref();
        video.set_onerror(Some(callback));
        video.set_onloadeddata(Some(callback));
        video.set_onended(Some(callback));
        self.seen.set(Instant::now());
        self.session = Some(Session {
            message: message.to_owned(),
            path: path.to_owned(),
            video,
            url,
            copier,
            texture: None,
            drawn_at: -1.0,
            told_unsupported: false,
            _wake: wake,
        });
        true
    }

    fn play(&self) {
        let Some(session) = &self.session else {
            return;
        };
        if let Ok(playing) = session.video.play() {
            wasm_bindgen_futures::spawn_local(async move {
                if wasm_bindgen_futures::JsFuture::from(playing).await.is_err() {
                    log::debug!("the browser did not start a video");
                }
            });
        }
    }

    pub fn toggle(&mut self, message: &str, path: &Path) {
        if self
            .fetching
            .as_ref()
            .is_some_and(|(wanted, _)| wanted == message)
        {
            self.fetching = None;
            return;
        }
        if !crate::web::sfs::is_loaded(path) {
            self.session = None;
            crate::web::sfs::request_load(path);
            self.fetching = Some((message.to_owned(), path.to_owned()));
            self.waker.wake();
            return;
        }
        let same = self
            .session
            .as_ref()
            .is_some_and(|session| session.message == message && session.path == path);
        if !same && !self.start_session(message, path) {
            return;
        }
        let Some(session) = &self.session else {
            return;
        };
        if same && !session.video.paused() {
            let _ = session.video.pause();
            return;
        }
        if session.video.ended() {
            session.video.set_current_time(0.0);
        }
        self.seen.set(Instant::now());
        self.play();
    }

    pub fn seek(&mut self, message: &str, fraction: f32) {
        let Some(session) = self.session.as_ref().filter(|s| s.message == message) else {
            return;
        };
        let total = session.video.duration();
        if total.is_finite() {
            session
                .video
                .set_current_time(total * f64::from(fraction.clamp(0.0, 1.0)));
        }
        self.waker.wake();
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        self.side = if expanded { EXPANDED_SIDE } else { SIDE };
        if let Some(session) = &mut self.session {
            session.drawn_at = -1.0;
        }
    }

    pub fn side(&self) -> u32 {
        self.side
    }

    pub fn muted(&self) -> bool {
        self.muted
    }

    pub fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        if let Some(session) = &self.session {
            session.video.set_muted(self.muted);
        }
    }

    pub fn stop(&mut self) {
        self.session = None;
        self.fetching = None;
    }

    pub fn is_active(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| !session.video.paused() && !session.video.ended())
    }

    pub fn resume(&mut self) {
        self.seen.set(Instant::now());
        self.play();
    }

    pub fn path(&self) -> Option<&Path> {
        self.session.as_ref().map(|session| session.path.as_path())
    }

    pub fn message(&self) -> Option<&str> {
        self.session
            .as_ref()
            .map(|session| session.message.as_str())
    }

    pub fn status(&self, message: &str) -> Option<Status> {
        let session = self.session.as_ref().filter(|s| s.message == message)?;
        let video = &session.video;
        let state = if video.paused() || video.ended() {
            State::Paused
        } else if session.texture.is_none() {
            State::Loading
        } else {
            State::Playing
        };
        let total = seconds(video.duration());
        Some(Status {
            state,
            position: seconds(video.current_time()).min(total),
            total,
            frame: session.texture.clone(),
        })
    }

    pub fn saw(&self, message: &str) {
        if self.message() == Some(message) {
            self.seen.set(Instant::now());
        }
    }

    /// Copies a new picture to its texture, and pauses a video no longer on screen.
    pub fn poll(&mut self, ctx: &egui::Context) -> Option<Notice> {
        if let Some((message, path)) = self
            .fetching
            .take_if(|(_, path)| crate::web::sfs::is_loaded(path))
        {
            self.toggle(&message, &path);
        }
        let side = self.side;
        let unseen = self.seen.get().elapsed() > UNSEEN;
        let session = self.session.as_mut()?;
        if session.video.error().is_some() && !session.told_unsupported {
            session.told_unsupported = true;
            return Some(Notice::Unsupported(session.path.clone()));
        }
        let playing = !session.video.paused() && !session.video.ended();
        if playing && unseen {
            let _ = session.video.pause();
        }
        // Readiness of 2 and up means the element holds a picture for this moment.
        let at = session.video.current_time();
        if session.video.ready_state() >= 2 && at != session.drawn_at {
            if let Some(image) = session.copier.picture(&session.video, side) {
                match &mut session.texture {
                    Some(texture) => texture.set(image, TextureOptions::LINEAR),
                    None => {
                        session.texture =
                            Some(ctx.load_texture("web-video", image, TextureOptions::LINEAR));
                    }
                }
                session.drawn_at = at;
            }
        }
        if playing && !unseen {
            ctx.request_repaint_after(FRAME_STEP);
        }
        None
    }
}

/// Waits for one of the element's events.
async fn event(video: &HtmlVideoElement, name: &str) -> bool {
    let (answer, answered) = futures::channel::oneshot::channel::<bool>();
    let answer = std::rc::Rc::new(Cell::new(Some(answer)));
    let settle = |outcome: bool| {
        let answer = answer.clone();
        Closure::<dyn FnMut(Event)>::new(move |_| {
            if let Some(answer) = answer.take() {
                let _ = answer.send(outcome);
            }
        })
    };
    let (done, failed) = (settle(true), settle(false));
    let target: &web_sys::EventTarget = video;
    let _ = target.add_event_listener_with_callback(name, done.as_ref().unchecked_ref());
    let _ = target.add_event_listener_with_callback("error", failed.as_ref().unchecked_ref());
    let outcome = answered.await.unwrap_or(false);
    let _ = target.remove_event_listener_with_callback(name, done.as_ref().unchecked_ref());
    let _ = target.remove_event_listener_with_callback("error", failed.as_ref().unchecked_ref());
    outcome
}

/// Pictures from a short silent video, for GIFs and animated stickers: up to `limit`
/// of them, `step` apart, with the longest side at most `side`.
pub(crate) async fn stepped_frames(
    path: &Path,
    limit: usize,
    step: Duration,
    side: u32,
) -> Option<Vec<ColorImage>> {
    let bytes = crate::web::sfs::read(path).ok()?;
    let (video, url) = element(&bytes)?;
    video.set_muted(true);
    let copier = Copier::new();
    let loaded = video.ready_state() >= 2 || event(&video, "loadeddata").await;
    let mut frames = Vec::new();
    if let (true, Some(copier)) = (loaded, copier) {
        let total = video.duration();
        let mut at = 0.0;
        while frames.len() < limit && at < total {
            if at > 0.0 {
                video.set_current_time(at);
                if !event(&video, "seeked").await {
                    break;
                }
            }
            let Some(frame) = copier.picture(&video, side) else {
                break;
            };
            frames.push(frame);
            at += step.as_secs_f64();
        }
    }
    video.remove_attribute("src").ok();
    video.load();
    let _ = web_sys::Url::revoke_object_url(&url);
    (!frames.is_empty()).then_some(frames)
}
