//! Voice messages through the browser's `<audio>`, which decodes Opus and keeps
//! the pitch at any speed by itself. The same interface as the desktop's player.

use std::path::Path;
use std::time::Duration;

use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{Event, HtmlAudioElement};

use crate::audio::{State, Status, supported_speed};
use crate::backend::Waker;

/// One clip and what is needed to let it go.
struct Clip {
    message: String,
    path: std::path::PathBuf,
    audio: HtmlAudioElement,
    url: String,
    // Kept for as long as the element can call them.
    _wake: Closure<dyn FnMut(Event)>,
}

impl Drop for Clip {
    fn drop(&mut self) {
        let _ = self.audio.pause();
        self.audio.set_onended(None);
        self.audio.set_ontimeupdate(None);
        self.audio.set_onloadedmetadata(None);
        self.audio.set_onerror(None);
        let _ = web_sys::Url::revoke_object_url(&self.url);
    }
}

pub struct Player {
    waker: Waker,
    clip: Option<Clip>,
    speed: f32,
    finished: Option<String>,
    /// Whether the clip's end has been told, so it is told once.
    ended_seen: bool,
    /// Whether the clip's failure has been told, so it is told once.
    failure_seen: bool,
    /// A clip still on its way from storage, which starts when it lands.
    fetching: Option<(String, std::path::PathBuf)>,
}

/// A position or length from the element, which says NaN or infinity until it knows.
fn seconds(value: f64) -> Duration {
    if value.is_finite() && value > 0.0 {
        Duration::from_secs_f64(value)
    } else {
        Duration::ZERO
    }
}

impl Player {
    pub fn new(waker: Waker) -> Self {
        Self {
            waker,
            clip: None,
            speed: 1.0,
            finished: None,
            ended_seen: false,
            failure_seen: false,
            fetching: None,
        }
    }

    pub fn speed(&self) -> f32 {
        self.speed
    }

    pub fn set_speed(&mut self, speed: f32) -> f32 {
        self.speed = supported_speed(speed);
        if let Some(clip) = &self.clip {
            clip.audio.set_playback_rate(f64::from(self.speed));
        }
        self.speed
    }

    /// The browser changes speed at once, so nothing is ever being prepared.
    pub fn preparing_speed(&self, _message: &str) -> bool {
        false
    }

    fn load(&mut self, message: &str, path: &Path) -> Result<(), String> {
        let bytes = crate::web::sfs::read(path)
            .map_err(|error| format!("Could not read the audio file: {error}"))?;
        let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes.as_slice()));
        let blob = web_sys::Blob::new_with_u8_array_sequence(&parts)
            .map_err(|_| "Could not hold the audio".to_owned())?;
        let url = web_sys::Url::create_object_url_with_blob(&blob)
            .map_err(|_| "Could not hold the audio".to_owned())?;
        let audio =
            HtmlAudioElement::new_with_src(&url).map_err(|_| "Could not play audio".to_owned())?;
        audio.set_playback_rate(f64::from(self.speed));
        let waker = self.waker.clone();
        let wake = Closure::<dyn FnMut(Event)>::new(move |_| waker.wake());
        let callback = wake.as_ref().unchecked_ref();
        audio.set_onended(Some(callback));
        audio.set_ontimeupdate(Some(callback));
        audio.set_onloadedmetadata(Some(callback));
        audio.set_onerror(Some(callback));
        self.clip = Some(Clip {
            message: message.to_owned(),
            path: path.to_owned(),
            audio,
            url,
            _wake: wake,
        });
        self.ended_seen = false;
        self.failure_seen = false;
        Ok(())
    }

    fn start(&self) {
        let Some(clip) = &self.clip else {
            return;
        };
        if let Ok(playing) = clip.audio.play() {
            wasm_bindgen_futures::spawn_local(async move {
                if wasm_bindgen_futures::JsFuture::from(playing).await.is_err() {
                    log::debug!("the browser did not start the audio");
                }
            });
        }
    }

    pub fn toggle(&mut self, message: &str, path: &Path) -> Result<(), String> {
        if self
            .fetching
            .as_ref()
            .is_some_and(|(wanted, _)| wanted == message)
        {
            self.fetching = None;
            return Ok(());
        }
        if !crate::web::sfs::is_loaded(path) {
            self.clip = None;
            crate::web::sfs::request_load(path);
            self.fetching = Some((message.to_owned(), path.to_owned()));
            self.waker.wake();
            return Ok(());
        }
        let same = self
            .clip
            .as_ref()
            .is_some_and(|clip| clip.message == message);
        if !same {
            self.load(message, path)?;
            self.start();
            return Ok(());
        }
        self.toggle_current()
    }

    pub fn seek(&mut self, message: &str, path: &Path, fraction: f32) -> Result<(), String> {
        if !crate::web::sfs::is_loaded(path) {
            return self.toggle(message, path);
        }
        if self
            .clip
            .as_ref()
            .is_none_or(|clip| clip.message != message)
        {
            self.load(message, path)?;
        }
        if let Some(clip) = &self.clip {
            let total = clip.audio.duration();
            if total.is_finite() {
                clip.audio
                    .set_current_time(total * f64::from(fraction.clamp(0.0, 1.0)));
            }
        }
        self.ended_seen = false;
        self.start();
        Ok(())
    }

    pub fn stop(&mut self) {
        self.clip = None;
        self.fetching = None;
    }

    pub fn current(&self) -> Option<&str> {
        self.fetching
            .as_ref()
            .map(|(message, _)| message.as_str())
            .or_else(|| self.clip.as_ref().map(|clip| clip.message.as_str()))
    }

    pub fn toggle_current(&mut self) -> Result<(), String> {
        let Some(clip) = &self.clip else {
            return Ok(());
        };
        if clip.audio.paused() {
            // Played to its end: from the top, as the desktop does.
            if clip.audio.ended() {
                clip.audio.set_current_time(0.0);
                self.ended_seen = false;
            }
            self.start();
        } else {
            let _ = clip.audio.pause();
        }
        self.waker.wake();
        Ok(())
    }

    /// Holds a paused clip of `message`, for the sample chats.
    #[cfg(any(test, feature = "demo"))]
    pub fn hold_paused(&mut self, _message: &str) {}

    pub fn take_finished(&mut self) -> Option<String> {
        self.finished.take()
    }

    pub fn is_playing(&self) -> bool {
        self.clip
            .as_ref()
            .is_some_and(|clip| !clip.audio.paused() && !clip.audio.ended())
    }

    pub fn status(&self, message: &str) -> Status {
        let idle = Status {
            state: State::Idle,
            position: Duration::ZERO,
            total: Duration::ZERO,
        };
        if self
            .fetching
            .as_ref()
            .is_some_and(|(wanted, _)| wanted == message)
        {
            return Status {
                state: State::Loading,
                ..idle
            };
        }
        let Some(clip) = self.clip.as_ref().filter(|clip| clip.message == message) else {
            return idle;
        };
        let total = seconds(clip.audio.duration());
        if clip.audio.ended() {
            return Status { total, ..idle };
        }
        // Nothing known about the clip yet, and not an error either.
        if clip.audio.ready_state() == 0 && clip.audio.error().is_none() {
            return Status {
                state: State::Loading,
                ..idle
            };
        }
        Status {
            state: if clip.audio.paused() {
                State::Paused
            } else {
                State::Playing
            },
            position: seconds(clip.audio.current_time()).min(total),
            total,
        }
    }

    /// The browser does not hand over its samples, so the message's own waveform
    /// is what shows.
    pub fn bars(&self, _message: &str) -> Option<&[u8]> {
        None
    }

    pub fn poll(&mut self) -> Result<(), String> {
        if let Some((message, path)) = self
            .fetching
            .take_if(|(_, path)| crate::web::sfs::is_loaded(path))
        {
            return self.toggle(&message, &path);
        }
        let Some(clip) = &self.clip else {
            return Ok(());
        };
        if clip.audio.ended() && !self.ended_seen {
            self.ended_seen = true;
            self.finished = Some(clip.message.clone());
        }
        if clip.audio.error().is_some() && !self.failure_seen {
            self.failure_seen = true;
            log::warn!("the browser could not play a voice message");
            let path = clip
                .path
                .extension()
                .map(|kind| kind.to_string_lossy().into_owned());
            return Err(format!(
                "This browser cannot play this audio{}",
                path.map_or_else(String::new, |kind| format!(" ({kind})"))
            ));
        }
        Ok(())
    }
}

/// A short sound repeating through its own element, until dropped.
pub struct Looping {
    audio: HtmlAudioElement,
    url: String,
}

impl Looping {
    /// Mono `samples` at `rate`. A page that has not been clicked yet may be refused
    /// sound, and then this stays silent.
    pub fn start(samples: &[f32], rate: u32) -> Option<Self> {
        let bytes = crate::pcm::wav(samples, rate);
        let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes.as_slice()));
        let blob = web_sys::Blob::new_with_u8_array_sequence(&parts).ok()?;
        let url = web_sys::Url::create_object_url_with_blob(&blob).ok()?;
        let audio = HtmlAudioElement::new_with_src(&url).ok()?;
        audio.set_loop(true);
        if let Ok(playing) = audio.play() {
            // Refused sound is not worth a console error.
            wasm_bindgen_futures::spawn_local(async move {
                let _ = wasm_bindgen_futures::JsFuture::from(playing).await;
            });
        }
        Some(Self { audio, url })
    }
}

impl Drop for Looping {
    fn drop(&mut self) {
        let _ = self.audio.pause();
        let _ = web_sys::Url::revoke_object_url(&self.url);
    }
}
