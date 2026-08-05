use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::Color32F;
use smithay::desktop::space::{space_render_elements, SpaceRenderElements};
use smithay::desktop::Window;
use smithay::output::Output;
use smithay::utils::{Physical, Point, Rectangle};

use crate::state::HwdeState;

// NOTE on the two things that were wrong here before:
//
// 1. `render_elements!` parses each trait bound in its `where` clause as a
//    single macro token-tree (`$bound:tt`). A multi-segment path like
//    `smithay::backend::renderer::ImportAll` is *not* one token tree (it's
//    `ident :: ident :: ident :: ident`), so writing a bound inline as a
//    full path fails to parse with "no rules expected `::`".
//
// 2. Being generic over `R: ImportAll + ImportMem` was never going to work
//    for *this* enum regardless of how the bound was spelled: the
//    `Wallpaper` variant is `TextureRenderElement<GlesTexture>`, which only
//    implements `RenderElement<R>` for `R::TextureId == GlesTexture` (i.e.
//    concretely `GlesRenderer`, not any generic renderer). Every backend in
//    this project (`winit_backend.rs` today, `backend_drm.rs` once finished)
//    uses `GlesRenderer` anyway, so there's no real need for genericity here.
//
// The fix for both is the same: use the macro's *fixed-renderer* form
// (`<=GlesRenderer>`, documented right on `render_elements!` itself) instead
// of a generic `<R> where R: ...`. This also means every variant's inner
// type must be written concretely against `GlesRenderer` rather than `R`.
smithay::backend::renderer::element::render_elements! {
    pub OutputRenderElement<=GlesRenderer>;
    /// Windows plus all four `wlr-layer-shell` layers, in the correct
    /// relative order - see `space_render_elements`.
    Space = SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
    /// The desktop wallpaper - always the backmost element.
    Wallpaper = TextureRenderElement<GlesTexture>,
    /// Minimal SSD: a solid-color grab bar (+ close-button hit region) for
    /// windows that negotiated server-side decoration. No title text yet -
    /// that needs a font-rasterization pipeline this compositor doesn't
    /// have; move-by-dragging and click-to-close both work already via
    /// `grabs.rs` and don't depend on anything being drawn here.
    Decoration = SolidColorRenderElement,
}

/// Height, in logical pixels, of the minimal SSD grab bar.
pub const SSD_HEIGHT: i32 = 28;
/// Width of the close-button hit region at the right edge of the bar.
pub const SSD_CLOSE_WIDTH: i32 = 28;

/// Bar geometry (logical, relative to the output) for a window whose
/// top-left corner in the `Space` is `window_loc` - shared by both the
/// renderer (below) and hit-testing (see [`ssd_hit_test`]).
pub fn ssd_bar_geometry(window_loc: Point<i32, smithay::utils::Logical>, window_width: i32) -> Rectangle<i32, smithay::utils::Logical> {
    Rectangle::new(
        Point::from((window_loc.x, window_loc.y - SSD_HEIGHT)),
        (window_width.max(SSD_CLOSE_WIDTH), SSD_HEIGHT).into(),
    )
}

/// What (if anything) at `pos` belongs to an SSD grab bar/close button,
/// checked before normal client hit-testing so these compositor-owned
/// controls intercept the click instead of passing through to whatever
/// client surface happens to be underneath.
pub enum SsdHit {
    Bar(u64),
    Close(u64),
}

pub fn ssd_hit_test(state: &HwdeState, pos: Point<f64, smithay::utils::Logical>) -> Option<SsdHit> {
    let pos_i = pos.to_i32_round();
    for managed in &state.windows {
        if !managed.is_ssd || managed.is_minimized {
            continue;
        }
        let Some(loc) = state.space.element_location(&managed.window) else { continue };
        let width = managed.window.geometry().size.w;
        let bar = ssd_bar_geometry(loc, width);
        if !bar.contains(pos_i) {
            continue;
        }
        let close_rect = Rectangle::new(
            Point::from((bar.loc.x + bar.size.w - SSD_HEIGHT + 4, bar.loc.y + 4)),
            (SSD_HEIGHT - 8, SSD_HEIGHT - 8).into(),
        );
        return Some(if close_rect.contains(pos_i) { SsdHit::Close(managed.id) } else { SsdHit::Bar(managed.id) });
    }
    None
}

/// Builds the decoration elements (grab bar + close button) for every
/// mapped, non-minimized window that has `is_ssd` set.
fn decoration_elements(state: &HwdeState, scale: f64) -> Vec<SolidColorRenderElement> {
    let mut elements = Vec::new();

    for managed in &state.windows {
        if !managed.is_ssd || managed.is_minimized {
            continue;
        }
        let Some(loc) = state.space.element_location(&managed.window) else { continue };
        let width = managed.window.geometry().size.w;
        let bar = ssd_bar_geometry(loc, width);

        let to_physical = |r: Rectangle<i32, smithay::utils::Logical>| -> Rectangle<i32, Physical> {
            Rectangle::new(
                ((r.loc.x as f64 * scale).round() as i32, (r.loc.y as f64 * scale).round() as i32).into(),
                ((r.size.w as f64 * scale).round() as i32, (r.size.h as f64 * scale).round() as i32).into(),
            )
        };

        // Bar background (dark, semi-opaque - matches the shell's own
        // window title bar styling closely enough to feel consistent).
        elements.push(SolidColorRenderElement::new(
            smithay::backend::renderer::element::Id::new(),
            to_physical(bar),
            smithay::backend::renderer::utils::CommitCounter::default(),
            Color32F::new(0.06, 0.06, 0.08, 0.95),
            Kind::Unspecified,
        ));

        // Close-button hit region, right-aligned in the bar, drawn as a
        // small red square so it's at least visually discoverable.
        let close_size = SSD_HEIGHT - 8;
        let close_rect = Rectangle::new(
            Point::from((bar.loc.x + bar.size.w - SSD_HEIGHT + 4, bar.loc.y + 4)),
            (close_size, close_size).into(),
        );
        elements.push(SolidColorRenderElement::new(
            smithay::backend::renderer::element::Id::new(),
            to_physical(close_rect),
            smithay::backend::renderer::utils::CommitCounter::default(),
            Color32F::new(0.75, 0.2, 0.2, 0.9),
            Kind::Unspecified,
        ));
    }

    elements
}

/// Draws a colored border around the focused window, if `config.border_width`
/// is positive - four thin solid-color rectangles (top/bottom/left/right)
/// framing the window's geometry, reusing the exact same
/// `SolidColorRenderElement`/physical-space conversion `decoration_elements`
/// already uses for the SSD grab bar. Skipped entirely for a minimized
/// focused window (nothing to frame) or when tiling/floating leaves no
/// focus at all.
fn focus_border_elements(state: &HwdeState, scale: f64) -> Vec<SolidColorRenderElement> {
    let border_width = state.config.border_width;
    if border_width <= 0 {
        return Vec::new();
    }
    let Some(id) = state.focused_window else { return Vec::new() };
    let Some(managed) = state.windows.iter().find(|w| w.id == id) else { return Vec::new() };
    if managed.is_minimized {
        return Vec::new();
    }
    let Some(loc) = state.space.element_location(&managed.window) else { return Vec::new() };
    let size = managed.window.geometry().size;

    let to_physical = |r: Rectangle<i32, smithay::utils::Logical>| -> Rectangle<i32, Physical> {
        Rectangle::new(
            ((r.loc.x as f64 * scale).round() as i32, (r.loc.y as f64 * scale).round() as i32).into(),
            ((r.size.w as f64 * scale).round() as i32, (r.size.h as f64 * scale).round() as i32).into(),
        )
    };

    // Accent blue - matches the shell's own focus-ring styling elsewhere
    // (Big Picture tiles, Settings section highlights) closely enough to
    // feel like the same design language rather than a compositor-only
    // color choice.
    let color = Color32F::new(0.30, 0.55, 0.95, 0.95);
    let bw = border_width;

    let rects = [
        // top
        Rectangle::new(Point::from((loc.x - bw, loc.y - bw)), (size.w + bw * 2, bw).into()),
        // bottom
        Rectangle::new(Point::from((loc.x - bw, loc.y + size.h)), (size.w + bw * 2, bw).into()),
        // left (excluding the corners the top/bottom bars already cover)
        Rectangle::new(Point::from((loc.x - bw, loc.y)), (bw, size.h).into()),
        // right
        Rectangle::new(Point::from((loc.x + size.w, loc.y)), (bw, size.h).into()),
    ];

    rects
        .into_iter()
        .map(|r| {
            SolidColorRenderElement::new(
                smithay::backend::renderer::element::Id::new(),
                to_physical(r),
                smithay::backend::renderer::utils::CommitCounter::default(),
                color,
                Kind::Unspecified,
            )
        })
        .collect()
}

/// Assembles the full, correctly-ordered element list for one output.
pub fn build_output_elements(
    state: &HwdeState,
    renderer: &mut GlesRenderer,
    output: &Output,
    output_size: (i32, i32),
) -> Vec<OutputRenderElement> {
    let scale = output.current_scale().fractional_scale();
    let mut elements: Vec<OutputRenderElement> = Vec::new();

    // 1a. Focused-window border - frontmost of all (drawn last, in front
    //     of even the SSD grab bar, since it should always read as "on
    //     top" of the window it's framing).
    elements.extend(focus_border_elements(state, scale).into_iter().map(OutputRenderElement::Decoration));

    // 1b. Decorations - grab bar/close button.
    elements.extend(decoration_elements(state, scale).into_iter().map(OutputRenderElement::Decoration));

    // 2. Windows + all layer-shell layers (space_render_elements already
    //    interleaves Top/Overlay before, and Background/Bottom after, the
    //    window stack itself).
    match space_render_elements::<_, Window, _>(renderer, [&state.space], output, 1.0) {
        Ok(space_elements) => {
            elements.extend(space_elements.into_iter().map(OutputRenderElement::Space));
        }
        Err(err) => {
            tracing::warn!("space_render_elements failed: {err:?}");
        }
    }

    // 3. Wallpaper - backmost (drawn first).
    if let Some(wallpaper) = state.wallpaper.render_element(output_size) {
        elements.push(OutputRenderElement::Wallpaper(wallpaper));
    }

    elements
}
