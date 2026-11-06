//! A call's microphone and speaker in a browser. The same interface as the desktop's, for the same
//! engine: 60 ms frames of 16 kHz samples each way, behind channels that stay put when a device
//! changes. The browser cancels the echo, and it only names devices it has been allowed to use.
//!
//! Both sides run in one audio context, at the browser's rate. A browser can only cancel the echo
//! of what it plays beside the microphone, so a speaker in a context of its own is heard back by
//! the peer. The sound's lead over the speaker is held on the browser's audio thread, as the page's
//! thread stalls: a stall delays the sound, and a lead large enough rides it out.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::FutureExt;
use futures::future::{Either, LocalBoxFuture, Shared, select};
use js_sys::{Array, Float32Array, Object, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{AudioContext, AudioContextState, AudioWorkletNode, MessageEvent};

use crate::pcm::Framer;
use crate::platform::Instant;
use crate::web::microphone;

/// The engine's rate.
pub const RATE: u32 = 16_000;
/// Samples in one of the engine's frames: 60 ms.
pub const FRAME_SAMPLES: usize = 960;
/// How many frames may wait for the speaker. Plenty, as a stalled page should lose none of them:
/// the worklet decides what is too late.
const SPEAKER_QUEUE: usize = 16;
/// How many frames may wait for the engine, for the same reason.
const MICROPHONE_QUEUE: usize = 8;
/// How often a call says how its sound is going.
const AUDIO_REPORT: std::time::Duration = std::time::Duration::from_secs(30);

/// Plays what the engine hands over, at the browser's rate. WhatsApp sends several frames to a
/// packet, so sound arrives in bursts, and the page's thread stalls on top of that. So it holds a
/// lead before it starts, and each time it runs dry it fades rather than clicks and holds more.
/// When it is more than a little behind it plays slightly fast until it has caught up, which is
/// not heard the way a jump is; only when far behind does it let sound go.
const SPEAKER_WORKLET: &str = r#"
registerProcessor("zapfast-speaker", class extends AudioWorkletProcessor {

    queue = [];
    held = 0;
    at = 0;
    lead = 0.2 * 16000;
    playing = false;
    catching = false;
    gain = 0;
    last = 0;
    underruns = 0;
    trimmed = 0;
    calm = 0;
    reported = 0;
    constructor() {
        super();
        this.port.onmessage = (event) => {
            this.queue.push(event.data);
            this.held += event.data.length;
            while (this.held > 16000 && this.queue.length > 1) {
                const gone = this.queue.shift();
                this.held -= gone.length - this.at;
                this.at = 0;
                this.trimmed += gone.length;
            }
        };
    }
    process(_, outputs) {
        const out = outputs[0];
        if (!out.length) return true;
        const count = out[0].length;
        const lead = this.lead, rate = 16000;
        if (!this.playing && this.held >= lead) {
            this.playing = true;
            this.gain = 0;
        }
        if (this.held > lead + 0.12 * rate) this.catching = true;
        else if (this.held <= lead + 0.04 * rate) this.catching = false;
        const step = (rate / sampleRate) * (this.catching ? 1.06 : 1);
        const ramp = 1 / (0.005 * sampleRate);
        const left = out[0];
        for (let index = 0; index < count; index++) {
            let value;
            if (this.playing) {
                const frame = this.queue[0];
                const whole = Math.floor(this.at);
                const fraction = this.at - whole;
                const next = whole + 1 < frame.length ? frame[whole + 1]
                    : this.queue.length > 1 ? this.queue[1][0] : frame[whole];
                value = frame[whole] + (next - frame[whole]) * fraction;
                this.gain = Math.min(1, this.gain + ramp);
                value *= this.gain;
                this.at += step;
                this.held -= step;
                while (this.queue.length && this.at >= this.queue[0].length) {
                    this.at -= this.queue.shift().length;
                }
                if (!this.queue.length) {
                    this.at = 0;
                    this.held = 0;
                    this.playing = false;
                    this.underruns++;
                    this.calm = 0;
                    this.lead = Math.min(this.lead + 0.06 * rate, 0.5 * rate);
                }
                this.last = value;
            } else {
                this.last *= 1 - ramp;
                value = this.last;
            }
            left[index] = value;
        }
        for (let channel = 1; channel < out.length; channel++) out[channel].set(left);
        this.calm += count;
        if (this.calm > 30 * sampleRate) {
            this.calm = 0;
            this.lead = Math.max(0.2 * rate, this.lead - 0.02 * rate);
        }
        this.reported += count;
        if (this.reported > 5 * sampleRate) {
            this.reported = 0;
            this.port.postMessage([this.underruns, this.trimmed, this.lead / 16, this.held / 16]);
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

/// The audio context a call's microphone and speaker share, closed when both have let it go.
struct Graph {
    context: AudioContext,
    /// Whether the worklets loaded, once they have.
    worklets: Shared<LocalBoxFuture<'static, bool>>,
}

impl Drop for Graph {
    fn drop(&mut self) {
        let _ = self.context.close();
    }
}

thread_local! {
    static GRAPH: RefCell<Weak<Graph>> = const { RefCell::new(Weak::new()) };
}

/// This call's graph, made by whichever of the two sides starts first.
fn graph() -> Result<Rc<Graph>, JsValue> {
    if let Some(graph) = GRAPH.with_borrow(Weak::upgrade) {
        return Ok(graph);
    }
    let context = AudioContext::new()?;
    let url = microphone::worklet_url(&format!("{}{SPEAKER_WORKLET}", microphone::WORKLET))?;
    let loading = JsFuture::from(context.audio_worklet()?.add_module(&url)?);
    let worklets = async move {
        let loaded = loading.await.is_ok();
        let _ = web_sys::Url::revoke_object_url(&url);
        loaded
    }
    .boxed_local()
    .shared();
    let graph = Rc::new(Graph { context, worklets });
    GRAPH.set(Rc::downgrade(&graph));
    Ok(graph)
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
        let (out, rx) = async_channel::bounded::<Vec<i16>>(MICROPHONE_QUEUE);
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
    let graph = match graph() {
        Ok(graph) if graph.worklets.clone().await => graph,
        _ => {
            log::warn!("[CALL] the browser would not open a microphone");
            return;
        }
    };
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
        let capture = match microphone::open_in(&graph.context, &asked, heard, ended).await {
            Ok(capture) => {
                *framer.borrow_mut() = Some(Framer::new(capture.rate(), RATE, FRAME_SAMPLES));
                opens.fetch_add(1, Ordering::Relaxed);
                log::info!(
                    "[CALL] microphone open at {} Hz, the browser cancelling echo: {:?}",
                    capture.rate(),
                    capture.cancels_echo()
                );
                // A context made after the wait for permission may have lost the click.
                let _ = graph.context.resume();
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

/// The speaker's node in the call's graph.
async fn open_speaker(graph: &Graph) -> Result<AudioWorkletNode, JsValue> {
    if !graph.worklets.clone().await {
        return Err(JsValue::NULL);
    }
    let node = AudioWorkletNode::new(&graph.context, "zapfast-speaker")?;
    node.connect_with_audio_node(&graph.context.destination())?;
    Ok(node)
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

/// How a call's sound has gone, for the log, which counts and never holds any of it.
#[derive(Default)]
struct AudioReport {
    frames: Cell<u64>,
    /// The longest the page went between two frames, in milliseconds. They come every 60 ms or so,
    /// so what is far past that is the page or the network holding them up.
    longest_gap_ms: Cell<u128>,
    /// What the worklet last said: times run dry, samples let go, its lead and what it holds.
    worklet: Cell<[f64; 4]>,
}

impl AudioReport {
    fn log(&self, when: &str) {
        let [underruns, trimmed, lead, held] = self.worklet.get();
        log::info!(
            "[CALL] speaker {when}: {} frames, longest gap {} ms, ran dry {underruns} times, let go \
             {} ms, holding {held:.0} ms against a lead of {lead:.0} ms",
            self.frames.get(),
            self.longest_gap_ms.get(),
            (trimmed / 16.0).round(),
        );
    }
}

async fn play_pump(
    frames: async_channel::Receiver<Vec<i16>>,
    swaps: async_channel::Receiver<Option<String>>,
    target: Option<String>,
    opens: Arc<AtomicUsize>,
) {
    let opened = match graph() {
        Ok(graph) => open_speaker(&graph).await.map(|node| (graph, node)),
        Err(error) => Err(error),
    };
    let Ok((graph, node)) = opened else {
        log::warn!("[CALL] the browser would not open a speaker");
        // Keeps the engine's frames going nowhere, rather than backing it up.
        while frames.recv().await.is_ok() {}
        return;
    };
    let context = graph.context.clone();
    opens.fetch_add(1, Ordering::Relaxed);
    if target.is_some() {
        choose_speaker(&context, target.as_deref());
    }
    let report = Rc::new(AudioReport::default());
    let told = {
        let report = Rc::clone(&report);
        wasm_bindgen::closure::Closure::<dyn FnMut(MessageEvent)>::new(
            move |event: MessageEvent| {
                let said = Array::from(&event.data());
                let number = |at| said.get(at).as_f64().unwrap_or(0.0);
                report
                    .worklet
                    .set([number(0), number(1), number(2), number(3)]);
            },
        )
    };
    if let Ok(port) = node.port() {
        port.set_onmessage(Some(told.as_ref().unchecked_ref()));
    }
    let mut last_frame: Option<Instant> = None;
    let mut reported = Instant::now();
    loop {
        match select(Box::pin(frames.recv()), Box::pin(swaps.recv())).await {
            Either::Left((Ok(frame), _)) => {
                // A context can start out held back until the page has been clicked.
                if context.state() == AudioContextState::Suspended {
                    let _ = context.resume();
                }
                if let Some(last) = last_frame {
                    let gap = last.elapsed().as_millis();
                    report
                        .longest_gap_ms
                        .set(report.longest_gap_ms.get().max(gap));
                }
                last_frame = Some(Instant::now());
                report.frames.set(report.frames.get() + 1);
                play(&node, &frame);
                if reported.elapsed() >= AUDIO_REPORT {
                    reported = Instant::now();
                    report.log("so far");
                    report.longest_gap_ms.set(0);
                }
            }
            Either::Right((Ok(next), _)) => choose_speaker(&context, next.as_deref()),
            // The engine is done, or the call has told this to stop.
            _ => break,
        }
    }
    report.log("at the end");
    if let Ok(port) = node.port() {
        port.set_onmessage(None);
    }
    let _ = node.disconnect();
}
