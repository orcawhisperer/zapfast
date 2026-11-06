//! The browser's microphone, for voice messages and calls. An audio worklet hears it on
//! the browser's audio thread and posts it to the page, so a busy page delays the sound
//! but loses none of it.

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AudioContext, AudioWorkletNode, Event, MediaStream, MediaStreamConstraints, MediaStreamTrack,
    MessageEvent,
};

/// Mixes what it hears to mono and posts it every 50 ms, one loudness bar's worth.
/// It writes nothing to its output, so joining the speakers plays nothing back.
const WORKLET: &str = r#"
registerProcessor("zapfast-microphone", class extends AudioWorkletProcessor {
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

/// The worklet's source, at an address its module loader can reach.
pub(crate) fn worklet_url(source: &str) -> Result<String, JsValue> {
    let parts = js_sys::Array::of1(&JsValue::from_str(source));
    let options = web_sys::BlobPropertyBag::new();
    options.set_type("text/javascript");
    let blob = web_sys::Blob::new_with_str_sequence_and_options(&parts, &options)?;
    web_sys::Url::create_object_url_with_blob(&blob)
}

/// Why the microphone did not open, in words for the person who asked for it.
pub fn refusal(error: &JsValue) -> String {
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

/// Asks for the microphone under `constraints`, the `audio` of `getUserMedia`.
pub(crate) async fn stream(constraints: &JsValue) -> Result<MediaStream, JsValue> {
    let window = web_sys::window().ok_or(JsValue::NULL)?;
    let asked = MediaStreamConstraints::new();
    asked.set_audio(constraints);
    JsFuture::from(
        window
            .navigator()
            .media_devices()?
            .get_user_media_with_constraints(&asked)?,
    )
    .await?
    .dyn_into()
}

/// Lets a stream go, which turns off the browser's sign that the page is listening.
pub(crate) fn release(stream: &MediaStream) {
    for track in stream.get_tracks().iter() {
        if let Ok(track) = track.dyn_into::<MediaStreamTrack>() {
            track.set_onended(None);
            track.stop();
        }
    }
}

/// An open microphone. Dropping it lets the microphone go.
pub struct Capture {
    stream: MediaStream,
    context: AudioContext,
    node: AudioWorkletNode,
    _heard: Closure<dyn FnMut(MessageEvent)>,
    _ended: Closure<dyn FnMut(Event)>,
}

impl Capture {
    /// The rate of what is heard, which is the browser's choice.
    pub fn rate(&self) -> u32 {
        self.context.sample_rate() as u32
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Ok(port) = self.node.port() {
            port.set_onmessage(None);
        }
        release(&self.stream);
        let _ = self.context.close();
    }
}

/// Opens the microphone under `constraints`. `heard` gets 50 ms of mono samples at a
/// time, and `ended` says the microphone was unplugged or its permission taken back.
pub async fn open(
    constraints: &JsValue,
    mut heard: impl FnMut(Vec<f32>) + 'static,
    mut ended: impl FnMut() + 'static,
) -> Result<Capture, JsValue> {
    let stream = stream(constraints).await?;
    // A stream given up on below must still be let go.
    let attached = attach().await;
    let (context, node) = match attached {
        Ok(attached) => attached,
        Err(error) => {
            release(&stream);
            return Err(error);
        }
    };
    let heard = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
        heard(js_sys::Float32Array::new(&event.data()).to_vec());
    });
    let ended = Closure::<dyn FnMut(Event)>::new(move |_: Event| ended());
    let capture = Capture {
        stream,
        context,
        node,
        _heard: heard,
        _ended: ended,
    };
    capture
        .node
        .port()?
        .set_onmessage(Some(capture._heard.as_ref().unchecked_ref()));
    for track in capture.stream.get_tracks().iter() {
        if let Ok(track) = track.dyn_into::<MediaStreamTrack>() {
            track.set_onended(Some(capture._ended.as_ref().unchecked_ref()));
        }
    }
    capture
        .context
        .create_media_stream_source(&capture.stream)?
        .connect_with_audio_node(&capture.node)?;
    // Silent, but a browser may only run a node that leads to its speakers.
    capture
        .node
        .connect_with_audio_node(&capture.context.destination())?;
    Ok(capture)
}

async fn attach() -> Result<(AudioContext, AudioWorkletNode), JsValue> {
    // The browser's rate, as a browser refuses to join a stream to a context at another.
    let context = AudioContext::new()?;
    let url = worklet_url(WORKLET)?;
    let loaded = JsFuture::from(context.audio_worklet()?.add_module(&url)?).await;
    let _ = web_sys::Url::revoke_object_url(&url);
    loaded?;
    // A context made after the wait for permission may have lost the click.
    JsFuture::from(context.resume()?).await?;
    let node = AudioWorkletNode::new(&context, "zapfast-microphone")?;
    Ok((context, node))
}
