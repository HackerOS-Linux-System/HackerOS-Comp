use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer, GlesTexProgram, Uniform};
use smithay::backend::renderer::element::{
    Element, Id, Kind, RenderElement, UnderlyingStorage,
};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::utils::{Buffer as BufferCoords, Physical, Point, Rectangle, Scale, Transform};
use smithay::utils::user_data::UserDataMap;
use smithay::reexports::wayland_server::Resource;
// `Window::wl_surface()` (used by `sole_fullscreen_hdr_surface` below)
// is a `WaylandFocus` trait method, not inherent on `Window` — same
// import `state/mod.rs` needs for its own `win.wl_surface()` call in
// `update_ipc_windows`.
use smithay::wayland::seat::WaylandFocus;

/// True when `output` currently shows exactly one mapped window, that
/// window is fullscreen (per `BlueState::window_meta`), and its current
/// surface has a negotiated HDR image description (per
/// `wp_color_management_v1`, tracked in `state.color_management_state.
/// surface_color_state`). This is the exact condition `render_udev`
/// gates the HDR tone-mapping fast path on — see this module's doc
/// comment for why "sole fullscreen" specifically, rather than "any HDR
/// surface".
///
/// `state.space.elements()`/`outputs_for_element` and `Window::
/// wl_surface()` are the same calls `BlueState::update_ipc_windows`
/// already makes (see `state/mod.rs`) — this mirrors that existing,
/// working pattern rather than inventing a new way to walk the space.
pub fn sole_fullscreen_hdr_surface(state: &crate::state::BlueState, output: &smithay::output::Output) -> bool {
    let mut on_this_output = state
        .space
        .elements()
        .filter(|w| state.space.outputs_for_element(w).iter().any(|o| o == output));

    let Some(window) = on_this_output.next() else { return false };
    if on_this_output.next().is_some() {
        // More than one window on this output — not the sole-fullscreen
        // case, bulk-composite normally (also covers the ordinary
        // "fullscreen window plus an OSD layer surface" case correctly,
        // since layer surfaces aren't `state.space` elements at all).
        return false;
    }

    let Some(wl_surface) = window.wl_surface() else { return false };
    let surface_id = wl_surface.id().protocol_id() as u64;
    let is_fullscreen = state
        .window_meta
        .get(&surface_id)
        .map(|m| m.is_fullscreen)
        .unwrap_or(false);
    if !is_fullscreen {
        return false;
    }

    state
        .color_management_state
        .surface_color_state
        .get(&wl_surface.id())
        .map(|desc| desc.is_hdr())
        .unwrap_or(false)
}

/// Transparent `RenderElement` wrapper that swaps in a custom texture
/// shader (e.g. [`compile_hdr_tonemap_shader`]'s program) for exactly the
/// one wrapped element's `draw()` call, then restores the default
/// immediately after — see this module's doc comment for why that's
/// possible despite `override_default_tex_program`'s "whole frame" doc
/// wording, and for the fullscreen-only scope this is actually used at.
pub struct HdrAwareElement<E> {
    inner: E,
    /// `Some(program)` to render this element through that shader;
    /// `None` makes this wrapper a pure passthrough (used for the
    /// non-HDR elements sharing the same `Vec` in `render_udev`, so the
    /// whole elements list can stay one homogeneous type without an
    /// enum).
    program: Option<GlesTexProgram>,
}

impl<E> HdrAwareElement<E> {
    pub fn new(inner: E, program: Option<GlesTexProgram>) -> Self {
        Self { inner, program }
    }

    /// Passthrough constructor for elements that don't need any shader
    /// override — reads slightly clearer at call sites than
    /// `HdrAwareElement::new(elem, None)`.
    pub fn passthrough(inner: E) -> Self {
        Self { inner, program: None }
    }
}

impl<E: Element> Element for HdrAwareElement<E> {
    fn id(&self) -> &Id {
        self.inner.id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }
    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.inner.location(scale)
    }
    fn src(&self) -> Rectangle<f64, BufferCoords> {
        self.inner.src()
    }
    fn transform(&self) -> Transform {
        self.inner.transform()
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }
    fn damage_since(&self, scale: Scale<f64>, commit: Option<CommitCounter>) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }
    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.inner.opaque_regions(scale)
    }
    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }
    fn kind(&self) -> Kind {
        self.inner.kind()
    }
    fn is_framebuffer_effect(&self) -> bool {
        self.inner.is_framebuffer_effect()
    }
}

impl<E: RenderElement<GlesRenderer>> RenderElement<GlesRenderer> for HdrAwareElement<E> {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, BufferCoords>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        if let Some(program) = &self.program {
            let uniforms: Vec<Uniform<'static>> = Vec::new();
            frame.override_default_tex_program(program.clone(), uniforms);
        }
        let result = self.inner.draw(frame, src, dst, damage, opaque_regions, cache);
        if self.program.is_some() {
            frame.clear_tex_program_override();
        }
        result
    }

    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        // Critical correctness point, not just an optimization: if this
        // element carries an HDR program override and its
        // `underlying_storage` still reported e.g. a DRM plane handle,
        // the DRM backend could place it on a scanout/overlay plane
        // directly — bypassing `draw()` (and therefore this shader)
        // entirely, silently undoing the tone-mapping. Forcing `None`
        // whenever an override is set means an HDR-tagged element is
        // always composited through GL, never direct-scanned-out.
        if self.program.is_some() {
            None
        } else {
            self.inner.underlying_storage(renderer)
        }
    }

    fn capture_framebuffer(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, BufferCoords>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), GlesError> {
        self.inner.capture_framebuffer(frame, src, dst, cache)
    }
}


// NOTE ON VERIFICATION: the shader contract (uniform names, the
// `//_DEFINES` marker, `GlesTexProgram`/`UniformName`/`Uniform` types)
// was read directly from source. The GLSL tone-mapping math itself
// (PQ EOTF constants from ST 2084, BT.2020→BT.709 primaries matrix) is
// written from the published standard coefficients, not copied from any
// existing shader — never run through an actual GLSL compiler in this
// session (no GPU/EGL context available here — see build.rb's `smoke`
// command doc for why). Suspect first if colors look wrong: the
// BT.2020→BT.709 matrix (fixed, standard) vs. the reference-white
// scaling constant (100.0 nits assumed here — some HDR content wants
// this driven by the `wp_color_management_surface_v1` mastering
// luminance metadata already captured in `ColorDescription`, but
// plumbing that into this shader's uniforms is itself part of the
// render-loop restructuring noted above, not attempted here).

/// GLSL ES fragment-shader body compiled via `compile_custom_texture_shader`.
/// The `//_DEFINES` line is mandatory (smithay substitutes it with
/// `#define EXTERNAL`/`#define NO_ALPHA`/`#define DEBUG_FLAGS` as
/// needed) — see this module's doc comment for the exact contract.
/// Kept in its own file (`hdr_tonemap.frag`, plain GLSL, no Rust escaping
/// to fight with when editing the shader math) and pulled in verbatim
/// via `include_str!` rather than as an inline Rust string literal.
const HDR_TONEMAP_FRAGMENT_SHADER: &str = include_str!("hdr_tonemap.frag");

/// Compile the HDR tone-mapping shader once, at renderer-init time
/// (alongside `protocols::dmabuf::init_dmabuf`/`color_management::
/// init_color_management` — same call sites in render/mod.rs). Cheap to
/// call once and hold onto; expensive-ish (a real GL shader compile) to
/// call per frame, which is exactly why this returns a reusable
/// `GlesTexProgram` rather than being inlined into the render loop.
///
/// No `additional_uniforms` needed: `tex`/`alpha`/`v_coords` (and `tint`
/// under `DEBUG_FLAGS`) are provided automatically by smithay for every
/// custom texture shader per the contract documented in
/// `compile_custom_texture_shader`'s doc comment — this shader doesn't
/// need anything beyond those.
pub fn compile_hdr_tonemap_shader(renderer: &mut GlesRenderer) -> Result<GlesTexProgram, GlesError> {
    renderer.compile_custom_texture_shader(HDR_TONEMAP_FRAGMENT_SHADER, &[])
}
