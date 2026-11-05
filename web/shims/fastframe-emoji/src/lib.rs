//! The browser's stand-in for fastframe-emoji, chosen by target in ZapFast's
//! manifest. The real crate times its font loading with `std::time::Instant`,
//! which panics in a page.
//!
//! It keeps the same contract: layout swaps each emoji for a transparent
//! placeholder, and its picture is painted over it afterwards. The pictures come
//! from a canvas, so the browser's shaping and the platform's own emoji font draw
//! every sequence, and no font ships in the download.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use egui::text::LayoutJob;
use egui::{
    Align2, Color32, ColorImage, FontId, Galley, Pos2, Rect, Stroke, TextFormat, TextureHandle,
    TextureOptions, Vec2,
};
use unicode_segmentation::UnicodeSegmentation as _;
use wasm_bindgen::JsCast as _;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};

/// What stands in for an emoji in a layout.
pub const PLACEHOLDER: char = '\u{2B1B}';

/// The side of the canvas each picture is drawn on, in pixels.
const CANVAS_SIDE: u32 = 96;

/// The size emoji are drawn at on it.
const DRAWN_SIZE: u32 = 72;

/// The painted emoji's side, relative to the row height of the text beside it.
const EMOJI_SIDE: f32 = 1.08;

/// Every platform family, the system's own first.
const EMOJI_FONTS: &str = "\"Apple Color Emoji\", \"Segoe UI Emoji\", \"Noto Color Emoji\", \"Twemoji Mozilla\", sans-serif";

/// A canvas is always there to ask.
pub fn available() -> bool {
    true
}

pub fn warm_up() {}

#[derive(Default)]
pub struct EmojiSetup;

impl EmojiSetup {
    pub fn bundled(self, _font: &'static [u8]) -> Self {
        self
    }

    pub fn synchronous(self, _enabled: bool) -> Self {
        self
    }

    pub fn install(self) -> bool {
        true
    }
}

/// Whether a code point shows as a picture with nothing asking it to, from
/// Unicode's Emoji_Presentation property.
fn presents_as_emoji(c: char) -> bool {
    let code = c as u32;
    if (0x1F000..=0x1FAFF).contains(&code) {
        // Letters and marks in this block that stay text unless asked.
        return !matches!(
            code,
            0x1F170 | 0x1F171 | 0x1F17E | 0x1F17F | 0x1F202 | 0x1F237
        );
    }
    matches!(
        code,
        0x231A..=0x231B
            | 0x23E9..=0x23EC
            | 0x23F0
            | 0x23F3
            | 0x25FD..=0x25FE
            | 0x2614..=0x2615
            | 0x2648..=0x2653
            | 0x267F
            | 0x2693
            | 0x26A1
            | 0x26AA..=0x26AB
            | 0x26BD..=0x26BE
            | 0x26C4..=0x26C5
            | 0x26CE
            | 0x26D4
            | 0x26EA
            | 0x26F2..=0x26F3
            | 0x26F5
            | 0x26FA
            | 0x26FD
            | 0x2705
            | 0x270A..=0x270B
            | 0x2728
            | 0x274C
            | 0x274E
            | 0x2753..=0x2755
            | 0x2757
            | 0x2795..=0x2797
            | 0x27B0
            | 0x27BF
            | 0x2B1B..=0x2B1C
            | 0x2B50
            | 0x2B55
    )
}

/// Whether a grapheme cluster shows as one picture.
fn is_emoji(cluster: &str) -> bool {
    let Some(first) = cluster.chars().next() else {
        return false;
    };
    if (first as u32) < 0xA9 {
        // Digits, `#` and `*` only as keycaps.
        return cluster.contains('\u{20E3}');
    }
    if presents_as_emoji(first) {
        return true;
    }
    // Others can ask for it, unless they are letters of a script that joins.
    let can_ask = matches!(first, '\u{A9}' | '\u{AE}')
        || ('\u{2000}'..='\u{33FF}').contains(&first)
        || ('\u{1F000}'..='\u{1FAFF}').contains(&first);
    can_ask
        && cluster.chars().any(|c| {
            matches!(
                c,
                '\u{FE0F}' | '\u{20E3}' | '\u{200D}' | '\u{1F3FB}'..='\u{1F3FF}'
            )
        })
}

/// A run of text, or one emoji.
enum Piece<'a> {
    Text(&'a str),
    Emoji(&'a str),
}

fn pieces(text: &str) -> Vec<Piece<'_>> {
    let mut found = Vec::new();
    let mut run = 0;
    for (at, cluster) in text.grapheme_indices(true) {
        if is_emoji(cluster) {
            if run < at {
                found.push(Piece::Text(&text[run..at]));
            }
            found.push(Piece::Emoji(cluster));
            run = at + cluster.len();
        }
    }
    if run < text.len() {
        found.push(Piece::Text(&text[run..]));
    }
    found
}

/// How many emoji `text` is, when it is nothing else.
pub fn only_emoji(text: &str) -> Option<usize> {
    let mut count = 0;
    for piece in pieces(text) {
        match piece {
            Piece::Emoji(_) => count += 1,
            Piece::Text(run) if run.trim().is_empty() => {}
            Piece::Text(_) => return None,
        }
    }
    (count > 0).then_some(count)
}

/// Pictures uploaded to one egui context; `None` when nothing draws a cluster.
#[derive(Clone, Default)]
struct Cache(Arc<Mutex<HashMap<String, Option<TextureHandle>>>>);

fn context_2d() -> Option<CanvasRenderingContext2d> {
    let canvas: HtmlCanvasElement = web_sys::window()?
        .document()?
        .create_element("canvas")
        .ok()?
        .dyn_into()
        .ok()?;
    canvas.set_width(CANVAS_SIDE);
    canvas.set_height(CANVAS_SIDE);
    let options = js_sys::Object::new();
    js_sys::Reflect::set(&options, &"willReadFrequently".into(), &true.into()).ok()?;
    canvas
        .get_context_with_context_options("2d", &options)
        .ok()??
        .dyn_into()
        .ok()
}

thread_local! {
    static CANVAS: Option<CanvasRenderingContext2d> = context_2d();
}

/// Draws `cluster` and crops it to what was painted, or `None` if nothing was.
fn draw(cluster: &str) -> Option<ColorImage> {
    CANVAS.with(|canvas| {
        let canvas = canvas.as_ref()?;
        let side = f64::from(CANVAS_SIDE);
        canvas.clear_rect(0.0, 0.0, side, side);
        canvas.set_font(&format!("{DRAWN_SIZE}px {EMOJI_FONTS}"));
        canvas.set_text_align("center");
        canvas.set_text_baseline("middle");
        canvas.fill_text(cluster, side / 2.0, side / 2.0).ok()?;
        let data = canvas.get_image_data(0.0, 0.0, side, side).ok()?.data();
        let side = CANVAS_SIDE as usize;
        let (mut left, mut top, mut right, mut bottom) = (side, side, 0, 0);
        for y in 0..side {
            for x in 0..side {
                if data[(y * side + x) * 4 + 3] != 0 {
                    left = left.min(x);
                    right = right.max(x + 1);
                    top = top.min(y);
                    bottom = bottom.max(y + 1);
                }
            }
        }
        if left >= right || top >= bottom {
            return None;
        }
        let (width, height) = (right - left, bottom - top);
        let mut cropped = Vec::with_capacity(width * height * 4);
        for y in top..bottom {
            let start = (y * side + left) * 4;
            cropped.extend_from_slice(&data[start..start + width * 4]);
        }
        Some(ColorImage::from_rgba_unmultiplied(
            [width, height],
            &cropped,
        ))
    })
}

/// The picture for `cluster`, drawn the first time it is seen.
fn texture(ctx: &egui::Context, cluster: &str) -> Option<TextureHandle> {
    let cache = ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<Cache>(egui::Id::new("web-emoji"))
            .clone()
    });
    let mut pictures = cache.0.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(known) = pictures.get(cluster) {
        return known.clone();
    }
    let made = draw(cluster)
        .map(|image| ctx.load_texture(format!("emoji-{cluster}"), image, TextureOptions::LINEAR));
    pictures.insert(cluster.to_owned(), made.clone());
    made
}

pub fn prewarm<'a>(_ctx: &egui::Context, _clusters: impl IntoIterator<Item = &'a str>) {}

/// The transparent format that holds an emoji's place.
fn placeholder(ui: &egui::Ui, format: &TextFormat) -> TextFormat {
    let (row_height, width) = ui.fonts_mut(|fonts| {
        let shaped = fonts.layout_no_wrap(
            PLACEHOLDER.to_string(),
            format.font_id.clone(),
            Color32::TRANSPARENT,
        );
        (fonts.row_height(&format.font_id), shaped.size().x)
    });
    let mut hidden = format.clone();
    hidden.color = Color32::TRANSPARENT;
    hidden.underline = Stroke::NONE;
    hidden.strikethrough = Stroke::NONE;
    if width > 0.0 {
        hidden.font_id.size *= row_height * EMOJI_SIDE / width;
    }
    hidden.line_height = Some(format.line_height.unwrap_or(row_height));
    hidden
}

/// Appends `text` with a placeholder for each emoji, recording them in
/// `placements`. Returns how many characters were appended.
pub fn append(
    ui: &egui::Ui,
    job: &mut LayoutJob,
    placements: &mut Vec<String>,
    text: &str,
    format: &TextFormat,
) -> usize {
    let start = job.text.len();
    let mut hidden = None;
    for piece in pieces(text) {
        match piece {
            Piece::Text(run) => job.append(run, 0.0, format.clone()),
            Piece::Emoji(cluster) => {
                let hidden = hidden.get_or_insert_with(|| placeholder(ui, format));
                job.append(&PLACEHOLDER.to_string(), 0.0, hidden.clone());
                placements.push(cluster.to_owned());
            }
        }
    }
    job.text[start..].chars().count()
}

/// Paints one emoji into `rect`, as text if no font draws it.
pub fn paint_cluster(ui: &egui::Ui, cluster: &str, rect: Rect) {
    let painter = ui.painter();
    let Some(texture) = texture(ui.ctx(), cluster) else {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            cluster,
            FontId::proportional(rect.height() * 0.8),
            ui.visuals().text_color(),
        );
        return;
    };
    let size = texture.size_vec2();
    let scale = (rect.width() / size.x).min(rect.height() * EMOJI_SIDE / size.y);
    painter.image(
        texture.id(),
        Rect::from_center_size(
            rect.center() + Vec2::new(0.0, rect.height() * 0.02),
            size * scale,
        ),
        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
        Color32::WHITE,
    );
}

/// A layout for a text editor: the characters stay, the emoji are transparent,
/// and each comes back as `(first character, character count, cluster)`.
pub fn editor_job(text: &str, format: &TextFormat) -> (LayoutJob, Vec<(usize, usize, String)>) {
    let mut job = LayoutJob::default();
    let mut clusters = Vec::new();
    let mut at = 0;
    for piece in pieces(text) {
        match piece {
            Piece::Text(run) => {
                job.append(run, 0.0, format.clone());
                at += run.chars().count();
            }
            Piece::Emoji(cluster) => {
                let mut hidden = format.clone();
                hidden.color = Color32::TRANSPARENT;
                job.append(cluster, 0.0, hidden);
                let length = cluster.chars().count();
                clusters.push((at, length, cluster.to_owned()));
                at += length;
            }
        }
    }
    (job, clusters)
}

/// Paints the pictures over a galley laid out with [`append`].
pub fn paint(ui: &egui::Ui, galley: &Galley, origin: Pos2, placements: &[String]) {
    for (rect, cluster) in placeholder_rects(galley).zip(placements) {
        paint_cluster(ui, cluster, rect.translate(origin.to_vec2()));
    }
}

/// The placeholders' rectangles in logical order, relative to the galley.
pub fn placeholder_rects(galley: &Galley) -> impl Iterator<Item = Rect> + '_ {
    galley.rows.iter().flat_map(|row| {
        let mut placeholders: Vec<_> = row
            .glyphs
            .iter()
            .filter(|glyph| glyph.chr == PLACEHOLDER)
            .collect();
        // Reordered right-to-left runs keep each glyph's first vertex.
        placeholders.sort_by_key(|glyph| glyph.first_vertex);
        placeholders.into_iter().map(move |glyph| {
            Rect::from_min_max(
                Pos2::new(glyph.pos.x, 0.0),
                Pos2::new(glyph.max_x(), row.size.y),
            )
            .translate(row.pos.to_vec2())
        })
    })
}

/// Leaves emoji in plain labels, menus and tooltips in the fonts' own glyphs.
/// Message text and chat names go through [`append`] and are not affected.
#[derive(Default)]
pub struct EmojiPlugin;

impl egui::Plugin for EmojiPlugin {
    fn debug_name(&self) -> &'static str {
        "zapfast web emoji"
    }
}
