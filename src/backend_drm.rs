use std::collections::HashMap;

use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{primary_gpu, UdevBackend, UdevEvent};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::drm::control::crtc;
use smithay::reexports::input::Libinput;
use smithay::reexports::wayland_server::Display;
use smithay::backend::drm::DrmNode;
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};

use crate::state::HwdeState;

/// Per-GPU state. Real fields once `device_added` is implemented would
/// include the opened `DrmDevice`, its `GbmDevice` allocator, and a
/// `HashMap<crtc::Handle, Surface>` of per-connector KMS surfaces (see
/// `anvil::udev::BackendData` for the reference shape) - left minimal here
/// since nothing populates it yet.
#[derive(Default)]
struct GpuState {
    #[allow(dead_code)]
    surfaces: HashMap<crtc::Handle, ()>,
}

/// Starts a full DRM/udev session. Returns once the session ends (VT
/// switch away + back is handled internally via `SessionEvent`, not by
/// returning).
///
/// # Real (session/device-discovery/input tier)
pub fn run_udev() -> anyhow::Result<()> {
    let mut event_loop: EventLoop<HwdeState> = EventLoop::try_new()?;
    let display: Display<HwdeState> = Display::new()?;
    let display_handle = display.handle();

    // --- session -----------------------------------------------------------
    let (session, notifier) = LibSeatSession::new()
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
        .udev_assign_seat(session.seat())
        .map_err(|_| anyhow::anyhow!("failed to assign udev seat to libinput"))?;
    let libinput_backend = LibinputInputBackend::new(libinput_context.clone());

    let mut gpus: HashMap<DrmNode, GpuState> = HashMap::new();
    gpus.insert(primary_gpu_node, GpuState::default());

    // TODO(hardware): construct `HwdeState` here (mirroring
    // `winit_backend.rs`'s construction almost exactly - compositor_state,
    // xdg_shell_state, seat, wallpaper, etc. are all backend-agnostic) and
    // call `device_added(primary_gpu_node, &primary_gpu_path, &mut state)`
    // for the primary GPU (and again for any already-present secondary GPUs
    // from `udev_backend.device_list()`) before entering the event loop.

    // --- event loop wiring (this part IS backend-agnostic plumbing) -----------
    event_loop
        .handle()
        .insert_source(udev_backend, move |event, _, _state: &mut HwdeState| match event {
            UdevEvent::Added { device_id, path } => {
                tracing::info!("udev: DRM device added: {path:?} ({device_id})");
                // TODO(hardware): DrmNode::from_dev_id(device_id) -> device_added(...)
            }
            UdevEvent::Changed { device_id } => {
                tracing::info!("udev: DRM device changed: {device_id}");
                // TODO(hardware): re-scan connectors on the existing device (handles
                // hotplugging a monitor) - see anvil::udev::device_changed.
            }
            UdevEvent::Removed { device_id } => {
                tracing::info!("udev: DRM device removed: {device_id}");
                // TODO(hardware): tear down that device's surfaces/outputs.
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert udev event source: {e}"))?;

    event_loop
        .handle()
        .insert_source(libinput_backend, move |event, _, state: &mut HwdeState| {
            state.process_input_event(event, todo_output_ref());
        })
        .map_err(|e| anyhow::anyhow!("failed to insert libinput event source: {e}"))?;

    event_loop
        .handle()
        .insert_source(notifier, move |event, &mut (), _state: &mut HwdeState| match event {
            SessionEvent::PauseSession => {
                tracing::info!("session paused (VT switched away) - suspending input/rendering");
                libinput_context.suspend();
                // TODO(hardware): pause every GPU's DrmOutputManager too, mirroring
                // anvil::udev's PauseSession handling.
            }
            SessionEvent::ActivateSession => {
                tracing::info!("session resumed (VT switched back)");
                if let Err(err) = libinput_context.resume() {
                    tracing::error!("failed to resume libinput: {err:?}");
                }
                // TODO(hardware): resume DRM devices + force a full redraw of every
                // output (the compositor missed however many vblanks happened while
                // suspended).
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert session notifier: {e}"))?;

    tracing::warn!(
        "backend_drm::run_udev() reached the point where hardware-in-the-loop work \
         starts (device_added / atomic KMS / render loop) - stopping here. \
         See this module's doc comment for what's left."
    );

    let _ = display_handle;
    let _ = event_loop;
    Ok(())
}

/// Fallback for when [`primary_gpu`] can't identify a card via PCI hints -
/// which happens routinely on VMs (`virtio-gpu`, `vboxvideo`, `qxl` and
/// similar paravirtualized devices typically don't expose the PCI class
/// info `primary_gpu` keys off of) even though a perfectly usable
/// `/dev/dri/cardN` exists. Just takes the lowest-numbered card node that
/// parses as a valid primary (non-render) [`DrmNode`].
///
/// This is plain directory enumeration, not hardware-in-the-loop rendering
/// work, so it carries the same "real code" confidence as the rest of this
/// section - unlike `device_added`/the render loop below.
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
        // DrmNode::from_path also rejects render-only nodes (/dev/dri/renderD*
        // wouldn't match our "card" filter anyway, but this also catches a
        // "cardN" that exists but isn't actually a DRM primary node for some
        // other reason) - so a successful parse here is a reasonable signal
        // this is usable, without yet opening/probing the device itself.
        if DrmNode::from_path(&candidate).is_ok() {
            tracing::info!("primary_gpu() found nothing via PCI hints; falling back to {}", candidate.display());
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// Placeholder so the event-loop wiring above type-checks in isolation;
/// real code replaces this with the `Output` corresponding to whichever
/// CRTC the input event's device is associated with (a device can span
/// multiple outputs on multi-monitor setups) - see
/// `anvil::udev::AnvilState::process_input_event` for how it resolves that
/// mapping via `UdevOutputId`.
fn todo_output_ref() -> &'static smithay::output::Output {
    unimplemented!(
        "backend_drm.rs is not wired into main.rs yet - this stub exists only to keep \
         the (currently unused) event-loop wiring above type-checking while the \
         hardware-dependent core is being built. See the module doc comment."
    )
}

/// # `TODO(hardware)`: real device/connector setup
///
/// Not implemented - this is the part that genuinely needs a physical
/// machine with a GPU to build correctly. Build it by adapting
/// `anvil::udev::AnvilState::device_added` (~750-876) and
/// `::connector_connected` (~876-1054) in the Smithay repository (tag
/// matching this project's `smithay = "0.7"` dependency):
///
/// 1. Open the DRM device (`DrmDevice::new`), get its `GbmDevice` allocator
///    bound to the same fd.
/// 2. Bind a `GlesRenderer` to the device via EGL
///    (`EGLDisplay::new`/`EGLContext::new`, `renderer.bind_wl_display` for
///    dmabuf import from clients).
/// 3. Enumerate connectors (`smithay_drm_extras::drm_scanner::DrmScanner`
///    is the convenience anvil itself uses) and for each connected one,
///    pick a CRTC + mode, and construct a `DrmCompositor`
///    (`smithay::backend::drm::compositor`) bound to that CRTC - this is
///    what actually issues atomic KMS commits and manages the GBM
///    buffer/damage-tracking lifecycle per frame.
/// 4. Create a matching `smithay::output::Output` + map it into
///    `HwdeState::space` at an appropriate position (see
///    `winit_backend.rs` for the non-multi-output single-`Output` version
///    of this).
/// 5. Drive rendering off `DrmCompositor`'s vblank/frame-submission
///    callback (registered as its own calloop event source per CRTC)
///    instead of `winit_backend.rs`'s "redraw every loop iteration" -
///    calling `render_elements::build_output_elements` +
///    `DrmCompositor::render_frame`/`queue_frame` each time, which already
///    works today with any `GlesRenderer`+`Output` pair regardless of
///    backend.
///
/// ## Non-atomic KMS fallback (needed for VirtualBox / older QEMU)
///
/// `DrmDevice::new(fd, allow_atomic, ...)` takes an `allow_atomic: bool`.
/// Step 1 above must **try `allow_atomic = true` first, and on failure
/// retry once with `allow_atomic = false`** before giving up on that
/// device - `virtio-gpu`, `vboxvideo`, and `qxl` frequently only implement
/// legacy KMS (`crtc::set` + `page_flip`, not the atomic commit ioctl), and
/// `DrmDevice::new` returning an error is exactly how that shows up. Do
/// *not* treat that error as fatal for the whole backend.
///
/// `DrmCompositor` itself handles both atomic and legacy paths internally
/// once constructed with a device that was opened in the matching mode -
/// the fallback only needs to happen at the `DrmDevice::new` call in step 1,
/// nothing downstream needs to branch on it explicitly.
///
/// ## Software vblank fallback (also mainly a VM concern)
///
/// Paravirtualized display devices often don't generate real vblank
/// interrupts, so `DrmCompositor`'s normal "wait for the kernel's page-flip
/// completion event" frame pacing can stall indefinitely. If a device's
/// first render_frame/queue_frame cycle doesn't produce a completion event
/// within a short timeout (a couple hundred ms is generous), fall back to a
/// plain `calloop::timer::Timer` firing every ~16ms (60Hz) to drive
/// `render_elements::build_output_elements` + present, instead of waiting on
/// hardware vblank for that CRTC. Anvil doesn't need this (it assumes real
/// hardware); it's specific to what this project needs for VM support.
///
/// ## VT-switching (Ctrl+Alt+Fn) interaction
///
/// The `SessionEvent::PauseSession`/`ActivateSession` handling already
/// wired up in `run_udev` above (real code, not `TODO(hardware)`) is only
/// half the story - it suspends/resumes `libinput`, but the `TODO(hardware)`
/// comments there also mark where each `DrmCompositor`/surface built here
/// needs `.pause()`/`.resume()` calls of its own called from those same two
/// event arms (surfaces must stop issuing KMS commits while the VT isn't
/// ours - the kernel will reject them anyway, but issuing them is still
/// wasted work and can wedge some drivers). On `ActivateSession`, also
/// force a full (non-damage-tracked) redraw of every output once resumed,
/// since whatever was on screen before the VT switch is stale.
#[allow(dead_code)]
fn device_added_todo() {}
