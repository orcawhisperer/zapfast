//! A call's microphone and speaker in a browser. The same interface as the desktop's, for the same
//! engine: 60 ms frames of 16 kHz samples each way, behind channels that stay put when a device
//! changes. The browser cancels the echo, and it only names devices it has been allowed to use.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::future::{Either, select};
use js_sys::{Array, Float32Array, Object, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{AudioContext, AudioContextOptions, AudioContextState, AudioWorkletNode};

use crate::pcm::Framer;
use crate::web::microphone;

/// The engine's rate.
pub const RATE: u32 = 16_000;
/// Samples in one of the engine's frames: 60 ms.
pub const FRAME_SAMPLES: usize = 960;
/// How many frames may wait for the speaker.
const SPEAKER_QUEUE: usize = 5;

/// Plays what the engine hands over. It waits for a little sound before starting, so a late packet
/// does not click, and when it falls far behind it catches up rather than staying late.
const SPEAKER_WORKLET: &str = r#"
registerProcessor("zapfast-speaker", class extends AudioWorkletProcessor {
    queue = [];
    held = 0;
    at = 0;
    playing = false;
    constructor() {
        super();
        this.port.onmessage = (event) => {
            this.queue.push(event.data);
            this.held += event.data.length;
            while (this.held > sampleRate * 0.4 && this.queue.length > 1) {
                this.held -= this.queue.shift().length - this.at;
                this.at = 0;
            }
        };
    }
    process(_, outputs) {
        const out = outputs[0];
        if (!out.length) return true;
        const samples = out[0].length;
        if (!this.playing && this.held < sampleRate * 0.12) return true;
        this.playing = true;
        for (let index = 0; index < samples; index++) {
            if (!this.queue.length) {
                this.playing = false;
                break;
            }
            const value = this.queue[0][this.at++];
            this.held--;
            for (const channel of out) channel[index] = value;
            if (this.at === this.queue[0].length) {
                this.queue.shift();
                this.at = 0;
            }
        }
        return true;
    }
});
"#;

thread_local! {
    /// The microphones and speakers last named, as the desktop's pickers read them at once.
    static MICROPHONES: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
    static SPEAKERS: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
    static CAMERAS: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
}

/// The microphones, as ids and names.
pub fn microphones() -> Vec<(String, String)> {
    MICROPHONES.with_borrow(Clone::clone)
}

/// The speakers, as ids and names. Empty where a browser cannot choose one.
pub fn speakers() -> Vec<(String, String)> {
    SPEAKERS.with_borrow(Clone::clone)
}

/// The cameras, as ids and names.
pub fn cameras() -> Vec<(String, String)> {
    CAMERAS.with_borrow(Clone::clone)
}

/// Asks the browser again what it has. Without permission yet it names nothing worth listing.
pub async fn refresh_devices() {
    let Some(listing) = web_sys::window()
        .and_then(|window| window.navigator().media_devices().ok())
        .and_then(|devices| devices.enumerate_devices().ok())
    else {
        return;
    };
    let Ok(found) = JsFuture::from(listing).await else {
        return;
    };
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let mut cameras = Vec::new();
    for device in Array::from(&found).iter() {
        let Ok(device) = device.dyn_into::<web_sys::MediaDeviceInfo>() else {
            continue;
        };
        let (id, label) = (device.device_id(), device.label());
        if id.is_empty() || label.is_empty() {
            continue;
        }
        match device.kind() {
            web_sys::MediaDeviceKind::Audioinput => inputs.push((id, label)),
            web_sys::MediaDeviceKind::Audiooutput => outputs.push((id, label)),
            web_sys::MediaDeviceKind::Videoinput => cameras.push((id, label)),
            _ => {}
        }
    }
    MICROPHONES.set(inputs);
    SPEAKERS.set(outputs);
    CAMERAS.set(cameras);
}

/// What the microphone is asked for: the browser's own echo cancelling and clean-up, which on the
/// desktop are `call_audio`'s.
fn constraints(device: Option<&str>) -> Result<JsValue, JsValue> {
    let asked = Object::new();
    for name in ["echoCancellation", "noiseSuppression", "autoGainControl"] {
        Reflect::set(&asked, &name.into(), &JsValue::TRUE)?;
    }
    if let Some(device) = device {
        let exact = Object::new();
        Reflect::set(&exact, &"exact".into(), &device.into())?;
        Reflect::set(&asked, &"deviceId".into(), &exact)?;
    }
    Ok(asked.into())
}

/// Asks for the microphone before a call starts, so a refusal fails the call rather than carrying
/// silence. What is `Some` names what is missing.
pub async fn ask_for_microphone(device: Option<&str>) -> Option<&'static str> {
    let asked = constraints(device).ok()?;
    match microphone::stream(&asked).await {
        Ok(stream) => {
            microphone::release(&stream);
            refresh_devices().await;
            None
        }
        Err(error) => {
            log::warn!("[CALL] {}", microphone::refusal(&error));
            Some("microphone")
        }
    }
}

/// Nothing to look for here: `ask_for_microphone` is where a page finds out.
pub fn unavailable(_microphone: Option<&str>, _speaker: Option<&str>) -> Option<&'static str> {
    None
}

/// What links a call's speaker to its microphone's echo canceller on the desktop. A browser does
/// that itself.
#[derive(Clone, Default)]
pub struct FarEnd;

impl FarEnd {
    pub fn new() -> Self {
        Self
    }
}

/// The microphone side: a channel the engine owns, refilled from whichever device is selected.
pub struct AudioInput {
    /// Asks the pump to restart on another device; dropping it stops the pump.
    swap: Option<async_channel::Sender<Option<String>>>,
    /// Set when the pump gave up on the selected device and reopened on the default.
    pub fell_back: async_channel::Receiver<()>,
    /// How many readers this call's pump has opened.
    pub opens: Arc<AtomicUsize>,
}

impl AudioInput {
    /// Starts the microphone.
    pub fn spawn(
        target: Option<String>,
        _far_end: &FarEnd,
    ) -> (Self, async_channel::Receiver<Vec<i16>>) {
        let (out, rx) = async_channel::bounded::<Vec<i16>>(4);
        let (swap, swaps) = async_channel::unbounded::<Option<String>>();
        let (fell, fell_back) = async_channel::bounded::<()>(1);
        let opens = Arc::new(AtomicUsize::new(0));
        wasm_bindgen_futures::spawn_local(mic_pump(out, swaps, fell, target, Arc::clone(&opens)));
        (
            Self {
                swap: Some(swap),
                fell_back,
                opens,
            },
            rx,
        )
    }

    /// Rebinds the microphone, keeping the engine's channel alive.
    pub fn bind(&self, target: Option<String>) {
        if let Some(swap) = &self.swap {
            let _ = swap.try_send(target);
        }
    }

    /// Stops the reader: dropping the swap sender is the pump's stop signal.
    pub fn stop(&mut self) {
        self.swap = None;
    }
}

async fn mic_pump(
    out: async_channel::Sender<Vec<i16>>,
    swaps: async_channel::Receiver<Option<String>>,
    fell: async_channel::Sender<()>,
    mut target: Option<String>,
    opens: Arc<AtomicUsize>,
) {
    loop {
        let (lost_tx, lost) = async_channel::bounded::<()>(1);
        // The rate is the browser's, known only once it is open, so frames wait for it.
        let framer: Rc<RefCell<Option<Framer>>> = Rc::new(RefCell::new(None));
        let heard = {
            let framer = Rc::clone(&framer);
            let out = out.clone();
            move |block: Vec<f32>| {
                if let Some(framer) = framer.borrow_mut().as_mut() {
                    framer.push(&block, |frame| {
                        // The engine fell behind: the newest sound is the one to keep.
                        let _ = out.force_send(frame);
                    });
                }
            }
        };
        let ended = move || {
            let _ = lost_tx.try_send(());
        };
        let asked = constraints(target.as_deref()).unwrap_or(JsValue::TRUE);
        let capture = match microphone::open(&asked, heard, ended).await {
            Ok(capture) => {
                *framer.borrow_mut() = Some(Framer::new(capture.rate(), RATE, FRAME_SAMPLES));
                opens.fetch_add(1, Ordering::Relaxed);
                Some(capture)
            }
            Err(error) => {
                log::warn!("[CALL] {}", microphone::refusal(&error));
                None
            }
        };
        let fell_over = capture.is_none() && target.is_some();
        if fell_over {
            let _ = fell.try_send(());
            target = None;
            continue;
        }
        match select(Box::pin(swaps.recv()), Box::pin(lost.recv())).await {
            Either::Left((Ok(next), _)) => target = next,
            // Nobody wants the microphone any more.
            Either::Left((Err(_), _)) => return,
            // Unplugged, so back to the default, if there still is one.
            Either::Right(_) => {
                let _ = fell.try_send(());
                target = None;
            }
        }
    }
}

/// The speaker side: the engine's frames written to whichever device is selected.
pub struct AudioOutput {
    swap: Option<async_channel::Sender<Option<String>>>,
    /// Set when the pump gave up on the selected device and reopened on the default.
    pub fell_back: async_channel::Receiver<()>,
    /// How many writer streams this call's pump has opened.
    pub opens: Arc<AtomicUsize>,
    /// Stays zero: the browser restarts its own streams.
    pub stalls: Arc<AtomicUsize>,
    /// Stays zero: the browser restarts its own streams.
    pub restarts: Arc<AtomicUsize>,
}

impl AudioOutput {
    /// Starts the speaker.
    pub fn spawn(
        target: Option<String>,
        _far_end: &FarEnd,
    ) -> (Self, async_channel::Sender<Vec<i16>>) {
        let (tx, rx) = async_channel::bounded::<Vec<i16>>(SPEAKER_QUEUE);
        let (swap, swaps) = async_channel::unbounded::<Option<String>>();
        let (_fell, fell_back) = async_channel::bounded::<()>(1);
        let opens = Arc::new(AtomicUsize::new(0));
        wasm_bindgen_futures::spawn_local(play_pump(rx, swaps, target, Arc::clone(&opens)));
        (
            Self {
                swap: Some(swap),
                fell_back,
                opens,
                stalls: Arc::default(),
                restarts: Arc::default(),
            },
            tx,
        )
    }

    /// The desktop's audio graph relinks when a camera starts. A browser's does not ask.
    pub fn relink(&self) {}

    /// Rebinds the speaker, keeping the engine's channel alive.
    pub fn bind(&self, target: Option<String>) {
        if let Some(swap) = &self.swap {
            let _ = swap.try_send(target);
        }
    }

    /// Stops the writer: dropping the swap sender is the pump's stop signal.
    pub fn stop(&mut self) {
        self.swap = None;
    }
}

/// An output context at the engine's rate, with the browser resampling for the device.
async fn open_speaker() -> Result<(AudioContext, AudioWorkletNode), JsValue> {
    let options = AudioContextOptions::new();
    options.set_sample_rate(RATE as f32);
    let context = AudioContext::new_with_context_options(&options)?;
    let url = microphone::worklet_url(SPEAKER_WORKLET)?;
    let loaded = JsFuture::from(context.audio_worklet()?.add_module(&url)?).await;
    let _ = web_sys::Url::revoke_object_url(&url);
    loaded?;
    let node = AudioWorkletNode::new(&context, "zapfast-speaker")?;
    node.connect_with_audio_node(&context.destination())?;
    Ok((context, node))
}

/// Points a context at a speaker, where the browser can. Firefox and Safari keep the default.
fn choose_speaker(context: &AudioContext, device: Option<&str>) {
    let set = Reflect::get(context, &"setSinkId".into())
        .ok()
        .and_then(|set| set.dyn_into::<js_sys::Function>().ok());
    if let Some(set) = set
        && let Ok(setting) = set.call1(context, &device.unwrap_or("").into())
        && let Ok(setting) = setting.dyn_into::<js_sys::Promise>()
    {
        wasm_bindgen_futures::spawn_local(async move {
            if JsFuture::from(setting).await.is_err() {
                log::warn!("[CALL] the browser would not use the chosen speaker");
            }
        });
    }
}

/// Hands one frame to the worklet.
fn play(node: &AudioWorkletNode, frame: &[i16]) {
    let samples: Vec<f32> = frame
        .iter()
        .map(|&sample| f32::from(sample) / f32::from(i16::MAX))
        .collect();
    let copy = Float32Array::from(samples.as_slice());
    if let Ok(port) = node.port() {
        let _ = port.post_message_with_transferable(&copy, &Array::of1(&copy.buffer()));
    }
}

async fn play_pump(
    frames: async_channel::Receiver<Vec<i16>>,
    swaps: async_channel::Receiver<Option<String>>,
    target: Option<String>,
    opens: Arc<AtomicUsize>,
) {
    let (context, node) = match open_speaker().await {
        Ok(opened) => opened,
        Err(_) => {
            log::warn!("[CALL] the browser would not open a speaker");
            // Keeps the engine's frames going nowhere, rather than backing it up.
            while frames.recv().await.is_ok() {}
            return;
        }
    };
    opens.fetch_add(1, Ordering::Relaxed);
    if target.is_some() {
        choose_speaker(&context, target.as_deref());
    }
    loop {
        match select(Box::pin(frames.recv()), Box::pin(swaps.recv())).await {
            Either::Left((Ok(frame), _)) => {
                // A context can start out held back until the page has been clicked.
                if context.state() == AudioContextState::Suspended {
                    let _ = context.resume();
                }
                play(&node, &frame);
            }
            Either::Right((Ok(next), _)) => choose_speaker(&context, next.as_deref()),
            // The engine is done, or the call has told this to stop.
            _ => break,
        }
    }
    let _ = context.close();
}
