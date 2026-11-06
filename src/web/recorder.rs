//! Recording voice messages from the browser's microphone. The same interface as
//! the desktop's recorder, without its threads: an audio worklet hears the
//! microphone on the browser's audio thread and posts it here, so a page busy with
//! a history sync delays the sound but loses none of it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AudioContext, AudioWorkletNode, Event, MediaStream, MediaStreamConstraints, MediaStreamTrack,
    MessageEvent,
};

use crate::backend::Waker;
use crate::platform::Instant;

/// Mixes what it hears to mono and posts it every 50 ms, one loudness bar's worth.
/// It writes nothing to its output, so joining the speakers plays nothing back.
const WORKLET: &str = r#"
registerProcessor("zapfast-recorder", class extends AudioWorkletProcessor {
    size = Math.max(1, Math.round(sampleRate / 20));
    held = new Float32Array(this.size);
    filled = 0;
    process(inputs) {
        const channels = inputs[0];
        if (!channels.length) return true;
        const frames = channels[0].length;
        for (let frame = 0; frame < frames; frame++) {
            let sum = 0;
            for (const channel of channels) sum += channel[frame];
            this.held[this.filled++] = sum / channels.length;
            if (this.filled === this.size) {
                this.port.postMessage(this.held);
                this.held = new Float32Array(this.size);
                this.filled = 0;
            }
        }
        return true;
    }
});
"#;

/// What holds the microphone open. Letting it go turns off the browser's sign that
/// the page is recording.
struct Microphone {
    stream: MediaStream,
    context: AudioContext,
    node: AudioWorkletNode,
    _heard: Closure<dyn FnMut(MessageEvent)>,
    _ended: Closure<dyn FnMut(Event)>,
}

impl Drop for Microphone {
    fn drop(&mut self) {
        if let Ok(port) = self.node.port() {
            port.set_onmessage(None);
        }
        for track in self.stream.get_tracks().iter() {
            if let Ok(track) = track.dyn_into::<MediaStreamTrack>() {
                track.set_onended(None);
                track.stop();
            }
        }
        let _ = self.context.close();
    }
}

#[derive(Default)]
struct Take {
    /// What was heard, mono, at `rate`.
    samples: RefCell<Vec<f32>>,
    rate: Cell<u32>,
    /// A loudness for each 50 ms.
    levels: RefCell<Vec<f32>>,
    /// When sound first came, which is unset while the microphone wakes.
    began: Cell<Option<Instant>>,
    failure: RefCell<Option<String>>,
    stepped: Cell<bool>,
    /// Set once nobody wants the microphone, which may still be being opened.
    stopped: Cell<bool>,
    microphone: RefCell<Option<Microphone>>,
}

impl Take {
    fn hear(&self, block: &[f32], waker: &Waker) {
        if self.stopped.get() {
            return;
        }
        if self.began.get().is_none() {
            self.began.set(Some(Instant::now()));
        }
        let loudness =
            (block.iter().map(|s| s * s).sum::<f32>() / block.len().max(1) as f32).sqrt();
        self.samples.borrow_mut().extend_from_slice(block);
        self.levels.borrow_mut().push(loudness);
        if !self.stepped.get() {
            waker.wake();
        }
    }

    fn fail(&self, reason: String) {
        *self.failure.borrow_mut() = Some(reason);
        self.microphone.borrow_mut().take();
    }
}

/// Why the microphone did not open, in words for the person who asked for it.
fn refusal(error: &JsValue) -> String {
    let name = error
        .dyn_ref::<js_sys::Error>()
        .map(|error| String::from(error.name()))
        .unwrap_or_default();
    match name.as_str() {
        "NotAllowedError" | "SecurityError" => {
            "The browser was not allowed to use the microphone".to_owned()
        }
        "NotFoundError" | "OverconstrainedError" => "No microphone was found".to_owned(),
        "NotReadableError" | "AbortError" => "The microphone is in use elsewhere".to_owned(),
        _ => "Could not open the microphone".to_owned(),
    }
}

/// The worklet's source, at an address its module loader can reach.
fn worklet_url() -> Result<String, JsValue> {
    let parts = js_sys::Array::of1(&JsValue::from_str(WORKLET));
    let options = web_sys::BlobPropertyBag::new();
    options.set_type("text/javascript");
    let blob = web_sys::Blob::new_with_str_sequence_and_options(&parts, &options)?;
    web_sys::Url::create_object_url_with_blob(&blob)
}

async fn open(take: Rc<Take>, waker: Waker) -> Result<(), JsValue> {
    let window = web_sys::window().ok_or(JsValue::NULL)?;
    let constraints = MediaStreamConstraints::new();
    constraints.set_audio(&JsValue::TRUE);
    let stream: MediaStream = JsFuture::from(
        window
            .navigator()
            .media_devices()?
            .get_user_media_with_constraints(&constraints)?,
    )
    .await?
    .dyn_into()?;

    // Held from here on, so a recording stopped during the awaits below still
    // lets the microphone go when this returns.
    let context = AudioContext::new()?;
    take.rate.set(context.sample_rate() as u32);
    let url = worklet_url()?;
    let loaded = JsFuture::from(context.audio_worklet()?.add_module(&url)?).await;
    let _ = web_sys::Url::revoke_object_url(&url);
    loaded?;
    // A context made after the wait for permission may have lost the click.
    JsFuture::from(context.resume()?).await?;

    let node = AudioWorkletNode::new(&context, "zapfast-recorder")?;
    let heard = {
        let take = Rc::clone(&take);
        let waker = waker.clone();
        Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            let block = js_sys::Float32Array::new(&event.data()).to_vec();
            take.hear(&block, &waker);
        })
    };
    node.port()?
        .set_onmessage(Some(heard.as_ref().unchecked_ref()));
    // A microphone unplugged, or its permission taken back.
    let ended = {
        let take = Rc::clone(&take);
        Closure::<dyn FnMut(Event)>::new(move |_: Event| {
            take.fail("The microphone stopped".to_owned());
            waker.wake();
        })
    };
    for track in stream.get_tracks().iter() {
        if let Ok(track) = track.dyn_into::<MediaStreamTrack>() {
            track.set_onended(Some(ended.as_ref().unchecked_ref()));
        }
    }
    context
        .create_media_stream_source(&stream)?
        .connect_with_audio_node(&node)?;
    // Silent, but a browser may only run a node that leads to its speakers.
    node.connect_with_audio_node(&context.destination())?;

    let microphone = Microphone {
        stream,
        context,
        node,
        _heard: heard,
        _ended: ended,
    };
    if !take.stopped.get() {
        *take.microphone.borrow_mut() = Some(microphone);
    }
    Ok(())
}

/// Records from the default microphone until told to stop.
pub struct Recorder {
    take: Rc<Take>,
}

impl Recorder {
    pub fn start(waker: Waker) -> Self {
        let take = Rc::new(Take::default());
        let opening = Rc::clone(&take);
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(error) = open(Rc::clone(&opening), waker.clone()).await
                && !opening.stopped.get()
            {
                opening.fail(refusal(&error));
                waker.wake();
            }
        });
        Self { take }
    }

    /// With no threads to fake one on, the demo records for real.
    #[cfg(feature = "demo")]
    pub fn simulated(waker: Waker) -> Self {
        Self::start(waker)
    }

    /// A take already under way, for demos.
    #[cfg(feature = "demo")]
    pub fn rehearsal() -> Self {
        let take = Take::default();
        *take.levels.borrow_mut() = (0..90)
            .map(|index| 0.05 + 0.2 * ((index as f32 * 0.6).sin().abs()))
            .collect();
        take.began.set(
            Instant::now()
                .checked_sub(Duration::from_millis(4_500))
                .or_else(|| Some(Instant::now())),
        );
        take.stopped.set(true);
        Self {
            take: Rc::new(take),
        }
    }

    /// How long the take has recorded sound: zero while the microphone wakes.
    pub fn elapsed(&self) -> Duration {
        self.take
            .began
            .get()
            .map_or(Duration::ZERO, |began| began.elapsed())
    }

    /// Whether sound has started to arrive, so what is said now is recorded.
    pub fn listening(&self) -> bool {
        self.take.began.get().is_some()
    }

    /// Stops new levels from asking for a frame of their own while `stepped`.
    pub fn set_stepped(&self, stepped: bool) {
        self.take.stepped.set(stepped);
    }

    pub fn levels(&self) -> Vec<f32> {
        self.take.levels.borrow().clone()
    }

    /// Error that stopped recording early.
    pub fn failure(&self) -> Option<String> {
        self.take.failure.borrow().clone()
    }

    /// Stops and returns mono 48 kHz samples. The worklet's last blocks may still
    /// be on their way, so a take can lose up to its final tenth of a second.
    pub fn finish(self) -> Result<Vec<f32>, String> {
        if let Some(error) = self.failure() {
            return Err(error);
        }
        self.take.stopped.set(true);
        self.take.microphone.borrow_mut().take();
        let heard = self.take.samples.take();
        if heard.is_empty() {
            return Err("The microphone did not record any audio".to_owned());
        }
        Ok(crate::voice::mono_at_rate(&heard, 1, self.take.rate.get()))
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.take.stopped.set(true);
        self.take.microphone.borrow_mut().take();
    }
}
