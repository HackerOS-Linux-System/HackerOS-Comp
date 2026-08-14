use std::collections::HashMap;
use std::os::unix::io::OwnedFd;
use std::path::Path;

use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::compositor::{DrmCompositor, FrameFlags};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmNode};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{Color32F, ImportMemWl};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{primary_gpu, UdevBackend, UdevEvent};
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::{EventLoop, LoopHandle};
use smithay::reexports::drm::control::{connector, crtc, Device as ControlDevice, ModeTypeFlags, ResourceHandles};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::Display;
use smithay::utils::DeviceFd;
use smithay::wayland::compositor::CompositorState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::shell::wlr_layer::WlrLayerShellState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::socket::ListeningSocketSource;
use smithay::desktop::{PopupManager, Space, Window};
use smithay::input::pointer::CursorImageStatus;
use smithay::input::SeatState;
use smithay::utils::Clock;

use crate::state::{ClientState, HwdeState};

/// A KMS output this backend has lit up: the `Output` HWDE's own window
/// management/render_elements code already knows how to treat like any
/// other output, plus the per-CRTC `DrmCompositor` that actually issues
/// atomic commits and manages the GBM buffer/damage-tracking lifecycle for
/// it. One of these exists per *connected* connector, not per connector
/// slot - an unplugged monitor has no `Surface`.
struct Surface {
    output: Output,
    compositor: DrmCompositor<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>,
}

/// Per-GPU state: the opened device, its GBM allocator, a `GlesRenderer`
/// bound to it via EGL, and one [`Surface`] per lit-up connector.
///
/// Lives on `HwdeState::drm_gpus` (see that field's doc comment for why),
/// keyed by [`DrmNode`] so a hotplugged second GPU (external eGPU/dock) is
/// just another entry rather than needing a distinct code path.
pub struct GpuState {
    node: DrmNode,
    #[allow(dead_code)] // kept alive by the DrmDevice/GbmDevice that borrow its lifetime via Clone
    fd: DrmDeviceFd,
    drm: DrmDevice,
    gbm: GbmDevice<DrmDeviceFd>,
    renderer: GlesRenderer,
    surfaces: HashMap<crtc::Handle, Surface>,
}

pub const OUTPUT_MAKE: &str = "HWDE";

/// Starts a full DRM/udev session. Returns once `state.running` goes false
/// (VT switch away + back is handled internally via `SessionEvent`, not by
/// returning) or an unrecoverable setup error occurs.
pub fn run_udev(wallpaper_path: std::path::PathBuf, extern_mode: Option<crate::ExternMode>) -> anyhow::Result<()> {
    let mut event_loop: EventLoop<HwdeState> = EventLoop::try_new()?;
    let display: Display<HwdeState> = Display::new()?;
    let display_handle = display.handle();
    let dh = display_handle.clone();

    // --- session -----------------------------------------------------------
    let (mut session, notifier) = LibSeatSession::new()
        .map_err(|e| anyhow::anyhow!("failed to open a libseat session (are you in the `seat` group / is seatd running?): {e}"))?;
    tracing::info!("opened libseat session on seat '{}'", session.seat());

    // --- primary GPU ---------------------------------------------------------
    let primary_gpu_path = if let Ok(path) = std::env::var("HWDE_DRM_DEVICE") {
        std::path::PathBuf::from(path)
    } else {
        match primary_gpu(session.seat())? {
            Some(path) => path,
            None => first_available_drm_node()?.ok_or_else(|| {
                anyhow::anyhow!(
                    "no primary GPU found on seat '{}', and no /dev/dri/cardN node exists either \
                     (are you inside a container/VM without a DRM device passed through?)",
                    session.seat()
                )
            })?,
        }
    };
    let primary_gpu_node = DrmNode::from_path(&primary_gpu_path)
        .map_err(|e| anyhow::anyhow!("{} is not a DRM node: {e}", primary_gpu_path.display()))?;
    tracing::info!("using {} as the primary GPU", primary_gpu_node);

    // --- udev hotplug monitoring ----------------------------------------------
    let udev_backend = UdevBackend::new(session.seat())
        .map_err(|e| anyhow::anyhow!("failed to initialize udev backend: {e}"))?;

    // --- real input via libinput ------------------------------------------------
    let mut libinput_context =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput_context
        .udev_assign_seat(&session.seat())
        .map_err(|_| anyhow::anyhow!("failed to assign udev seat to libinput"))?;
    let libinput_backend = LibinputInputBackend::new(libinput_context.clone());

    // --- Wayland socket / display dispatch (identical to winit_backend.rs) ----
    let socket_source = ListeningSocketSource::new_auto()?;
    let socket_name = socket_source.socket_name().to_string_lossy().into_owned();
    event_loop
        .handle()
        .insert_source(socket_source, |client_stream, _, state: &mut HwdeState| {
            if let Err(err) = state
                .display_handle
                .insert_client(client_stream, std::sync::Arc::new(ClientState::default()))
            {
                tracing::warn!("failed to add wayland client: {err}");
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to init wayland socket source: {e}"))?;
    tracing::info!("comphwde (drm) listening on Wayland socket {socket_name}");

    event_loop
        .handle()
        .insert_source(
            smithay::reexports::calloop::generic::Generic::new(
                display,
                smithay::reexports::calloop::Interest::READ,
                smithay::reexports::calloop::Mode::Level,
            ),
            |_, display, state: &mut HwdeState| {
                // Safety: `display` is kept alive for the lifetime of the event loop.
                unsafe { display.get_mut().dispatch_clients(state)? };
                Ok(smithay::reexports::calloop::PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to init wayland display source: {e}"))?;

    // --- Wayland protocol state (identical set to winit_backend.rs) -----------
    let compositor_state = CompositorState::new::<HwdeState>(&dh);
    let xdg_shell_state = XdgShellState::new::<HwdeState>(&dh);
    let xdg_decoration_state = XdgDecorationState::new::<HwdeState>(&dh);
    let layer_shell_state = WlrLayerShellState::new::<HwdeState>(&dh);
    let data_device_state = DataDeviceState::new::<HwdeState>(&dh);
    let primary_selection_state = PrimarySelectionState::new::<HwdeState>(&dh);
    let shm_state = ShmState::new::<HwdeState>(&dh, vec![]);
    let output_manager_state = OutputManagerState::new_with_xdg_output::<HwdeState>(&dh);

    // Loaded here (rather than only inline in the `HwdeState { ... }`
    // literal further down, where it used to be loaded) since
    // `seat.add_keyboard` just below needs `xkb_layout`/etc. from it
    // before `HwdeState` itself exists yet - same reasoning as
    // `winit_backend.rs`'s identical move for `output_transform`/
    // `output_scale`.
    let config = crate::config::load_for(extern_mode.as_ref().map(|m| m.name.as_str()));

    let mut seat_state = SeatState::<HwdeState>::new();
    let mut seat = seat_state.new_wl_seat(&dh, "hwde-seat".to_string());
    let pointer = seat.add_pointer();
    seat.add_keyboard(config.xkb_config(), 200, 25)?;
    seat.add_touch();

    // See winit_backend.rs's identical block: only `Some` for
    // `--extern-sde` (see `HwdeState::foreign_toplevels`'s doc comment).
    let foreign_toplevels = match &extern_mode {
        Some(mode) if mode.name == "sde" => Some(crate::foreign_toplevel::ForeignToplevelManagerState::new(&dh)),
        _ => None,
    };

    let mut state = HwdeState {
        display_handle: dh.clone(),
        handle: event_loop.handle(),
        running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        start_time: std::time::Instant::now(),
        clock: Clock::new(),
        space: Space::default(),
        popups: PopupManager::default(),
        windows: Vec::new(),
        next_window_id: 1,
        compositor_state,
        xdg_shell_state,
        xdg_decoration_state,
        layer_shell_state,
        data_device_state,
        primary_selection_state,
        shm_state,
        output_manager_state,
        seat_state,
        seat,
        pointer,
        cursor_status: CursorImageStatus::default_named(),
        dnd_icon: None,
        wallpaper: crate::wallpaper::Wallpaper::new(wallpaper_path),
        pending_wallpaper_reload: false,
        socket_name: Some(socket_name),
        config,
        active_workspace: 0,
        focused_window: None,
        tiling_enabled: std::collections::HashSet::new(),
        floating_windows: std::collections::HashSet::new(),
        pinned_surfaces: std::collections::HashMap::new(),
        title_cache: std::cell::RefCell::new(crate::title_text::TitleTextureCache::default()),
        gesture_swipe: None,
        idle_inhibit_manager_state: smithay::wayland::idle_inhibit::IdleInhibitManagerState::new::<HwdeState>(&dh),
        single_pixel_buffer_state: smithay::wayland::single_pixel_buffer::SinglePixelBufferState::new::<HwdeState>(&dh),
        relative_pointer_manager_state: smithay::wayland::relative_pointer::RelativePointerManagerState::new::<HwdeState>(&dh),
        tablet_manager_state: smithay::wayland::tablet_manager::TabletManagerState::new::<HwdeState>(&dh),
        virtual_keyboard_manager_state: smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState::new::<HwdeState, _>(&dh, |_client| true),
        idle_inhibiting_surfaces: std::collections::HashSet::new(),
        last_input_activity: std::time::Instant::now(),
        idle_dimmed: false,
        extern_name: extern_mode.as_ref().map(|m| m.name.clone()),
        foreign_toplevels,
        #[cfg(feature = "xwayland")]
        xwm: None,
        #[cfg(feature = "xwayland")]
        xdisplay: None,
        #[cfg(feature = "xwayland")]
        xwayland_shell_state: smithay::wayland::xwayland_shell::XWaylandShellState::new::<HwdeState>(&dh),
        drm_gpus: HashMap::new(),
    };

    // --- light up the primary GPU (and whatever's plugged into it) now, before
    //     entering the event loop, so there's something on screen from frame one --
    if let Err(err) = device_added(&mut session, primary_gpu_node, &primary_gpu_path, &mut state) {
        anyhow::bail!("failed to initialize primary GPU {}: {err}", primary_gpu_path.display());
    }

    // Any other GPU udev already knows about at startup (rare - eGPU/dock
    // already plugged in before comphwde launched) - anything hotplugged
    // *after* this point comes through the UdevEvent::Added arm below.
    for (dev_id, path) in udev_backend.device_list() {
        if let Ok(node) = DrmNode::from_dev_id(dev_id) {
            if node == primary_gpu_node {
                continue;
            }
            if let Err(err) = device_added(&mut session, node, path, &mut state) {
                tracing::warn!("failed to initialize secondary GPU {}: {err} (continuing without it)", path.display());
            }
        }
    }

    match &extern_mode {
        // See winit_backend.rs's identical branch for why HackerLand gets
        // routed separately from the generic --extern-<n>/sde-ipc path.
        Some(mode) if mode.name == "hackerland" => crate::hackerland_ipc::init(&event_loop.handle())?,
        // See winit_backend.rs's identical branch: SDE's window
        // management moves to wlr-foreign-toplevel-management, with
        // sde-ipc kept running alongside it for everything that protocol
        // has no equivalent for.
        Some(mode) if mode.name == "sde" => {
            crate::sde_toplevel_ipc::init(&event_loop.handle())?;
            crate::extern_ipc::init(&event_loop.handle(), mode.name.clone())?;
        }
        Some(mode) => crate::extern_ipc::init(&event_loop.handle(), mode.name.clone())?,
        None => crate::ipc::init(&event_loop.handle())?,
    }

    #[cfg(feature = "xwayland")]
    if let Err(err) = crate::xwayland::start(&mut state) {
        tracing::warn!("failed to start XWayland: {err} (X11 apps will not work this session)");
    }

    // --- hotplug / input / session event sources -------------------------------
    event_loop
        .handle()
        .insert_source(udev_backend, move |event, _, state: &mut HwdeState| match event {
            UdevEvent::Added { device_id, path } => {
                tracing::info!("udev: DRM device added: {path:?} ({device_id})");
                match DrmNode::from_dev_id(device_id) {
                    Ok(node) if !state.drm_gpus.contains_key(&node) => {
                        if let Err(err) = device_added(&mut session, node, &path, state) {
                            tracing::error!("failed to initialize hotplugged GPU {}: {err:?}", path.display());
                        }
                    }
                    Ok(_) => { /* already tracked (e.g. the primary GPU we opened above) */ }
                    Err(err) => tracing::warn!("udev added event for {path:?} isn't a DRM node: {err:?}"),
                }
            }
            UdevEvent::Changed { device_id } => {
                tracing::info!("udev: DRM device changed: {device_id}");
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    device_changed(node, state);
                }
            }
            UdevEvent::Removed { device_id } => {
                tracing::info!("udev: DRM device removed: {device_id}");
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    device_removed(node, state);
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert udev event source: {e}"))?;

    event_loop
        .handle()
        .insert_source(libinput_backend, move |event, _, state: &mut HwdeState| {
            let Some(output) = state.space.outputs().next().cloned() else {
                // No lit-up output yet (e.g. every connector on the primary
                // GPU came up disconnected) - nothing sensible to route
                // pointer/keyboard events to yet.
                return;
            };
            // NOTE: routes every input device to the first mapped output
            // regardless of which physical GPU/connector it's actually
            // associated with. Fine for the overwhelmingly common
            // single-output case; a real multi-monitor setup would need to
            // resolve this per libinput device the way
            // `anvil::udev::AnvilState::process_input_event` does via
            // `UdevOutputId` - not implemented in this pass.
            state.process_input_event(event, &output);
        })
        .map_err(|e| anyhow::anyhow!("failed to insert libinput event source: {e}"))?;

    event_loop
        .handle()
        .insert_source(notifier, move |event, &mut (), state: &mut HwdeState| match event {
            SessionEvent::PauseSession => {
                tracing::info!("session paused (VT switched away) - suspending input/rendering");
                libinput_context.suspend();
                for gpu in state.drm_gpus.values_mut() {
                    for surface in gpu.surfaces.values_mut() {
                        if let Err(err) = surface.compositor.reset_state() {
                            tracing::warn!("failed to reset drm surface state during pause/resume: {err:?}");
                        }
                    }
                }
            }
            SessionEvent::ActivateSession => {
                tracing::info!("session resumed (VT switched back)");
                if let Err(err) = libinput_context.resume() {
                    tracing::error!("failed to resume libinput: {err:?}");
                }
                let mut to_render = Vec::new();
                for (&node, gpu) in state.drm_gpus.iter_mut() {
                    for (&crtc, surface) in gpu.surfaces.iter_mut() {
                        if let Err(err) = surface.compositor.reset_state() {
                            tracing::warn!("failed to reset drm surface state during pause/resume: {err:?}");
                        }
                        surface.compositor.reset_buffers();
                        to_render.push((node, crtc));
                    }
                }
                for (node, crtc) in to_render {
                    render_surface(state, node, crtc);
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert session notifier: {e}"))?;

    tracing::info!("HWDE compositor (comphwde, DRM/udev backend) ready - entering main loop");

    while state.running.load(std::sync::atomic::Ordering::SeqCst) {
        if state.pending_wallpaper_reload {
            if let Some(gpu) = state.drm_gpus.values_mut().next() {
                state.wallpaper.load(&mut gpu.renderer);
            }
            state.pending_wallpaper_reload = false;
        }

        event_loop.dispatch(Some(std::time::Duration::from_millis(16)), &mut state)?;
        state.display_handle.flush_clients()?;
    }

    Ok(())
}

/// Opens the DRM device at `path`, binds a `GlesRenderer` to it via EGL/GBM,
/// lights up every already-connected connector on it (each gets its own
/// `Output` + `DrmCompositor`), and inserts the resulting [`GpuState`] into
/// `state.drm_gpus`.
///
/// Adapted from `anvil::udev::AnvilState::device_added` against Smithay
/// 0.7.0's actual API (verified by reading that crate version's source
/// directly, not from memory - `DrmDevice::new` in this version no longer
/// takes an `allow_atomic` bool; it tries atomic internally via
/// `set_client_capability` and falls back to legacy on its own, with
/// `SMITHAY_USE_LEGACY=1` available to force it - so the "retry with
/// allow_atomic=false" fallback this function's previous doc comment
/// described no longer applies and isn't needed here).
///
/// Not yet run against real hardware in this environment (no `/dev/dri` in
/// this sandbox) - every call in here is checked against Smithay/`drm`-crate
/// 0.7.0/0.14.1 source, but a compile-run-fix loop on an actual machine is
/// still the way to catch anything subtly off in the GPU-buffer-lifetime
/// specifics (allocator flags, format negotiation) that don't show up as
/// type errors.
fn device_added(session: &mut LibSeatSession, node: DrmNode, path: &Path, state: &mut HwdeState) -> anyhow::Result<()> {
    let owned_fd: OwnedFd = session
        .open(path, OFlags::RDWR | OFlags::CLOEXEC)
        .map_err(|e| anyhow::anyhow!("failed to open {}: {e:?}", path.display()))?;
    let fd = DrmDeviceFd::new(DeviceFd::from(owned_fd));

    let (drm, drm_notifier) =
        DrmDevice::new(fd.clone(), false).map_err(|e| anyhow::anyhow!("failed to initialize DRM device {}: {e}", path.display()))?;

    let gbm = GbmDevice::new(fd.clone()).map_err(|e| anyhow::anyhow!("failed to create GBM device for {}: {e}", path.display()))?;

    // Safety: `gbm` outlives the `EGLDisplay` (it's cloned into `GpuState`
    // below and dropped no earlier), which is what `EGLDisplay::new`
    // requires of its `EGLNativeDisplay` argument.
    let egl_display =
        unsafe { EGLDisplay::new(gbm.clone()) }.map_err(|e| anyhow::anyhow!("failed to create EGL display for {}: {e}", path.display()))?;
    let egl_context = EGLContext::new(&egl_display).map_err(|e| anyhow::anyhow!("failed to create EGL context for {}: {e}", path.display()))?;
    // Safety: `egl_context` is not used again after this - ownership moves
    // into the renderer, matching every other Smithay backend's use of this
    // constructor (see `winit`'s own internals, which do the same).
    let mut renderer =
        unsafe { GlesRenderer::new(egl_context) }.map_err(|e| anyhow::anyhow!("failed to create GlesRenderer for {}: {e}", path.display()))?;

    state.shm_state.update_formats(renderer.shm_formats());

    // First GPU to come up owns the wallpaper texture - matches
    // winit_backend.rs's single-renderer assumption. A second GPU (rare -
    // eGPU/dock) skips this rather than re-decoding the image into a
    // texture cache on a renderer that likely isn't even driving the
    // wallpaper's output.
    if state.drm_gpus.is_empty() {
        state.wallpaper.load(&mut renderer);
    }

    let mut gpu = GpuState {
        node,
        fd,
        drm,
        gbm,
        renderer,
        surfaces: HashMap::new(),
    };

    // Register this device's page-flip/vblank notifier now, before we start
    // queuing any frames on it. `DrmDeviceNotifier` implements calloop's
    // `EventSource` directly (see `backend::drm::device::mod`'s impl), so
    // it goes straight into the loop - no wrapper type needed.
    let handle: LoopHandle<'static, HwdeState> = state.handle.clone();
    handle
        .insert_source(drm_notifier, move |event, _, state: &mut HwdeState| match event {
            DrmEvent::VBlank(crtc) => {
                if let Some(gpu) = state.drm_gpus.get_mut(&node) {
                    if let Some(surface) = gpu.surfaces.get_mut(&crtc) {
                        if let Err(err) = surface.compositor.frame_submitted() {
                            tracing::error!("frame_submitted failed for {node}/{crtc:?}: {err:?}");
                        }
                    }
                }
                render_surface(state, node, crtc);
            }
            DrmEvent::Error(err) => {
                tracing::error!("DRM device {node} reported an error: {err:?}");
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to register DRM device notifier for {}: {e}", path.display()))?;

    let res = gpu
        .drm
        .resource_handles()
        .map_err(|e| anyhow::anyhow!("failed to get resource handles for {}: {e}", path.display()))?;

    let mut newly_lit: Vec<crtc::Handle> = Vec::new();
    for &conn_handle in res.connectors() {
        let info = match gpu.drm.get_connector(conn_handle, true) {
            Ok(info) => info,
            Err(err) => {
                tracing::warn!("failed to probe connector {conn_handle:?} on {node}: {err:?}");
                continue;
            }
        };
        if info.state() != connector::State::Connected || info.modes().is_empty() {
            continue;
        }
        match connector_connected(&mut gpu, &res, &info, &state.display_handle, &mut state.space, &state.config) {
            Ok(crtc) => {
                tracing::info!("{node}: lit up {} ({:?}) on {crtc:?}", info.interface().as_str(), info.interface_id());
                newly_lit.push(crtc);
            }
            Err(err) => tracing::error!("{node}: failed to light up {} connector: {err:?}", info.interface().as_str()),
        }
    }

    state.drm_gpus.insert(node, gpu);
    // Nothing will vblank until a first frame has actually been queued -
    // kick that off now instead of waiting on an event that will never
    // arrive for a freshly-lit connector.
    for crtc in newly_lit {
        render_surface(state, node, crtc);
    }

    Ok(())
}

/// Re-scans a device's connectors after a `UdevEvent::Changed` (monitor
/// plugged/unplugged on an already-open GPU).
///
/// Deliberately not an incremental diff: it tears down every surface this
/// device currently has and re-enumerates from scratch, the same way
/// `device_added` builds the initial set. That briefly blanks every output
/// on this GPU before relighting whatever's still connected, which is a
/// cheap price for a monitor hotplug (not a latency-sensitive path) in
/// exchange for a much simpler, easier-to-verify-by-inspection borrow
/// structure than incrementally diffing two connector sets against live
/// surfaces would need.
fn device_changed(node: DrmNode, state: &mut HwdeState) {
    let Some(gpu) = state.drm_gpus.get_mut(&node) else { return };
    for surface in std::mem::take(&mut gpu.surfaces).into_values() {
        state.space.unmap_output(&surface.output);
    }

    let Some(gpu) = state.drm_gpus.get(&node) else { return };
    let res = match gpu.drm.resource_handles() {
        Ok(res) => res,
        Err(err) => {
            tracing::warn!("{node}: failed to re-scan resource handles: {err:?}");
            return;
        }
    };
    let connector_handles: Vec<connector::Handle> = res.connectors().to_vec();

    let mut newly_lit = Vec::new();
    for conn_handle in connector_handles {
        let Some(gpu) = state.drm_gpus.get_mut(&node) else { break };
        let info = match gpu.drm.get_connector(conn_handle, true) {
            Ok(info) => info,
            Err(err) => {
                tracing::warn!("{node}: failed to probe connector {conn_handle:?}: {err:?}");
                continue;
            }
        };
        if info.state() != connector::State::Connected || info.modes().is_empty() {
            continue;
        }
        match connector_connected(gpu, &res, &info, &state.display_handle, &mut state.space, &state.config) {
            // (`gpu` here is borrowed from `state.drm_gpus` a few lines
            // up, `&state.config` from a different field of the same
            // `state` - disjoint field borrows, expected to be fine, but
            // this specific pairing is new as of adding `config` to this
            // call - the one spot to look at first if this particular
            // line doesn't compile.)
            Ok(crtc) => {
                tracing::info!("{node}: lit up {} ({:?}) on {crtc:?}", info.interface().as_str(), info.interface_id());
                newly_lit.push(crtc);
            }
            Err(err) => tracing::error!("{node}: failed to light up {} connector: {err:?}", info.interface().as_str()),
        }
    }
    for crtc in newly_lit {
        render_surface(state, node, crtc);
    }
}

/// Tears down every surface/output for a GPU that udev reports as fully
/// removed (as opposed to `device_changed`, which is just a connector
/// hotplug on a GPU that's still there).
fn device_removed(node: DrmNode, state: &mut HwdeState) {
    if let Some(gpu) = state.drm_gpus.remove(&node) {
        let count = gpu.surfaces.len();
        for surface in gpu.surfaces.into_values() {
            state.space.unmap_output(&surface.output);
        }
        tracing::info!("{node}: device removed, {count} output(s) torn down");
    }
}

/// Best-effort helper for identifying whether a connector is currently
/// backing a live surface - looks up the CRTC currently bound to a
/// connector via its active encoder, if any. Not used by `device_changed`
/// above (which just does a full rescan instead), but kept for anything
/// that wants a cheaper "is this specific connector already lit" check
/// without a full teardown/relight cycle.
#[allow(dead_code)]
fn current_encoder_crtc(drm: &DrmDevice, info: &connector::Info) -> Option<crtc::Handle> {
    let enc = info.current_encoder()?;
    drm.get_encoder(enc).ok()?.crtc()
}

/// Picks a mode + free CRTC for `info` and builds the `Output` +
/// `DrmCompositor` pair for it, inserting the result into `gpu.surfaces`.
/// Returns the CRTC it landed on (so the caller can trigger that surface's
/// first render once it's actually reachable via `state.drm_gpus`).
fn connector_connected(
    gpu: &mut GpuState,
    res: &ResourceHandles,
    info: &connector::Info,
    display_handle: &smithay::reexports::wayland_server::DisplayHandle,
    space: &mut Space<Window>,
    config: &crate::config::CompositorConfig,
) -> anyhow::Result<crtc::Handle> {
    let crtc = find_crtc_for_connector(&gpu.drm, res, info, &gpu.surfaces)
        .ok_or_else(|| anyhow::anyhow!("no free CRTC for connector {}", info.interface().as_str()))?;

    let drm_mode = *info
        .modes()
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .unwrap_or(&info.modes()[0]);

    let surface = gpu
        .drm
        .create_surface(crtc, drm_mode, &[info.handle()])
        .map_err(|e| anyhow::anyhow!("failed to create DRM surface: {e}"))?;

    let output_name = format!("{}-{}", info.interface().as_str(), info.interface_id());
    let (phys_w, phys_h) = info.size().unwrap_or((0, 0));
    let output = Output::new(
        output_name.clone(),
        PhysicalProperties {
            size: (phys_w as i32, phys_h as i32).into(),
            subpixel: Subpixel::Unknown,
            make: OUTPUT_MAKE.into(),
            model: format!("{:?}", gpu.node),
        },
    );
    let mode: OutputMode = drm_mode.into();
    output.create_global::<HwdeState>(display_handle);
    output.change_current_state(
        Some(mode),
        Some(crate::config::parse_transform(&config.output_transform)),
        Some(smithay::output::Scale::Fractional(config.output_scale)),
        None,
    );
    output.set_preferred(mode);

    // Position: a configured override for this specific connector name
    // wins (see `CompositorConfig::outputs`/`OutputPlacement`); otherwise
    // fall back to the existing automatic behavior - placed side by side
    // with whatever's already mapped, left to right, simplest reasonable
    // default for a first multi-monitor pass.
    let position = match config.outputs.get(&output_name) {
        Some(placement) => (placement.x, placement.y),
        None => {
            let x_offset: i32 = space.outputs().filter_map(|o| o.current_mode()).map(|m| m.size.w).sum();
            (x_offset, 0)
        }
    };
    space.map_output(&output, position);

    let allocator = GbmAllocator::new(gpu.gbm.clone(), GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
    let exporter = GbmFramebufferExporter::new(gpu.gbm.clone(), Some(gpu.node));
    let color_formats = [Fourcc::Argb8888, Fourcc::Xrgb8888];
    let renderer_formats = gpu.renderer.egl_context().dmabuf_render_formats().clone();

    let compositor = DrmCompositor::new(
        &output,
        surface,
        None,
        allocator,
        exporter,
        color_formats,
        renderer_formats,
        gpu.drm.cursor_size(),
        Some(gpu.gbm.clone()),
    )
    .map_err(|e| anyhow::anyhow!("failed to build DrmCompositor: {e:?}"))?;

    gpu.surfaces.insert(crtc, Surface { output, compositor });
    Ok(crtc)
}

/// Finds a CRTC for `info` that isn't already claimed by another surface on
/// the same device: prefers whatever CRTC its currently-active encoder (if
/// any) already reports, then falls back to scanning every encoder listed
/// for the connector and intersecting each one's `possible_crtcs()` against
/// the device's full CRTC list, skipping anything already in `used`.
fn find_crtc_for_connector(
    drm: &DrmDevice,
    res: &ResourceHandles,
    info: &connector::Info,
    used: &HashMap<crtc::Handle, Surface>,
) -> Option<crtc::Handle> {
    if let Some(enc) = info.current_encoder() {
        if let Ok(enc_info) = drm.get_encoder(enc) {
            if let Some(crtc) = enc_info.crtc() {
                if !used.contains_key(&crtc) {
                    return Some(crtc);
                }
            }
        }
    }

    for &enc_handle in info.encoders() {
        let Ok(enc_info) = drm.get_encoder(enc_handle) else { continue };
        for crtc in res.filter_crtcs(enc_info.possible_crtcs()) {
            if !used.contains_key(&crtc) {
                return Some(crtc);
            }
        }
    }

    None
}

/// Renders and (if there's actually new damage) queues one frame for
/// `(node, crtc)`. Called both right after a surface is first created and
/// from every subsequent `DrmEvent::VBlank` for that CRTC - this backend
/// always redraws on vblank rather than doing damage-driven scheduling,
/// same simple "just redraw" policy `winit_backend.rs` uses every loop
/// iteration.
///
/// Reaches into `state.drm_gpus` via disjoint field-destructuring (see
/// `render_elements::RenderInputs`'s doc comment) instead of going through
/// `RenderInputs::from(&state)`, since the renderer it needs to pass to
/// `build_output_elements` is itself a field of `state`.
fn render_surface(state: &mut HwdeState, node: DrmNode, crtc: crtc::Handle) {
    // Recomputed here (before the destructure below borrows pieces of
    // `state` individually) rather than inline in the `RenderInputs`
    // literal, since `is_idle_inhibited()` needs `&HwdeState` as a whole -
    // see `HwdeState::idle_dimmed`'s doc comment for why this is the only
    // place (alongside `winit_backend.rs`'s equivalent per-frame line)
    // that ever writes it.
    let idle_dimmed = state.config.idle_dim_timeout_secs > 0
        && !state.is_idle_inhibited()
        && state.last_input_activity.elapsed().as_secs() >= state.config.idle_dim_timeout_secs as u64;
    state.idle_dimmed = idle_dimmed;

    // Same "compute before the destructure below" reasoning as
    // `idle_dimmed` above - `cursor_status`/`pointer` aren't among the
    // fields borrowed out below, so reading them from `state` as a whole
    // has to happen first.
    let cursor_visible = !matches!(state.cursor_status, smithay::input::pointer::CursorImageStatus::Hidden);
    let cursor_location = state.pointer.current_location();

    let HwdeState {
        ref windows,
        ref space,
        ref config,
        ref focused_window,
        ref wallpaper,
        ref title_cache,
        ref mut drm_gpus,
        ..
    } = *state;

    let Some(gpu) = drm_gpus.get_mut(&node) else { return };
    let Some(surface) = gpu.surfaces.get_mut(&crtc) else { return };

    let output_size = surface
        .output
        .current_mode()
        .map(|m| (m.size.w, m.size.h))
        .unwrap_or((0, 0));

    let inputs = crate::render_elements::RenderInputs {
        windows,
        space,
        config,
        focused_window: *focused_window,
        wallpaper,
        title_cache,
        idle_dimmed,
        cursor_visible,
        cursor_location,
    };
    let elements = crate::render_elements::build_output_elements(inputs, &mut gpu.renderer, &surface.output, output_size);

    match surface
        .compositor
        .render_frame(&mut gpu.renderer, &elements, Color32F::new(0.05, 0.05, 0.08, 1.0), FrameFlags::DEFAULT)
    {
        Ok(render_frame_result) => {
            if !render_frame_result.is_empty {
                if let Err(err) = surface.compositor.queue_frame(()) {
                    tracing::error!("{node}/{crtc:?}: failed to queue frame: {err:?}");
                }
            }
        }
        Err(err) => tracing::error!("{node}/{crtc:?}: render_frame failed: {err:?}"),
    }
}

/// Fallback for when [`primary_gpu`] can't identify a card via PCI hints -
/// which happens routinely on VMs (`virtio-gpu`, `vboxvideo`, `qxl` and
/// similar paravirtualized devices typically don't expose the PCI class
/// info `primary_gpu` keys off of) even though a perfectly usable
/// `/dev/dri/cardN` exists. Just takes the lowest-numbered card node that
/// parses as a valid primary (non-render) [`DrmNode`].
fn first_available_drm_node() -> anyhow::Result<Option<std::path::PathBuf>> {
    let mut candidates: Vec<std::path::PathBuf> = std::fs::read_dir("/dev/dri")
        .map_err(|e| anyhow::anyhow!("failed to read /dev/dri: {e}"))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.starts_with("card") && name["card".len()..].chars().all(|c| c.is_ascii_digit()))
        })
        .collect();
    // /dev/dri/card0 before card1 before card10, not lexicographic order.
    candidates.sort_by_key(|path| {
        path.file_name()
            .and_then(|n| n.to_str())
            .and_then(|name| name["card".len()..].parse::<u32>().ok())
            .unwrap_or(u32::MAX)
    });

    for candidate in candidates {
        if DrmNode::from_path(&candidate).is_ok() {
            tracing::info!("primary_gpu() found nothing via PCI hints; falling back to {}", candidate.display());
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}
