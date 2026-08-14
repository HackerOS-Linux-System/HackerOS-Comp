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

use crate::config::CompositorConfig;
use crate::state::{HwdeState, ManagedWindow};
use crate::wallpaper::Wallpaper;

/// Just the pieces of [`HwdeState`] that `build_output_elements` (and the
/// two private helpers it calls) actually read.
///
/// This exists so the DRM backend (`backend_drm.rs`) can call
/// `build_output_elements` while its `GlesRenderer` is itself a field of
/// `HwdeState` (inside `drm_gpus`). Taking `&HwdeState` wholesale there
/// would alias the `&mut GlesRenderer` borrowed from the same struct -
/// the borrow checker rejects that even though the fields involved are
/// disjoint. Building a `RenderInputs` from individually-borrowed fields
/// (`let HwdeState { ref space, ref mut drm_gpus, .. } = *state;`) sidesteps
/// it. The winit backend's renderer isn't a field of `HwdeState` at all, so
/// it just uses the `From<&HwdeState>` impl below and this is invisible to
/// it.
pub struct RenderInputs<'a> {
    pub windows: &'a [ManagedWindow],
    pub space: &'a smithay::desktop::Space<Window>,
    pub config: &'a CompositorConfig,
    pub focused_window: Option<u64>,
    pub wallpaper: &'a Wallpaper,
    pub title_cache: &'a std::cell::RefCell<crate::title_text::TitleTextureCache>,
    pub idle_dimmed: bool,
    pub cursor_visible: bool,
    pub cursor_location: smithay::utils::Point<f64, smithay::utils::Logical>,
}

impl<'a> From<&'a HwdeState> for RenderInputs<'a> {
    fn from(state: &'a HwdeState) -> Self {
        RenderInputs {
            windows: &state.windows,
            space: &state.space,
            config: &state.config,
            focused_window: state.focused_window,
            wallpaper: &state.wallpaper,
            title_cache: &state.title_cache,
            idle_dimmed: state.idle_dimmed,
            cursor_visible: !matches!(state.cursor_status, smithay::input::pointer::CursorImageStatus::Hidden),
            cursor_location: state.pointer.current_location(),
        }
    }
}

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
    /// Minimal SSD: a solid-color grab bar (+ close-button hit region) for
    /// windows that negotiated server-side decoration, plus - see
    /// `Texture` below - rasterized title text on top of it.
    Decoration = SolidColorRenderElement,
    /// Any GPU-texture-backed element with no client surface behind it -
    /// the wallpaper (`wallpaper.rs`, always the backmost element pushed)
    /// and rasterized window titles (`title_text.rs`) both push through
    /// this one variant, even though they're semantically different
    /// things, because `render_elements!` generates one `From<T>` impl
    /// per *type*, not per role - two variants both wrapping
    /// `TextureRenderElement<GlesTexture>` is a compile error (conflicting
    /// `From` impls: `E0119`, hit and fixed during this project's first
    /// real compile), not just a style choice. Which one a given push
    /// actually is only matters for the order it's added in
    /// `build_output_elements`, not which variant name it goes through.
    Texture = TextureRenderElement<GlesTexture>,
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
        if !managed.is_ssd || managed.is_minimized || managed.is_fullscreen {
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
/// mapped, non-minimized, non-fullscreen window that has `is_ssd` set -
/// see `ManagedWindow::is_fullscreen`'s doc comment for why fullscreen
/// specifically also suppresses this, unlike maximize.
fn decoration_elements(inputs: &RenderInputs, scale: f64) -> Vec<SolidColorRenderElement> {
    let mut elements = Vec::new();

    for managed in inputs.windows {
        if !managed.is_ssd || managed.is_minimized || managed.is_fullscreen {
            continue;
        }
        let Some(loc) = inputs.space.element_location(&managed.window) else { continue };
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

/// Synthetic software cursor - a small filled arrow silhouette (five
/// stacked/offset rectangles, hotspot at the top-left corner), drawn at
/// `inputs.cursor_location`.
///
/// **Why this exists**: nothing rendered a cursor at all before this -
/// `HwdeState::cursor_status` (set from `SeatHandler::cursor_image` in
/// `handlers/seat.rs`) was tracked but never read by anything in
/// `render_elements.rs` or either backend. On `winit_backend.rs` this was
/// mostly masked by the host OS's own cursor showing through the window
/// it renders into; on `backend_drm.rs` - a real, exclusive KMS session
/// with no host cursor to fall back on - this meant **no visible pointer
/// at all**.
///
/// **Why a synthetic shape instead of the client's actual requested
/// cursor**: `CursorImageStatus::Surface(wl_surface)` (what a real
/// client sets via `wl_pointer.set_cursor`, e.g. a themed arrow or an
/// I-beam over text) needs walking that surface's buffer/subsurface
/// tree through Smithay's surface-render-element helpers and resolving
/// its hotspot from the surface's cursor role data - real, doable work,
/// just enough additional unverified API surface (the exact helper
/// function, its exact signature, where the hotspot actually lives) that
/// getting *a* visible cursor working correctly and low-risk first,
/// rather than stacking more uncertainty on top in the same pass, felt
/// like the right scope for this round. `CursorImageStatus::Named(..)`
/// (a themed icon by name, e.g. before any client has set a custom
/// surface) is further out of scope again - it needs an XCursor theme
/// loader, a whole separate file-format-parsing subsystem. This function
/// only distinguishes `Hidden` (nothing drawn - `inputs.cursor_visible`)
/// from "anything else" (this placeholder arrow) - it doesn't yet look
/// at *what* the non-hidden status actually is.
fn cursor_elements(inputs: &RenderInputs, scale: f64) -> Vec<SolidColorRenderElement> {
    if !inputs.cursor_visible {
        return Vec::new();
    }

    let to_physical = |r: Rectangle<i32, smithay::utils::Logical>| -> Rectangle<i32, Physical> {
        Rectangle::new(
            ((r.loc.x as f64 * scale).round() as i32, (r.loc.y as f64 * scale).round() as i32).into(),
            ((r.size.w as f64 * scale).round() as i32, (r.size.h as f64 * scale).round() as i32).into(),
        )
    };

    let origin = inputs.cursor_location.to_i32_round::<i32>();
    let white = Color32F::new(1.0, 1.0, 1.0, 1.0);
    let outline = Color32F::new(0.0, 0.0, 0.0, 1.0);

    // Five 2px-tall horizontal slices, each starting further right than
    // the last and one shorter, approximating a left-leaning arrow
    // silhouette - crude, but unambiguous and immediately recognizable
    // as "a pointer", which is the entire bar this needs to clear (there
    // was none before). `slices` is `(y_offset, x_offset, width)`, all
    // logical px, hotspot at `(0, 0)` (this shape's top-left corner).
    const SLICES: &[(i32, i32, i32)] = &[(0, 0, 14), (2, 0, 12), (4, 0, 10), (6, 0, 8), (8, 2, 4)];

    let mut elements = Vec::with_capacity(SLICES.len() * 2);
    for &(dy, dx, width) in SLICES {
        let rect_logical: Rectangle<i32, smithay::utils::Logical> =
            Rectangle::new((origin.x + dx, origin.y + dy).into(), (width, 2).into());
        // A 1px dark outline behind each slice (the white slice above it
        // in the element list - pushed first, and first = frontmost, see
        // this file's established convention - covers its center and
        // leaves the outline visible only at the edges) so the cursor
        // stays visible over both light and dark window content, not
        // just one or the other.
        let outline_logical: Rectangle<i32, smithay::utils::Logical> =
            Rectangle::new((origin.x + dx - 1, origin.y + dy - 1).into(), (width + 2, 4).into());
        elements.push(SolidColorRenderElement::new(
            smithay::backend::renderer::element::Id::new(),
            to_physical(rect_logical),
            smithay::backend::renderer::utils::CommitCounter::default(),
            white,
            Kind::Unspecified,
        ));
        elements.push(SolidColorRenderElement::new(
            smithay::backend::renderer::element::Id::new(),
            to_physical(outline_logical),
            smithay::backend::renderer::utils::CommitCounter::default(),
            outline,
            Kind::Unspecified,
        ));
    }
    elements
}

/// Builds title-text elements for every window `decoration_elements`
/// drew a bar for - kept as a separate pass (rather than folded into
/// `decoration_elements`) because it needs `&mut GlesRenderer` (to
/// rasterize/upload a texture, via `TitleTextureCache`) while
/// `decoration_elements` only needs the read-only `RenderInputs`; see
/// `build_output_elements` for why that split matters when the renderer
/// is itself borrowed from `HwdeState` (DRM backend case).
fn title_elements(inputs: &RenderInputs, renderer: &mut GlesRenderer, scale: f64) -> Vec<TextureRenderElement<GlesTexture>> {
    let mut cache = inputs.title_cache.borrow_mut();
    let mut elements = Vec::new();

    for managed in inputs.windows {
        if !managed.is_ssd || managed.is_minimized || managed.is_fullscreen {
            continue;
        }
        let Some(loc) = inputs.space.element_location(&managed.window) else { continue };
        let width = managed.window.geometry().size.w;
        let bar = ssd_bar_geometry(loc, width);
        let title = managed.window.toplevel().map(crate::state::with_title).unwrap_or_default();

        if let Some(element) = cache.element_for(renderer, managed.id, &title, bar, scale) {
            elements.push(element);
        }
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
fn focus_border_elements(inputs: &RenderInputs, scale: f64) -> Vec<SolidColorRenderElement> {
    let border_width = inputs.config.border_width;
    if border_width <= 0 {
        return Vec::new();
    }
    let Some(id) = inputs.focused_window else { return Vec::new() };
    let Some(managed) = inputs.windows.iter().find(|w| w.id == id) else { return Vec::new() };
    if managed.is_minimized || managed.is_fullscreen {
        return Vec::new();
    }
    let Some(loc) = inputs.space.element_location(&managed.window) else { return Vec::new() };
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
    inputs: RenderInputs,
    renderer: &mut GlesRenderer,
    output: &Output,
    output_size: (i32, i32),
) -> Vec<OutputRenderElement> {
    let scale = output.current_scale().fractional_scale();
    let mut elements: Vec<OutputRenderElement> = Vec::new();

    // -1. Cursor - absolute frontmost, even over the idle-dim overlay
    //     below, so there's still a visible pointer to click/move with to
    //     wake from it (this is a screensaver, not a lock screen - see
    //     that overlay's own comment).
    elements.extend(cursor_elements(&inputs, scale).into_iter().map(OutputRenderElement::Decoration));

    // 0. Idle-dim overlay - frontmost of literally everything else (even the
    //    focus border), since the whole point is to visually cover the
    //    screen. See `HwdeState::idle_dimmed`'s doc comment in `state.rs`
    //    for what this is - a screensaver-style dimmer, NOT a lock screen
    //    or a security boundary of any kind. Input is never intercepted
    //    because of this flag (see `input.rs`); it only ever affects what
    //    gets drawn.
    if inputs.idle_dimmed {
        elements.push(OutputRenderElement::Decoration(SolidColorRenderElement::new(
            smithay::backend::renderer::element::Id::new(),
            Rectangle::new((0, 0).into(), (output_size.0, output_size.1).into()),
            smithay::backend::renderer::utils::CommitCounter::default(),
            // Near-opaque rather than fully opaque (0.92 alpha) so a
            // client rendering *underneath* isn't a complete guess if
            // something ever goes wrong upstream of this flag - a fully
            // black, fully opaque overlay would look identical whether
            // the session is idly dimmed or the compositor lost track of
            // every window, which is a worse failure mode to be
            // indistinguishable from.
            Color32F::new(0.0, 0.0, 0.0, 0.92),
            Kind::Unspecified,
        )));
    }

    // 1a. Focused-window border - frontmost of all (drawn last, in front
    //     of even the SSD grab bar, since it should always read as "on
    //     top" of the window it's framing).
    elements.extend(focus_border_elements(&inputs, scale).into_iter().map(OutputRenderElement::Decoration));

    // 1b. Title text - drawn just behind the border but in front of the
    //     bar itself, so it sits legibly on top of the `Decoration` bar
    //     background pushed below. Needs `renderer` (texture rasterize/
    //     upload), unlike the other decoration passes here.
    elements.extend(title_elements(&inputs, renderer, scale).into_iter().map(OutputRenderElement::Texture));

    // 1c. Decorations - grab bar/close button.
    elements.extend(decoration_elements(&inputs, scale).into_iter().map(OutputRenderElement::Decoration));

    // 2. Windows + all layer-shell layers (space_render_elements already
    //    interleaves Top/Overlay before, and Background/Bottom after, the
    //    window stack itself).
    match space_render_elements::<_, Window, _>(renderer, [inputs.space], output, 1.0) {
        Ok(space_elements) => {
            elements.extend(space_elements.into_iter().map(OutputRenderElement::Space));
        }
        Err(err) => {
            tracing::warn!("space_render_elements failed: {err:?}");
        }
    }

    // 3. Wallpaper - backmost (drawn first).
    if let Some(wallpaper) = inputs.wallpaper.render_element(output_size) {
        elements.push(OutputRenderElement::Texture(wallpaper));
    }

    elements
}
