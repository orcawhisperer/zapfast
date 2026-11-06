//! A call's microphone and speaker in a browser. The same interface as the desktop's, for the same
//! engine: 60 ms frames of 16 kHz samples each way, behind channels that stay put when a device
//! changes. It only names devices it has been allowed to use.
//!
//! One audio node runs both sides on the browser's audio thread, which is the one thread here that
//! keeps time. The sound's lead over the speaker is held there, as the page's thread stalls: a
//! stall delays the sound, and a lead large enough rides it out.
//!
//! The echo is taken out here too, by the desktop's own chain, on the microphone as it comes. A
//! browser's canceller runs WebRTC's stock settings, made for a short and mostly direct echo: with
//! a clipping speaker and a long tail, Brave took 1 dB off and Zen 5, where this chain takes 18. The
//! node hands back what it heard and what it played side by side, so the two are lined up whatever
//! the page's thread was doing, and the chain finds the distance the room adds.
//!
//! A one-to-one call takes its sound from the library as packets ([`whatsapp_codec`]), not as the
//! samples of its playout timer. That timer gives up the ticks it missed, each of them 20 ms of
//! sound that never reaches the speaker, and a page's thread misses plenty: the lead here then runs
//! out with the sound still queued in the library. Packets decoded as they come lose nothing to a
//! stall. A group's call mixes several people in the library, so it still comes through the timer.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use bytes::Bytes;
use futures::FutureExt;
use futures::future::{Either, LocalBoxFuture, Shared, select};
use js_sys::{Array, Float32Array, Object, Reflect};
use wacore::voip::rtp::RTP_PAYLOAD_TYPE_MLOW_RED;
use wacore::voip::{AudioCodec, EncodedAudioFrame, MlowDecoder, MlowEncoder};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AudioContext, AudioContextState, AudioWorkletNode, Event, MediaStream,
    MediaStreamAudioSourceNode, MediaStreamTrack, MessageEvent,
};

#[path = "../call_audio/chain.rs"]
pub mod chain;

use crate::pcm::{Framer, PacketOrder, conceal_into, fade_in};
use crate::platform::Instant;
use crate::web::microphone;

/// The engine's rate.
pub const RATE: u32 = 16_000;
/// Samples in one of the engine's frames: 60 ms.
pub const FRAME_SAMPLES: usize = 960;
/// One chunk of the echo canceller: 10 ms.
const CHUNK: usize = RATE as usize / 100;
/// How many frames may wait for the speaker. Plenty, as a stalled page should lose none of them:
/// the node decides what is too late.
const SPEAKER_QUEUE: usize = 32;
/// How many frames may wait for the engine, for the same reason.
const MICROPHONE_QUEUE: usize = 8;
/// How often a call says how its sound is going.
const AUDIO_REPORT: std::time::Duration = std::time::Duration::from_secs(30);
/// The one-byte comfort-noise packet that stands for a muted frame, as the engine sends it, so the
/// peer's media timer stays fed while nothing is said.
const COMFORT_NOISE: [u8; 1] = [0x90];
/// How much of a lost stretch is filled in, in packets. Past this it is plainly a gap.
const CONCEALED_PACKETS: u16 = 2;

/// Runs both ends of a call's sound.
///
/// The speaker holds a lead before it starts, as WhatsApp sends several frames to a packet and the
/// page's thread stalls on top of that. What the lead never needed over a few seconds is only
/// delay, which goes first with frames that are nearly silent and otherwise by playing very slightly
/// fast, neither of which is heard the way a jump is. Running dry fades rather than clicks, and
/// holds more next time, unless it was only the peer going quiet.
///
/// The microphone goes in, what is played comes out, and both go back to the page side by side.
const CALL_WORKLET: &str = r#"
const RATE = 16000;
const QUIET = 3e-5;
registerProcessor("zapfast-call", class extends AudioWorkletProcessor {
    queue = [];
    held = 0;
    at = 0;
    lead = 0.2 * RATE;
    playing = false;
    gain = 0;
    tail = 0;
    last = 0;
    lowest = Infinity;
    watched = 0;
    spare = 0;
    wasQuiet = true;
    underruns = 0;
    trimmed = 0;
    shed = 0;
    calm = 0;
    reported = 0;
    size = Math.round(sampleRate / 100);
    pair = new Float32Array(2 * this.size);
    filled = 0;
    ramp = 1 / (0.005 * sampleRate);
    decay = Math.exp(-1 / (0.002 * sampleRate));
    constructor() {
        super();
        this.port.onmessage = ({ data }) => data ? this.arrive(data) : this.flush();
    }
    arrive(frame) {
        if (!frame.length) return;
        if (this.playing) this.lowest = Math.min(this.lowest, this.held);
        let energy = 0;
        for (let index = 0; index < frame.length; index++) energy += frame[index] * frame[index];
        frame.quiet = energy / frame.length < QUIET;
        this.queue.push(frame);
        this.held += frame.length;
        while (this.held > RATE && this.queue.length > 1) {
            const gone = this.queue.shift();
            const left = gone.length - this.at;
            this.held -= left;
            this.trimmed += left;
            this.at = 0;
            this.join();
        }
    }
    flush() {
        this.queue = [];
        this.held = 0;
        this.at = 0;
        this.playing = false;
        this.tail = this.last;
    }
    // A jump in the sound: what was playing fades out as what follows fades in.
    join() {
        this.tail = this.last;
        this.gain = 0;
    }
    sample(extra) {
        const frame = this.queue[0];
        const whole = Math.floor(this.at);
        const fraction = this.at - whole;
        const next = whole + 1 < frame.length ? frame[whole + 1]
            : this.queue.length > 1 ? this.queue[1][0] : frame[whole];
        this.gain = Math.min(1, this.gain + this.ramp);
        const value = (frame[whole] + (next - frame[whole]) * fraction) * this.gain;
        const step = RATE / sampleRate + extra;
        this.at += step;
        this.held -= step;
        this.spare = Math.max(0, this.spare - extra);
        this.wasQuiet = frame.quiet;
        while (this.queue.length && this.at >= this.queue[0].length) {
            this.at -= this.queue.shift().length;
            while (this.queue.length > 1 && this.queue[0].quiet
                && this.spare >= this.queue[0].length) {
                const gone = this.queue.shift();
                this.held -= gone.length;
                this.spare -= gone.length;
                this.shed += gone.length;
                this.join();
            }
        }
        if (!this.queue.length) {
            this.at = 0;
            this.held = 0;
            this.playing = false;
            this.spare = 0;
            this.lowest = Infinity;
            this.watched = 0;
            // A peer going quiet stops sending, which is no reason to hold more.
            if (!this.wasQuiet) {
                this.underruns++;
                this.calm = 0;
                this.lead = Math.min(this.lead + 0.04 * RATE, 0.4 * RATE);
            }
        }
        return value;
    }
    process(inputs, outputs) {
        const out = outputs[0];
        const count = out.length ? out[0].length : 128;
        if (!this.playing && this.held >= this.lead) {
            this.playing = true;
            this.gain = 0;
        }
        const extra = (RATE / sampleRate) * (this.spare > 0.2 * RATE ? 0.05 : this.spare > 0 ? 0.025 : 0);
        const heard = inputs[0];
        const left = out[0];
        for (let index = 0; index < count; index++) {
            this.tail *= this.decay;
            const going = this.playing;
            const value = (going ? this.sample(extra) : 0) + this.tail;
            if (going && !this.playing) this.tail = value;
            this.last = value;
            if (left) left[index] = value;
            let sum = 0;
            for (let channel = 0; channel < heard.length; channel++) sum += heard[channel][index];
            this.pair[this.filled] = heard.length ? sum / heard.length : 0;
            this.pair[this.size + this.filled] = value;
            if (++this.filled === this.size) {
                this.port.postMessage(this.pair, [this.pair.buffer]);
                this.pair = new Float32Array(2 * this.size);
                this.filled = 0;
            }
        }
        for (let channel = 1; channel < out.length; channel++) out[channel].set(left);
        this.watched += count;
        if (this.watched > 3 * sampleRate) {
            this.watched = 0;
            this.spare = this.playing && this.lowest !== Infinity
                ? Math.max(0, this.lowest - 0.4 * this.lead) : 0;
            this.lowest = Infinity;
        }
        this.calm += count;
        if (this.calm > 30 * sampleRate) {
            this.calm = 0;
            this.lead = Math.max(0.2 * RATE, this.lead - 0.02 * RATE);
        }
        this.reported += count;
        if (this.reported > 5 * sampleRate) {
            this.reported = 0;
            this.port.postMessage([this.underruns, this.trimmed, this.lead / 16, this.held / 16, this.shed / 16]);
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

/// What the microphone is asked for: the sound as it comes, without the browser's own echo
/// cancelling and clean-up, as the call's chain does all of that.
fn constraints(device: Option<&str>) -> Result<JsValue, JsValue> {
    let asked = Object::new();
    for name in ["echoCancellation", "noiseSuppression", "autoGainControl"] {
        Reflect::set(&asked, &name.into(), &JsValue::FALSE)?;
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

/// How a call's sound has gone, for the log, which counts and never holds any of it.
#[derive(Default)]
struct AudioReport {
    frames: Cell<u64>,
    /// The longest the page went between two frames, in milliseconds. What is far past a packet's
    /// length is the page or the network holding them up.
    longest_gap_ms: Cell<u128>,
    /// What the node last said: times run dry, samples let go, its lead, what it holds, and how
    /// much it has let go of quiet frames, the last three in milliseconds.
    node: Cell<[f64; 5]>,
    /// How long the echo chain has taken, and over how many chunks. A chunk is 10 ms of sound, so
    /// this has to stay far under that on a thread that does everything else too.
    chain_ms: Cell<f64>,
    chunks: Cell<u64>,
}

impl AudioReport {
    fn log(&self, when: &str, echo: (f64, f64, i32)) {
        let [underruns, trimmed, lead, held, shed] = self.node.get();
        log::info!(
            "[CALL] speaker {when}: {} frames, longest gap {} ms, ran dry {underruns} times, let go \
             {} ms and {shed:.0} ms of quiet, holding {held:.0} ms against a lead of {lead:.0} ms; \
             echo chain {:.2} ms a chunk, taking {:.0} dB off an echo {:.0} dB down at {} ms",
            self.frames.get(),
            self.longest_gap_ms.get(),
            (trimmed / 16.0).round(),
            self.chain_ms.get() / self.chunks.get().max(1) as f64,
            echo.0,
            echo.1,
            echo.2,
        );
    }
}

/// Where a call's microphone goes: through the echo chain, and in frames to the engine.
struct Cleaner {
    chain: chain::Chain,
    heard: Framer,
    played: Framer,
    heard_chunks: VecDeque<Vec<i16>>,
    played_chunks: VecDeque<Vec<i16>>,
    frame: Vec<i16>,
    /// The engine's channel, while a microphone is wanted.
    out: Option<async_channel::Sender<Vec<i16>>>,
    muted: Arc<AtomicBool>,
    report: Rc<AudioReport>,
}

impl Cleaner {
    fn new(rate: u32, report: Rc<AudioReport>) -> Self {
        Self {
            chain: chain::Chain::new(&chain::ChainSettings::call()),
            heard: Framer::new(rate, RATE, CHUNK),
            played: Framer::new(rate, RATE, CHUNK),
            heard_chunks: VecDeque::new(),
            played_chunks: VecDeque::new(),
            frame: Vec::with_capacity(FRAME_SAMPLES),
            out: None,
            muted: Arc::default(),
            report,
        }
    }

    /// Takes the microphone and what was played at the same moment, one after the other.
    fn take(&mut self, pair: &[f32]) {
        let (heard, played) = pair.split_at(pair.len() / 2);
        let Self {
            heard_chunks,
            played_chunks,
            ..
        } = self;
        self.heard
            .push(heard, |chunk| heard_chunks.push_back(chunk));
        self.played
            .push(played, |chunk| played_chunks.push_back(chunk));
        let began = Instant::now();
        let mut chunks = 0;
        while let (Some(heard), Some(played)) = (
            self.heard_chunks.pop_front(),
            self.played_chunks.pop_front(),
        ) {
            let mut far = [0.0_f32; CHUNK];
            let mut near = [0.0_f32; CHUNK];
            for (to, from) in far.iter_mut().zip(&played) {
                *to = f32::from(*from) / f32::from(i16::MAX);
            }
            for (to, from) in near.iter_mut().zip(&heard) {
                *to = f32::from(*from) / f32::from(i16::MAX);
            }
            self.chain.render(&far);
            self.chain.capture(&mut near, None);
            chunks += 1;
            for sample in near {
                self.frame
                    .push((sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16);
            }
            if self.frame.len() >= FRAME_SAMPLES {
                let mut frame =
                    std::mem::replace(&mut self.frame, Vec::with_capacity(FRAME_SAMPLES));
                // Exact zeros are how the engine's path knows a frame is muted.
                if self.muted.load(Ordering::Relaxed) {
                    frame.fill(0);
                }
                if let Some(out) = &self.out {
                    // The engine fell behind: the newest sound is the one to keep.
                    let _ = out.force_send(frame);
                }
            }
        }
        if chunks > 0 {
            self.report.chunks.set(self.report.chunks.get() + chunks);
            self.report
                .chain_ms
                .set(self.report.chain_ms.get() + began.elapsed().as_secs_f64() * 1000.0);
        }
    }
}

/// The audio context a call's microphone and speaker share, closed when both have let it go.
struct Graph {
    context: AudioContext,
    /// The node both run in, once it has loaded.
    node: Shared<LocalBoxFuture<'static, Option<AudioWorkletNode>>>,
    cleaner: Rc<RefCell<Cleaner>>,
    report: Rc<AudioReport>,
    _told: Closure<dyn FnMut(MessageEvent)>,
}

impl Drop for Graph {
    fn drop(&mut self) {
        if let Some(Some(node)) = self.node.peek()
            && let Ok(port) = node.port()
        {
            port.set_onmessage(None);
        }
        let _ = self.context.close();
    }
}

thread_local! {
    static GRAPH: RefCell<Weak<Graph>> = const { RefCell::new(Weak::new()) };
}

/// What the canceller of the call under way reports: echo reduction, echo return loss, delay.
pub(crate) fn echo_state() -> Option<(f64, f64, i32)> {
    GRAPH
        .with_borrow(Weak::upgrade)
        .map(|graph| graph.cleaner.borrow().chain.metrics())
}

/// This call's graph, made by whichever of the two sides starts first.
fn graph() -> Result<Rc<Graph>, JsValue> {
    if let Some(graph) = GRAPH.with_borrow(Weak::upgrade) {
        return Ok(graph);
    }
    let context = AudioContext::new()?;
    let url = microphone::worklet_url(CALL_WORKLET)?;
    let loading = JsFuture::from(context.audio_worklet()?.add_module(&url)?);
    let report = Rc::new(AudioReport::default());
    let cleaner = Rc::new(RefCell::new(Cleaner::new(
        context.sample_rate() as u32,
        Rc::clone(&report),
    )));
    let told = {
        let cleaner = Rc::clone(&cleaner);
        let report = Rc::clone(&report);
        Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            let data = event.data();
            if let Some(pair) = data.dyn_ref::<Float32Array>() {
                cleaner.borrow_mut().take(&pair.to_vec());
            } else {
                let said = Array::from(&data);
                let number = |at| said.get(at).as_f64().unwrap_or(0.0);
                report
                    .node
                    .set([number(0), number(1), number(2), number(3), number(4)]);
            }
        })
    };
    let listener: js_sys::Function = told.as_ref().clone().unchecked_into();
    let node = {
        let context = context.clone();
        async move {
            let loaded = loading.await.is_ok();
            let _ = web_sys::Url::revoke_object_url(&url);
            if !loaded {
                return None;
            }
            let node = AudioWorkletNode::new(&context, "zapfast-call").ok()?;
            node.port().ok()?.set_onmessage(Some(&listener));
            node.connect_with_audio_node(&context.destination()).ok()?;
            Some(node)
        }
    }
    .boxed_local()
    .shared();
    let graph = Rc::new(Graph {
        context,
        node,
        cleaner,
        report,
        _told: told,
    });
    GRAPH.set(Rc::downgrade(&graph));
    Ok(graph)
}

/// What links a call's speaker to its microphone's echo canceller on the desktop. Here both run in
/// one node.
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
    muted: Arc<AtomicBool>,
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
        let muted = Arc::new(AtomicBool::new(false));
        wasm_bindgen_futures::spawn_local(mic_pump(
            out,
            swaps,
            fell,
            target,
            Arc::clone(&opens),
            Arc::clone(&muted),
        ));
        (
            Self {
                swap: Some(swap),
                fell_back,
                opens,
                muted,
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

    /// Silences what goes to the engine. The library silences a microphone it gets as samples, but
    /// not one it gets as packets, so this is all that stands between the peer and a muted room.
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }
}

/// A call's microphone, joined to the call's node. Dropping it lets the microphone go.
struct Joined {
    stream: MediaStream,
    source: MediaStreamAudioSourceNode,
    _ended: Closure<dyn FnMut(Event)>,
}

impl Joined {
    async fn open(
        graph: &Graph,
        node: &AudioWorkletNode,
        asked: &JsValue,
        mut ended: impl FnMut() + 'static,
    ) -> Result<Self, JsValue> {
        let stream = microphone::stream(asked).await?;
        let source = match graph.context.create_media_stream_source(&stream) {
            Ok(source) => source,
            Err(error) => {
                microphone::release(&stream);
                return Err(error);
            }
        };
        let joined = Self {
            stream,
            source,
            _ended: Closure::<dyn FnMut(Event)>::new(move |_: Event| ended()),
        };
        for track in joined.stream.get_tracks().iter() {
            if let Ok(track) = track.dyn_into::<MediaStreamTrack>() {
                track.set_onended(Some(joined._ended.as_ref().unchecked_ref()));
            }
        }
        joined.source.connect_with_audio_node(node)?;
        Ok(joined)
    }
}

impl Drop for Joined {
    fn drop(&mut self) {
        let _ = self.source.disconnect();
        microphone::release(&self.stream);
    }
}

/// Lets the engine's channel go when the pump ends, however it ends, unless a later call's pump has
/// taken over the same graph.
struct Listening {
    cleaner: Rc<RefCell<Cleaner>>,
    out: async_channel::Sender<Vec<i16>>,
}

impl Drop for Listening {
    fn drop(&mut self) {
        let mut cleaner = self.cleaner.borrow_mut();
        if cleaner
            .out
            .as_ref()
            .is_some_and(|out| out.same_channel(&self.out))
        {
            cleaner.out = None;
        }
    }
}

async fn mic_pump(
    out: async_channel::Sender<Vec<i16>>,
    swaps: async_channel::Receiver<Option<String>>,
    fell: async_channel::Sender<()>,
    mut target: Option<String>,
    opens: Arc<AtomicUsize>,
    muted: Arc<AtomicBool>,
) {
    let opened = match graph() {
        Ok(graph) => graph.node.clone().await.map(|node| (graph, node)),
        Err(_) => None,
    };
    let Some((graph, node)) = opened else {
        log::warn!("[CALL] the browser would not open a microphone");
        return;
    };
    {
        let mut cleaner = graph.cleaner.borrow_mut();
        cleaner.out = Some(out.clone());
        cleaner.muted = muted;
    }
    let _listening = Listening {
        cleaner: Rc::clone(&graph.cleaner),
        out,
    };
    loop {
        let (lost_tx, lost) = async_channel::bounded::<()>(1);
        let ended = move || {
            let _ = lost_tx.try_send(());
        };
        let asked = constraints(target.as_deref()).unwrap_or(JsValue::TRUE);
        let joined = match Joined::open(&graph, &node, &asked, ended).await {
            Ok(joined) => {
                opens.fetch_add(1, Ordering::Relaxed);
                log::info!(
                    "[CALL] microphone open at {} Hz, the browser cancelling echo itself: {:?}",
                    graph.context.sample_rate(),
                    microphone::cancels_echo(&joined.stream)
                );
                // A context made after the wait for permission may have lost the click.
                let _ = graph.context.resume();
                Some(joined)
            }
            Err(error) => {
                log::warn!("[CALL] {}", microphone::refusal(&error));
                None
            }
        };
        let fell_over = joined.is_none() && target.is_some();
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

/// Carries a one-to-one call's sound as packets of WhatsApp's own codec, the library's encoded
/// endpoints, between the engine's channels and the microphone's and speaker's: decoded as they
/// come, so the speaker's node is what keeps time. The format is the one the library would
/// otherwise run itself, so the peer sees no difference.
pub fn whatsapp_codec(
    microphone: async_channel::Receiver<Vec<i16>>,
    speaker: async_channel::Sender<Vec<i16>>,
) -> (
    async_channel::Receiver<Bytes>,
    async_channel::Sender<EncodedAudioFrame>,
) {
    let (packets, source) = async_channel::bounded::<Bytes>(MICROPHONE_QUEUE);
    let (sink, arriving) = async_channel::bounded::<EncodedAudioFrame>(64);
    wasm_bindgen_futures::spawn_local(encode_ours(microphone, packets));
    wasm_bindgen_futures::spawn_local(decode_theirs(arriving, speaker));
    (source, sink)
}

async fn encode_ours(
    microphone: async_channel::Receiver<Vec<i16>>,
    packets: async_channel::Sender<Bytes>,
) {
    let mut encoder = MlowEncoder::new();
    let mut packet = Vec::new();
    let mut refused = 0_u32;
    while let Ok(frame) = microphone.recv().await {
        // A muted frame goes without the codec, as the library's own path sends it.
        let encoded = if frame.iter().all(|&sample| sample == 0) {
            Bytes::from_static(&COMFORT_NOISE)
        } else if encoder.encode_i16_into(&frame, &mut packet).is_ok() {
            Bytes::copy_from_slice(&packet)
        } else {
            refused += 1;
            continue;
        };
        // The engine fell behind: the newest sound is the one to keep.
        if packets.force_send(encoded).is_err() {
            break;
        }
    }
    if refused > 0 {
        log::warn!("[CALL] the codec refused {refused} frames of the microphone");
    }
}

async fn decode_theirs(
    arriving: async_channel::Receiver<EncodedAudioFrame>,
    speaker: async_channel::Sender<Vec<i16>>,
) {
    let mut decoder = MlowDecoder::new();
    let mut order = PacketOrder::default();
    let mut last: Vec<i16> = Vec::new();
    let (mut played, mut lost_packets, mut late, mut foreign) = (0_u64, 0_u64, 0_u64, 0_u64);
    while let Ok(frame) = arriving.recv().await {
        if frame.codec != AudioCodec::Mlow {
            // Another codec needs libopus, which a page has none of.
            if foreign == 0 {
                log::warn!("[CALL] the peer's sound is in a codec this version cannot play");
            }
            foreign += 1;
            continue;
        }
        let Some(lost) = order.admit(frame.sequence_number) else {
            // Its place has been played: it would only scramble the sound, and the decoder.
            late += 1;
            continue;
        };
        lost_packets += u64::from(lost);
        for _ in 0..lost.min(CONCEALED_PACKETS) {
            let mut filled = Vec::with_capacity(last.len());
            conceal_into(&mut last, &mut filled);
            if !filled.is_empty() {
                let _ = speaker.force_send(filled);
            }
        }
        decoder.set_redundancy(i32::from(frame.payload_type == RTP_PAYLOAD_TYPE_MLOW_RED));
        let mut samples: Vec<i16> = decoder
            .decode(&frame.data)
            .into_iter()
            .map(|sample| (sample * 32_767.0).clamp(-32_768.0, 32_767.0) as i16)
            .collect();
        if lost > 0 {
            fade_in(&mut samples);
        }
        played += 1;
        last.clone_from(&samples);
        if speaker.force_send(samples).is_err() {
            break;
        }
    }
    log::info!(
        "[CALL] the peer's sound: {played} packets played, {lost_packets} lost, {late} too late, \
         {foreign} in another codec"
    );
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
    let opened = match graph() {
        Ok(graph) => graph.node.clone().await.map(|node| (graph, node)),
        Err(_) => None,
    };
    let Some((graph, node)) = opened else {
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
    let report = Rc::clone(&graph.report);
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
                    report.log("so far", graph.cleaner.borrow().chain.metrics());
                    report.longest_gap_ms.set(0);
                }
            }
            Either::Right((Ok(next), _)) => choose_speaker(&context, next.as_deref()),
            // The engine is done, or the call has told this to stop.
            _ => break,
        }
    }
    report.log("at the end", graph.cleaner.borrow().chain.metrics());
    // What is still queued is for a call that is over.
    if let Ok(port) = node.port() {
        let _ = port.post_message(&JsValue::NULL);
    }
}
