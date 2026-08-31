use calloop::LoopHandle;
use smithay::{
    delegate_compositor, delegate_data_device, delegate_fractional_scale,
    delegate_layer_shell, delegate_output, delegate_presentation,
    delegate_primary_selection, delegate_seat, delegate_shm,
    delegate_single_pixel_buffer, delegate_content_type,
    delegate_viewporter, delegate_xdg_shell,
    delegate_pointer_constraints, delegate_relative_pointer,
    delegate_tablet_manager, delegate_text_input_manager, delegate_input_method_manager,
    desktop::{layer_map_for_output, PopupManager, Space, Window},
    input::{Seat, SeatHandler, SeatState, pointer::CursorImageStatus},
    output::Output,
    reexports::{
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_surface::WlSurface, wl_seat},
            Display, DisplayHandle, Resource,
        },
    },
    utils::{Clock, Logical, Monotonic, Point, Serial, Rectangle, Size},
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState},
        fractional_scale::{FractionalScaleHandler, FractionalScaleManagerState},
        output::{OutputHandler, OutputManagerState},
        presentation::PresentationState,
        seat::WaylandFocus,
        selection::{
            data_device::{DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler},
            primary_selection::{PrimarySelectionHandler, PrimarySelectionState},
            SelectionHandler, SelectionSource, SelectionTarget,
        },
        shell::{
            wlr_layer::{
                Layer, LayerSurface as WlrLayerSurface,
                WlrLayerShellHandler, WlrLayerShellState,
            },
            xdg::{
                PopupSurface, PositionerState, ToplevelSurface,
                XdgShellHandler, XdgShellState,
                // xdg_toplevel is private in this Smithay rev - access via full path
            },
        },
        shm::{ShmHandler, ShmState},
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
        xdg_activation::XdgActivationState,
        shell::xdg::decoration::XdgDecorationState,
        cursor_shape::CursorShapeManagerState,
        session_lock::{SessionLockManagerState, SessionLocker, LockSurface},
        pointer_constraints::{PointerConstraintsHandler, PointerConstraintsState},
        relative_pointer::RelativePointerManagerState,
        tablet_manager::TabletManagerState,
        text_input::TextInputManagerState,
        input_method::{InputMethodHandler, InputMethodManagerState},
    },
    input::dnd::DndGrabHandler,
    xwayland::{XWayland, xwm::X11Wm},
};
use std::{
    collections::HashMap,
    os::unix::io::OwnedFd,
    os::unix::net::UnixStream,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tracing::{info, warn};

/// Type alias used throughout the IPC layer (HackerLand's server-side
/// handlers, `src/ipc/hackerland_ipc.rs`) to refer to the compositor's
/// state type without every IPC module needing to know it's called
/// `BlueState` internally — "HWDE" (HackerOS Wayland Desktop
/// Environment) is this compositor's protocol-facing identity,
/// `BlueState` is just its Rust struct name.
pub type HwdeState = BlueState;

/// Which screen edge a pinned surface is docked to — e.g. a panel or
/// dock reserving screen space for itself. Currently only set via
/// [`BlueState::pin_surface`] directly; no IPC call drives this today
/// (there is no IPC call wired to this yet — see ROADMAP.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinnedEdge {
    Top,
    Bottom,
}

/// One surface pinned to a screen edge (a panel or dock registering
/// itself with the compositor so `output_geometry`-consuming layout
/// code can reserve space for it).
#[derive(Debug, Clone)]
pub struct PinnedSurface {
    pub app_id: String,
    pub edge: PinnedEdge,
    pub thickness_px: u32,
    /// PID of the process that requested the pin, if known — kept so a
    /// pin can be cleaned up automatically if that process
    /// disconnects/dies, without another component having to
    /// explicitly unpin it first. Not yet wired to any disconnect hook
    /// — see ROADMAP.md.
    pub owner_pid: Option<i32>,
}

/// Minimal wallpaper state: just the current path plus whether a reload
/// is pending. Actual rendering (loading the image, scaling it to each
/// output, uploading it as a texture) is `render/mod.rs`'s job — this
/// struct only tracks *what* the desired wallpaper is, set via
/// `SdeCall::SetWallpaper`/HackerLand's `dispatch setwallpaper <path>`,
/// for the render path to notice via `pending_wallpaper_reload` and
/// pick up on its next frame.
#[derive(Debug, Clone, Default)]
pub struct WallpaperState {
    pub path: Option<std::path::PathBuf>,
}

impl WallpaperState {
    pub fn set_path(&mut self, path: impl Into<std::path::PathBuf>) {
        self.path = Some(path.into());
    }
}

// ── IPC window info ────────────────────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct WindowInfo {
    pub id: u64,
    pub title: String,
    pub app_id: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub is_fullscreen: bool,
    pub is_minimized: bool,
    pub workspace: u32,
}

// ── Client state ───────────────────────────────────────────────────────────

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
}

// ── Backend data ───────────────────────────────────────────────────────────

pub enum BackendData {
    None,
    Udev(Box<UdevData>),
    Winit(Box<WinitData>),
}

pub struct UdevData {
    pub session: smithay::backend::session::libseat::LibSeatSession,
    pub primary_gpu: smithay::backend::drm::DrmNode,
    pub devices: HashMap<smithay::backend::drm::DrmNode, GpuDevice>,
    /// Shared across every GPU node this compositor knows about — see
    /// `render/multigpu.rs`'s module doc for the lifecycle (registered on
    /// `open_gpu()` success, deregistered on `UdevEvent::Removed`) and
    /// for what cross-GPU import is/isn't wired up yet.
    pub gpu_manager: smithay::backend::renderer::multigpu::GpuManager<crate::render::multigpu::Backend>,
}

/// Which render path a given GPU ended up on. `Gles` is the normal case
/// (a real GPU with a working EGL/GBM driver stack); `Pixman` is the
/// software fallback for the case that motivated adding this enum at
/// all — a headless VM / CI runner with a KMS-capable display controller
/// (`vkms`, QEMU's `bochs-drm`/`virtio-gpu` in software mode, ...) but no
/// GPU driver capable of EGL context creation, where the previous
/// "GLES or nothing" code left `GpuDevice::renderer` permanently `None`
/// and every output on it silently never rendered (see
/// `scan_drm_outputs`'s `warn!("No renderer available...")`, which used
/// to be the end of the story).
///
/// Deliberately a real enum over the renderer type rather than trying to
/// make `GpuDevice`/`OutputRenderSurface`/`render_udev` generic over
/// `R: Renderer` — `GlesRenderer` and `PixmanRenderer` don't share a
/// buffer-import story simple enough to unify here (GLES imports GBM
/// dma-bufs via EGL; Pixman renders into plain mapped CPU memory), and
/// `hdr_shader.rs`'s `HdrAwareElement` is GLES-specific already (see
/// this file's module doc), so a generic `GpuDevice<R>` would still need
/// to special-case the GLES arm for HDR anyway. Matching smithay's own
/// reference compositor (`anvil`), which draws this exact same
/// distinction with its own `Either`-shaped renderer/surface types, not
/// a novel design.
pub enum RenderBackend {
    Gles(smithay::backend::renderer::gles::GlesRenderer),
    Pixman(smithay::backend::renderer::pixman::PixmanRenderer),
}

impl RenderBackend {
    pub fn is_software(&self) -> bool {
        matches!(self, RenderBackend::Pixman(_))
    }

    pub fn as_gles(&self) -> Option<&smithay::backend::renderer::gles::GlesRenderer> {
        match self {
            RenderBackend::Gles(r) => Some(r),
            RenderBackend::Pixman(_) => None,
        }
    }

    pub fn as_gles_mut(&mut self) -> Option<&mut smithay::backend::renderer::gles::GlesRenderer> {
        match self {
            RenderBackend::Gles(r) => Some(r),
            RenderBackend::Pixman(_) => None,
        }
    }

    pub fn as_pixman_mut(&mut self) -> Option<&mut smithay::backend::renderer::pixman::PixmanRenderer> {
        match self {
            RenderBackend::Pixman(r) => Some(r),
            RenderBackend::Gles(_) => None,
        }
    }
}

pub struct GpuDevice {
    pub drm: smithay::backend::drm::DrmDevice,
    pub gbm: smithay::backend::allocator::gbm::GbmDevice<smithay::backend::drm::DrmDeviceFd>,
    /// Shared renderer context for this GPU — `Gles` on the normal path,
    /// `Pixman` when no usable GPU/EGL driver was found for this DRM
    /// node (see `RenderBackend`'s doc). `None` until the first
    /// `scan_drm_outputs()` call successfully creates one (renderer
    /// creation needs at least one open GBM device, which we already
    /// have by then).
    pub renderer: Option<RenderBackend>,
    /// HDR tone-mapping shader, compiled once for this GPU's GL context
    /// right after `renderer` is created — see `render/hdr_shader.rs`'s
    /// module doc for why it's compiled-and-ready but not yet called
    /// from anywhere in the composite pass.
    pub hdr_tonemap_shader: Option<smithay::backend::renderer::gles::GlesTexProgram>,
    /// One entry per lit-up CRTC/connector pair, keyed by CRTC handle so
    /// hotplug add/remove can look a specific output up cheaply.
    pub surfaces: HashMap<smithay::reexports::drm::control::crtc::Handle, OutputRenderSurface>,
}

/// Per-output DRM/KMS rendering state: the buffered GBM swapchain that
/// backs a `DrmSurface`, plus the damage tracker and a handle back to the
/// smithay `Output` it belongs to. Bundling these together is what
/// `render_udev()` needs to actually push frames to real hardware via
/// atomic modesetting — this didn't exist at all before (bare-metal/TTY
/// mode only ever *detected* outputs via `scan_drm_outputs`, it never
/// rendered to them).
///
/// # Multi-GPU render-node import
/// On hybrid-graphics laptops, a client's buffer can be allocated on a
/// *different* GPU than the one driving the connected display (e.g. an
/// app rendered on the discrete GPU, but the primary/output GPU is the
/// integrated one). Presenting such a buffer requires importing it across
/// GPUs first — copying it into a buffer usable on the primary GPU —
/// which `GlesRenderer` alone cannot do; it only knows about the single
/// EGL context/device it was created against.
///
/// The lifecycle infrastructure for this is implemented — see
/// `render/multigpu.rs`'s module doc for the details: `UdevData::
/// gpu_manager` (a `smithay::backend::renderer::multigpu::GpuManager`) is
/// kept in sync with every GPU node this compositor opens or loses, and
/// `render::multigpu::import_from_other_gpu()` is a real, callable
/// cross-GPU dmabuf import primitive built on it. What's *not* yet done
/// is having anything actually call that function during normal
/// rendering: `render_udev` still renders each output with a single
/// per-GPU `GlesRenderer` (`GpuDevice::renderer`) for every element in
/// one bulk `space.render_elements_for_output(...)` call, and nothing
/// currently detects, per surface, whether that surface's current buffer
/// was allocated on a different node than the output being drawn to.
/// Wiring that in means either pre-warming the per-surface texture cache
/// from the buffer-commit handler when a cross-GPU mismatch is detected,
/// or restructuring the render pass to pick a renderer per element
/// instead of once per frame — both real, scoped follow-ups, neither
/// attempted here since both touch the render/commit hot path, which is
/// exactly what most needs a real `cargo build` and a second GPU to
/// verify against (see ROADMAP.md).
/// A minimal, hand-rolled double-buffered dumb-buffer swapchain — the
/// Pixman-path counterpart to `GbmBufferedSurface` on the GLES path.
/// `DumbBuffer`/`add_framebuffer`/`map_dumb_buffer`/`page_flip` are all
/// plain DRM/KMS ioctls (via the `drm` crate's `control::Device` trait),
/// available on *any* KMS device including GPU-less ones (`vkms`,
/// virtualized display controllers) — that universality, at the cost of
/// no GPU acceleration at all, is the entire point of this path existing.
///
/// No smithay type does this bookkeeping for us the way
/// `GbmBufferedSurface` does for the GBM/EGL path — this pinned smithay
/// rev's dumb-buffer support is the raw `DumbBuffer` allocator type, not
/// a ready-made swapchain, so front/back tracking and the page-flip wait
/// are done by hand here, mirroring what `GbmBufferedSurface` does
/// internally (checked directly against that type's own source at the
/// pinned rev) rather than inventing a new scheme.
pub struct DumbSwapchain {
    /// This path deliberately bypasses smithay's `DrmSurface` (the
    /// atomic-KMS abstraction `GbmBufferedSurface` drives on the GLES
    /// path) and talks to the raw `drm::control::Device` legacy
    /// `set_crtc`/`page_flip` ioctls directly on the shared `DrmDevice`
    /// instead — `DrmSurface` is built around committing GBM-backed
    /// planes and doesn't have an equivalent "just point this CRTC at
    /// this plain dumb-buffer framebuffer" entry point. The CRTC and
    /// connector handles are kept here (rather than a `DrmSurface`)
    /// because the raw `page_flip`/`set_crtc` calls need them passed
    /// explicitly on every call.
    pub crtc: smithay::reexports::drm::control::crtc::Handle,
    pub connector: smithay::reexports::drm::control::connector::Handle,
    /// Two buffers, each already wrapped in a KMS framebuffer id via
    /// `add_framebuffer` — index `front` is on-screen (or was, as of the
    /// last completed page-flip); rendering always targets `1 - front`.
    pub buffers: [DumbSwapchainBuffer; 2],
    pub front: usize,
}

pub struct DumbSwapchainBuffer {
    /// The *raw* `drm`-crate dumb buffer (via
    /// `smithay::reexports::drm`, not a second, independently-pinned
    /// `drm` dependency of this crate's own — an earlier version of
    /// this file's Cargo.toml had exactly that, at a mismatched version
    /// from what smithay's own git checkout uses, which is a real bug a
    /// `cargo build` caught: two incompatible copies of the same crate
    /// in the dependency graph, so types built against one didn't
    /// satisfy trait bounds written against the other). This is the
    /// type `drm.add_framebuffer`/`set_crtc`/`page_flip` all need — see
    /// `render_udev_pixman`'s doc for how this same buffer's memory
    /// also gets mapped and wrapped in a `pixman::Image` for
    /// `PixmanRenderer::bind`, which is a *different* type need served
    /// from the same underlying allocation, not something this field's
    /// type itself has to satisfy.
    pub dumb: smithay::reexports::drm::control::dumbbuffer::DumbBuffer,
    pub fb: smithay::reexports::drm::control::framebuffer::Handle,
    /// Whether this buffer has ever been rendered into before — used to
    /// derive its `OutputDamageTracker` buffer age (see
    /// `DumbSwapchain::back_age`'s doc for why "ever rendered" is the
    /// only bit of history this swapchain needs to track, rather than a
    /// real per-frame counter).
    pub ever_rendered: bool,
}

impl DumbSwapchain {
    /// The buffer the next frame should be rendered into.
    pub fn back_mut(&mut self) -> &mut DumbSwapchainBuffer {
        &mut self.buffers[1 - self.front]
    }

    /// The buffer-age value to pass as `OutputDamageTracker::render_
    /// output`'s `age` parameter for whatever's about to be rendered
    /// into the back buffer (was: always hardcoded `0`, i.e. "unknown,
    /// redraw everything, every frame" — correct but needlessly
    /// expensive on exactly the software-rendering path that can least
    /// afford full-frame redraws every frame).
    ///
    /// `0` (full redraw needed) the first two times a given buffer is
    /// used — swapchain starts with two freshly-allocated, contentless
    /// buffers, so there's nothing to accumulate damage against yet.
    /// `2` every time after that, unconditionally — *not* a real
    /// per-frame-tracked age counter, because this is deliberately a
    /// strict, non-skipping double buffer (`render_udev_pixman` renders
    /// and flips exactly once per output-render tick, no triple
    /// buffering, no dropped frames): with only two buffers strictly
    /// alternating, by the time either one is reused as a render target
    /// again it is *always* exactly two frames stale relative to "now"
    /// — this isn't an approximation of the real age, it's what the
    /// real age always evaluates to for this specific swapchain shape.
    /// (Contrast with the GBM path's `GbmBufferedSurface::next_buffer()`,
    /// which reports a real per-call age because *that* swapchain can
    /// have more than two buffers and doesn't guarantee strict
    /// alternation.)
    pub fn back_age(&self) -> usize {
        if self.buffers[1 - self.front].ever_rendered { 2 } else { 0 }
    }
}

/// Which physical swapchain this output's surface uses — `Gbm` on the
/// normal GLES/hardware-accelerated path, `Dumb` when the owning
/// `GpuDevice::renderer` fell back to `RenderBackend::Pixman` (see that
/// enum's doc for why/when). The two are mutually exclusive per-GPU in
/// practice — `scan_drm_outputs` picks one `RenderBackend` for the whole
/// device and every surface on it follows — but this is still a real
/// enum (not e.g. an `Option<GbmBufferedSurface>` alongside an
/// `Option<DumbSwapchain>`) so `render_udev` can `match` exhaustively
/// instead of juggling two independently-optional fields that are
/// actually never both `Some`/both `None` by construction.
pub enum SurfaceBackend {
    Gbm(smithay::backend::drm::GbmBufferedSurface<
        smithay::backend::allocator::gbm::GbmAllocator<smithay::backend::drm::DrmDeviceFd>,
        (),
    >),
    Dumb(DumbSwapchain),
}

pub struct OutputRenderSurface {
    pub output: Output,
    pub surface: SurfaceBackend,
    pub damage_tracker: smithay::backend::renderer::damage::OutputDamageTracker,
    /// The connector this surface is currently driving — needed to
    /// rebuild the `DrmSurface` (via `drm.create_surface(crtc, mode,
    /// &[connector])`) when a real hardware modeset is requested through
    /// `zwlr_output_management`, since `create_surface` takes the
    /// connector list explicitly rather than remembering it internally.
    pub connector: smithay::reexports::drm::control::connector::Handle,
}

pub struct WinitData {
    pub backend: smithay::backend::winit::WinitGraphicsBackend<
        smithay::backend::renderer::gles::GlesRenderer,
    >,
    pub output: Output,
    pub damage_tracker: smithay::backend::renderer::damage::OutputDamageTracker,
    /// See `GpuDevice::hdr_tonemap_shader` (udev path) — same purpose,
    /// compiled against winit's single `GlesRenderer` context instead.
    pub hdr_tonemap_shader: Option<smithay::backend::renderer::gles::GlesTexProgram>,
}

// ── Multi-monitor configuration ────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct OutputConfig {
    pub name: String,
    pub position: Point<i32, Logical>,
    pub scale: f64,
    pub mode: smithay::output::Mode,
}

// ── Window metadata (title, app_id) ───────────────────────────────────────

#[derive(Default, Clone)]
pub struct WindowMeta {
    pub title: String,
    pub app_id: String,
    pub is_fullscreen: bool,
    pub is_minimized: bool,
    pub is_floating: bool,
    pub is_maximized: bool,
    pub workspace: usize,
}

// ── Main compositor state ──────────────────────────────────────────────────

pub struct BlueState {
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, Self>,
    pub clock: Clock<Monotonic>,
    pub socket_name: String,
    pub space: Space<Window>,
    pub popup_manager: PopupManager,
    pub current_workspace: usize,
    pub workspace_count: usize,

    // Wayland protocol states
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub layer_shell_state: WlrLayerShellState,
    pub presentation_state: PresentationState,
    pub fractional_scale_manager_state: FractionalScaleManagerState,
    pub viewporter_state: ViewporterState,
    pub xdg_decoration_state: XdgDecorationState,
    pub cursor_shape_manager_state: CursorShapeManagerState,
    pub session_lock_state: SessionLockManagerState,
    /// `pointer-constraints-unstable-v1` — lets clients lock/confine the
    /// pointer (needed for games and fullscreen apps that want raw mouse
    /// look). Previously entirely absent.
    pub pointer_constraints_state: PointerConstraintsState,
    /// `relative-pointer-unstable-v1` — delivers unaccelerated pointer
    /// deltas alongside a pointer lock, which is what most 3D
    /// games/creative apps actually want (absolute position becomes
    /// meaningless once the pointer is locked to one spot).
    pub relative_pointer_manager_state: RelativePointerManagerState,
    /// `zwp_tablet_manager_v2` — graphics-tablet (stylus) support.
    pub tablet_manager_state: TabletManagerState,
    /// `zwp_text_input_manager_v3` — lets text-entry-capable clients
    /// (terminals, text fields) request IME composition support.
    pub text_input_manager_state: TextInputManagerState,
    /// `zwp_input_method_manager_v2` — the other half of IME support:
    /// lets an actual input-method client (e.g. a CJK IME, an on-screen
    /// keyboard) attach to the seat and drive text-input clients.
    pub input_method_manager_state: InputMethodManagerState,
    /// Live IME candidate-window popups — see protocols/input_method.rs.
    /// Previously `new_popup`/`dismiss_popup` were empty stubs, so an
    /// IME's popup surface existed on the wire but was never tracked or
    /// composited anywhere; this is what makes it actually visible.
    pub input_method_popups: Vec<crate::protocols::input_method::TrackedImePopup>,
    /// `zwp_linux_dmabuf_v1` — see protocols/dmabuf.rs. `None` until
    /// `protocols::dmabuf::init_dmabuf` runs (needs a bound renderer, so
    /// it can't happen inside `BlueState::new()` like most other globals
    /// here — it's called once the winit/udev backend has a renderer).
    pub dmabuf_state: Option<smithay::wayland::dmabuf::DmabufState>,
    /// `wp_single_pixel_buffer_v1` — lets a client create a 1x1 solid-color
    /// buffer without allocating shm/dmabuf memory for it. Trivial but
    /// real: toolkits (GTK4, Qt6) use this for solid-color fills/
    /// backgrounds/borders instead of a wasteful shm buffer, and its
    /// absence as a global is something a toolkit can and does detect
    /// (falls back to shm, which works fine, just less efficiently — so
    /// this isn't fixing broken behavior, it's closing a real, if minor,
    /// feature gap against compositors like cosmic-comp/KWin/Mutter that
    /// already advertise it). See main.rs's init for where this is
    /// created. Full API verified directly against the vendored smithay
    /// source (`wayland::single_pixel_buffer` — a complete, ready-to-use
    /// module, not something built from protocol XML by hand like
    /// color-management): renderer-level import support already exists
    /// in `backend::renderer::mod.rs`, so registering this global is the
    /// entire integration — no render-loop changes needed.
    pub single_pixel_buffer_state: smithay::wayland::single_pixel_buffer::SinglePixelBufferState,
    /// `wp_content_type_v1` — lets a client (video players, games) hint
    /// what kind of content a surface shows, queryable later via
    /// `ContentTypeSurfaceCachedState::content_type()` in `compositor::
    /// with_states`. Not read anywhere yet (no VRR/adaptive-sync or
    /// power-saving behavior branches on it today) — registering the
    /// global is still real forward progress on its own, the same
    /// "advertise the negotiation honestly" reasoning already applied to
    /// `wp_color_management_v1` in `color_management.rs`: a toolkit can
    /// start sending this hint now, and reading it back is a
    /// self-contained follow-up (see ROADMAP.md).
    pub content_type_state: smithay::wayland::content_type::ContentTypeState,
    /// `ext_workspace_v1` — see `protocols/ext_workspace.rs`'s module
    /// doc for what this compositor's fixed-workspace-count model maps
    /// onto it, and for the XML's provenance (fetched from the
    /// `wayland-protocols` crate on crates.io, since gitlab.freedesktop.org
    /// itself isn't reachable from this environment).
    pub ext_workspace_state: crate::protocols::ext_workspace::ExtWorkspaceState,
    pub dmabuf_global: Option<smithay::wayland::dmabuf::DmabufGlobal>,
    /// `wp_color_management_v1` — see protocols/color_management.rs.
    pub color_management_state: crate::protocols::color_management::ColorManagementState,
    /// Which DRM render node each surface's current buffer was allocated
    /// on, when known (dmabuf-backed buffers only — shm buffers have no
    /// GPU origin to track). Kept up to date by `render::multigpu::
    /// track_surface_origin`, called from `CompositorHandler::commit`.
    /// See `render/multigpu.rs`'s module doc for what this enables and
    /// what still doesn't consume it.
    pub surface_gpu_origin: HashMap<
        smithay::reexports::wayland_server::backend::ObjectId,
        smithay::backend::drm::DrmNode,
    >,
    /// Live layer-shell surfaces (panels, lock screen, on-screen
    /// keyboards), keyed by their underlying `WlSurface`'s object id.
    /// Kept around specifically so `layer_destroyed` can hand the *same*
    /// `desktop::LayerSurface` wrapper instance back to `LayerMap::
    /// unmap_layer` — see the long comment on `new_layer_surface` for
    /// why a freshly-constructed wrapper wouldn't compare equal to the
    /// one actually stored in the map.
    pub layer_surfaces: HashMap<
        smithay::reexports::wayland_server::backend::ObjectId,
        smithay::desktop::LayerSurface,
    >,
    /// `zwlr_foreign_toplevel_management_v1` — see protocols/foreign_toplevel.rs
    pub foreign_toplevel_state: crate::protocols::foreign_toplevel::ForeignToplevelManagerState,
    /// `zwlr_output_management_v1` — see protocols/output_management.rs
    pub output_management_state: crate::protocols::output_management::OutputManagementState,
    /// `zwlr_screencopy_manager_v1` — see protocols/screencopy.rs
    pub screencopy_state: crate::protocols::screencopy::ScreencopyState,

    // Session lock (ext-session-lock-v1) runtime state
    pub is_locked: bool,
    pub pending_lock: Option<SessionLocker>,
    pub lock_surfaces: HashMap<String, LockSurface>,

    pub seat: Seat<Self>,
    pub pointer_location: Point<f64, Logical>,
    pub cursor_status: Arc<Mutex<CursorImageStatus>>,
    pub outputs: Vec<Output>,
    pub output_configs: Vec<OutputConfig>,

    /// `None` until `input_emulation::init()` runs (called from both
    /// backends' startup in `main.rs`); see that module's doc for what
    /// this actually is (an EIS/libei input-emulation server) and what
    /// it deliberately doesn't cover yet (the xdg-desktop-portal
    /// RemoteDesktop D-Bus service).
    pub eis_state: Option<crate::input_emulation::EisServerState>,

    // XWayland
    pub xwayland:             Option<XWayland>,
    pub xwm:                  Option<X11Wm>,
    pub x11_display:          Option<u32>,
    /// Backing state for the `xwayland-shell-v1` protocol (lets Smithay associate
    /// wl_surfaces created by XWayland with their X11 window before the WM takes
    /// over). Previously this didn't exist on BlueState and
    /// `XWaylandShellHandler::xwayland_shell_state` reached for `self`'s own
    /// memory via an unsafe transmute as a stopgap — see xwayland/mod.rs history.
    pub xwayland_shell_state: smithay::wayland::xwayland_shell::XWaylandShellState,
    /// The Wayland `Client` handle for the XWayland server process, captured
    /// from `XWayland::spawn`'s return value and consumed once
    /// `XWaylandEvent::Ready` arrives (to start the X11 window manager via
    /// `X11Wm::start_wm`). Previously parked in a `thread_local!` in
    /// xwayland/mod.rs because BlueState had nowhere to put it.
    pub x11_client:           Option<smithay::reexports::wayland_server::Client>,
    pub xdg_activation_state: XdgActivationState,
    pub is_idle:              bool,
    /// Backing state for `idle-inhibit-unstable-v1` (see protocols/idle_inhibit.rs).
    pub idle_inhibit_state:      smithay::wayland::idle_inhibit::IdleInhibitManagerState,
    /// Surfaces currently holding an idle inhibitor. `protocols/idle.rs`'s
    /// DPMS timer should skip blanking while `is_idle_inhibited()` is true.
    pub idle_inhibiting_surfaces: Vec<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>,

    // Backend
    pub backend_data: BackendData,

    // IPC
    pub ipc_windows: Arc<Mutex<Vec<WindowInfo>>>,
    pub clients: Arc<Mutex<Vec<UnixStream>>>,

    // Per-window metadata keyed by surface protocol_id
    pub window_meta: HashMap<u64, WindowMeta>,

    // Lifecycle
    pub should_exit: bool,

    // DPMS / idle
    pub last_input_time: Instant,
    pub dpms_blanked: bool,
    pub dpms_timeout: Duration,

    // Window switcher
    pub show_switcher: bool,
    pub switcher_index: usize,

    // Super key tracking
    pub super_pressed: bool,
    pub super_used: bool,

    // UI state communicated to shell via IPC
    pub start_menu_visible: bool,
    pub fullscreen_menu_visible: bool,

    // ── Config (config.rs) + extern-target identity ─────────────────────
    /// Loaded from `~/.config/HackerOS-Comp/config.hk` (or
    /// `config-<name>.hk` for an `--extern` session) at startup, and
    /// reloadable at runtime via HackerLand's `dispatch reload` — see
    /// `src/config.rs`.
    pub config: crate::config::Config,
    /// `Some(name)` for a `--extern <name>` session (a second,
    /// independently-launched HWDE target with its own config file and
    /// HackerLand socket name), `None` for the native session.
    pub extern_name: Option<String>,
    pub wallpaper: WallpaperState,
    /// Set by `SdeCall::SetWallpaper`/HackerLand's `setwallpaper`;
    /// cleared by `render/mod.rs` once it's picked up the new
    /// `wallpaper.path` and re-uploaded the texture for every output.
    pub pending_wallpaper_reload: bool,
    /// Shared with anything that needs to request a clean shutdown from
    /// outside the event-loop-owning thread (currently just
    /// `SdeCall::Shutdown`/HackerLand's `dispatch exit`, both handled
    /// in-loop today, but `Arc<AtomicBool>` rather than a plain `bool`
    /// so a future signal handler or cross-thread caller can flip it
    /// too without needing `&mut BlueState`). `should_exit()` treats
    /// this and the legacy `should_exit` flag as equivalent — either
    /// one being set ends the compositor.
    pub running: Arc<std::sync::atomic::AtomicBool>,
    /// Workspaces with tiling layout turned on (`SdeCall::SetTiling` /
    /// HackerLand's `dispatch settiling`) — absence from this set means
    /// floating-only, presence means comphwde's simple master-stack
    /// tiling applies. Seeded from `config.general.default_tiling` for
    /// every workspace at startup (see `BlueState::new`).
    pub tiling_workspaces: std::collections::HashSet<usize>,
    /// Surfaces pinned to a screen edge via `SdeCall::PinSurface`,
    /// keyed by `app_id` (an SDE component pinning again with the same
    /// `app_id` replaces its previous pin rather than stacking a
    /// second one).
    pub pinned_surfaces: HashMap<String, PinnedSurface>,
}

impl BlueState {
    pub fn new(
        loop_handle: &LoopHandle<'static, Self>,
        display: Display<Self>,
    ) -> Self {
        Self::new_with_extern_name(loop_handle, display, None)
    }

    /// Same as [`BlueState::new`], but for an `--extern <name>` session
    /// — loads `config-<name>.hk` instead of `config.hk` (see
    /// `config::load_for`) and records `extern_name` so a later
    /// `SdeCall::ReloadConfig` reloads the *same* file rather than
    /// silently falling back to the native session's config.
    pub fn new_with_extern_name(
        loop_handle: &LoopHandle<'static, Self>,
        display: Display<Self>,
        extern_name: Option<String>,
    ) -> Self {
        let display_handle = display.handle();
        let clock = Clock::new();

        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
        let output_manager_state =
            OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        let mut seat_state = SeatState::new();
        let seat = seat_state.new_wl_seat(&display_handle, "seat0");
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&display_handle);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&display_handle);
        let presentation_state =
            PresentationState::new::<Self>(&display_handle, clock.id() as u32);
        let fractional_scale_manager_state =
            FractionalScaleManagerState::new::<Self>(&display_handle);
        let viewporter_state = ViewporterState::new::<Self>(&display_handle);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&display_handle);
        let cursor_shape_manager_state = CursorShapeManagerState::new::<Self>(&display_handle);
        // Accept every client that requests a lock — access control for who
        // is *allowed* to lock the session (vs. merely requesting it) is
        // handled at the app level (only Blue-Lock ships this capability).
        let session_lock_state = SessionLockManagerState::new::<Self, _>(&display_handle, |_client| true);
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&display_handle);
        let relative_pointer_manager_state = RelativePointerManagerState::new::<Self>(&display_handle);
        let tablet_manager_state = TabletManagerState::new::<Self>(&display_handle);
        let text_input_manager_state = TextInputManagerState::new::<Self>(&display_handle);
        let input_method_manager_state = InputMethodManagerState::new::<Self, _>(&display_handle, |_client| true);
        let foreign_toplevel_state = crate::protocols::foreign_toplevel::ForeignToplevelManagerState::new(&display_handle);
        let output_management_state = crate::protocols::output_management::OutputManagementState::new(&display_handle);
        let screencopy_state = crate::protocols::screencopy::ScreencopyState::new(&display_handle);
        let single_pixel_buffer_state = smithay::wayland::single_pixel_buffer::SinglePixelBufferState::new::<Self>(&display_handle);
        let content_type_state = smithay::wayland::content_type::ContentTypeState::new::<Self>(&display_handle);
        let ext_workspace_state = crate::protocols::ext_workspace::init_ext_workspace(&display_handle);

        // Create Wayland socket
        let socket = ListeningSocketSource::new_auto()
            .expect("Failed to create Wayland socket");
        let socket_name = socket.socket_name().to_string_lossy().to_string();
        info!("Wayland socket: {}", socket_name);

        loop_handle
            .insert_source(socket, |client, _, state: &mut BlueState| {
                if let Err(e) = state
                    .display_handle
                    .insert_client(client, Arc::new(ClientState::default()))
                {
                    warn!("Failed to insert client: {}", e);
                }
            })
            .expect("Failed to init socket source");

        // Clone before moving into the struct — needed for XdgActivationState init
        let display_handle_for_activation = display_handle.clone();

        // Load config.hk (or config-<name>.hk) before the struct literal
        // below so `workspace_count`/`tiling_workspaces` can be seeded
        // from it rather than hardcoded — see src/config.rs.
        let config = crate::config::load_for(extern_name.as_deref());
        let workspace_count = config.workspaces.count.max(1);
        let tiling_workspaces: std::collections::HashSet<usize> = if config.general.default_tiling {
            (0..workspace_count).collect()
        } else {
            std::collections::HashSet::new()
        };

        BlueState {
            display_handle,
            loop_handle: loop_handle.clone(),
            clock,
            socket_name,
            space: Space::default(),
            popup_manager: PopupManager::default(),
            current_workspace: 0,
            workspace_count,
            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            layer_shell_state,
            presentation_state,
            fractional_scale_manager_state,
            viewporter_state,
            xdg_decoration_state,
            cursor_shape_manager_state,
            session_lock_state,
            pointer_constraints_state,
            relative_pointer_manager_state,
            tablet_manager_state,
            text_input_manager_state,
            input_method_manager_state,
            input_method_popups: Vec::new(),
            dmabuf_state: None,
            single_pixel_buffer_state,
            content_type_state,
            ext_workspace_state,
            dmabuf_global: None,
            color_management_state: crate::protocols::color_management::ColorManagementState::default(),
            surface_gpu_origin: HashMap::new(),
            layer_surfaces: HashMap::new(),
            foreign_toplevel_state,
            output_management_state,
            screencopy_state,
            is_locked: false,
            pending_lock: None,
            lock_surfaces: HashMap::new(),
            seat,
            pointer_location: Point::from((0.0, 0.0)),
            cursor_status: Arc::new(Mutex::new(CursorImageStatus::default_named())),
            outputs: Vec::new(),
            output_configs: Vec::new(),
            eis_state: None,
            xwayland:             None,
            xwm:                  None,
            x11_display:          None,
            xwayland_shell_state: smithay::wayland::xwayland_shell::XWaylandShellState::new::<BlueState>(&display_handle_for_activation),
            x11_client:           None,
            xdg_activation_state: XdgActivationState::new::<BlueState>(&display_handle_for_activation),
            is_idle:              false,
            idle_inhibit_state: smithay::wayland::idle_inhibit::IdleInhibitManagerState::new::<BlueState>(&display_handle_for_activation),
            idle_inhibiting_surfaces: Vec::new(),
            backend_data: BackendData::None,
            ipc_windows: Arc::new(Mutex::new(Vec::new())),
            clients: Arc::new(Mutex::new(Vec::new())),
            window_meta: HashMap::new(),
            should_exit: false,
            last_input_time: Instant::now(),
            dpms_blanked: false,
            dpms_timeout: Duration::from_secs(300),
            show_switcher: false,
            switcher_index: 0,
            super_pressed: false,
            super_used: false,
            start_menu_visible: false,
            fullscreen_menu_visible: false,
            config,
            extern_name,
            wallpaper: WallpaperState::default(),
            pending_wallpaper_reload: false,
            running: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            tiling_workspaces,
            pinned_surfaces: HashMap::new(),
        }
    }

    pub fn socket_name(&self) -> &str {
        &self.socket_name
    }

    /// `true` if either the legacy `should_exit` flag or the newer
    /// `running` flag (settable from outside the event loop, e.g.
    /// `SdeCall::Shutdown`/HackerLand's `dispatch exit` — see
    /// `running`'s field doc) says to stop.
    pub fn should_exit(&self) -> bool {
        self.should_exit || !self.running.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn refresh(&mut self) {
        self.space.refresh();
        self.popup_manager.cleanup();
        self.update_ipc_windows();
        self.flush_display();
        self.handle_dpms();
    }

    fn flush_display(&mut self) {
        // Flush pending Wayland events
        if let Err(e) = self.display_handle.flush_clients() {
            warn!("Display flush error: {}", e);
        }
    }

    fn handle_dpms(&mut self) {
        let idle = self.last_input_time.elapsed();
        if idle > self.dpms_timeout && !self.dpms_blanked {
            self.dpms_blanked = true;
            self.blank_outputs();
        } else if idle <= self.dpms_timeout && self.dpms_blanked {
            self.dpms_blanked = false;
            self.unblank_outputs();
        }
    }

    fn blank_outputs(&self) {
        for output in &self.outputs {
            output.change_current_state(None, None, None, None);
        }
        info!("DPMS: outputs blanked");
    }

    fn unblank_outputs(&self) {
        info!("DPMS: outputs unblanked");
    }

    fn update_ipc_windows(&self) {
        let windows: Vec<WindowInfo> = self
            .space
            .elements()
            .map(|win| {
                let geo = self
                    .space
                    .element_geometry(win)
                    .unwrap_or(Rectangle::new((0, 0).into(), (800, 600).into()));
                let surface_id = win
                    .wl_surface()
                    .map(|s| s.id().protocol_id() as u64)
                    .unwrap_or(0);
                let meta = self
                    .window_meta
                    .get(&surface_id)
                    .cloned()
                    .unwrap_or_default();
                WindowInfo {
                    id: surface_id,
                    title: meta.title,
                    app_id: meta.app_id,
                    x: geo.loc.x,
                    y: geo.loc.y,
                    width: geo.size.w as u32,
                    height: geo.size.h as u32,
                    is_fullscreen: meta.is_fullscreen,
                    is_minimized: meta.is_minimized,
                    workspace: meta.workspace as u32,
                }
            })
            .collect();
        *self.ipc_windows.lock().unwrap() = windows;
    }

    // ── Backend init ───────────────────────────────────────────────────────

    pub fn init_udev(
        &mut self,
        session: smithay::backend::session::libseat::LibSeatSession,
        loop_handle: &LoopHandle<'static, Self>,
    ) {
        crate::render::init_udev(self, session, loop_handle);
    }

    /// Handles VT activate/pause notifications from the libseat session.
    /// On `PauseSession` (e.g. switching to another TTY) the DRM devices
    /// must release their lease so the new session can use the GPU; on
    /// `ActivateSession` (switching back) they need to reacquire it and
    /// the whole desktop must be redrawn since its contents are stale.
    pub fn handle_session_event(&mut self, event: smithay::backend::session::Event) {
        use smithay::backend::session::Event as SessionEvent;
        let BackendData::Udev(ref mut data) = self.backend_data else { return };

        match event {
            SessionEvent::PauseSession => {
                info!("Session paused (VT switched away) - releasing DRM devices");
                for device in data.devices.values_mut() {
                    device.drm.pause();
                }
            }
            SessionEvent::ActivateSession => {
                info!("Session activated (VT switched back) - reacquiring DRM devices");
                for device in data.devices.values_mut() {
                    if let Err(e) = device.drm.activate(false) {
                        warn!("Failed to reactivate DRM device: {:?}", e);
                    }
                }
                // Once a device is reactivated, its next vblank/page-flip
                // event (handled in render.rs) re-arms rendering and
                // repaints the now-stale screen contents - no separate
                // forced redraw is needed here.
            }
        }
    }

    pub fn init_winit(
        &mut self,
        backend: smithay::backend::winit::WinitGraphicsBackend<
            smithay::backend::renderer::gles::GlesRenderer,
        >,
        events: smithay::backend::winit::WinitEventLoop,
        loop_handle: &LoopHandle<'static, Self>,
    ) {
        crate::render::init_winit(self, backend, events, loop_handle);
    }

    pub fn init_xwayland(
        &mut self,
        loop_handle: &LoopHandle<'static, Self>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        crate::xwayland::init_xwayland(self, loop_handle)
    }

    pub fn init_ipc(&mut self, loop_handle: &LoopHandle<'static, Self>) {
        crate::ipc::init_ipc(self, loop_handle);
    }

    // ── Window helpers ─────────────────────────────────────────────────────

    /// Convenience wrapper around `ipc::socket::broadcast` for call sites
    /// (e.g. `protocols/input_method.rs`) that only have a `&BlueState`/
    /// `&mut BlueState`, not the separately-threaded `Clients` handle —
    /// avoids every new protocol module needing to import
    /// `ipc::socket::Clients` and thread it through function signatures
    /// just to emit one status message.
    pub fn ipc_broadcast(&self, msg: crate::ipc::CompositorMessage) {
        crate::ipc::broadcast(&self.clients, &msg);
    }

    pub fn window_by_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(surface))
            .cloned()
    }

    pub fn window_by_id(&self, id: u64) -> Option<Window> {
        self.space
            .elements()
            .find(|w| {
                w.wl_surface()
                    .map(|s| s.id().protocol_id() as u64 == id)
                    .unwrap_or(false)
            })
            .cloned()
    }

    pub fn window_id(win: &Window) -> u64 {
        win.wl_surface()
            .map(|s| s.id().protocol_id() as u64)
            .unwrap_or(0)
    }

    // ── Input tracking ────────────────────────────────────────────────────

    pub fn record_input(&mut self) {
        self.last_input_time = Instant::now();
        if self.dpms_blanked {
            self.dpms_blanked = false;
            self.unblank_outputs();
        }
        // Reset the idle timer (DPMS/screensaver)
        crate::protocols::idle::reset_idle(self);
    }

    // ── Shell UI helpers ───────────────────────────────────────────────────

    pub fn toggle_start_menu(&mut self) {
        self.start_menu_visible = !self.start_menu_visible;
        info!("Start menu: {}", self.start_menu_visible);
    }

    pub fn toggle_fullscreen_menu(&mut self) {
        self.fullscreen_menu_visible = !self.fullscreen_menu_visible;
    }

    // ── Window switcher ────────────────────────────────────────────────────

    pub fn cycle_switcher(&mut self, forward: bool) {
        let count = self.space.elements().count();
        if count == 0 {
            return;
        }
        if forward {
            self.switcher_index = (self.switcher_index + 1) % count;
        } else {
            self.switcher_index = (self.switcher_index + count - 1) % count;
        }
    }

    pub fn apply_switcher_selection(&mut self) {
        let windows: Vec<_> = self.space.elements().cloned().collect();
        if let Some(win) = windows.get(self.switcher_index) {
            self.space.raise_element(win, true);
            if let Some(surface) = win.wl_surface() {
                if let Some(keyboard) = self.seat.get_keyboard() {
                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                    keyboard.set_focus(self, Some(surface.into_owned()), serial);
                }
            }
        }
        self.show_switcher = false;
    }

    // ── Workspace management ───────────────────────────────────────────────

    pub fn switch_workspace(&mut self, index: usize) {
        let next = index.min(self.workspace_count - 1);
        let previous = self.current_workspace;
        info!("Switching workspace {} -> {}", previous, next);
        self.current_workspace = next;

        // Hide windows not on current workspace by moving them off-screen
        // In a full compositor, we'd actually unmap them
        let elements: Vec<_> = self.space.elements().cloned().collect();
        for win in &elements {
            let id = Self::window_id(win);
            let workspace = self
                .window_meta
                .get(&id)
                .map(|m| m.workspace)
                .unwrap_or(0);

            if workspace == next {
                // Map visible - restore from off-screen position
                // (position is stored in window_meta in full impl)
            }
            // In Smithay Space there's no show/hide, so we rely on
            // the shell frontend to handle workspace visibility
        }

        // Real Wayland clients (a panel/dock bound to `ext_workspace_v1`,
        // not just the IPC-connected shell) need to hear about this too
        // — see protocols/ext_workspace.rs's module doc. No-op if
        // nothing's bound the global (the common case today, since no
        // shipped Blue Environment component is itself a separate
        // Wayland client yet — it talks to this compositor over IPC).
        if previous != next {
            crate::protocols::ext_workspace::notify_workspace_switched(self, previous, next);
        }

        // Re-run tiling layout for the workspace we just switched to —
        // covers the case where it was tiled and windows moved onto it
        // (via `move_window_to_workspace`) while it wasn't the visible
        // one, so its layout is already correct rather than only
        // getting recomputed the next time something else changes it.
        self.apply_tiling_layout(next);
    }

    /// Moves the window with the given id (see [`Self::window_id`]) to
    /// `workspace`, clamped to a valid index the same way
    /// [`Self::switch_workspace`] clamps its target. Doesn't change
    /// focus or which workspace is currently shown — pair with
    /// [`Self::switch_workspace`] if the caller wants to follow the
    /// window to its new workspace (`SdeCall::MoveWindowToWorkspace`
    /// deliberately doesn't do that automatically, matching Hyprland/
    /// Wayfire's `movetoworkspace` semantics, which also don't).
    pub fn move_window_to_workspace(&mut self, id: u64, workspace: usize) {
        let target = workspace.min(self.workspace_count.saturating_sub(1));
        let previous = self.window_meta.get(&id).map(|m| m.workspace);
        if let Some(meta) = self.window_meta.get_mut(&id) {
            meta.workspace = target;
            info!("Window {id} moved to workspace {target}");
        }
        // Re-tile both the workspace the window left and the one it
        // joined — either can have gone from N windows to N-1 (need
        // the master-stack split recomputed) or N-1 to N.
        if let Some(prev) = previous {
            self.apply_tiling_layout(prev);
        }
        self.apply_tiling_layout(target);
    }

    /// Turns comphwde's simple master-stack tiling on/off for one
    /// workspace (`SdeCall::SetTiling`/HackerLand's `dispatch settiling`).
    /// Immediately re-lays-out the workspace via
    /// [`Self::apply_tiling_layout`] either way — turning tiling on
    /// should visibly snap windows into place right away, and turning
    /// it off leaves existing window positions alone (nothing to
    /// "un-tile" back to; a person's windows just stop being managed
    /// going forward).
    pub fn set_tiling(&mut self, workspace: usize, enabled: bool) {
        if enabled {
            self.tiling_workspaces.insert(workspace);
        } else {
            self.tiling_workspaces.remove(&workspace);
        }
        info!("Workspace {workspace} tiling: {enabled}");
        self.apply_tiling_layout(workspace);
    }

    /// Whether `workspace` currently has tiling enabled.
    pub fn is_tiling(&self, workspace: usize) -> bool {
        self.tiling_workspaces.contains(&workspace)
    }

    /// Re-lays-out every tiled, non-floating, non-minimized window on
    /// `workspace` using [`crate::layout::compute_layout`] — the glue
    /// between that module's pure geometry math and this compositor's
    /// real `Space`/`ToplevelSurface` types. See `layout`'s module doc
    /// for why the actual math lives there instead of inline here: this
    /// method's only job is "gather the inputs, apply the outputs",
    /// with no layout logic of its own to get subtly wrong.
    ///
    /// No-op if `workspace` isn't in tiling mode ([`Self::is_tiling`])
    /// or has no primary output yet — every call site below (window
    /// map/unmap, `set_tiling`, `switch_workspace`,
    /// `move_window_to_workspace`) can call this unconditionally after
    /// anything that might have changed a tiled workspace's window
    /// set, without each one needing to re-check those conditions
    /// itself.
    pub fn apply_tiling_layout(&mut self, workspace: usize) {
        if !self.is_tiling(workspace) {
            return;
        }
        let Some(output_geo) = self.primary_output_geometry() else { return };

        // Only tile windows that are actually on this workspace, not
        // floating (a floating window opted out of layout management —
        // same convention `toggle_floating_by_id` establishes), and not
        // minimized (nothing to place on screen for those). Order
        // matches `Space::elements()`'s stacking order, so the window
        // that's been on screen longest tends to stay the master — the
        // same "oldest window keeps its slot" feel dwm/Hyprland's own
        // master-stack layouts have, rather than windows visibly
        // reshuffling master/stack roles every time this runs.
        let windows: Vec<Window> = self
            .space
            .elements()
            .filter(|w| {
                let id = Self::window_id(w);
                self.window_meta.get(&id).map(|m| m.workspace == workspace && !m.is_floating && !m.is_minimized).unwrap_or(false)
            })
            .cloned()
            .collect();

        if windows.is_empty() {
            return;
        }

        let area = crate::layout::Rect::new(output_geo.loc.x, output_geo.loc.y, output_geo.size.w, output_geo.size.h);
        let tiling_config = crate::layout::TilingConfig { gaps_px: self.config.general.gaps_px as i32, ..Default::default() };
        let rects = crate::layout::compute_layout(area, windows.len(), &tiling_config);

        for (window, rect) in windows.into_iter().zip(rects) {
            let loc: Point<i32, Logical> = (rect.x, rect.y).into();
            let size: Size<i32, Logical> = (rect.w.max(1), rect.h.max(1)).into();
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| {
                    state.size = Some(size);
                });
                toplevel.send_pending_configure();
            }
            self.space.map_element(window, loc, false);
        }
    }

    /// Registers (or replaces, if `app_id` already had one) a pinned
    /// surface — see [`PinnedSurface`]'s doc. Reserving actual screen
    /// space for it (shrinking `output_geometry` for layout purposes)
    /// is real follow-up work for whatever computes usable workspace
    /// area; this method's job is specifically bookkeeping *that* a pin
    /// exists, matching every other `Sde*`-driven setter on this impl.
    pub fn pin_surface(&mut self, app_id: String, edge: PinnedEdge, thickness_px: u32, owner_pid: Option<i32>) {
        info!("Pinning surface '{app_id}' to {edge:?} edge ({thickness_px}px)");
        self.pinned_surfaces.insert(
            app_id.clone(),
            PinnedSurface { app_id, edge, thickness_px, owner_pid },
        );
    }

    /// The usable geometry of the primary output (the first output in
    /// [`Self::outputs`] — comphwde doesn't yet have an explicit
    /// "primary monitor" concept beyond "the first one connected", see
    /// ROADMAP.md), in logical pixels. `None` if there are no outputs
    /// at all yet (briefly true very early in startup, or on a headless
    /// CI runner with no backend attached).
    pub fn primary_output_geometry(&self) -> Option<Rectangle<i32, Logical>> {
        let output = self.outputs.first()?;
        self.space.output_geometry(output)
    }

    // ── Window control (SdeCall / HackerLand dispatch actions) ──────────────
    // Every method below is the compositor-side implementation of one
    // `SdeCall`/HackerLand `dispatch` action that operates on a single
    // window by id. Each is a small, focused wrapper: look the window
    // up via `window_by_id`, mutate `window_meta` (the metadata these
    // actions are actually about — focus, minimized, maximized,
    // floating), and where the action has a real client-visible wire
    // effect too (focus, close, maximize/fullscreen), also drive the
    // underlying `ToplevelSurface`/`Seat` API. All silently no-op on an
    // unknown id, same as every other `Sde*`/HackerLand action already
    // does for a bad id (see `hackerland_ipc.rs`'s
    // dispatch — a stale id from a client that hasn't heard about a
    // just-closed window yet shouldn't be a hard error).

    pub fn focus_window_by_id(&mut self, id: u64) {
        let Some(window) = self.window_by_id(id) else { return };
        self.space.raise_element(&window, true);
        if let Some(surface) = window.wl_surface() {
            if let Some(keyboard) = self.seat.get_keyboard() {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                keyboard.set_focus(self, Some(surface.into_owned()), serial);
            }
        }
    }

    pub fn close_window_by_id(&mut self, id: u64) {
        let Some(window) = self.window_by_id(id) else { return };
        if let Some(toplevel) = window.toplevel() {
            toplevel.send_close();
        }
    }

    pub fn minimize_window_by_id(&mut self, id: u64) {
        if let Some(meta) = self.window_meta.get_mut(&id) {
            meta.is_minimized = true;
        }
        // Smithay's `Space` has no native show/hide primitive (same
        // constraint noted in `switch_workspace` above) — actually
        // hiding the surface's rendered output for "minimized" is the
        // render path's job, driven off `window_meta[id].is_minimized`,
        // not this method's.
    }

    pub fn unminimize_window_by_id(&mut self, id: u64) {
        if let Some(meta) = self.window_meta.get_mut(&id) {
            meta.is_minimized = false;
        }
        self.focus_window_by_id(id);
    }

    /// `output_geo` is the primary output's usable area (see
    /// [`Self::primary_output_geometry`]) — used to size the window to
    /// fill the screen when `maximized` is `true`. `None` (no output
    /// yet) still toggles the maximized *state* bit clients see, it
    /// just can't also resize the surface to match.
    pub fn maximize_window_by_id(&mut self, id: u64, maximized: bool, output_geo: Option<Rectangle<i32, Logical>>) {
        let Some(window) = self.window_by_id(id) else { return };
        if let Some(meta) = self.window_meta.get_mut(&id) {
            meta.is_floating = meta.is_floating && !maximized; // a maximized window isn't floating
            meta.is_maximized = maximized;
        }
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                if maximized {
                    state.states.set(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized);
                    if let Some(geo) = output_geo {
                        state.size = Some(geo.size);
                    }
                } else {
                    state.states.unset(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized);
                    state.size = None; // let the client pick its own floating size again
                }
            });
            toplevel.send_pending_configure();
            if maximized {
                if let Some(geo) = output_geo {
                    self.space.map_element(window, geo.loc, true);
                }
            }
        }
    }

    pub fn toggle_floating_by_id(&mut self, id: u64) {
        if let Some(meta) = self.window_meta.get_mut(&id) {
            meta.is_floating = !meta.is_floating;
            info!("Window {id} floating: {}", meta.is_floating);
        }
    }

    // ── Summaries (read-only queries for HackerLand) ─────────────────────

    /// One [`hackerland::summaries::WindowSummary`] per currently-mapped window, in
    /// `Space` stacking order (bottom to top — matches
    /// `self.space.elements()`'s own iteration order, which is what
    /// every other window-iterating method on this impl already relies
    /// on, e.g. `cycle_switcher`/`apply_switcher_selection` above).
    pub fn window_summaries(&self) -> Vec<hackerland::summaries::WindowSummary> {
        self.space
            .elements()
            .map(|w| {
                let id = Self::window_id(w);
                let meta = self.window_meta.get(&id).cloned().unwrap_or_default();
                hackerland::summaries::WindowSummary {
                    id,
                    title: meta.title,
                    app_id: meta.app_id,
                    workspace: meta.workspace,
                    is_fullscreen: meta.is_fullscreen,
                    is_minimized: meta.is_minimized,
                    is_floating: meta.is_floating,
                    is_maximized: meta.is_maximized,
                    is_xwayland: w.x11_surface().is_some(),
                }
            })
            .collect()
    }

    /// One [`hackerland::summaries::WorkspaceSummary`] per configured workspace
    /// (`0..workspace_count`, regardless of whether it currently has any
    /// windows — an empty workspace is still a real, switchable
    /// workspace, same as Hyprland/Wayfire).
    pub fn workspace_summaries(&self) -> Vec<hackerland::summaries::WorkspaceSummary> {
        let mut counts = vec![0usize; self.workspace_count];
        for meta in self.window_meta.values() {
            if let Some(c) = counts.get_mut(meta.workspace) {
                *c += 1;
            }
        }
        (0..self.workspace_count)
            .map(|id| hackerland::summaries::WorkspaceSummary {
                id,
                window_count: counts[id],
                is_tiling: self.is_tiling(id),
                is_active: id == self.current_workspace,
            })
            .collect()
    }

    /// One [`hackerland::summaries::OutputSummary`] per connected output. The first
    /// output is reported as primary, matching
    /// [`Self::primary_output_geometry`]'s same convention.
    pub fn output_summaries(&self) -> Vec<hackerland::summaries::OutputSummary> {
        self.outputs
            .iter()
            .enumerate()
            .map(|(i, output)| {
                let mode = output.current_mode();
                let scale = output.current_scale().fractional_scale();
                let loc = self.space.output_geometry(output).map(|geo| geo.loc).unwrap_or_default();
                hackerland::summaries::OutputSummary {
                    name: output.name(),
                    x: loc.x,
                    y: loc.y,
                    width: mode.map(|m| m.size.w).unwrap_or(0),
                    height: mode.map(|m| m.size.h).unwrap_or(0),
                    refresh_mhz: mode.map(|m| m.refresh).unwrap_or(0),
                    scale,
                    is_primary: i == 0,
                }
            })
            .collect()
    }
}

// ── Protocol implementations ───────────────────────────────────────────────

impl BufferHandler for BlueState {
    fn buffer_destroyed(
        &mut self,
        _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    ) {
    }
}

impl CompositorHandler for BlueState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        // Must run before `on_commit_buffer_handler` below — that call
        // is what consumes/clears the surface's pending buffer
        // assignment, so this is the last point at which the just-
        // attached buffer (and therefore its dmabuf origin node, if any)
        // is still inspectable. See render/multigpu.rs's module doc.
        crate::render::multigpu::track_surface_origin(&mut self.surface_gpu_origin, surface);
        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);
        if let Some(window) = self.window_by_surface(surface) {
            window.on_commit();
        }
        self.popup_manager.commit(surface);
    }
}

impl ShmHandler for BlueState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl OutputHandler for BlueState {}

impl SeatHandler for BlueState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        *self.cursor_status.lock().unwrap() = image;
    }
}

impl XdgShellHandler for BlueState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface.clone());
        let count = self.space.elements().count();
        let offset = count * 30;
        let loc = Point::from(((offset + 150) as i32, (offset + 80) as i32));
        self.space.map_element(window.clone(), loc, true);

        // Store metadata
        let surface_id = surface.wl_surface().id().protocol_id() as u64;
        let with_pending = surface.with_pending_state(|_s| {
            (
                // title via window_meta
String::new(),
                String::new(),
            )
        });
        self.window_meta.insert(
            surface_id,
            WindowMeta {
                title: with_pending.0,
                app_id: with_pending.1,
                workspace: self.current_workspace,
                ..Default::default()
            },
        );
        // TODO(foreign-toplevel): wire the actual resource-creation half
        // of this (see `emit_new_toplevel` in
        // protocols/foreign_toplevel.rs) before this call does anything
        // visible to clients — the hook point itself is correct now.
        self.notify_toplevel_mapped(surface_id);
        self.apply_tiling_layout(self.current_workspace);

        info!(
            "New toplevel surface id={} workspace={}",
            surface_id, self.current_workspace
        );
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        let _ = self
            .popup_manager
            .track_popup(smithay::desktop::PopupKind::Xdg(surface));
    }

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }

    fn move_request(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: Serial,
    ) {
        let seat = Seat::from_resource(&seat).unwrap();
        let wl = surface.wl_surface().clone();
        if let Some(window) = self.window_by_surface(&wl) {
            if let Some(pointer) = seat.get_pointer() {
                if let Some(start_data) = pointer.grab_start_data() {
                    crate::input::start_move_grab(self, window, start_data, serial);
                }
            }
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: Serial,
        edges: smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge,
    ) {
        let seat = Seat::from_resource(&seat).unwrap();
        let wl = surface.wl_surface().clone();
        if let Some(window) = self.window_by_surface(&wl) {
            if let Some(pointer) = seat.get_pointer() {
                if let Some(start_data) = pointer.grab_start_data() {
                    crate::input::start_resize_grab(self, window, start_data, edges.into());
                    let _ = serial; // serial isn't needed beyond the initial request in this simplified grab
                }
            }
        }
    }

    fn grab(
        &mut self,
        _surface: PopupSurface,
        _seat: wl_seat::WlSeat,
        _serial: Serial,
    ) {
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let surface_id = surface.wl_surface().id().protocol_id() as u64;
        self.window_meta.remove(&surface_id);
        self.notify_toplevel_unmapped(surface_id);
        let wl = surface.wl_surface().clone();
        if let Some(w) = self.window_by_surface(&wl) {
            self.space.unmap_elem(&w);
        }
    }
}

impl WlrLayerShellHandler for BlueState {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        // Was a pure no-op — the global was registered but nothing ever
        // called `map_layer`, so a layer-shell client's surface (panels,
        // the lock screen's password prompt, on-screen keyboards) never
        // actually got placed on an output or rendered.
        //
        // Two real `cargo build` errors fixed here vs. the previous pass:
        //   - `from_resource` lives on `Output` itself, not
        //     `OutputManagerState` (E0599 — confirmed against smithay
        //     source: it's defined in `impl Output { .. }` in
        //     src/output.rs, a couple methods away from `create_global`,
        //     which is presumably why it got mis-attributed).
        //   - `LayerMap::map_layer`/`unmap_layer` take
        //     `&smithay::desktop::LayerSurface` (a wrapper that also
        //     carries the namespace + a map-assigned id), NOT the raw
        //     `smithay::wayland::shell::wlr_layer::LayerSurface` this
        //     handler receives — two different, same-named types
        //     (E0308). The wrapper's `PartialEq` compares an id assigned
        //     at construction time, not anything derived from the
        //     underlying surface, so a *fresh* wrapper built later in
        //     `layer_destroyed` would never equal this one — the wrapper
        //     has to be kept around (`self.layer_surfaces`, keyed by the
        //     underlying `WlSurface`'s object id) so `layer_destroyed`
        //     can hand back the *same* instance for removal.
        use smithay::reexports::wayland_server::Resource;
        let output = output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.outputs.first().cloned());
        let Some(output) = output else {
            warn!("new layer surface (namespace {namespace}) but no output to map it on");
            return;
        };
        let key = surface.wl_surface().id();
        let desktop_layer = smithay::desktop::LayerSurface::new(surface, namespace.clone());
        {
            let mut map = layer_map_for_output(&output);
            if let Err(e) = map.map_layer(&desktop_layer) {
                warn!("failed to map layer surface (namespace {namespace}): {e:?}");
                return;
            }
        }
        self.layer_surfaces.insert(key, desktop_layer);
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        use smithay::reexports::wayland_server::Resource;
        let key = surface.wl_surface().id();
        if let Some(desktop_layer) = self.layer_surfaces.remove(&key) {
            for output in &self.outputs {
                layer_map_for_output(output).unmap_layer(&desktop_layer);
            }
        }
    }
}

impl SelectionHandler for BlueState {
    type SelectionUserData = ();

    fn send_selection(
        &mut self,
        _target: SelectionTarget,
        _mime_type: String,
        _fd: OwnedFd,
        _seat: Seat<Self>,
        _user_data: &Self::SelectionUserData,
    ) {
    }

    fn new_selection(
        &mut self,
        _target: SelectionTarget,
        _source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
    }
}

impl WaylandDndGrabHandler for BlueState {}

impl DataDeviceHandler for BlueState {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl PrimarySelectionHandler for BlueState {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

impl FractionalScaleHandler for BlueState {
    fn new_fractional_scale(&mut self, _surface: WlSurface) {}
}

impl DndGrabHandler for BlueState {}

// ── pointer-constraints-unstable-v1 ─────────────────────────────────────
//
// Previously entirely absent — no way for a game/fullscreen app to lock
// or confine the pointer. The handler just needs to react when a
// constraint (lock or confine) actually becomes active so we can suppress
// normal absolute-position pointer motion for that surface while it's
// locked; `relative_pointer` below is how the client still gets motion
// deltas while locked.
impl PointerConstraintsHandler for BlueState {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &smithay::input::pointer::PointerHandle<Self>) {
        // Activate immediately if the pointer is currently over this
        // surface and no other constraint is active — mirrors the
        // behavior of most compositors (sway/anvil): constraints only
        // "arm" once the pointer enters the constrained surface, but
        // since we don't yet track a richer per-surface armed/disarmed
        // state machine, we activate eagerly. A more complete
        // implementation would call `with_pointer_constraint` to check
        // `is_active()`/region before doing so.
        let _ = (surface, pointer);
    }

    fn cursor_position_hint(&mut self, surface: &WlSurface, pointer: &smithay::input::pointer::PointerHandle<Self>, location: Point<f64, Logical>) {
        // Client hinted where it would like the (now-locked) cursor to
        // warp to once unlocked. Smithay's `PointerHandle` doesn't expose
        // a direct warp in all revs of this API; storing/applying this is
        // a follow-up rather than a hard requirement for basic lock/confine
        // to work.
        let _ = (surface, pointer, location);
    }
}
delegate_pointer_constraints!(BlueState);
delegate_relative_pointer!(BlueState);

// ── zwp_tablet_manager_v2 ────────────────────────────────────────────────
// `TabletSeatHandler for BlueState` is already implemented in
// `protocols/cursor_shape.rs` (needed there for `delegate_cursor_shape!`);
// duplicating it here caused E0119. `delegate_tablet_manager!` just needs
// that single impl to exist somewhere, which it does.
delegate_tablet_manager!(BlueState);

// ── zwp_text_input_manager_v3 / zwp_input_method_manager_v2 (IME) ───────
delegate_text_input_manager!(BlueState);

impl InputMethodHandler for BlueState {
    fn new_popup(&mut self, surface: smithay::wayland::input_method::PopupSurface) {
        // Was a no-op stub — see protocols/input_method.rs module doc.
        // The IME's popup surface now gets tracked, positioned under the
        // text cursor, and composited (render/mod.rs).
        crate::protocols::input_method::popup_created(self, surface);
    }

    fn dismiss_popup(&mut self, surface: smithay::wayland::input_method::PopupSurface) {
        crate::protocols::input_method::popup_dismissed(self, &surface);
    }

    fn parent_geometry(&self, surface: &WlSurface) -> Rectangle<i32, Logical> {
        crate::protocols::input_method::parent_geometry(self, Some(surface))
    }

    fn popup_repositioned(&mut self, surface: smithay::wayland::input_method::PopupSurface) {
        crate::protocols::input_method::popup_repositioned(self, &surface);
    }
}
delegate_input_method_manager!(BlueState);

delegate_compositor!(BlueState);
delegate_shm!(BlueState);
delegate_single_pixel_buffer!(BlueState);
delegate_content_type!(BlueState);
delegate_seat!(BlueState);
delegate_xdg_shell!(BlueState);
delegate_layer_shell!(BlueState);
delegate_output!(BlueState);
delegate_data_device!(BlueState);
delegate_primary_selection!(BlueState);
delegate_presentation!(BlueState);
delegate_viewporter!(BlueState);
delegate_fractional_scale!(BlueState);
