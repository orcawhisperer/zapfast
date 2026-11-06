//! What a page does with a picture through the browser: encoding a WebP, which the
//! desktop leaves to libwebp, and putting one on the clipboard.

use futures::channel::oneshot;
use js_sys::{Array, Object, Reflect, Uint8Array};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{Clamped, JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Blob, CanvasRenderingContext2d, HtmlCanvasElement, ImageData};

// web-sys offers this only behind an unstable flag.
#[wasm_bindgen]
extern "C" {
    type ClipboardItem;

    #[wasm_bindgen(constructor, catch)]
    fn new(items: &JsValue) -> Result<ClipboardItem, JsValue>;
}

async fn canvas_blob(
    canvas: &HtmlCanvasElement,
    kind: &str,
    quality: f32,
) -> Result<Option<Blob>, JsValue> {
    let (answer, answered) = oneshot::channel();
    let done = Closure::once(move |blob: Option<Blob>| {
        let _ = answer.send(blob);
    });
    canvas.to_blob_with_type_and_encoder_options(
        done.as_ref().unchecked_ref(),
        kind,
        &JsValue::from_f64(f64::from(quality)),
    )?;
    Ok(answered.await.ok().flatten())
}

async fn draw_webp(picture: &image::RgbaImage, quality: f32) -> Result<Blob, JsValue> {
    let (width, height) = picture.dimensions();
    let canvas: HtmlCanvasElement = web_sys::window()
        .and_then(|window| window.document())
        .ok_or(JsValue::NULL)?
        .create_element("canvas")?
        .dyn_into()?;
    canvas.set_width(width);
    canvas.set_height(height);
    let context: CanvasRenderingContext2d =
        canvas.get_context("2d")?.ok_or(JsValue::NULL)?.dyn_into()?;
    let pixels =
        ImageData::new_with_u8_clamped_array_and_sh(Clamped(picture.as_raw()), width, height)?;
    context.put_image_data(&pixels, 0.0, 0.0)?;
    canvas_blob(&canvas, "image/webp", quality)
        .await?
        .ok_or(JsValue::NULL)
}

/// A lossy WebP at `quality`, from 0 to 100. A browser without a WebP encoder hands
/// back a PNG instead, which is no use as a sticker.
pub async fn webp(picture: &image::RgbaImage, quality: f32) -> Result<Vec<u8>, String> {
    let blob = draw_webp(picture, quality / 100.0)
        .await
        .map_err(|_| "The browser could not encode the sticker".to_owned())?;
    if blob.type_() != "image/webp" {
        return Err("This browser cannot make stickers".to_owned());
    }
    let buffer = JsFuture::from(blob.array_buffer())
        .await
        .map_err(|_| "The browser could not encode the sticker".to_owned())?;
    Ok(Uint8Array::new(&buffer).to_vec())
}

/// Puts a PNG on the clipboard, which only `image/png` reliably reaches.
pub async fn copy_png(png: &[u8]) -> Result<(), JsValue> {
    let options = web_sys::BlobPropertyBag::new();
    options.set_type("image/png");
    let parts = Array::of1(&Uint8Array::from(png));
    let blob = Blob::new_with_u8_array_sequence_and_options(&parts, &options)?;
    let items = Object::new();
    Reflect::set(&items, &"image/png".into(), &blob)?;
    let item = ClipboardItem::new(&items)?;
    let navigator = web_sys::window().ok_or(JsValue::NULL)?.navigator();
    let clipboard = Reflect::get(&navigator, &"clipboard".into())?;
    let write: js_sys::Function = Reflect::get(&clipboard, &"write".into())?.dyn_into()?;
    let writing: js_sys::Promise = write.call1(&clipboard, &Array::of1(&item))?.dyn_into()?;
    JsFuture::from(writing).await?;
    Ok(())
}
