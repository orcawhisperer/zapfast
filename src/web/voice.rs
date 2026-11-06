//! Voice messages to Opus through the browser's own encoder, WebCodecs, since
//! libopus needs a C library that a page lacks. `crate::voice` wraps the packets in
//! the OGG file WhatsApp expects, as it does the desktop's.

use std::cell::RefCell;
use std::rc::Rc;

use js_sys::{Float32Array, Object, Promise, Reflect, Uint8Array};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use crate::voice::{BITRATE, FRAME, RATE};

/// The encoder's delay when it does not say: libopus's for this rate.
const USUAL_PRE_SKIP: u16 = 312;

/// Fed a second at a time, so a long take does not make one huge buffer.
const SLICE: usize = RATE as usize;

/// Silence after the take, to push its end out past the encoder's delay. What is
/// past the end is trimmed away when the packets are wrapped.
const TAIL: usize = 4 * FRAME;

// web-sys offers these only behind an unstable flag.
#[wasm_bindgen]
extern "C" {
    type AudioEncoder;

    #[wasm_bindgen(constructor, catch)]
    fn new(init: &JsValue) -> Result<AudioEncoder, JsValue>;

    #[wasm_bindgen(static_method_of = AudioEncoder, js_name = isConfigSupported, catch)]
    fn is_config_supported(config: &JsValue) -> Result<Promise, JsValue>;

    #[wasm_bindgen(method, catch)]
    fn configure(this: &AudioEncoder, config: &JsValue) -> Result<(), JsValue>;

    #[wasm_bindgen(method, catch)]
    fn encode(this: &AudioEncoder, data: &AudioData) -> Result<(), JsValue>;

    #[wasm_bindgen(method)]
    fn flush(this: &AudioEncoder) -> Promise;

    #[wasm_bindgen(method, catch)]
    fn close(this: &AudioEncoder) -> Result<(), JsValue>;

    type AudioData;

    #[wasm_bindgen(constructor, catch)]
    fn new(init: &JsValue) -> Result<AudioData, JsValue>;

    #[wasm_bindgen(method)]
    fn close(this: &AudioData);

    type EncodedAudioChunk;

    #[wasm_bindgen(method, getter, js_name = byteLength)]
    fn byte_length(this: &EncodedAudioChunk) -> u32;

    #[wasm_bindgen(method, getter)]
    fn duration(this: &EncodedAudioChunk) -> Option<f64>;

    #[wasm_bindgen(method, js_name = copyTo, catch)]
    fn copy_to(this: &EncodedAudioChunk, target: &Uint8Array) -> Result<(), JsValue>;
}

fn object(fields: &[(&str, JsValue)]) -> Result<JsValue, JsValue> {
    let object = Object::new();
    for (name, value) in fields {
        Reflect::set(&object, &JsValue::from_str(name), value)?;
    }
    Ok(object.into())
}

fn config() -> Result<JsValue, JsValue> {
    object(&[
        ("codec", "opus".into()),
        ("sampleRate", RATE.into()),
        ("numberOfChannels", 1.into()),
        ("bitrate", BITRATE.into()),
        (
            "opus",
            object(&[
                ("frameDuration", 20_000.into()),
                ("application", "voip".into()),
                ("signal", "voice".into()),
            ])?,
        ),
    ])
}

/// The encoder's delay, from the OpusHead its first output may carry.
fn stated_pre_skip(metadata: &JsValue) -> Option<u16> {
    let settings = Reflect::get(metadata, &"decoderConfig".into()).ok()?;
    let description = Reflect::get(&settings, &"description".into()).ok()?;
    let head = if description.is_instance_of::<js_sys::ArrayBuffer>() {
        Uint8Array::new(&description)
    } else {
        let view = description.dyn_ref::<js_sys::DataView>();
        match view {
            Some(view) => Uint8Array::new_with_byte_offset_and_length(
                &view.buffer(),
                view.byte_offset() as u32,
                view.byte_length() as u32,
            ),
            None => description.dyn_into::<Uint8Array>().ok()?,
        }
    }
    .to_vec();
    head.strip_prefix(b"OpusHead")
        .and_then(|head| head.get(2..4))
        .map(|skip| u16::from_le_bytes([skip[0], skip[1]]))
}

#[derive(Default)]
struct Encoded {
    /// Each packet, and how many samples it holds.
    packets: Vec<(Vec<u8>, u64)>,
    pre_skip: Option<u16>,
    failure: Option<String>,
}

async fn packets(samples: &[f32]) -> Result<Encoded, JsValue> {
    let support = JsFuture::from(AudioEncoder::is_config_supported(&config()?)?).await?;
    if !Reflect::get(&support, &"supported".into())?.is_truthy() {
        return Err(JsValue::NULL);
    }
    let encoded = Rc::new(RefCell::new(Encoded::default()));
    let output = {
        let encoded = Rc::clone(&encoded);
        Closure::<dyn FnMut(EncodedAudioChunk, JsValue)>::new(
            move |chunk: EncodedAudioChunk, metadata: JsValue| {
                let mut encoded = encoded.borrow_mut();
                if encoded.pre_skip.is_none() {
                    encoded.pre_skip = stated_pre_skip(&metadata);
                }
                let target = Uint8Array::new_with_length(chunk.byte_length());
                match chunk.copy_to(&target) {
                    Ok(()) => {
                        let held = chunk.duration().map_or(FRAME as u64, |micros| {
                            (micros * f64::from(RATE) / 1_000_000.0).round() as u64
                        });
                        encoded.packets.push((target.to_vec(), held));
                    }
                    Err(_) => encoded.failure = Some("the encoder lost a packet".to_owned()),
                }
            },
        )
    };
    let error = {
        let encoded = Rc::clone(&encoded);
        Closure::<dyn FnMut(JsValue)>::new(move |_: JsValue| {
            encoded.borrow_mut().failure = Some("the encoder stopped".to_owned());
        })
    };
    let encoder = AudioEncoder::new(&object(&[
        ("output", output.as_ref().clone()),
        ("error", error.as_ref().clone()),
    ])?)?;
    encoder.configure(&config()?)?;
    let tail = vec![0f32; TAIL];
    let mut sent = 0u64;
    for slice in samples.chunks(SLICE).chain(std::iter::once(&tail[..])) {
        let data = AudioData::new(&object(&[
            ("format", "f32".into()),
            ("sampleRate", RATE.into()),
            ("numberOfFrames", (slice.len() as u32).into()),
            ("numberOfChannels", 1.into()),
            (
                "timestamp",
                (sent as f64 * 1_000_000.0 / f64::from(RATE)).into(),
            ),
            ("data", Float32Array::from(slice).into()),
        ])?)?;
        encoder.encode(&data)?;
        data.close();
        sent += slice.len() as u64;
    }
    JsFuture::from(encoder.flush()).await?;
    let _ = encoder.close();
    Ok(encoded.take())
}

/// An OGG/Opus file from mono samples at `RATE`.
pub async fn encode(samples: &[f32]) -> Result<Vec<u8>, String> {
    let encoded = packets(samples).await.map_err(|error| {
        // Not offered at all, or offered without Opus.
        if error.is_null() || error.is_instance_of::<js_sys::ReferenceError>() {
            "This browser has no Opus encoder for voice messages".to_owned()
        } else {
            "The browser's Opus encoder refused the recording".to_owned()
        }
    })?;
    if let Some(failure) = encoded.failure {
        return Err(failure);
    }
    crate::voice::wrap_packets(
        &encoded.packets,
        encoded.pre_skip.unwrap_or(USUAL_PRE_SKIP),
        samples.len(),
    )
}
