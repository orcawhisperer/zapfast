//! The browser's stand-in for fastframe-emoji, chosen by target in ZapFast's
//! manifest. The real crate times its font loading with `std::time::Instant`,
//! which panics in a page. Here every emoji stays ordinary text, drawn in
//! whichever font egui has for it, so nothing is painted over a placeholder.

use egui::text::LayoutJob;
use egui::{Align2, Color32, FontFamily, FontId, Galley, Pos2, Rect, TextFormat};

/// What a placeholder would be. None is ever laid out here.
pub const PLACEHOLDER: char = '\u{2B1B}';

/// Colour emoji stay off, so callers take their plain-text paths.
pub fn available() -> bool {
    false
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
        false
    }
}

/// Never sized as emoji alone: that needs the pictures.
pub fn only_emoji(_text: &str) -> Option<usize> {
    None
}

pub fn prewarm<'a>(_ctx: &egui::Context, _clusters: impl IntoIterator<Item = &'a str>) {}

/// Appends the text as it is. No placement is recorded.
pub fn append(
    _ui: &egui::Ui,
    job: &mut LayoutJob,
    _placements: &mut Vec<String>,
    text: &str,
    format: &TextFormat,
) -> usize {
    let start = job.text.len();
    job.append(text, 0.0, format.clone());
    job.text[start..].chars().count()
}

/// Writes one emoji in the middle of `rect`.
pub fn paint_cluster(ui: &egui::Ui, cluster: &str, rect: Rect) {
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        cluster,
        FontId::new(rect.height() * 0.8, FontFamily::Proportional),
        Color32::WHITE,
    );
}

/// The layout of `text` with nothing to paint over it.
pub fn editor_job(text: &str, format: &TextFormat) -> (LayoutJob, Vec<(usize, usize, String)>) {
    let mut job = LayoutJob::default();
    job.append(text, 0.0, format.clone());
    (job, Vec::new())
}

pub fn paint(_ui: &egui::Ui, _galley: &Galley, _origin: Pos2, _placements: &[String]) {}

pub fn placeholder_rects(_galley: &Galley, _origin: Pos2) -> Vec<Rect> {
    Vec::new()
}

/// Colours nothing: there are no pictures.
#[derive(Default)]
pub struct EmojiPlugin;

impl egui::Plugin for EmojiPlugin {
    fn debug_name(&self) -> &'static str {
        "zapfast web emoji"
    }
}
