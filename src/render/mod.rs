use smithay::{
    backend::{
        allocator::{
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Fourcc,
        },
        drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, GbmBufferedSurface},
        egl::{EGLContext, EGLDevice, EGLDisplay},
        renderer::{
            damage::OutputDamageTracker,
            element::{
                surface::WaylandSurfaceRenderElement,
            },
            gles::GlesRenderer,
            multigpu::{gbm::GbmGlesBackend, GpuManager},
            pixman::PixmanRenderer,
            Bind, ImportDma,
        },
        session::{Session, libseat::LibSeatSession},
        udev::{all_gpus, primary_gpu, UdevBackend, UdevEvent},
        winit::{WinitGraphicsBackend, WinitEventLoop, WinitEvent},
    },
    output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::{LoopHandle, timer::{Timer, TimeoutAction}},
        rustix::fs::OFlags,
        drm::control::{
            connector, crtc, Device as DrmControlDevice, ModeTypeFlags,
            PageFlipFlags,
        },
        // `.size()`/`.pitch()` on the raw `drm::control::dumbbuffer::
        // DumbBuffer` (used in `create_dumb_swapchain`/
        // `render_udev_pixman`) are trait methods on `drm::buffer::
        // Buffer`, not inherent methods — a real compiler error (not a
        // guess) confirmed this: "private field, not a method... trait
        // `Buffer` which provides `size` is implemented but not in
        // scope". `drm::buffer::Buffer` is reexported through smithay
        // the same way `drm::control::*` already is, so this is the
        // same reexport path, not a new direct `drm` dependency (see
        // Cargo.toml's own note on why a *direct* `drm` dependency
        // caused real version-conflict bugs earlier).
        drm::buffer::Buffer as DrmBufferTrait,
    },
    utils::{DeviceFd, Point, Size, Transform},
};
use std::{collections::HashMap, os::unix::io::OwnedFd, time::Duration};
use tracing::{error, info, warn};

use crate::state::{
    BackendData, BlueState, DumbSwapchain, DumbSwapchainBuffer, GpuDevice, OutputRenderSurface,
    RenderBackend, SurfaceBackend, UdevData, WinitData,
};

/// Multi-GPU render-node import (hybrid-graphics laptops) — see module
/// doc for what's implemented (GpuManager lifecycle + the cross-copy
/// primitive) vs. what's still an open follow-up (per-surface origin
/// tracking to actually call it from the render loop).
pub mod multigpu;

/// HDR tone-mapping shader (compiled, not yet wired into the composite
/// pass — see module doc for exactly why and what's left).
pub mod hdr_shader;

// ── Winit (nested/dev mode) ───────────────────────────────────────────────

pub fn init_winit(
    state: &mut BlueState,
    mut backend: WinitGraphicsBackend<GlesRenderer>,
    events: WinitEventLoop,
    loop_handle: &LoopHandle<'static, BlueState>,
) {
    let size = backend.window_size();
    let output = Output::new("winit".to_string(), PhysicalProperties {
        size: Size::from((0, 0)),
        subpixel: Subpixel::Unknown,
        make: "Blue".to_string(),
        model: "Winit".to_string(),
        serial_number: String::new(),
    });
    let mode = OutputMode {
        size: Size::from((size.w as i32, size.h as i32)),
        refresh: 60_000,
    };
    output.change_current_state(Some(mode), Some(Transform::Normal), Some(Scale::Integer(1)), Some(Point::from((0, 0))));
    output.set_preferred(mode);
    state.space.map_output(&output, Point::from((0, 0)));
    let damage_tracker = OutputDamageTracker::from_output(&output);
    state.outputs.push(output.clone());

    // Register the client-facing dmabuf + color-management globals now
    // that a real GlesRenderer exists (`WinitGraphicsBackend::renderer()`
    // — a plain accessor, doesn't consume/move `backend`). Doing this
    // under winit (not just udev) is what makes both testable without
    // real DRM hardware — e.g. `build.rb check`'s headless smoke test.
    {
        let display_handle = state.display_handle.clone();
        let (dmabuf_state, dmabuf_global) =
            crate::protocols::dmabuf::init_dmabuf(&display_handle, backend.renderer(), None);
        state.dmabuf_state = Some(dmabuf_state);
        state.dmabuf_global = dmabuf_global;
        state.color_management_state = crate::protocols::color_management::init_color_management(&display_handle);
    }
    let hdr_tonemap_shader = match hdr_shader::compile_hdr_tonemap_shader(backend.renderer()) {
        Ok(program) => Some(program),
        Err(e) => {
            warn!("HDR tone-mapping shader failed to compile (not fatal — HDR content just won't be tone-mapped): {e:?}");
            None
        }
    };

    state.backend_data = BackendData::Winit(Box::new(WinitData { backend, output, damage_tracker, hdr_tonemap_shader }));
    state.seat.add_keyboard(smithay::input::keyboard::XkbConfig::default(), 400, 30).expect("keyboard");
    let _ = state.seat.add_pointer();

    loop_handle.insert_source(events, |event, _, state| {
        match event {
            WinitEvent::Resized { size, .. } => {
                if let BackendData::Winit(ref mut d) = state.backend_data {
                    let m = OutputMode { size: Size::from((size.w as i32, size.h as i32)), refresh: 60_000 };
                    d.output.change_current_state(Some(m), None, None, None);
                    d.damage_tracker = OutputDamageTracker::from_output(&d.output);
                }
            }
            WinitEvent::Input(ev) => crate::input::handle_input(state, ev),
            WinitEvent::CloseRequested => { state.should_exit = true; }
            WinitEvent::Redraw => {
                if let BackendData::Winit(ref d) = state.backend_data {
                    let output = d.output.clone();
                    drop(d);
                    render_winit(state, &output);
                }
            }
            WinitEvent::Focus(_) => {}
        }
    }).expect("winit source");
}

pub fn render_winit(state: &mut BlueState, output: &Output) {
    // Phase 1: collect render elements (borrow ends before render)
    let elements = {
        let BackendData::Winit(ref mut d) = state.backend_data else { return };
        let (renderer, _) = match d.backend.bind() {
            Ok(r) => r,
            Err(e) => { error!("bind: {}", e); return; }
        };
        // SpaceRenderElements wraps WaylandSurfaceRenderElement - use the correct type
        use smithay::desktop::space::SpaceRenderElements;
        // Was hardcoded to 1.0 — the fractional-scale protocol is
        // registered (protocols/mod.rs) and clients negotiate a scale
        // through it, but the renderer never actually applied it, so
        // HiDPI outputs always rendered at 1x regardless of what was
        // advertised. Pull the output's real (possibly fractional) scale
        // instead.
        let output_scale = output.current_scale().fractional_scale() as f32;
        let mut elems: Vec<SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>> =
            state.space.render_elements_for_output(renderer, output, output_scale)
                .unwrap_or_default();
        // IME candidate-window popups aren't part of `state.space` (they
        // aren't xdg-shell windows), so `render_elements_for_output`
        // never picks them up — this is the composite half of the fix in
        // protocols/input_method.rs (positioning was the other half).
        elems.extend(crate::protocols::input_method::render_elements(
            &state.input_method_popups, renderer, output_scale as f64,
        ));
        elems.extend(layer_shell_elements(output, renderer, output_scale as f64));
        elems
    };

    // Phase 2: render with fresh borrow
    let BackendData::Winit(ref mut d) = state.backend_data else { return };
    let (renderer, mut frame) = match d.backend.bind() {
        Ok(r) => r,
        Err(e) => { error!("bind2: {}", e); return; }
    };
    if let Err(e) = d.damage_tracker.render_output(renderer, &mut frame, 0, &elements, [0.08_f32, 0.10, 0.15, 1.0]) {
        warn!("render_output: {:?}", e);
    }
    drop(frame);
    if let Err(e) = d.backend.submit(None) { warn!("submit: {:?}", e); }
    d.backend.window().request_redraw();
}

// ── Udev/DRM (TTY mode) ────────────────────────────────────────────────────

/// Rebuilds the GPU inventory from `BackendData::Udev`'s current
/// `devices` map and broadcasts it as `CompositorMessage::GpuList` over
/// IPC. Called once at udev-backend startup and again on every
/// `UdevEvent::Added`/`Removed` — previously it was only ever sent once
/// at startup, so a shell Settings/Displays panel that was open across a
/// GPU hotplug (e.g. an eGPU or a USB-C dock's DP-alt-mode GPU being
/// plugged/unplugged) kept showing a stale snapshot until the next
/// compositor restart. A no-op (does nothing, logs nothing) if the
/// backend isn't udev — safe to call unconditionally from either event
/// branch.
fn broadcast_gpu_list(state: &BlueState) {
    if let BackendData::Udev(ref udev) = state.backend_data {
        let gpus = udev.devices.iter().map(|(node, gpu)| {
            crate::ipc::GpuInfo {
                node: format!("{node:?}"),
                primary: *node == udev.primary_gpu,
                output_count: gpu.surfaces.len() as u32,
            }
        }).collect();
        state.ipc_broadcast(crate::ipc::CompositorMessage::GpuList { gpus });
    }
}

pub fn init_udev(
    state: &mut BlueState,
    mut session: LibSeatSession,
    loop_handle: &LoopHandle<'static, BlueState>,
) {
    // Session trait must be in scope for .seat()
    let seat_name = session.seat();
    info!("udev backend, seat: {}", seat_name);

    // primary_gpu returns PathBuf - convert to DrmNode
    let primary_path = primary_gpu(&seat_name)
        .ok().flatten()
        .or_else(|| all_gpus(&seat_name).ok().and_then(|v| v.into_iter().next()))
        .expect("No GPU found");
    let primary_node = DrmNode::from_path(&primary_path).expect("DrmNode");
    info!("Primary GPU: {:?}", primary_node);

    let udev_backend = UdevBackend::new(&seat_name).expect("udev backend");
    let mut devices: HashMap<DrmNode, GpuDevice> = HashMap::new();
    // See render/multigpu.rs's module doc — this is registered with
    // every GPU node below (and on hotplug `UdevEvent::Added`/
    // `Removed`), independent of whether a second GPU is actually
    // present, so the infra is always correct/ready rather than only
    // exercised on hybrid-graphics hardware.
    let mut gpu_manager = GpuManager::new(GbmGlesBackend::default())
        .expect("GpuManager::new is infallible for GbmGlesBackend (no devices enumerated yet)");

    if let Ok((gpu, notifier)) = open_gpu(&primary_node, &mut session) {
        if let Err(e) = gpu_manager.as_mut().add_node(primary_node, gpu.gbm.clone()) {
            warn!("multi-gpu: failed to register primary GPU {primary_node:?} with GpuManager: {e:?}");
        }
        devices.insert(primary_node, gpu);
        register_drm_notifier(loop_handle, primary_node, notifier);
    }

    state.backend_data = BackendData::Udev(Box::new(UdevData {
        session, primary_gpu: primary_node, devices, gpu_manager,
    }));
    // scan_drm_outputs needs backend_data populated first (it looks the
    // GpuDevice up by node to create the renderer/surfaces), so this runs
    // after the assignment above rather than before it like the old code.
    if let BackendData::Udev(ref mut udev) = state.backend_data {
        if let Some(gpu) = udev.devices.get_mut(&primary_node) {
            let drm_clone_ptr: *mut DrmDevice = &mut gpu.drm;
            // SAFETY: `create_surface` needs `&mut DrmDevice`, but we
            // also need `&mut BlueState` for `scan_drm_outputs` itself
            // (to map outputs into `state.space`, create the renderer on
            // `state.backend_data`, etc) — both ultimately reach through
            // `state.backend_data`, so the borrow checker can't see that
            // `&mut gpu.drm` and `&mut state` don't actually alias in a
            // way that matters here (`scan_drm_outputs` never mutates
            // `udev.devices` itself while it holds the `drm` reference,
            // only outputs/renderer/surfaces on the *same* `gpu` entry
            // through a fresh `get_mut` lookup, which the pointer here
            // doesn't hold onto — see `scan_drm_outputs`'s own body for
            // where it re-borrows `gpu`). No other code runs between
            // this point and the end of `scan_drm_outputs`.
            unsafe { scan_drm_outputs(state, &mut *drm_clone_ptr, primary_node, loop_handle); }
        }
    }
    state.notify_output_topology_changed();

    // Real GPU inventory, not a guess — see CompositorMessage::GpuList's
    // doc comment. Sent once here at startup and re-sent on every
    // hotplug Added/Removed event below (`broadcast_gpu_list`) so a
    // shell Settings panel open across a hotplug actually reflects it,
    // rather than only ever showing the boot-time snapshot.
    broadcast_gpu_list(state);

    state.seat.add_keyboard(smithay::input::keyboard::XkbConfig::default(), 400, 30).expect("kb");
    let _ = state.seat.add_pointer();

    loop_handle.insert_source(udev_backend, |event, _, state| {
        match event {
            UdevEvent::Added { path, .. } => {
                if let Ok(node) = DrmNode::from_path(&path) {
                    let lh = state.loop_handle.clone();
                    if let BackendData::Udev(ref mut data) = state.backend_data {
                        let mut sess = data.session.clone();
                        if let Ok((gpu, notifier)) = open_gpu(&node, &mut sess) {
                            if let Err(e) = data.gpu_manager.as_mut().add_node(node, gpu.gbm.clone()) {
                                warn!("multi-gpu: failed to register hotplugged GPU {node:?} with GpuManager: {e:?}");
                            }
                            data.devices.insert(node, gpu);
                            // Previously dropped for hotplugged GPUs (only
                            // the primary GPU's notifier, opened before
                            // the event loop even started, was
                            // registered) — a secondary GPU plugged in
                            // after boot would render one frame and then
                            // stall since nothing ever called
                            // `frame_submitted()` for it.
                            register_drm_notifier(&lh, node, notifier);
                        }
                    }
                    // Scan for outputs on the newly-added GPU the same
                    // way the primary GPU is scanned at startup.
                    if let BackendData::Udev(ref mut udev) = state.backend_data {
                        if let Some(gpu) = udev.devices.get_mut(&node) {
                            let drm_ptr: *mut DrmDevice = &mut gpu.drm;
                            // SAFETY: see the identical pattern (and
                            // rationale) in `init_udev` above.
                            unsafe { scan_drm_outputs(state, &mut *drm_ptr, node, &lh); }
                        }
                    }
                    state.notify_output_topology_changed();
                    // A hotplugged GPU changes both the inventory
                    // (`gpu_manager`/`devices` above) and, via
                    // `scan_drm_outputs`, potentially the output count
                    // per GPU — re-send so a Settings panel already open
                    // when this happens actually updates instead of
                    // showing a stale boot-time snapshot (see
                    // `CompositorMessage::GpuList`'s doc comment, updated
                    // alongside this fix).
                    broadcast_gpu_list(state);
                }
            }
            UdevEvent::Changed { .. } => {}
            UdevEvent::Removed { device_id } => {
                // Previously a no-op: unplugging a monitor (or, via
                // udev's DRM device_id, an entire GPU) left its
                // GbmBufferedSurface/DrmSurface alive with nothing ever
                // rendering to it again, and the `Output` stayed mapped
                // in `state.space` forever, so windows could keep being
                // placed on a monitor that no longer exists.
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    let mut removed_outputs: Vec<Output> = Vec::new();
                    if let BackendData::Udev(ref mut udev) = state.backend_data {
                        if let Some(gpu) = udev.devices.get_mut(&node) {
                            removed_outputs = gpu.surfaces.values().map(|s| s.output.clone()).collect();
                            gpu.surfaces.clear();
                        }
                        udev.devices.remove(&node);
                        // Keep GpuManager in sync — an internal texture
                        // cache entry pointing at a now-closed device fd
                        // would otherwise be a use-after-free risk the
                        // next time something tried a cross-GPU import
                        // involving this node (see render/multigpu.rs).
                        udev.gpu_manager.as_mut().remove_node(&node);
                    }
                    for output in removed_outputs {
                        state.space.unmap_output(&output);
                        state.outputs.retain(|o| o != &output);
                    }
                    state.notify_output_topology_changed();
                    info!("Removed DRM device {:?} and its outputs", node);
                    // Same reasoning as the `Added` branch: the GPU
                    // inventory a Settings panel is showing just changed
                    // (a whole device disappeared, or at minimum its
                    // output_count dropped to 0), so it needs to hear
                    // about it now, not just at next compositor restart.
                    broadcast_gpu_list(state);
                }
            }
        }
    }).expect("udev source");

    // Render timer — previously this only called `state.refresh()` (space
    // bookkeeping, no drawing). It now also drives `render_udev()` for
    // every lit-up output so bare-metal/TTY mode actually displays
    // something.
    loop_handle.insert_source(
        Timer::from_duration(Duration::from_millis(16)),
        |_, _, state| {
            state.refresh();
            render_all_udev_outputs(state);
            TimeoutAction::ToDuration(Duration::from_millis(16))
        },
    ).expect("render timer");
}

/// Registers a GPU's `DrmDeviceNotifier` on the event loop so
/// `DrmEvent::VBlank` (a previous pageflip completed) and
/// `DrmEvent::Error` reach either `GbmBufferedSurface::frame_submitted()`
/// (GLES path) or the dumb-swapchain's own front/back swap bookkeeping
/// (Pixman path — see `SurfaceBackend::Dumb`). Required for either
/// swapchain to keep cycling buffers past the first frame — see the
/// comment on `open_gpu`'s return type.
fn register_drm_notifier(
    loop_handle: &LoopHandle<'static, BlueState>,
    node: DrmNode,
    notifier: smithay::backend::drm::DrmDeviceNotifier,
) {
    if let Err(e) = loop_handle.insert_source(notifier, move |event, metadata, state| {
        match event {
            DrmEvent::VBlank(crtc) => {
                if let BackendData::Udev(ref mut udev) = state.backend_data {
                    if let Some(gpu) = udev.devices.get_mut(&node) {
                        if let Some(surface) = gpu.surfaces.get_mut(&crtc) {
                            match &mut surface.surface {
                                SurfaceBackend::Gbm(gbm_surface) => {
                                    if let Err(e) = gbm_surface.frame_submitted() {
                                        warn!("frame_submitted failed for {:?}: {}", crtc, e);
                                    }
                                }
                                // The dumb-buffer swapchain has no
                                // equivalent "tell it the flip
                                // completed" call to make — its
                                // front/back index is advanced
                                // synchronously in `render_udev_pixman`
                                // right after a successful `page_flip`
                                // ioctl, not asynchronously here on
                                // `VBlank`. This arm still exists (rather
                                // than matching just `Gbm` and ignoring
                                // `Dumb`) so a future person adding
                                // per-flip bookkeeping to the Pixman path
                                // has an obvious place to put it, and so
                                // this `match` stays exhaustive as
                                // `SurfaceBackend` grows.
                                SurfaceBackend::Dumb(_) => {}
                            }
                        }
                    }
                }
                let _ = metadata;
            }
            DrmEvent::Error(e) => warn!("DRM device error on {:?}: {}", node, e),
        }
    }) {
        error!("Failed to register DRM notifier for {:?}: {:?}", node, e);
    }
}

/// Return type note: previously this dropped the `DrmDeviceNotifier` that
/// `DrmDevice::new` hands back (`let (drm, _notifier) = ...`). That
/// notifier is what delivers `DrmEvent::VBlank`/`DrmEvent::Error` once
/// registered on the event loop — without it, `GbmBufferedSurface`'s
/// internal swapchain never gets told a buffer was released by the
/// previous pageflip and will stall (or error out) after the first frame.
/// It's now returned alongside the device so `init_udev`/hotplug handling
/// can register it.
fn open_gpu(
    node: &DrmNode,
    session: &mut LibSeatSession,
) -> Result<(GpuDevice, smithay::backend::drm::DrmDeviceNotifier), Box<dyn std::error::Error>> {
    // Session trait in scope via import above
    let path = node.dev_path().ok_or("no dev path")?;
    let owned_fd: OwnedFd = session.open(&path, OFlags::empty())
        .map_err(|e| format!("session.open: {}", e))?;
    let drm_fd = DrmDeviceFd::new(DeviceFd::from(owned_fd));
    // DrmDevice::new returns (DrmDevice, DrmDeviceNotifier)
    let (drm, notifier) = DrmDevice::new(drm_fd.clone(), true)
        .map_err(|e| format!("DrmDevice: {}", e))?;
    let gbm = GbmDevice::new(drm_fd)
        .map_err(|e| format!("GbmDevice: {}", e))?;
    Ok((GpuDevice { drm, gbm, renderer: None, hdr_tonemap_shader: None, surfaces: HashMap::new() }, notifier))
}

/// Detects connected outputs on a DRM device AND (unlike the previous
/// version of this function) actually lights each one up: it allocates a
/// `DrmSurface` for a free CRTC, wraps it in a `GbmBufferedSurface` swap
/// chain, and stashes the pair in `GpuDevice::surfaces` so `render_udev()`
/// has something to draw into. It also lazily creates the shared EGL/GLES
/// renderer for the GPU the first time it's needed.
///
/// ## Caveats (please read before relying on this in production)
/// This is a best-effort, from-scratch implementation written without a
/// working Rust toolchain available to compile/test against the exact
/// pinned `smithay` commit (`82912edf`) — there was no network access to a
/// build environment capable of pulling and building the full smithay +
/// libdrm/gbm/EGL dependency tree in this session. The overall structure
/// (EGLDisplay → EGLContext → GlesRenderer, DrmSurface → GbmAllocator →
/// GbmBufferedSurface, DRM device fd as a calloop event source consuming
/// `DrmEvent::VBlank`/`DrmEvent::Error`) matches the architecture used by
/// smithay's own reference compositor (`anvil/src/udev.rs`), but exact
/// method names/signatures can drift between smithay revisions. Treat this
/// as a strong scaffold to compile-fix against the pinned rev, not as
/// verified-working code — the previous state (outputs detected but never
/// rendered to) is unambiguously worse, so this is a net improvement
/// either way, but plan to `cargo build` and iterate before shipping it.
///
/// Single-CRTC-per-connector, no explicit plane management beyond what
/// `GbmBufferedSurface`/`DrmSurface` do internally. Two gaps this comment
/// used to list here have since been addressed elsewhere: hotplug-aware
/// surface teardown (`UdevEvent::Removed` in `init_udev`) turned out to
/// already be implemented when actually checked, and multi-GPU
/// render-node import lifecycle now exists (`render/multigpu.rs`) — what
/// remains for the latter is per-surface origin routing, not the
/// infrastructure itself; see ROADMAP.md for both.
fn scan_drm_outputs(
    state: &mut BlueState,
    drm: &mut DrmDevice,
    node: DrmNode,
    _loop_handle: &LoopHandle<'static, BlueState>,
) {
    let Ok(resources) = drm.resource_handles() else { return };

    // Lazily create the shared renderer for this GPU — GLES if a usable
    // EGL driver stack exists, Pixman (software) otherwise. See
    // `create_renderer_for_gpu`'s doc for exactly which real-world case
    // the fallback is for.
    let mut is_software = false;
    let renderer_ready = if let BackendData::Udev(ref mut udev) = state.backend_data {
        if let Some(gpu) = udev.devices.get_mut(&node) {
            if gpu.renderer.is_none() {
                match create_renderer_for_gpu(&gpu.gbm, node) {
                    Ok(RenderBackend::Gles(mut r)) => {
                        gpu.hdr_tonemap_shader = match hdr_shader::compile_hdr_tonemap_shader(&mut r) {
                            Ok(program) => Some(program),
                            Err(e) => {
                                warn!("HDR tone-mapping shader failed to compile for {:?} (not fatal — HDR content just won't be tone-mapped): {e:?}", node);
                                None
                            }
                        };
                        gpu.renderer = Some(RenderBackend::Gles(r));
                        true
                    }
                    Ok(RenderBackend::Pixman(r)) => {
                        // No HDR tone-mapping on the software path —
                        // `hdr_shader.rs`'s wrapper is GLES-specific
                        // (see this file's module doc); Pixman-rendered
                        // outputs simply don't tone-map HDR content,
                        // same as before this fallback existed (they
                        // rendered nothing at all).
                        is_software = true;
                        gpu.renderer = Some(RenderBackend::Pixman(r));
                        true
                    }
                    Err(e) => { error!("Failed to create any renderer (GLES or Pixman) for {:?}: {}", node, e); false }
                }
            } else {
                is_software = gpu.renderer.as_ref().map(RenderBackend::is_software).unwrap_or(false);
                true
            }
        } else {
            false
        }
    } else {
        false
    };
    if !renderer_ready {
        warn!("No renderer available for GPU {:?}, outputs on it will not render", node);
    } else if state.dmabuf_state.is_none() && !is_software {
        // dmabuf/color-management are both GLES/EGL-specific client
        // protocols (dmabuf import needs an EGL context to bind
        // against; see `protocols/dmabuf.rs`) — skip advertising them
        // for a software-only GPU rather than registering globals that
        // would immediately fail every import a client attempted
        // through them. A client on a Pixman-backed output still gets
        // normal shm-buffer rendering, just not zero-copy dmabuf.
        // First GPU with a working renderer: register the client-facing
        // dmabuf + color-management globals (see protocols/dmabuf.rs,
        // protocols/color_management.rs). Only done once — multi-GPU
        // per-device feedback (advertising a different `main_device`
        // tranche per render node) is the multi-GPU follow-up already
        // flagged above, not attempted here.
        let display_handle = state.display_handle.clone();
        // `DrmNode::dev_id()` returns `u64` directly, not a `Result` —
        // confirmed by a real `cargo build` error (`E0599: no method
        // named 'ok' found for type 'u64'`) once this was actually
        // compiled; `libc::dev_t` is a plain alias for `u64` on Linux,
        // so no cast is needed either.
        let dev_id = Some(node.dev_id());

        let init_result = if let BackendData::Udev(ref udev) = state.backend_data {
            udev.devices.get(&node)
                .and_then(|gpu| gpu.renderer.as_ref())
                // `dmabuf::init_dmabuf` takes a `&GlesRenderer` — only
                // reachable when this GPU actually landed on the GLES
                // arm, which the `!is_software` check above already
                // guarantees for every path that reaches here, but
                // `and_then` on `as_gles()` keeps this block correct
                // even if that guard is ever loosened later.
                .and_then(RenderBackend::as_gles)
                .map(|renderer| crate::protocols::dmabuf::init_dmabuf(&display_handle, renderer, dev_id))
        } else {
            None
        };
        // Borrow of `state.backend_data` (via `udev`/`gpu`/`renderer`)
        // ends at the close of the `if let` above, so assigning back into
        // `state` here is a fresh, disjoint borrow — same reasoning as
        // `dmabuf::init_dmabuf`'s doc comment.
        if let Some((dmabuf_state, dmabuf_global)) = init_result {
            state.dmabuf_state = Some(dmabuf_state);
            state.dmabuf_global = dmabuf_global;
            state.color_management_state = crate::protocols::color_management::init_color_management(&display_handle);
        }
    }

    let mut used_crtcs: Vec<crtc::Handle> = Vec::new();
    // Starting offset for laying out this GPU's connectors left-to-right:
    // previously this was hardcoded to 0 on every call, so a second GPU
    // hotplugged later (or this same device rescanned) placed its
    // outputs starting back at x=0, overlapping whatever was already
    // mapped there instead of extending the desktop to the right of it.
    // Anchoring to the current rightmost edge across *all* already-known
    // outputs (not just this GPU's) makes both the multi-GPU case and a
    // same-device rescan lay out correctly. Falls back to 0 when there
    // are no outputs yet (first scan, nothing to be right of).
    let mut x_off: i32 = state
        .outputs
        .iter()
        .filter_map(|o| {
            let loc = o.current_location();
            let mode = o.current_mode()?;
            // Physical width in logical/layout coordinates — divide the
            // mode's pixel width by the output's scale, matching how
            // `space.map_output` below places things (this compositor
            // only ever uses integer scale, see `Scale::Integer` calls
            // in this file, so this division is exact).
            let scale = o.current_scale().integer_scale().max(1);
            Some(loc.x + mode.size.w / scale)
        })
        .max()
        .unwrap_or(0);

    for conn_handle in resources.connectors() {
        let Ok(conn) = drm.get_connector(*conn_handle, false) else { continue };
        if conn.state() != connector::State::Connected { continue; }
        let mode = conn.modes().iter()
            .filter(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .max_by_key(|m| m.vrefresh())
            .or_else(|| conn.modes().first())
            .copied();
        let Some(mode) = mode else { continue };
        let (w, h) = mode.size();
        let conn_id = u32::from(*conn_handle);
        let name = format!("{}-{}", conn.interface() as u8, conn_id);
        let phys = conn.size().unwrap_or((0, 0));

        // Pick the first CRTC that can drive this connector and isn't
        // already claimed by an earlier connector in this same scan.
        // `Encoder::possible_crtcs()` returns a `CrtcListFilter` bitmask
        // directly usable by `ResourceHandles::filter_crtcs` — collecting
        // per-encoder results into a `Vec` first (the previous version of
        // this code) doesn't typecheck, since `filter_crtcs` wants that
        // bitmask type, not a pre-filtered list of handles.
        let possible_crtcs: Vec<crtc::Handle> = conn
            .encoders()
            .iter()
            .filter_map(|e| drm.get_encoder(*e).ok())
            .flat_map(|enc| resources.filter_crtcs(enc.possible_crtcs()))
            .collect();
        let possible_crtcs = if possible_crtcs.is_empty() {
            resources.crtcs().to_vec()
        } else {
            possible_crtcs
        };
        let Some(&crtc) = possible_crtcs.iter().find(|c| !used_crtcs.contains(c)) else {
            warn!("No free CRTC for connector {}, skipping", name);
            continue;
        };

        let output = Output::new(name.clone(), PhysicalProperties {
            size: Size::from((phys.0 as i32, phys.1 as i32)),
            subpixel: Subpixel::Unknown,
            make: "Unknown".to_string(),
            model: name.clone(),
            serial_number: String::new(),
        });

        // Register every mode this connector actually reports (not just
        // the one we're about to select) so `zwlr_output_management`
        // can advertise a real per-resolution/refresh list instead of
        // only ever showing whatever's currently active — see
        // `protocols/output_management.rs::advertise_head`, which reads
        // this back via `output.modes()`. EDID mode lists commonly
        // repeat the same (w,h)@refresh combination more than once
        // (e.g. once for the "preferred" flag, again as a plain
        // supported mode), so this dedupes on the exact triplet before
        // calling `add_mode` — smithay doesn't dedupe for us, and a
        // client-visible list with visually identical duplicate entries
        // would just be confusing in a resolution picker.
        let mut seen_modes: std::collections::HashSet<(i32, i32, i32)> = Default::default();
        for m in conn.modes() {
            let (mw, mh) = m.size();
            let refresh_mhz = m.vrefresh() as i32 * 1000;
            if !seen_modes.insert((mw as i32, mh as i32, refresh_mhz)) { continue; }
            output.add_mode(OutputMode { size: Size::from((mw as i32, mh as i32)), refresh: refresh_mhz });
        }

        let sm = OutputMode { size: Size::from((w as i32, h as i32)), refresh: mode.vrefresh() as i32 * 1000 };
        output.change_current_state(Some(sm), Some(Transform::Normal), Some(Scale::Integer(1)), Some(Point::from((x_off, 0))));
        output.set_preferred(sm);
        state.space.map_output(&output, Point::from((x_off, 0)));

        // Build the swapchain for this CRTC/connector — a GBM/EGL one on
        // the normal GLES path, or a plain double-buffered dumb-buffer
        // one on the Pixman (software) fallback path. Which arm runs is
        // decided once per GPU (whatever `create_renderer_for_gpu` chose
        // above), not per-output, since it follows from what renderer
        // context this GPU has, not from anything connector-specific.
        if let BackendData::Udev(ref mut udev) = state.backend_data {
            if let Some(gpu) = udev.devices.get_mut(&node) {
                let is_software = gpu.renderer.as_ref().map(RenderBackend::is_software).unwrap_or(false);
                if is_software {
                    match create_dumb_swapchain(drm, crtc, *conn_handle, mode, w as u32, h as u32) {
                        Ok(dumb) => {
                            let damage_tracker = OutputDamageTracker::from_output(&output);
                            gpu.surfaces.insert(crtc, OutputRenderSurface {
                                output: output.clone(),
                                surface: SurfaceBackend::Dumb(dumb),
                                damage_tracker,
                                connector: *conn_handle,
                            });
                            used_crtcs.push(crtc);
                            info!("Lit up output {} on CRTC {:?} ({}x{}@{}) [software/Pixman]", name, crtc, w, h, mode.vrefresh());
                        }
                        Err(e) => error!("create_dumb_swapchain failed for {}: {}", name, e),
                    }
                } else {
                    match drm.create_surface(crtc, mode, &[*conn_handle]) {
                        Ok(drm_surface) => {
                            let allocator = GbmAllocator::new(gpu.gbm.clone(), GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
                            // `GbmDevice` has no `supported_formats()` method
                            // (that was a guess that didn't hold up against
                            // the real API).
                            //
                            // **Modifier negotiation — was actually wrong,
                            // not just absent.** `GbmBufferedSurface::new`'s
                            // real signature (checked directly against
                            // smithay's own source at
                            // `backend/drm/surface/gbm.rs`, not assumed
                            // this time) is `fn new(drm, allocator,
                            // color_formats: &[Fourcc], renderer_formats:
                            // impl IntoIterator<Item = Format>)` — a real
                            // *fourth* parameter carrying (format,
                            // modifier) pairs, which the previous version
                            // of this code passed `None` for. `Option<T>`
                            // does implement `IntoIterator<Item = T>` (so
                            // that compiled, or would have), but `None`
                            // means an *empty* iterator — this was
                            // silently telling GBM "the renderer supports
                            // zero formats with any modifier", not merely
                            // "no preference"/"figure it out yourself".
                            // Fixed below by passing the renderer's own
                            // `dmabuf_formats()` — the exact same
                            // `Vec<Format>` (format+modifier pairs)
                            // `protocols/dmabuf.rs` already queries from
                            // this renderer for the dmabuf-feedback
                            // protocol, so this isn't a new/unverified API
                            // call, just reusing one already proven to
                            // compile-shape-correctly elsewhere in this
                            // codebase. `GbmBufferedSurface::new` then does
                            // the actual negotiation internally (real
                            // tiled/compressed vendor modifiers now
                            // reachable when the renderer and the DRM
                            // plane both advertise support for the same
                            // one), not this code manually parsing a
                            // plane's `IN_FORMATS` blob by hand.
                            let formats: &[smithay::backend::allocator::Fourcc] = &[
                                smithay::backend::allocator::Fourcc::Xrgb2101010,
                                smithay::backend::allocator::Fourcc::Argb2101010,
                                smithay::backend::allocator::Fourcc::Xrgb8888,
                                smithay::backend::allocator::Fourcc::Argb8888,
                            ];
                            let renderer_formats: Vec<smithay::backend::allocator::Format> = gpu.renderer.as_ref()
                                .and_then(RenderBackend::as_gles)
                                .map(|r| r.dmabuf_formats().into_iter().collect())
                                .unwrap_or_default();
                            match GbmBufferedSurface::new(drm_surface, allocator, formats, renderer_formats) {
                                Ok(gbm_surface) => {
                                    let damage_tracker = OutputDamageTracker::from_output(&output);
                                    gpu.surfaces.insert(crtc, OutputRenderSurface {
                                        output: output.clone(),
                                        surface: SurfaceBackend::Gbm(gbm_surface),
                                        damage_tracker,
                                        connector: *conn_handle,
                                    });
                                    used_crtcs.push(crtc);
                                    info!("Lit up output {} on CRTC {:?} ({}x{}@{})", name, crtc, w, h, mode.vrefresh());
                                }
                                Err(e) => error!("GbmBufferedSurface::new failed for {}: {}", name, e),
                            }
                        }
                        Err(e) => error!("drm.create_surface failed for {}: {}", name, e),
                    }
                }
            }
        }

        state.outputs.push(output);
        x_off += w as i32;
    }
}

/// Real hardware modeset: called from
/// `protocols/output_management.rs::apply_configuration` after
/// `Output::change_current_state` updates the *logical* mode, to also
/// rebuild the physical `DrmSurface` with a matching `drm::control::Mode`
/// — without this, changing resolution in Settings updated what the
/// compositor's own layout/scaling logic believed the output size was,
/// but never actually re-programmed the display controller, so the
/// screen kept outputting the old physical timing regardless of what
/// smithay's `Output` object said.
///
/// Deliberately mirrors `scan_drm_outputs`'s surface-creation block
/// (same allocator flags, same format candidate list) rather than
/// factoring out a shared helper — this is hand-written against the API
/// with no compiler in this environment to check it, and matching
/// already-used call shapes as closely as possible is the main risk
/// mitigation available. Returns `false` (leaving the old surface
/// running) on any failure rather than tearing down a working output.
pub fn apply_hardware_modeset(state: &mut BlueState, output_name: &str, width: i32, height: i32, refresh_mhz: i32) -> bool {
    let BackendData::Udev(ref mut udev) = state.backend_data else {
        // Nothing to do under the winit (nested/dev) backend — there's
        // no physical display timing to reprogram, the logical
        // `change_current_state` the caller already did is sufficient.
        return true;
    };

    for gpu in udev.devices.values_mut() {
        let Some((&crtc, _)) = gpu.surfaces.iter().find(|(_, s)| s.output.name() == output_name) else { continue };
        let connector = gpu.surfaces[&crtc].connector;

        let Ok(conn) = gpu.drm.get_connector(connector, false) else {
            warn!("apply_hardware_modeset: connector for {} vanished", output_name);
            return false;
        };
        // Match against the connector's own reported modes rather than
        // trusting the caller's (w,h,refresh) blindly — only a mode the
        // display itself advertises is something `create_surface` can
        // actually commit.
        let Some(drm_mode) = conn.modes().iter().find(|m| {
            let (mw, mh) = m.size();
            mw as i32 == width && mh as i32 == height && m.vrefresh() as i32 * 1000 == refresh_mhz
        }).copied() else {
            warn!("apply_hardware_modeset: {} has no matching mode for {}x{}@{}", output_name, width, height, refresh_mhz);
            return false;
        };

        // Same GLES-vs-Pixman branch as `scan_drm_outputs` — a hardware
        // modeset has to rebuild whichever kind of swapchain this
        // output already had (a software-fallback GPU doesn't suddenly
        // gain a GBM/EGL path just because Settings requested a
        // different resolution), so this reads the *existing* surface's
        // kind rather than re-deciding from scratch.
        let was_software = matches!(gpu.surfaces.get(&crtc).map(|s| &s.surface), Some(SurfaceBackend::Dumb(_)));

        if was_software {
            match create_dumb_swapchain(&mut gpu.drm, crtc, connector, drm_mode, width as u32, height as u32) {
                Ok(dumb) => {
                    if let Some(existing) = gpu.surfaces.get_mut(&crtc) {
                        existing.surface = SurfaceBackend::Dumb(dumb);
                        existing.damage_tracker = OutputDamageTracker::from_output(&existing.output);
                    }
                    info!("Hardware modeset applied: {} -> {}x{}@{} [software/Pixman]", output_name, width, height, refresh_mhz);
                    return true;
                }
                Err(e) => { error!("apply_hardware_modeset: create_dumb_swapchain failed for {}: {}", output_name, e); return false; }
            }
        }

        let drm_surface = match gpu.drm.create_surface(crtc, drm_mode, &[connector]) {
            Ok(s) => s,
            Err(e) => { error!("apply_hardware_modeset: drm.create_surface failed for {}: {}", output_name, e); return false; }
        };

        let allocator = GbmAllocator::new(gpu.gbm.clone(), GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
        // Same priority-ordered format list as scan_drm_outputs (10-bit
        // first, 8-bit fallback within the same call) — see that
        // function's comment for why this shape rather than a retry
        // loop. Same `renderer.dmabuf_formats()` fix for the
        // renderer_formats/modifier-negotiation argument too — see that
        // function's comment for the full story on why `None` there was
        // an actual bug (empty formats, not "no preference"), not just a
        // missing-feature placeholder.
        let formats: &[smithay::backend::allocator::Fourcc] = &[
            smithay::backend::allocator::Fourcc::Xrgb2101010,
            smithay::backend::allocator::Fourcc::Argb2101010,
            smithay::backend::allocator::Fourcc::Xrgb8888,
            smithay::backend::allocator::Fourcc::Argb8888,
        ];
        let renderer_formats: Vec<smithay::backend::allocator::Format> = gpu.renderer.as_ref()
            .and_then(RenderBackend::as_gles)
            .map(|r| r.dmabuf_formats().into_iter().collect())
            .unwrap_or_default();
        let gbm_surface = match GbmBufferedSurface::new(drm_surface, allocator, formats, renderer_formats) {
            Ok(s) => s,
            Err(e) => { error!("apply_hardware_modeset: GbmBufferedSurface::new failed for {}: {}", output_name, e); return false; }
        };

        // Replace the old surface — dropping the previous `gbm_surface`
        // (and the `DrmSurface` it owned) tears down its DRM resources;
        // the fresh one above already has the new mode committed.
        if let Some(existing) = gpu.surfaces.get_mut(&crtc) {
            existing.surface = SurfaceBackend::Gbm(gbm_surface);
            existing.damage_tracker = OutputDamageTracker::from_output(&existing.output);
        }
        info!("Hardware modeset applied: {} -> {}x{}@{}", output_name, width, height, refresh_mhz);
        return true;
    }

    warn!("apply_hardware_modeset: no DRM surface found for output {}", output_name);
    false
}

/// Creates a GLES renderer bound to the GPU's GBM device via EGL. This is
/// what was completely missing before: without it there was no way to
/// actually draw anything for the udev/TTY backend, only the winit
/// (nested) backend had a renderer.
fn create_gles_renderer(
    gbm: &GbmDevice<DrmDeviceFd>,
) -> Result<GlesRenderer, Box<dyn std::error::Error>> {
    // Safety: EGLDisplay::new requires the display handle to outlive the
    // context/renderer built from it, which it does here since `gbm` (and
    // therefore the EGLDisplay built from it) lives inside `GpuDevice` for
    // the lifetime of the GPU device entry.
    let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
    let egl_device = EGLDevice::device_for_display(&egl_display)?;
    let _ = egl_device; // currently unused beyond validating the display is usable
    let egl_context = EGLContext::new(&egl_display)?;
    let renderer = unsafe { GlesRenderer::new(egl_context)? };
    Ok(renderer)
}

/// Picks a `RenderBackend` for this GPU node: GLES/EGL if it's available
/// (the normal, hardware-accelerated case), falling back to the
/// CPU-only `PixmanRenderer` if it isn't — see `state::RenderBackend`'s
/// doc for exactly which real-world case that's for (a KMS-capable
/// device with no working GPU driver, e.g. a headless VM/CI runner using
/// `vkms` or a software `virtio-gpu`).
///
/// `PixmanRenderer::new()` has no GBM/EGL dependency at all — it's a
/// pure in-memory software rasterizer (confirmed directly against
/// `smithay`'s `backend::renderer::pixman` module at the pinned rev:
/// its constructor takes no device/display argument), so unlike the
/// GLES arm there's nothing here that can fail for GPU-driver reasons;
/// its `Result` is kept for symmetry with `create_gles_renderer` and to
/// surface a genuine allocation failure rather than unwrap it away.
fn create_renderer_for_gpu(gbm: &GbmDevice<DrmDeviceFd>, node: DrmNode) -> Result<RenderBackend, Box<dyn std::error::Error>> {
    match create_gles_renderer(gbm) {
        Ok(r) => Ok(RenderBackend::Gles(r)),
        Err(e) => {
            warn!(
                "GPU {:?}: EGL/GLES renderer unavailable ({}), falling back to the \
                 software Pixman renderer — this output will still render, just \
                 without GPU acceleration (expect noticeably higher CPU usage and \
                 lower framerates, especially at high resolutions)",
                node, e
            );
            let renderer = PixmanRenderer::new()?;
            Ok(RenderBackend::Pixman(renderer))
        }
    }
}

/// Allocates the Pixman path's swapchain for one CRTC: two dumb buffers
/// (plain KMS-mappable system memory, no GBM/EGL involved at all — see
/// `state::DumbSwapchain`'s doc), each wrapped in its own framebuffer id,
/// with the first one committed as the CRTC's initial scanout buffer via
/// `set_crtc` (mirroring what `GbmBufferedSurface::new`'s own first
/// modeset does internally on the GLES path — a freshly created surface
/// needs *something* on screen before the first `page_flip`, which
/// (unlike `set_crtc`) cannot itself perform the initial modeset).
///
/// Written without a compiler available to verify the exact `drm`-crate
/// method signatures at this pinned smithay rev's dependency version —
/// same caveat, and same reasoning for why this is still worth adding
/// rather than leaving Pixman-backed GPUs entirely dark, as
/// `scan_drm_outputs`'s own doc comment already states for the GBM path.
fn create_dumb_swapchain(
    drm: &mut DrmDevice,
    crtc: crtc::Handle,
    connector: connector::Handle,
    mode: smithay::reexports::drm::control::Mode,
    width: u32,
    height: u32,
) -> Result<DumbSwapchain, Box<dyn std::error::Error>> {
    // Second reversal on this function, both from real `cargo build`
    // output rather than guessing — worth recording exactly what
    // happened so the next person (or the next compile pass) doesn't
    // repeat either mistake:
    //
    // 1. First version: raw `drm.create_dumb_buffer(...)` (the
    //    `drm`-crate `control::Device` trait method). Wrong docstring
    //    claim at the time ("this returns smithay's own
    //    allocator::dumb::DumbBuffer") — it doesn't; it returns
    //    `drm::control::dumbbuffer::DumbBuffer`, a same-named but
    //    distinct type, confirmed by the compiler's own "DumbBuffer and
    //    DumbBuffer have similar names, but are actually distinct
    //    types" error.
    // 2. Second version: switched to smithay's own
    //    `backend::allocator::dumb::DumbAllocator`/`DumbBuffer` on the
    //    theory that *that* was the type `PixmanRenderer::bind` needed.
    //    Also wrong, and the real compiler output settles both
    //    questions at once: (a) smithay's `allocator::dumb::DumbBuffer`
    //    does NOT implement `drm::buffer::Buffer` at all (so
    //    `add_framebuffer` rejects it — only `GbmBuffer`,
    //    `gbm::BufferObject<T>`, and the RAW
    //    `drm::control::dumbbuffer::DumbBuffer` implement that trait,
    //    per the compiler's own "the following other types implement
    //    trait" list), and (b) `PixmanRenderer` doesn't implement
    //    `Bind<DumbBuffer>` for *either* DumbBuffer type anyway — its
    //    only two `Bind` impls are `Bind<Dmabuf>` and
    //    `Bind<pixman::Image<'static, 'static>>` (also straight from
    //    the compiler's own error).
    //
    // Correct shape, now: raw `drm.create_dumb_buffer(...)` for
    // KMS/framebuffer purposes (`add_framebuffer`/`set_crtc`/
    // `page_flip` all want *this* type) — reverting to what version 1
    // used, which really was right for this half of the problem — and
    // separately, at render time, map that raw dumb buffer's memory and
    // wrap the mapping in a `pixman::Image` (see `render_udev_pixman`)
    // for the `PixmanRenderer::bind` half, which neither earlier version
    // did at all. Two different buffer-shaped needs (KMS scanout object,
    // CPU-mappable render target), two different types bridging into
    // them from the one underlying dumb-buffer allocation — not one
    // type serving both, which is what both earlier attempts assumed.
    let make_buffer = |drm: &mut DrmDevice| -> Result<DumbSwapchainBuffer, Box<dyn std::error::Error>> {
        // XRGB8888 rather than the 10-bit formats `scan_drm_outputs`
        // prioritizes for the GBM path — dumb buffers are the
        // universally-supported baseline path precisely because every
        // KMS driver has to accept plain linear 8bpc XRGB8888 for them.
        let dumb = drm
            .create_dumb_buffer((width as u32, height as u32), Fourcc::Xrgb8888, 32)
            .map_err(|e| format!("create_dumb_buffer: {e}"))?;
        let fb = drm
            .add_framebuffer(&dumb, 24, 32)
            .map_err(|e| format!("add_framebuffer (dumb): {e}"))?;
        Ok(DumbSwapchainBuffer { dumb, fb, ever_rendered: false })
    };

    let buf0 = make_buffer(drm)?;
    let buf1 = make_buffer(drm)?;

    // Initial modeset — commits `buf0` as the CRTC's scanout buffer
    // *and* programs the display timing in one call, exactly like the
    // very first frame of a `GbmBufferedSurface`-backed output needs to
    // happen somewhere too (there it happens implicitly inside
    // `GbmBufferedSurface::new`; here it's explicit since we're not
    // going through that type at all). Legacy `set_crtc` (not an atomic
    // commit) — the deliberately simpler, more universally-supported
    // KMS entry point; atomic-only Pixman-fallback devices are rare
    // enough (and legacy `set_crtc` broad enough) that this isn't worth
    // the extra complexity of an atomic property-blob commit for the
    // one-time initial modeset.
    drm.set_crtc(crtc, Some(buf0.fb), (0, 0), &[connector], Some(mode))
        .map_err(|e| format!("set_crtc (dumb initial modeset): {e}"))?;

    Ok(DumbSwapchain {
        crtc,
        connector,
        buffers: [buf0, buf1],
        front: 0,
    })
}

/// Actually renders and presents a frame for one DRM-backed output.
/// Mirrors `render_winit()`'s two-phase borrow pattern (collect render
/// elements, then render), but sources the renderer from the GPU device
/// map instead of `WinitGraphicsBackend`, and pushes the result through
/// `GbmBufferedSurface::queue_buffer()` for an atomic pageflip instead of
/// `WinitGraphicsBackend::submit()`.
/// Render elements for every mapped layer-shell surface on `output`
/// (panels, the lock screen, on-screen keyboards, ...). Was previously
/// nothing — `new_layer_surface`/`layer_destroyed` in state/mod.rs were
/// no-ops, so nothing was ever mapped into smithay's `LayerMap` in the
/// first place; this is the composite-side half of that fix, same
/// relationship as `input_method::render_elements` is to the IME popup
/// positioning fix.
fn layer_shell_elements<R, E>(
    output: &Output,
    renderer: &mut R,
    scale: f64,
) -> Vec<E>
where
    R: smithay::backend::renderer::Renderer + smithay::backend::renderer::ImportAll,
    R::TextureId: Clone + 'static,
    E: From<smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement<R>>,
{
    use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
    use smithay::desktop::layer_map_for_output;

    let map = layer_map_for_output(output);
    let mut out = Vec::new();
    for layer in map.layers() {
        let wl_surface = layer.wl_surface();
        let Some(geo) = map.layer_geometry(layer) else { continue };
        let phys = smithay::utils::Point::<i32, smithay::utils::Physical>::from((
            (geo.loc.x as f64 * scale).round() as i32,
            (geo.loc.y as f64 * scale).round() as i32,
        ));
        out.extend(render_elements_from_surface_tree(
            renderer,
            wl_surface,
            phys,
            scale,
            1.0,
            smithay::backend::renderer::element::Kind::Unspecified,
        ));
    }
    out
}

/// Dispatches to the GLES or Pixman render path depending on which kind
/// of swapchain this output ended up with (decided once, per-GPU, back
/// in `scan_drm_outputs` — see `create_renderer_for_gpu`'s doc). Kept as
/// a thin `match` rather than folding the branch into one function
/// because the two paths' element types genuinely differ
/// (`WaylandSurfaceRenderElement<GlesRenderer>` vs
/// `WaylandSurfaceRenderElement<PixmanRenderer>` are unrelated concrete
/// types — smithay's renderer element types are generic over the
/// renderer, not trait objects — so there's no single code path that
/// typechecks for both without a lot of unnecessary generic plumbing
/// through a function that, unlike `render_winit`, only ever needs to
/// run for one output at a time anyway).
pub fn render_udev(state: &mut BlueState, node: DrmNode, crtc: crtc::Handle) {
    let is_software = if let BackendData::Udev(ref udev) = state.backend_data {
        udev.devices.get(&node)
            .and_then(|g| g.surfaces.get(&crtc))
            .map(|s| matches!(s.surface, SurfaceBackend::Dumb(_)))
            .unwrap_or(false)
    } else {
        return;
    };
    if is_software {
        render_udev_pixman(state, node, crtc);
    } else {
        render_udev_gles(state, node, crtc);
    }
}

/// The original GLES/GBM render path — HDR tone-mapping, screencopy,
/// atomic pageflip via `GbmBufferedSurface`. Unchanged in behavior from
/// before the Pixman fallback existed; only renamed (was `render_udev`)
/// and given a `SurfaceBackend::Gbm(..)` match instead of assuming every
/// surface is GBM-backed.
fn render_udev_gles(state: &mut BlueState, node: DrmNode, crtc: crtc::Handle) {
    // Snapshot the output + this GPU's compiled HDR program before
    // taking `state.backend_data`'s mutable borrow below — both are
    // cheap to clone (smithay's `Output` and `GlesTexProgram` wrap an
    // internal `Arc`), which lets the fullscreen/HDR detection call
    // just below take a plain `&BlueState` instead of having to be
    // threaded through the udev backend's mutable borrow.
    let Some((output, hdr_program)) = (if let BackendData::Udev(ref udev) = state.backend_data {
        udev.devices.get(&node).and_then(|g| {
            g.surfaces.get(&crtc).map(|s| (s.output.clone(), g.hdr_tonemap_shader.clone()))
        })
    } else {
        None
    }) else {
        return;
    };

    // See `render::hdr_shader`'s module doc for exactly what this covers
    // (a single fullscreen HDR surface) and why it's scoped that
    // narrowly (popup collection). `None` here just means "composite
    // this output the normal way" — the common case.
    let fullscreen_hdr_program =
        hdr_program.filter(|_| hdr_shader::sole_fullscreen_hdr_surface(state, &output));

    let BackendData::Udev(ref mut udev) = state.backend_data else { return };
    let Some(gpu) = udev.devices.get_mut(&node) else { return };
    let Some(renderer) = gpu.renderer.as_mut().and_then(RenderBackend::as_gles_mut) else { return };
    let Some(surface) = gpu.surfaces.get_mut(&crtc) else { return };
    let SurfaceBackend::Gbm(ref mut gbm_surface) = surface.surface else { return };

    let output_scale = surface.output.current_scale().fractional_scale() as f32;

    use smithay::desktop::space::SpaceRenderElements;
    use hdr_shader::HdrAwareElement;
    let mut elements: Vec<HdrAwareElement<SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>>> =
        state.space.render_elements_for_output(renderer, &surface.output, output_scale)
            .unwrap_or_default()
            .into_iter()
            // Only the elements making up the sole fullscreen surface
            // get the HDR program (`fullscreen_hdr_program` is `None`
            // unless that exact situation was detected above, in which
            // case this bulk call's output *is* that one surface's
            // elements — nothing else is visible on this output while
            // it's fullscreen).
            .map(|e| HdrAwareElement::new(e, fullscreen_hdr_program.clone()))
            .collect();
    elements.extend(
        crate::protocols::input_method::render_elements(
            &state.input_method_popups, renderer, output_scale as f64,
        ).into_iter().map(HdrAwareElement::passthrough)
    );
    elements.extend(
        layer_shell_elements(&surface.output, renderer, output_scale as f64)
            .into_iter().map(HdrAwareElement::passthrough)
    );

    let mode_size = surface.output.current_mode().map(|m| m.size).unwrap_or(Size::from((0, 0)));

    let (mut dmabuf, age) = match gbm_surface.next_buffer() {
        Ok(v) => v,
        Err(e) => { warn!("next_buffer failed for {:?}: {}", crtc, e); return; }
    };
    // `Renderer::bind` returns the bound render *target* (a `GlesTarget`
    // here) rather than mutating the renderer in place — that target,
    // not a `GlesFrame`, is what `OutputDamageTracker::render_output`
    // wants. `render_output` handles the renderer.render()/frame
    // lifecycle internally; calling `renderer.render()` ourselves first
    // (the previous version of this code) was wrong for this smithay
    // rev and duplicated work `render_output` already does.
    let mut target = match renderer.bind(&mut dmabuf) {
        Ok(t) => t,
        Err(e) => { warn!("renderer.bind failed for {:?}: {}", crtc, e); return; }
    };

    let render_result = match surface.damage_tracker.render_output(
        renderer, &mut target, age as usize, &elements, [0.08_f32, 0.10, 0.15, 1.0],
    ) {
        Ok(r) => r,
        Err(e) => {
            warn!("render_output failed for {:?}: {:?}", crtc, e);
            drop(target);
            return;
        }
    };

    // Service any pending screencopy requests (grim et al.) for this
    // output — must happen here, while `target` is still bound, since
    // `copy_framebuffer` (see `protocols/screencopy.rs::service_screencopy`)
    // reads from whatever's currently bound, same as `glReadPixels` would.
    // `state.screencopy_state` and `state.backend_data` (which `renderer`
    // is borrowed from) are disjoint fields, so this borrow-checks even
    // though `renderer` is still alive.
    let output_name = surface.output.name();
    crate::protocols::screencopy::service_screencopy(
        &mut state.screencopy_state, renderer, &target, &output_name, (mode_size.w, mode_size.h),
        render_result.damage.map(|v| v.as_slice()),
    );

    // `target` (and the mutable borrow of `renderer`/`dmabuf` it holds)
    // must go out of scope before `dmabuf` can be handed off to KMS via
    // `queue_buffer` below.
    drop(target);

    // `render_result.damage` is `Option<&Vec<Rectangle<i32, Physical>>>`
    // at this pinned smithay rev (a lifetime-bound reference into the
    // damage tracker's internal state, not an owned Vec as first
    // assumed) — `queue_buffer` wants it owned, so `.cloned()` converts
    // `Option<&Vec<_>>` to `Option<Vec<_>>`, exactly as the compiler's
    // own suggestion for this error.
    if let Err(e) = gbm_surface.queue_buffer(None, render_result.damage.cloned(), ()) {
        warn!("queue_buffer (pageflip) failed for {:?}: {}", crtc, e);
    }
}

/// The Pixman/software render path — no HDR tone-mapping (GLES-only, see
/// module doc), screencopy now serviced too (via
/// `screencopy::service_screencopy_pixman`, see that function's own doc
/// for why it's a separate function rather than a generic one shared
/// with the GLES path), no multi-GPU cross-import (a software-fallback
/// GPU is definitionally the *only* renderer for whatever it's driving —
/// there's no second GPU to import from in that scenario). What it does
/// give a GPU-less output: an actual visible desktop, where before this
/// existed the output was detected but permanently blank (see
/// `create_renderer_for_gpu`'s doc for the motivating case).
///
/// Same "written without a compiler in this environment" caveat as
/// `scan_drm_outputs`/`create_dumb_swapchain` — the exact shape of
/// `PixmanRenderer::bind`'s accepted target type in particular is the
/// riskiest guess in this function (smithay's Pixman renderer binds
/// against something implementing its internal buffer-access traits;
/// mapped `DumbBuffer` memory is architecturally the right shape for
/// that — plain CPU-addressable pixels, which is exactly what Pixman
/// needs and exactly why the dumb-buffer swapchain was built around
/// `DumbBuffer` rather than e.g. GBM linear buffers — but the precise
/// method/type names at this pinned rev aren't verified against a
/// compiler). Treat as a scaffold to compile-fix, same as the rest of
/// this file's udev path was when it was first written.
fn render_udev_pixman(state: &mut BlueState, node: DrmNode, crtc: crtc::Handle) {
    let BackendData::Udev(ref mut udev) = state.backend_data else { return };
    let Some(gpu) = udev.devices.get_mut(&node) else { return };
    let Some(renderer) = gpu.renderer.as_mut().and_then(RenderBackend::as_pixman_mut) else { return };
    let Some(surface) = gpu.surfaces.get_mut(&crtc) else { return };
    let SurfaceBackend::Dumb(ref mut swapchain) = surface.surface else { return };

    let output_scale = surface.output.current_scale().fractional_scale() as f32;

    use smithay::desktop::space::SpaceRenderElements;
    let mut elements: Vec<SpaceRenderElements<PixmanRenderer, WaylandSurfaceRenderElement<PixmanRenderer>>> =
        state.space.render_elements_for_output(renderer, &surface.output, output_scale)
            .unwrap_or_default();
    elements.extend(crate::protocols::input_method::render_elements(
        &state.input_method_popups, renderer, output_scale as f64,
    ));
    elements.extend(layer_shell_elements(&surface.output, renderer, output_scale as f64));

    // Render into the back buffer (index `1 - front`). `PixmanRenderer`
    // only implements `Bind<Dmabuf>` and `Bind<pixman::Image<'static,
    // 'static>>` (confirmed by a real compiler error — an earlier
    // version of this function tried `renderer.bind(&mut back.dumb)`
    // directly, which doesn't typecheck against either impl) — a plain
    // KMS dumb buffer isn't either of those on its own, so it has to be
    // mapped into CPU-addressable memory first and wrapped in a
    // `pixman::Image` view over that mapping, which *is* bindable.
    //
    // `map_dumb_buffer` — real `drm`-crate `control::Device` trait
    // method (mirrors `add_framebuffer`/`create_dumb_buffer`, all three
    // are part of the same dumb-buffer ioctl family) — returns a
    // `DumbMapping` guard whose `Deref<Target = [u8]>` is the mapped
    // memory; kept alive in `_mapping` for this whole scope, since
    // `image` (built via the `unsafe` `from_raw_mut`, which lets the
    // lifetime be claimed as `'static` even though the real backing
    // memory is only valid as long as `_mapping` is) has no compiler-
    // enforced tie to it — dropping `_mapping` before `image`/`target`
    // are done being used would be a real use-after-unmap bug, so it's
    // named `_mapping` (not `_`) specifically as a reminder not to
    // reorder these two lines relative to the rendering below.
    let age = swapchain.back_age();
    let back = swapchain.back_mut();
    let (buf_w, buf_h) = back.dumb.size();
    let stride = back.dumb.pitch() as usize;
    let mut _mapping = match gpu.drm.map_dumb_buffer(&mut back.dumb) {
        Ok(m) => m,
        Err(e) => { warn!("map_dumb_buffer failed for {:?}: {}", crtc, e); return; }
    };
    // XRGB8888, matching `create_dumb_swapchain`'s allocation format —
    // `TryFrom<Fourcc>` rather than naming a `pixman::FormatCode`
    // variant directly (pixman's own C-derived format naming
    // convention doesn't line up 1:1 with `Fourcc`'s, and the crate
    // ships this exact conversion — evidenced by its own
    // `UnsupportedDrmFourcc`/`UnsupportedFormatCode` error types —
    // specifically so callers don't have to hand-map between the two
    // naming schemes).
    let Ok(pixman_format) = smithay::reexports::pixman::FormatCode::try_from(Fourcc::Xrgb8888) else {
        warn!("pixman FormatCode has no Xrgb8888 equivalent — should not happen, this format is one of pixman's most basic ones");
        return;
    };
    let image = unsafe {
        smithay::reexports::pixman::Image::from_raw_mut(
            pixman_format,
            buf_w as usize,
            buf_h as usize,
            _mapping.as_mut_ptr() as *mut u32,
            stride,
            false,
        )
    };
    let mut image = match image {
        Ok(img) => img,
        Err(e) => { warn!("pixman::Image::from_raw_mut failed for {:?}: {:?}", crtc, e); return; }
    };
    let mut target = match renderer.bind(&mut image) {
        Ok(t) => t,
        Err(e) => { warn!("pixman renderer.bind failed for {:?}: {}", crtc, e); return; }
    };

    let render_result = match surface.damage_tracker.render_output(
        renderer, &mut target, age, &elements, [0.08_f32, 0.10, 0.15, 1.0],
    ) {
        Ok(r) => r,
        Err(e) => {
            warn!("render_output (pixman) failed for {:?}: {:?}", crtc, e);
            drop(target);
            drop(image);
            drop(_mapping);
            return;
        }
    };

    // Service pending screencopy requests (grim et al.) — same
    // must-happen-while-bound requirement as the GLES path (see that
    // function's own comment on this same call), just against the
    // Pixman target/renderer instead. Real per-frame damage now passed
    // through (not `None`) so `copy_with_damage` screencopy requests —
    // the ones a screen recorder actually cares about, see
    // `service_screencopy`'s own doc on why those specifically wait for
    // real damage — get serviced on Pixman-backed outputs too, not just
    // GLES ones.
    if let Some(mode) = surface.output.current_mode() {
        let output_name = surface.output.name();
        crate::protocols::screencopy::service_screencopy_pixman(
            &mut state.screencopy_state, renderer, &target, &output_name,
            (mode.size.w, mode.size.h), render_result.damage.map(|v| v.as_slice()),
        );
    }

    drop(target);
    // Drop the pixman `Image` view and the underlying `DumbMapping`
    // (unmapping it) before touching `gpu.drm`/`back` again below —
    // `map_dumb_buffer`'s `DumbMapping` guard plausibly borrows from
    // `gpu.drm` for its unmap-on-drop bookkeeping (real drm-rs mmap
    // guards commonly do), which would otherwise conflict with the
    // `gpu.drm.page_flip(...)` call just below. Explicit, not relied on
    // implicit end-of-scope drop timing, precisely because which of
    // `back.dumb` vs `gpu.drm` the guard's lifetime is actually tied to
    // isn't independently confirmed here (see this function's own
    // top-of-block comment) — dropping early removes the ambiguity
    // either way.
    drop(image);
    drop(_mapping);
    back.ever_rendered = true;

    // Legacy page-flip ioctl (not an atomic commit — same reasoning as
    // `create_dumb_swapchain`'s initial `set_crtc`) on the *raw* device,
    // since this path never went through smithay's `DrmSurface`/
    // `GbmBufferedSurface` at all. `PageFlipFlags::EVENT` requests the
    // `DrmEvent::VBlank` this GPU's notifier is already wired to receive
    // (see `register_drm_notifier`) — consumed there today only for the
    // GBM path's `frame_submitted()`; the Pixman path doesn't need to
    // wait for it before advancing `front` below (unlike
    // `GbmBufferedSurface`, there's no internal swapchain state that
    // needs the previous flip acknowledged first), just requests the
    // event for symmetry/future bookkeeping.
    match gpu.drm.page_flip(crtc, back.fb, PageFlipFlags::EVENT, None) {
        Ok(()) => {
            swapchain.front = 1 - swapchain.front;
        }
        Err(e) => warn!("page_flip (dumb/pixman) failed for {:?}: {}", crtc, e),
    }
}

/// Iterates every lit-up output across every GPU and renders a frame for
/// each. Called from the 16ms render timer in `init_udev` (previously
/// that timer only called `state.refresh()`, which does bookkeeping but
/// never actually drew a frame in udev mode).
pub fn render_all_udev_outputs(state: &mut BlueState) {
    let targets: Vec<(DrmNode, crtc::Handle)> = if let BackendData::Udev(ref udev) = state.backend_data {
        udev.devices
            .iter()
            .flat_map(|(node, gpu)| gpu.surfaces.keys().map(move |c| (*node, *c)))
            .collect()
    } else {
        Vec::new()
    };
    for (node, crtc) in targets {
        render_udev(state, node, crtc);
    }
}
