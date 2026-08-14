use std::collections::HashMap;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::texture::{TextureBuffer, TextureRenderElement};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::utils::{Physical, Point, Transform};

/// Paths tried, in order, for a usable system font - the first one that
/// both exists and parses wins. Deliberately bold weights (a grab bar is
/// a small, dark strip - a regular weight tends to read as too thin/faint
/// against it at the sizes involved), and deliberately spread across the
/// two font families most Linux desktop distros ship at least one of
/// (DejaVu, Liberation) plus Noto as a broader-coverage fallback.
const FONT_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
    "/usr/share/fonts/liberation-sans/LiberationSans-Bold.ttf",
    "/usr/share/fonts/truetype/liberation2/LiberationSans-Bold.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
    "/usr/share/fonts/noto/NotoSans-Bold.ttf",
];

/// Base point size titles are rasterized at (logical pixels - multiplied
/// by the output's scale factor before rasterizing, see `element_for`),
/// chosen to sit comfortably inside `render_elements::SSD_HEIGHT` (28px)
/// once the bar's own vertical padding is accounted for.
const TITLE_PX: f32 = 13.0;
/// Left padding (logical px) between the bar's left edge and the first
/// glyph.
const TITLE_PAD_LEFT: i32 = 10;
/// Text color (light gray-white - readable on the bar's dark background
/// without being pure white, which read as slightly too harsh against
/// `decoration_elements`' bar color).
const TITLE_COLOR: (u8, u8, u8) = (225, 225, 230);

enum FontState {
    /// Lookup hasn't been attempted yet.
    Unresolved,
    Loaded(fontdue::Font),
    /// Tried every candidate in [`FONT_CANDIDATES`], none worked - cached
    /// so every subsequent frame doesn't re-probe the filesystem.
    Unavailable,
}

struct CachedTitle {
    text: String,
    max_width_px: i32,
    texture: TextureBuffer<GlesTexture>,
    /// Rasterized bitmap height in (already-scale-adjusted) physical
    /// pixels - kept alongside the texture since `TextureBuffer` isn't
    /// otherwise inspected for its dimensions here (see `element_for`'s
    /// vertical-centering calc, the one place this is used).
    tex_height_px: i32,
}

/// Per-compositor-process cache of rasterized title textures, one per
/// window id - see the module doc comment. Lives on `HwdeState` behind a
/// `RefCell` (see that field's doc comment for why) so `render_elements.rs`
/// can mutate it from functions that only borrow `&RenderInputs`.
#[derive(Default)]
pub struct TitleTextureCache {
    font: OnceFont,
    entries: HashMap<u64, CachedTitle>,
}

// Small newtype instead of a bare `FontState` field so `#[derive(Default)]`
// works on `TitleTextureCache` above without `FontState` itself needing a
// (slightly misleading - "unresolved" isn't really "default") `Default`
// impl.
#[derive(Default)]
struct OnceFont(Option<FontState>);

impl TitleTextureCache {
    fn font(&mut self) -> Option<&fontdue::Font> {
        if self.font.0.is_none() {
            self.font.0 = Some(match load_font() {
                Some(font) => FontState::Loaded(font),
                None => FontState::Unavailable,
            });
        }
        match self.font.0.as_ref() {
            Some(FontState::Loaded(font)) => Some(font),
            _ => None,
        }
    }

    /// Drops a window's cached texture - call when a window closes so
    /// `entries` doesn't grow unboundedly over a long session (window ids
    /// are never reused - see `HwdeState::next_window_id` - so a stale
    /// entry would otherwise sit there forever).
    pub fn forget(&mut self, window_id: u64) {
        self.entries.remove(&window_id);
    }

    /// Builds (or reuses, from cache) a title-text render element for
    /// `window_id`, positioned left-aligned with [`TITLE_PAD_LEFT`]
    /// padding inside `bar` (logical-space grab-bar rectangle, as
    /// computed by `render_elements::ssd_bar_geometry`), truncated with
    /// an ellipsis if it would otherwise overlap the close button.
    /// Returns `None` if there's no font available, the title is empty,
    /// or texture upload fails - callers should treat that exactly like
    /// "no title text drawn this frame", which is already how the bar/
    /// close button rendered before this module existed.
    pub fn element_for(
        &mut self,
        renderer: &mut GlesRenderer,
        window_id: u64,
        title: &str,
        bar: smithay::utils::Rectangle<i32, smithay::utils::Logical>,
        scale: f64,
    ) -> Option<TextureRenderElement<GlesTexture>> {
        if title.is_empty() {
            return None;
        }

        // Everything below works in *physical* pixels (raster size, max
        // width, position) rather than logical - unlike `decoration_elements`/
        // `focus_border_elements`, which build a logical-space `Rectangle`
        // and convert once at the end. Text has to be rasterized directly
        // at its final physical size (there's no cheap way to "scale a
        // raster up" without it turning blurry on HiDPI outputs), so it's
        // simplest to just stay in physical units from the raster step
        // onward rather than rasterize-then-convert like the flat-color
        // elements do.
        let physical_px = TITLE_PX * scale as f32;

        // Leave room for the close button (see `render_elements::SSD_CLOSE_WIDTH`)
        // plus padding on both sides.
        let max_width_px = (((bar.size.w - crate::render_elements::SSD_CLOSE_WIDTH - TITLE_PAD_LEFT - 6) as f64 * scale).round() as i32).max(0);
        if max_width_px == 0 {
            return None;
        }

        let needs_rebuild = match self.entries.get(&window_id) {
            Some(cached) => cached.text != title || cached.max_width_px != max_width_px,
            None => true,
        };

        if needs_rebuild {
            let font = self.font()?;
            let (rgba, w, h) = rasterize_fitting(font, title, physical_px, max_width_px);
            if w == 0 || h == 0 {
                return None;
            }
            match TextureBuffer::from_memory(renderer, &rgba, Fourcc::Abgr8888, (w, h), false, 1, Transform::Normal, None) {
                Ok(texture) => {
                    self.entries.insert(window_id, CachedTitle { text: title.to_string(), max_width_px, texture, tex_height_px: h });
                }
                Err(err) => {
                    tracing::debug!("SSD title text: texture upload failed for window {window_id}: {err}");
                    return None;
                }
            }
        }

        let cached = self.entries.get(&window_id)?;
        // Left-aligned with padding, vertically centered in the bar - all
        // in physical pixels (see the comment above `physical_px`).
        let x = bar.loc.x as f64 * scale + TITLE_PAD_LEFT as f64 * scale;
        let y = bar.loc.y as f64 * scale + (bar.size.h as f64 * scale - cached.tex_height_px as f64) / 2.0;

        Some(TextureRenderElement::from_texture_buffer(
            Point::<f64, Physical>::from((x, y)),
            &cached.texture,
            None,
            None,
            None,
            Kind::Unspecified,
        ))
    }
}

fn load_font() -> Option<fontdue::Font> {
    for path in FONT_CANDIDATES {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        match fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
            Ok(font) => {
                tracing::info!("SSD title text: using font {path}");
                return Some(font);
            }
            Err(err) => tracing::debug!("SSD title text: {path} exists but didn't parse as a font: {err}"),
        }
    }
    tracing::warn!(
        "SSD title text: no usable font found among {} candidate system paths - window titles won't be drawn on the grab bar (drag-to-move and click-to-close are unaffected)",
        FONT_CANDIDATES.len()
    );
    None
}

/// Rasterizes `text` at `px`, shrinking (dropping trailing characters and
/// appending an ellipsis) until it fits within `max_width_px`, or
/// returning an empty result if even a single ellipsis character doesn't
/// fit. Returns `(rgba_bytes, width, height)` in the same
/// row-major-top-to-bottom, 4-bytes-per-pixel layout `wallpaper.rs` uses
/// for `Fourcc::Abgr8888` (i.e. byte order R, G, B, A per pixel).
fn rasterize_fitting(font: &fontdue::Font, text: &str, px: f32, max_width_px: i32) -> (Vec<u8>, i32, i32) {
    let mut candidate = text.to_string();
    loop {
        let (buf, w, h) = rasterize(font, &candidate, px);
        if w <= max_width_px || candidate.chars().count() <= 1 {
            return (buf, w, h);
        }
        // Drop the last character and try again with a trailing ellipsis.
        // Loop is bounded by `candidate`'s length, so this always
        // terminates (worst case: single "…" glyph, handled by the
        // `<= 1` check above returning even if it's still technically
        // too wide - better than drawing nothing).
        let mut chars: Vec<char> = candidate.chars().collect();
        chars.pop();
        if chars.last() == Some(&'…') {
            chars.pop();
        }
        chars.push('…');
        candidate = chars.into_iter().collect();
    }
}

/// Rasterizes `text` at `px` into a tightly-cropped RGBA bitmap (width =
/// total glyph advance, height = the font's ascent-to-descent line
/// height at `px`), transparent everywhere no glyph covers.
fn rasterize(font: &fontdue::Font, text: &str, px: f32) -> (Vec<u8>, i32, i32) {
    let line = font.horizontal_line_metrics(px).unwrap_or(fontdue::LineMetrics {
        ascent: px,
        descent: 0.0,
        line_gap: 0.0,
        new_line_size: px,
    });
    let ascent = line.ascent;

    struct Placed {
        x: f32,
        metrics: fontdue::Metrics,
        bitmap: Vec<u8>,
    }

    let mut placed = Vec::with_capacity(text.chars().count());
    let mut cursor_x = 0.0f32;
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, px);
        placed.push(Placed { x: cursor_x, metrics, bitmap });
        cursor_x += placed.last().map(|p| p.metrics.advance_width).unwrap_or(0.0);
    }

    let width = cursor_x.ceil().max(1.0) as i32;
    let height = px.ceil().max(1.0) as i32;
    let mut buf = vec![0u8; (width as usize) * (height as usize) * 4];

    for p in &placed {
        let gx0 = p.x.round() as i32 + p.metrics.xmin;
        // fontdue's `ymin` is measured up from the glyph's own baseline;
        // converting to "distance down from the top of our bitmap" needs
        // the font's ascent as the baseline's y position from the top.
        let gy0 = (ascent - p.metrics.ymin as f32 - p.metrics.height as f32).round() as i32;
        for row in 0..p.metrics.height {
            for col in 0..p.metrics.width {
                let coverage = p.bitmap[row * p.metrics.width + col];
                if coverage == 0 {
                    continue;
                }
                let px_x = gx0 + col as i32;
                let px_y = gy0 + row as i32;
                if px_x < 0 || px_y < 0 || px_x >= width || px_y >= height {
                    continue;
                }
                let idx = ((px_y * width + px_x) * 4) as usize;
                buf[idx] = TITLE_COLOR.0;
                buf[idx + 1] = TITLE_COLOR.1;
                buf[idx + 2] = TITLE_COLOR.2;
                buf[idx + 3] = coverage;
            }
        }
    }

    (buf, width, height)
}
