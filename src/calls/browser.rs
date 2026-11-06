//! A call's video in a browser. The desktop's camera and H.264 run on threads through openh264;
//! here WebCodecs does the coding off the page's thread, and the same rate control steers it. A
//! browser without H.264 in WebCodecs says so in the log, and the call goes on without our picture
//! or the peer's, as it does on the desktop when a camera cannot start.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use js_sys::{Object, Promise, Reflect, Uint8Array};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{HtmlVideoElement, MediaStream, MediaStreamConstraints, MediaStreamTrack};

use super::*;
use crate::web::video::Copier;

/// Constrained Baseline at level 3.1, which covers the largest step and is what peers expect.
const ENCODED_AS: &str = "avc1.42E01F";

/// How far a decoder may fall behind before its picture is given up on until the next keyframe.
const DECODE_BACKLOG: u32 = 16;

/// How far an encoder may fall behind before a picture is skipped.
const ENCODE_BACKLOG: u32 = 2;

/// The longest side of a peer's picture on a one-to-one call. Larger costs more to copy out of the
/// browser than a window shows.
const LARGEST_PICTURE: u32 = 1280;

/// How often the encoder's rate may change, as each change reconfigures it.
const RATE_CHANGE_GAP: std::time::Duration = std::time::Duration::from_secs(1);

// web-sys offers these only behind an unstable flag.
#[wasm_bindgen]
extern "C" {
    type VideoEncoder;

    #[wasm_bindgen(constructor, catch)]
    fn new(init: &JsValue) -> Result<VideoEncoder, JsValue>;

    #[wasm_bindgen(static_method_of = VideoEncoder, js_name = isConfigSupported, catch)]
    fn is_config_supported(config: &JsValue) -> Result<Promise, JsValue>;

    #[wasm_bindgen(method, catch)]
    fn configure(this: &VideoEncoder, config: &JsValue) -> Result<(), JsValue>;

    #[wasm_bindgen(method, catch)]
    fn encode(this: &VideoEncoder, frame: &JsFrame, options: &JsValue) -> Result<(), JsValue>;

    #[wasm_bindgen(method, getter, js_name = encodeQueueSize)]
    fn encode_queue_size(this: &VideoEncoder) -> u32;

    #[wasm_bindgen(method, catch)]
    fn close(this: &VideoEncoder) -> Result<(), JsValue>;

    type VideoDecoder;

    #[wasm_bindgen(js_name = VideoDecoder, constructor, catch)]
    fn new_decoder(init: &JsValue) -> Result<VideoDecoder, JsValue>;

    #[wasm_bindgen(static_method_of = VideoDecoder, js_class = "VideoDecoder", js_name = isConfigSupported, catch)]
    fn decoder_config_supported(config: &JsValue) -> Result<Promise, JsValue>;

    #[wasm_bindgen(method, js_class = "VideoDecoder", js_name = configure, catch)]
    fn configure_decoder(this: &VideoDecoder, config: &JsValue) -> Result<(), JsValue>;

    #[wasm_bindgen(method, js_class = "VideoDecoder", js_name = decode, catch)]
    fn decode(this: &VideoDecoder, chunk: &EncodedVideoChunk) -> Result<(), JsValue>;

    #[wasm_bindgen(method, getter, js_class = "VideoDecoder", js_name = decodeQueueSize)]
    fn decode_queue_size(this: &VideoDecoder) -> u32;

    #[wasm_bindgen(method, js_class = "VideoDecoder", js_name = close, catch)]
    fn close_decoder(this: &VideoDecoder) -> Result<(), JsValue>;

    #[wasm_bindgen(js_name = VideoFrame)]
    type JsFrame;

    #[wasm_bindgen(js_class = "VideoFrame", constructor, catch)]
    fn from_video(video: &HtmlVideoElement, init: &JsValue) -> Result<JsFrame, JsValue>;

    #[wasm_bindgen(method, getter, js_class = "VideoFrame", js_name = displayWidth)]
    fn display_width(this: &JsFrame) -> u32;

    #[wasm_bindgen(method, getter, js_class = "VideoFrame", js_name = displayHeight)]
    fn display_height(this: &JsFrame) -> u32;

    #[wasm_bindgen(method, js_class = "VideoFrame", js_name = close)]
    fn close_frame(this: &JsFrame);

    type EncodedVideoChunk;

    #[wasm_bindgen(constructor, catch)]
    fn new(init: &JsValue) -> Result<EncodedVideoChunk, JsValue>;

    #[wasm_bindgen(method, getter, js_name = byteLength)]
    fn byte_length(this: &EncodedVideoChunk) -> u32;

    #[wasm_bindgen(method, getter)]
    fn timestamp(this: &EncodedVideoChunk) -> f64;

    #[wasm_bindgen(method, getter, js_name = type)]
    fn kind(this: &EncodedVideoChunk) -> String;

    #[wasm_bindgen(method, js_name = copyTo, catch)]
    fn copy_to(this: &EncodedVideoChunk, target: &Uint8Array) -> Result<(), JsValue>;
}

fn object(fields: &[(&str, JsValue)]) -> Result<JsValue, JsValue> {
    let object = Object::new();
    for (name, value) in fields {
        Reflect::set(&object, &JsValue::from_str(name), value)?;
    }
    Ok(object.into())
}

/// A JavaScript error's words, for an error that goes no further than a log.
fn js_error(what: &str, error: JsValue) -> anyhow::Error {
    let name = error
        .dyn_ref::<js_sys::Error>()
        .map_or_else(|| "an error".to_owned(), |error| String::from(error.name()));
    anyhow!("{what}: {name}")
}

fn encoder_config(size: (usize, usize), bitrate: u32, fps: u32) -> Result<JsValue, JsValue> {
    object(&[
        ("codec", ENCODED_AS.into()),
        ("width", (size.0 as u32).into()),
        ("height", (size.1 as u32).into()),
        ("bitrate", bitrate.into()),
        ("framerate", fps.into()),
        ("latencyMode", "realtime".into()),
        ("avc", object(&[("format", "annexb".into())])?),
    ])
}

/// Asks for the camera, up to the largest size a call sends.
async fn open_camera(device: Option<&str>) -> Result<MediaStream, JsValue> {
    let ideal = |value: u32| object(&[("ideal", value.into())]);
    let mut wanted = vec![
        ("width", ideal(CAPTURE_LONG_SIDE as u32)?),
        ("height", ideal(CAPTURE_SHORT_SIDE as u32)?),
        ("frameRate", ideal(VIDEO_FPS)?),
    ];
    if let Some(device) = device {
        wanted.push(("deviceId", object(&[("exact", device.into())])?));
    }
    let asked = MediaStreamConstraints::new();
    asked.set_video(&object(&wanted)?);
    JsFuture::from(
        web_sys::window()
            .ok_or(JsValue::NULL)?
            .navigator()
            .media_devices()?
            .get_user_media_with_constraints(&asked)?,
    )
    .await?
    .dyn_into()
}

/// The element the camera plays in, which a picture is taken from each frame. It has to be in the
/// document to play at all, so it is there at one pixel, unseen.
fn camera_element(stream: &MediaStream) -> Result<HtmlVideoElement, JsValue> {
    let document = web_sys::window()
        .and_then(|window| window.document())
        .ok_or(JsValue::NULL)?;
    let video: HtmlVideoElement = document.create_element("video")?.dyn_into()?;
    video.set_muted(true);
    video.set_attribute("playsinline", "")?;
    video.set_attribute(
        "style",
        "position:fixed;width:1px;height:1px;opacity:0;pointer-events:none",
    )?;
    video.set_src_object(Some(stream));
    document.body().ok_or(JsValue::NULL)?.append_child(&video)?;
    Ok(video)
}

/// Lets the camera go.
fn release(stream: &MediaStream, video: &HtmlVideoElement) {
    for track in stream.get_tracks().iter() {
        if let Ok(track) = track.dyn_into::<MediaStreamTrack>() {
            track.stop();
        }
    }
    video.set_src_object(None);
    video.remove();
}

/// What a picture handed to the encoder is waiting to become.
struct Pending {
    timestamp: f64,
    captured: Instant,
    stamp: u32,
}

/// What the encoder's output hands on to the call's engine.
struct Output {
    pending: RefCell<VecDeque<Pending>>,
    timed: async_channel::Sender<TimedVideoFrame>,
    rate: Arc<RateShare>,
    diag: Arc<crate::call_diag::CallVideo>,
    /// Set when a frame did not reach the peer: its decoder has lost its reference, so nothing
    /// but a frame it can start from is worth sending.
    resync: Cell<bool>,
}

impl Output {
    fn take(&self, chunk: &EncodedVideoChunk) {
        let at = chunk.timestamp();
        let pending = {
            let mut pending = self.pending.borrow_mut();
            // An encoder may skip a picture, which never comes back.
            while pending.front().is_some_and(|next| next.timestamp < at) {
                pending.pop_front();
            }
            pending.pop_front()
        };
        let Some(pending) = pending else {
            return;
        };
        // Led by an access-unit delimiter, as the desktop's encoder's are, so the peer's
        // depacketizer sees the boundaries an encoder would have produced.
        const DELIMITER: [u8; 6] = [0x00, 0x00, 0x00, 0x01, 0x09, 0xF0];
        let body = Uint8Array::new_with_length(chunk.byte_length());
        if chunk.copy_to(&body).is_err() {
            return;
        }
        let mut unit = Vec::with_capacity(DELIMITER.len() + body.length() as usize);
        unit.extend_from_slice(&DELIMITER);
        unit.extend(body.to_vec());
        self.diag.counters.sent(&unit);
        self.diag.sent.write(&unit);
        let keyframe = chunk.kind() == "key";
        let len = unit.len();
        let captured_at = whatsapp_rust::wacore::time::Instant::now() - pending.captured.elapsed();
        let frame = TimedVideoFrame::builder()
            .data(unit)
            .timestamp(pending.stamp)
            .captured_at(captured_at)
            .build();
        if self.timed.try_send(frame).is_err() {
            self.resync.set(true);
            self.rate.note_queue_drop();
        } else {
            self.rate.note_sent(len);
            if keyframe {
                self.resync.set(false);
            }
        }
    }
}

/// The browser's encoder, for as long as the camera runs.
struct Encoder {
    encoder: VideoEncoder,
    _output: Closure<dyn FnMut(EncodedVideoChunk, JsValue)>,
    _error: Closure<dyn FnMut(JsValue)>,
}

impl Encoder {
    fn new(output: &Rc<Output>, failed: &Rc<Cell<bool>>) -> Result<Self, JsValue> {
        let chunks = {
            let output = Rc::clone(output);
            Closure::<dyn FnMut(EncodedVideoChunk, JsValue)>::new(
                move |chunk: EncodedVideoChunk, _: JsValue| output.take(&chunk),
            )
        };
        let error = {
            let failed = Rc::clone(failed);
            Closure::<dyn FnMut(JsValue)>::new(move |_: JsValue| failed.set(true))
        };
        let encoder = VideoEncoder::new(&object(&[
            ("output", chunks.as_ref().clone()),
            ("error", error.as_ref().clone()),
        ])?)?;
        Ok(Self {
            encoder,
            _output: chunks,
            _error: error,
        })
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.encoder.close();
    }
}

/// The camera side of a call in a browser: the same interface as the desktop's.
pub(super) struct CameraCapture {
    pub(super) timed: async_channel::Receiver<TimedVideoFrame>,
    /// Cleared when the camera is no longer wanted, and when it ends by itself.
    running: Arc<AtomicBool>,
    /// Set when the engine needs a keyframe; the next frame is encoded as one.
    pub(super) keyframe: Arc<AtomicBool>,
}

impl CameraCapture {
    /// Opens the camera and the encoder, and returns once both are up, so a call never signals
    /// video it cannot send.
    pub(super) async fn start(
        device: Option<String>,
        ticks: async_channel::Sender<VideoTick>,
        rate: Arc<RateShare>,
        diag: Arc<crate::call_diag::CallVideo>,
    ) -> Result<Self> {
        let first = rate.load();
        let config = encoder_config(
            (first.tier().long_side, first.tier().short_side),
            rate.encoder_bitrate(first),
            first.tier().fps,
        )
        .map_err(|error| js_error("the camera encoder", error))?;
        let support = JsFuture::from(
            VideoEncoder::is_config_supported(&config)
                .map_err(|_| anyhow!("this browser has no video encoder"))?,
        )
        .await
        .map_err(|error| js_error("the camera encoder", error))?;
        if !Reflect::get(&support, &"supported".into()).is_ok_and(|supported| supported.is_truthy())
        {
            return Err(anyhow!("this browser cannot encode H.264"));
        }
        let stream = open_camera(device.as_deref())
            .await
            .map_err(|error| js_error("the camera", error))?;
        let video = camera_element(&stream).map_err(|error| js_error("the camera", error))?;
        // It plays once it is asked to, and its size is known once it has.
        let played = match video.play() {
            Ok(playing) => crate::platform::time::timeout(CAMERA_STARTUP, JsFuture::from(playing))
                .await
                .map_err(|_| anyhow!("the camera did not start in time"))
                .and_then(|played| played.map_err(|error| js_error("the camera", error))),
            Err(error) => Err(js_error("the camera", error)),
        };
        if let Err(error) = played {
            release(&stream, &video);
            return Err(error);
        }
        let (frames, timed) = async_channel::bounded::<TimedVideoFrame>(4);
        let running = Arc::new(AtomicBool::new(true));
        let keyframe = Arc::new(AtomicBool::new(false));
        wasm_bindgen_futures::spawn_local(capture_frames(
            stream,
            video,
            Controls {
                running: Arc::clone(&running),
                keyframe: Arc::clone(&keyframe),
                rate,
                diag,
            },
            frames,
            ticks,
        ));
        Ok(Self {
            timed,
            running,
            keyframe,
        })
    }

    /// Ends the camera, which the capture sees before its next frame.
    pub(super) fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }

    pub(super) fn running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

impl Drop for CameraCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Controls {
    running: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    rate: Arc<RateShare>,
    diag: Arc<crate::call_diag::CallVideo>,
}

/// Takes a picture from the camera every frame until the call stops it, encoding each.
async fn capture_frames(
    stream: MediaStream,
    video: HtmlVideoElement,
    controls: Controls,
    timed: async_channel::Sender<TimedVideoFrame>,
    ticks: async_channel::Sender<VideoTick>,
) {
    let failed = Rc::new(Cell::new(false));
    let output = Rc::new(Output {
        pending: RefCell::default(),
        timed,
        rate: Arc::clone(&controls.rate),
        diag: Arc::clone(&controls.diag),
        resync: Cell::new(false),
    });
    match Encoder::new(&output, &failed) {
        Ok(encoder) => {
            run_capture(
                &stream, &video, &controls, &encoder, &output, &failed, &ticks,
            )
            .await;
        }
        Err(error) => log::warn!("{}", js_error("[CALL] the camera encoder", error)),
    }
    controls.running.store(false, Ordering::Relaxed);
    release(&stream, &video);
    log::info!("[CALL] camera disabled");
}

async fn run_capture(
    stream: &MediaStream,
    video: &HtmlVideoElement,
    controls: &Controls,
    encoder: &Encoder,
    output: &Rc<Output>,
    failed: &Rc<Cell<bool>>,
    ticks: &async_channel::Sender<VideoTick>,
) {
    let copier = Copier::new();
    let rate = &controls.rate;
    let mut setting = rate.load();
    let mut applied: Option<(usize, u32)> = None;
    let mut applied_at: Option<Instant> = None;
    let mut clock = FrameClock::new(Instant::now(), f64::from(setting.tier().fps));
    let mut last_keyframe: Option<Instant> = None;
    let mut resynced_at: Option<Instant> = None;
    let mut preview_turn = false;
    let began = Instant::now();
    let tracks: Vec<MediaStreamTrack> = stream
        .get_tracks()
        .iter()
        .filter_map(|track| track.dyn_into().ok())
        .collect();
    log::info!("[CALL] camera enabled");
    loop {
        let fps = setting.tier().fps.max(1);
        crate::platform::time::sleep(std::time::Duration::from_millis(u64::from(1000 / fps))).await;
        if !controls.running.load(Ordering::Relaxed) {
            return;
        }
        if tracks
            .iter()
            .all(|track| track.ready_state() == web_sys::MediaStreamTrackState::Ended)
        {
            log::warn!("[CALL] the camera stopped delivering frames");
            return;
        }
        if failed.get() {
            log::warn!("[CALL] the camera encoder stopped");
            return;
        }
        let (width, height) = (video.video_width() as usize, video.video_height() as usize);
        if width == 0 || height == 0 {
            continue;
        }
        // A congested data channel leaves part of what was sent unsent, which is as much a
        // lost reference as a full queue here.
        if crate::web::relay::take_shed_units() > 0 {
            output.resync.set(true);
            rate.note_queue_drop();
        }

        let wanted = rate.load();
        let bitrate = rate.encoder_bitrate(wanted);
        // The rate moves in small steps, and each change reconfigures the encoder.
        let new_tier = applied.is_none_or(|(tier, _)| tier != wanted.tier);
        let new_rate = applied.is_some_and(|(_, at)| at != bitrate)
            && applied_at.is_none_or(|at| at.elapsed() >= RATE_CHANGE_GAP);
        if new_tier || new_rate {
            let tier = wanted.tier();
            let size = crate::camera::fit_even((width, height), (tier.long_side, tier.short_side));
            match encoder_config(size, bitrate, tier.fps)
                .and_then(|config| encoder.encoder.configure(&config))
            {
                Ok(()) => {
                    if new_tier {
                        clock.respace(f64::from(tier.fps));
                        log::info!(
                            "[CALL] camera now sends {}x{} at {} fps, {} kbps",
                            size.0,
                            size.1,
                            tier.fps,
                            bitrate / 1000
                        );
                    }
                    setting = wanted;
                    applied = Some((wanted.tier, bitrate));
                    applied_at = Some(Instant::now());
                }
                Err(_) => {
                    log::warn!("[CALL] the camera encoder could not be reconfigured");
                    continue;
                }
            }
        }

        if encoder.encoder.encode_queue_size() <= ENCODE_BACKLOG {
            let resync_due =
                output.resync.get() && resynced_at.is_none_or(|at| at.elapsed() >= RESYNC_GAP);
            // A delta frame the peer cannot decode only adds to a queue that is already full.
            if !output.resync.get() || resync_due {
                let backstop = last_keyframe.is_none_or(|at| at.elapsed() >= KEYFRAME_EVERY);
                let keyframe =
                    controls.keyframe.swap(false, Ordering::Relaxed) || resync_due || backstop;
                if resync_due {
                    resynced_at = Some(Instant::now());
                }
                if keyframe {
                    last_keyframe = Some(Instant::now());
                }
                let captured = Instant::now();
                let timestamp = captured.duration_since(began).as_micros() as f64;
                let made = object(&[("timestamp", timestamp.into())])
                    .and_then(|init| JsFrame::from_video(video, &init));
                if let Ok(frame) = made {
                    output.pending.borrow_mut().push_back(Pending {
                        timestamp,
                        captured,
                        stamp: clock.stamp(captured),
                    });
                    let options = object(&[("keyFrame", keyframe.into())]).unwrap_or(JsValue::NULL);
                    if encoder.encoder.encode(&frame, &options).is_err() {
                        log::warn!("[CALL] a camera frame could not be encoded");
                    }
                    frame.close_frame();
                }
            }
        }

        // Every other frame: a corner of the window does not need them all, and each costs a
        // copy out of the browser.
        preview_turn = !preview_turn;
        if preview_turn && let Some(copier) = &copier {
            let shown = preview_size((width, height));
            if let Some(image) = copier.picture(video, shown.0.max(shown.1) as u32) {
                let _ = ticks.try_send(VideoTick::Local(Arc::new(image)));
            }
        }
    }
}

/// One sender's picture being decoded.
struct RemoteStream {
    decoder: Option<RemoteDecoder>,
    /// Whether a keyframe has arrived since the decoder last lost track.
    started: bool,
    waiting: FirstKeyframe,
    /// The peer's camera rotation, which each frame carries and the pictures take.
    orientation: Rc<Cell<u8>>,
    /// Set when the browser's decoder gave up on the stream.
    broke: Rc<Cell<bool>>,
    next_timestamp: f64,
}

struct RemoteDecoder {
    decoder: VideoDecoder,
    _output: Closure<dyn FnMut(JsFrame)>,
    _error: Closure<dyn FnMut(JsValue)>,
}

impl Drop for RemoteDecoder {
    fn drop(&mut self) {
        let _ = self.decoder.close_decoder();
    }
}

async fn new_decoder(
    codec: &str,
    key: &Option<Jid>,
    ticks: &async_channel::Sender<VideoTick>,
    diag: &Arc<crate::call_diag::CallVideo>,
    stream: &RemoteStream,
) -> Result<RemoteDecoder, JsValue> {
    let config = object(&[("codec", codec.into()), ("optimizeForLatency", true.into())])?;
    let support = JsFuture::from(VideoDecoder::decoder_config_supported(&config)?).await?;
    if !Reflect::get(&support, &"supported".into())?.is_truthy() {
        return Err(JsValue::NULL);
    }
    let copier = Copier::new().ok_or(JsValue::NULL)?;
    let frames = {
        let key = key.clone();
        let ticks = ticks.clone();
        let diag = Arc::clone(diag);
        let orientation = Rc::clone(&stream.orientation);
        Closure::<dyn FnMut(JsFrame)>::new(move |frame: JsFrame| {
            diag.counters.decoded();
            let size = (
                frame.display_width() as usize,
                frame.display_height() as usize,
            );
            // A member's picture fills a tile of a grid, never the window.
            let side = if key.is_some() {
                let tile = tile_picture(size);
                tile.0.max(tile.1) as u32
            } else {
                LARGEST_PICTURE
            };
            let source: &JsValue = frame.as_ref();
            let picture = copier.picture_of(source, size.0 as u32, size.1 as u32, side);
            frame.close_frame();
            let Some(picture) = picture else {
                return;
            };
            let image = Arc::new(crate::video::turned(
                picture,
                crate::video::orientation_turns(orientation.get()),
            ));
            let tick = match &key {
                Some(sender) => VideoTick::RemoteFrom {
                    sender: sender.clone(),
                    image,
                },
                None => VideoTick::Remote(image),
            };
            let _ = ticks.try_send(tick);
        })
    };
    let error = {
        let broke = Rc::clone(&stream.broke);
        Closure::<dyn FnMut(JsValue)>::new(move |_: JsValue| broke.set(true))
    };
    let decoder = VideoDecoder::new_decoder(&object(&[
        ("output", frames.as_ref().clone()),
        ("error", error.as_ref().clone()),
    ])?)?;
    decoder.configure_decoder(&config)?;
    Ok(RemoteDecoder {
        decoder,
        _output: frames,
        _error: error,
    })
}

/// Decodes the peer's access units in the browser, and hands the pictures to the window.
pub(super) fn decode_remote(
    frames: async_channel::Receiver<VideoFrame>,
    ticks: async_channel::Sender<VideoTick>,
    diag: Arc<crate::call_diag::CallVideo>,
) {
    wasm_bindgen_futures::spawn_local(async move {
        let mut streams: HashMap<Option<Jid>, RemoteStream> = HashMap::new();
        // The order the senders began in, so the cap lets go of the oldest stream.
        let mut order: VecDeque<Option<Jid>> = VecDeque::new();
        let mut profile = None;
        let mut refused = false;
        while let Ok(frame) = frames.recv().await {
            diag.received.write(&frame.data);
            let seen = crate::call_diag::stream_profile(&frame.data);
            if let Some(seen) = seen
                && profile != Some(seen)
            {
                profile = Some(seen);
                log::info!(
                    "[CALL] the peer's video is H.264 {} (profile_idc={} constraints=0x{:02x} level_idc={})",
                    seen.name(),
                    seen.profile,
                    seen.constraints,
                    seen.level
                );
            }
            let key = frame.sender.as_ref().map(Jid::to_non_ad);
            if !streams.contains_key(&key) {
                if order.len() >= MAX_REMOTE_STREAMS
                    && let Some(oldest) = order.pop_front()
                {
                    streams.remove(&oldest);
                }
                order.push_back(key.clone());
                streams.insert(
                    key.clone(),
                    RemoteStream {
                        decoder: None,
                        started: false,
                        waiting: FirstKeyframe::default(),
                        orientation: Rc::default(),
                        broke: Rc::default(),
                        next_timestamp: 0.0,
                    },
                );
            }
            let Some(stream) = streams.get_mut(&key) else {
                continue;
            };
            let backlog = stream
                .decoder
                .as_ref()
                .is_some_and(|remote| remote.decoder.decode_queue_size() > DECODE_BACKLOG);
            if stream.broke.replace(false) || backlog {
                diag.counters.undecodable();
                stream.decoder = None;
                stream.started = false;
            }
            if !stream.started {
                if !frame.keyframe {
                    // None of this can be decoded until a keyframe, and a peer sends one only
                    // when asked.
                    if stream.waiting.ask(Instant::now()) {
                        let _ = ticks.try_send(VideoTick::Lost);
                    }
                    continue;
                }
                stream.started = true;
            }
            stream.orientation.set(frame.orientation & 0x03);
            if stream.decoder.is_none() {
                let codec = seen.map_or_else(
                    || ENCODED_AS.to_owned(),
                    |seen| {
                        format!(
                            "avc1.{:02X}{:02X}{:02X}",
                            seen.profile, seen.constraints, seen.level
                        )
                    },
                );
                match new_decoder(&codec, &key, &ticks, &diag, stream).await {
                    Ok(decoder) => stream.decoder = Some(decoder),
                    Err(_) => {
                        if !refused {
                            refused = true;
                            log::warn!("[CALL] this browser would not decode the peer's video");
                        }
                        stream.started = false;
                        continue;
                    }
                }
            }
            let Some(remote) = &stream.decoder else {
                continue;
            };
            // Copied into a buffer of the browser's own.
            let timestamp = stream.next_timestamp;
            stream.next_timestamp += 40_000.0;
            let chunk = object(&[
                ("type", if frame.keyframe { "key" } else { "delta" }.into()),
                ("timestamp", timestamp.into()),
                ("data", Uint8Array::from(frame.data.as_slice()).into()),
            ])
            .and_then(|init| EncodedVideoChunk::new(&init));
            match chunk.and_then(|chunk| remote.decoder.decode(&chunk)) {
                Ok(()) => {}
                Err(_) => {
                    diag.counters.undecodable();
                    let _ = ticks.try_send(VideoTick::Lost);
                    stream.broke.set(true);
                }
            }
        }
    });
}
