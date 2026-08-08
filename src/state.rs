use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use smithay::desktop::{PopupKind, PopupManager, Space, Window};
use smithay::input::pointer::{CursorImageStatus, PointerHandle};
use smithay::input::{Seat, SeatState};
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::utils::{Clock, Logical, Monotonic, Point, Rectangle};
use smithay::wayland::compositor::CompositorClientState;
use smithay::wayland::compositor::CompositorState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::wlr_layer::WlrLayerShellState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shell::xdg::{ToplevelSurface, XdgShellState};
use smithay::wayland::shm::ShmState;
use smithay::wayland::seat::WaylandFocus;

use crate::wallpaper::Wallpaper;

/// Per-client bookkeeping Smithay requires.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// A window HWDE knows about, on top of what `smithay::desktop::Window`
/// itself tracks. `Space<Window>` has no notion of "minimized" (plain
/// xdg-shell doesn't either - that's purely a window-manager concept), so
/// we track it here: minimizing unmaps the window from `Space` but keeps
/// the `Window` handle around so it can be unmapped-back-in later.
pub struct ManagedWindow {
    pub id: u64,
    pub window: Window,
    pub is_minimized: bool,
    pub is_maximized: bool,
    /// True once the client has negotiated `Mode::ServerSide` via
    /// xdg-decoration - see `handlers/decoration.rs`. Drives whether
    /// `render_elements.rs` draws a grab bar/close button for this window.
    pub is_ssd: bool,
    /// Geometry to restore to when un-maximizing.
    pub saved_geometry: Option<Rectangle<i32, Logical>>,
    /// Which virtual desktop (`0..config.workspace_count`) this window
    /// belongs to - see `HwdeState::switch_workspace`/`move_window_to_workspace`.
    pub workspace: u32,
    /// Location remembered when this window is unmapped purely because its
    /// workspace isn't the active one (as opposed to being minimized), so
    /// switching back restores it exactly where it was.
    pub saved_workspace_location: Option<Point<i32, Logical>>,
}

/// Which screen edge a *pinned* shell surface (an extern-mode shell's
/// panel/dock, registered via `sde-ipc`'s `SdeCall::PinSurface`) is
/// anchored to. Mirrors `sde_ipc::PinnedEdge` (this crate doesn't depend
/// on `sde-ipc` for the plain `hwde-ipc`/native-mode build path, so it's
/// a small local copy rather than a re-export - see `extern_ipc.rs` for
/// the conversion at the one boundary that needs it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinnedEdge {
    Top,
    Bottom,
}

/// Top-level compositor state, analogous to `AnvilState` in Smithay's
/// reference compositor but scoped to what HWDE needs.
pub struct HwdeState {
    pub display_handle: DisplayHandle,
    pub handle: LoopHandle<'static, HwdeState>,
    pub running: Arc<AtomicBool>,
    pub start_time: std::time::Instant,
    pub clock: Clock<Monotonic>,

    // Desktop / window management
    pub space: Space<Window>,
    pub popups: PopupManager,
    pub windows: Vec<ManagedWindow>,
    pub next_window_id: u64,

    // Wayland protocol state
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub xdg_decoration_state: XdgDecorationState,
    pub layer_shell_state: WlrLayerShellState,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: smithay::wayland::selection::primary_selection::PrimarySelectionState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<HwdeState>,
    pub seat: Seat<HwdeState>,
    pub pointer: PointerHandle<HwdeState>,
    pub cursor_status: CursorImageStatus,
    pub dnd_icon: Option<WlSurface>,

    // HWDE-specific
    pub wallpaper: Wallpaper,
    pub pending_wallpaper_reload: bool,
    pub socket_name: Option<String>,
    /// `~/.config/HWDE/compositor.toml` - keybindings, workspace count,
    /// gaps. See `config.rs`. Re-read on `IpcRequest::ReloadConfig`.
    pub config: crate::config::CompositorConfig,
    /// Currently visible virtual desktop, in `0..config.workspace_count`.
    pub active_workspace: u32,
    /// Id of whichever window last received focus via `focus_window_by_id`,
    /// so `focus_next_window` (the `focus_next` keybinding action) has a
    /// starting point to advance from without needing to reverse-lookup the
    /// keyboard's focused surface.
    pub focused_window: Option<u64>,
    /// Workspaces with master-stack tiling turned on - see
    /// `apply_tiling_layout`/`toggle_tiling`. Absent from this set means
    /// "floating" (today's default, unchanged behaviour), so existing
    /// workspaces don't suddenly start rearranging windows.
    pub tiling_enabled: std::collections::HashSet<u32>,
    /// Windows individually excluded from master-stack tiling even though
    /// their workspace has tiling turned on - v0.2 addition, see
    /// `toggle_floating_by_id`/the `toggle_floating` keybinding action
    /// (default `super+shift+f`) and `IpcRequest::ToggleFloatingWindow`.
    /// A window in this set keeps whatever geometry it had when it was
    /// floated (dialogs, picture-in-picture players, etc. that shouldn't
    /// get forced into the tiling grid) while every other window on the
    /// same workspace continues to tile normally.
    pub floating_windows: std::collections::HashSet<u64>,

    /// `app_id -> (edge, thickness_px)` for shell surfaces registered via
    /// `SdeCall::PinSurface` (extern mode only; empty and unused in native
    /// HWDE mode) - e.g. SDE's top panel and bottom dock. A window whose
    /// `app_id` is a key here is pinned to that screen edge instead of
    /// being placed/tiled like a normal window - see `place_new_window`
    /// and `pin_surface`.
    pub pinned_surfaces: std::collections::HashMap<String, (PinnedEdge, u32)>,
    /// `None` in native HWDE mode, `Some("sde")` (etc) in extern mode -
    /// see `main.rs`'s `ExternMode`. Threaded through to `config.rs` so a
    /// `ReloadConfig` request (from either `ipc.rs` or `extern_ipc.rs`, or
    /// the `reload_config` keybinding action) re-reads the *right*
    /// `compositor.toml`.
    pub extern_name: Option<String>,

    #[cfg(feature = "xwayland")]
    pub xwm: Option<smithay::xwayland::X11Wm>,
    #[cfg(feature = "xwayland")]
    pub xdisplay: Option<u32>,
    #[cfg(feature = "xwayland")]
    pub xwayland_shell_state: smithay::wayland::xwayland_shell::XWaylandShellState,

    /// One entry per GPU that `backend_drm.rs`'s udev/DRM backend has
    /// opened (almost always exactly one - the primary GPU - on typical
    /// single-GPU laptops/desktops). Empty and unused when running under
    /// `winit_backend.rs` instead. Lives on `HwdeState` itself (rather than
    /// e.g. being threaded alongside it through the event loop) so it's
    /// reachable as an ordinary field from *any* calloop callback that
    /// already gets `&mut HwdeState` - the udev/libinput/per-device DRM
    /// event sources all do. See `backend_drm.rs`'s module doc for why
    /// rendering has to reach it via disjoint field-destructuring rather
    /// than through `render_elements::RenderInputs`'s normal `&HwdeState`
    /// path.
    #[cfg(feature = "drm-experimental")]
    pub drm_gpus: std::collections::HashMap<smithay::backend::drm::DrmNode, crate::backend_drm::GpuState>,
}

impl HwdeState {
    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        let (window, loc) = self.space.element_under(pos)?;
        let (surface, surface_loc) =
            window.surface_under(pos - loc.to_f64(), smithay::desktop::WindowSurfaceType::ALL)?;
        Some((surface, surface_loc.to_f64() + loc.to_f64()))
    }

    /// Places a newly-mapped window in a simple cascading layout, mirroring
    /// the SolidJS shell's own `openWindow` cascade so behavior feels
    /// consistent whether an app is a Wayland/X11 client or an in-shell app,
    /// and registers it in `windows` with a fresh id.
    pub fn place_new_window(&mut self, window: &Window, activate: bool) -> u64 {
        let id = self.next_window_id;
        self.next_window_id += 1;

        let app_id = window.toplevel().map(with_app_id).unwrap_or_default();
        if let Some(&(edge, thickness)) = self.pinned_surfaces.get(&app_id) {
            // A registered shell surface (panel/dock): pinned to its edge,
            // full output width, never cascaded and never tiled - see
            // `pin_geometry`/`pin_surface`.
            let geo = self.pin_geometry(edge, thickness);
            request_size(window, geo.size);
            self.space.map_element(window.clone(), geo.loc, activate);
            self.windows.push(ManagedWindow {
                id,
                window: window.clone(),
                is_minimized: false,
                is_maximized: false,
                is_ssd: false,
                saved_geometry: None,
                workspace: self.active_workspace,
                saved_workspace_location: None,
            });
            self.floating_windows.insert(id);
            self.space.raise_element(window, true);
            return id;
        }

        let count = self.space.elements().count() as i32;
        let loc = (80 + count * 24, 60 + count * 24);
        self.space.map_element(window.clone(), loc, activate);

        self.windows.push(ManagedWindow {
            id,
            window: window.clone(),
            is_minimized: false,
            is_maximized: false,
            is_ssd: false,
            saved_geometry: None,
            workspace: self.active_workspace,
            saved_workspace_location: None,
        });
        self.floating_windows.remove(&id); // ids are never reused, but keep this invariant explicit
        self.apply_tiling_layout();
        id
    }

    /// Computes the pinned rectangle (full output width, `thickness_px`
    /// tall, flush against `edge`) for a registered shell surface. Public
    /// (crate-visible) so `extern_ipc.rs` can call it too when it needs to
    /// reposition a surface that was already mapped *before*
    /// `SdeCall::PinSurface` registered it - see `pin_surface`.
    pub(crate) fn pin_geometry(&self, edge: PinnedEdge, thickness: u32) -> Rectangle<i32, Logical> {
        let output = self.primary_output_geometry();
        let thickness = thickness as i32;
        match edge {
            PinnedEdge::Top => Rectangle::new(output.loc, (output.size.w, thickness).into()),
            PinnedEdge::Bottom => Rectangle::new(
                (output.loc.x, output.loc.y + output.size.h - thickness).into(),
                (output.size.w, thickness).into(),
            ),
        }
    }

    /// Registers (or updates) `app_id` as pinned to `edge` at `thickness_px`
    /// - see `SdeCall::PinSurface`. Repositions any already-mapped window
    /// with that `app_id` immediately (rather than waiting for it to be
    /// remapped), so call order between "launch the panel" and "pin the
    /// panel" doesn't matter to SDE's startup sequencing.
    pub fn pin_surface(&mut self, app_id: String, edge: PinnedEdge, thickness: u32) {
        self.pinned_surfaces.insert(app_id.clone(), (edge, thickness));

        let matches: Vec<(u64, Window)> = self
            .windows
            .iter()
            .filter(|w| w.window.toplevel().map(with_app_id).unwrap_or_default() == app_id)
            .map(|w| (w.id, w.window.clone()))
            .collect();
        for (id, window) in matches {
            let geo = self.pin_geometry(edge, thickness);
            request_size(&window, geo.size);
            self.space.map_element(window.clone(), geo.loc, false);
            self.floating_windows.insert(id);
            self.space.raise_element(&window, true);
        }
        self.apply_tiling_layout();
    }

    /// Hides every window on the current workspace and shows every
    /// non-minimized window on `target`, remembering/restoring each
    /// window's position across the switch. A no-op if `target` is already
    /// active or out of range.
    ///
    /// This is deliberately implemented in terms of the same
    /// map_element/unmap_elem calls `minimize_window_by_id` already uses -
    /// from `Space`'s point of view a window hidden because its workspace
    /// isn't active looks identical to one that's individually minimized;
    /// the two states just have different owners (this method vs. the user
    /// clicking minimize) and are tracked separately so they don't clobber
    /// each other when a window is minimized on a background workspace.
    pub fn switch_workspace(&mut self, target: u32) {
        let workspace_count = self.config.workspace_count.max(1);
        if target >= workspace_count || target == self.active_workspace {
            return;
        }

        for managed in self.windows.iter_mut() {
            if managed.workspace == self.active_workspace {
                managed.saved_workspace_location = self.space.element_location(&managed.window);
            }
        }
        let to_hide: Vec<Window> = self
            .windows
            .iter()
            .filter(|w| w.workspace == self.active_workspace)
            .map(|w| w.window.clone())
            .collect();
        for window in &to_hide {
            self.space.unmap_elem(window);
        }

        let to_show: Vec<(Window, Point<i32, Logical>)> = self
            .windows
            .iter()
            .filter(|w| w.workspace == target && !w.is_minimized)
            .map(|w| (w.window.clone(), w.saved_workspace_location.unwrap_or_else(|| (80, 60).into())))
            .collect();
        for (window, loc) in to_show {
            self.space.map_element(window, loc, false);
        }

        self.active_workspace = target;
        tracing::info!("switched to workspace {target}/{workspace_count}");
        self.apply_tiling_layout();
    }

    /// Reassigns a window to a different workspace, hiding/showing it
    /// immediately if that crosses the active/inactive boundary.
    pub fn move_window_to_workspace(&mut self, id: u64, target: u32) {
        let workspace_count = self.config.workspace_count.max(1);
        let target = target.min(workspace_count - 1);
        let Some(previous_workspace) = self.windows.iter().find(|w| w.id == id).map(|w| w.workspace) else {
            return;
        };
        if previous_workspace == target {
            return;
        }
        let was_active = previous_workspace == self.active_workspace;
        let is_now_active = target == self.active_workspace;
        let (window, is_minimized) = {
            let Some(managed) = self.find_managed_mut(id) else { return };
            managed.workspace = target;
            (managed.window.clone(), managed.is_minimized)
        };

        if was_active && !is_now_active {
            let loc = self.space.element_location(&window);
            if let Some(managed) = self.find_managed_mut(id) {
                managed.saved_workspace_location = loc;
            }
            self.space.unmap_elem(&window);
        } else if !was_active && is_now_active && !is_minimized {
            let loc = self
                .windows
                .iter()
                .find(|w| w.id == id)
                .and_then(|w| w.saved_workspace_location)
                .unwrap_or_else(|| (80, 60).into());
            self.space.map_element(window, loc, true);
        }
        self.apply_tiling_layout();
    }

    /// Answers `IpcRequest::ListWorkspaces`.
    pub fn workspace_summaries(&self) -> Vec<hwde_ipc::WorkspaceSummary> {
        let workspace_count = self.config.workspace_count.max(1);
        (0..workspace_count)
            .map(|id| hwde_ipc::WorkspaceSummary {
                id,
                is_active: id == self.active_workspace,
                window_count: self.windows.iter().filter(|w| w.workspace == id).count() as u32,
                is_tiling: self.tiling_enabled.contains(&id),
            })
            .collect()
    }

    /// Handler for the hardcoded `Ctrl+Alt+Shift+Escape` combo - see
    /// `config.rs::is_emergency_reset`'s doc comment for the full design
    /// rationale. Deliberately does *not* touch any shell-replacement
    /// plugin state itself (the compositor has no visibility into that -
    /// it lives entirely in `starthwde`); it only pushes a fire-and-forget
    /// notification and logs the attempt either way, so the person
    /// pressing the combo gets compositor-log confirmation it registered
    /// even if the shell-side reset silently no-ops (e.g. nothing was
    /// active to reset).
    pub fn trigger_emergency_reset(&mut self) {
        tracing::warn!("emergency reset combo pressed - notifying starthwde");
        match hwde_ipc::send_emergency_reset(std::time::Duration::from_millis(500)) {
            Ok(()) => tracing::info!("emergency reset: notified starthwde"),
            Err(err) => tracing::warn!("emergency reset: could not reach starthwde ({err}) - is it running?"),
        }
    }

    /// Executes a `config.rs::Action` resolved from a matched keybinding -
    /// the single place keyboard shortcuts (`input.rs`) and (in principle)
    /// any future on-screen-shortcut UI funnel into.
    pub fn run_action(&mut self, action: crate::config::Action) {
        use crate::config::Action;
        match action {
            Action::Spawn(command) => {
                tracing::info!("keybinding: spawn {command}");
                let mut cmd = std::process::Command::new(&command);
                if let Some(name) = &self.socket_name {
                    cmd.env("WAYLAND_DISPLAY", name);
                }
                #[cfg(feature = "xwayland")]
                if let Some(display) = self.xdisplay {
                    cmd.env("DISPLAY", format!(":{display}"));
                }
                if let Err(err) = cmd.spawn() {
                    tracing::warn!("keybinding spawn '{command}' failed: {err}");
                }
            }
            Action::CloseWindow => {
                if let Some(id) = self.focused_window {
                    self.close_window_by_id(id);
                }
            }
            Action::ToggleMaximize => {
                if let Some(id) = self.focused_window {
                    let is_maximized = self.windows.iter().find(|w| w.id == id).map(|w| w.is_maximized).unwrap_or(false);
                    let geo = self.primary_output_geometry();
                    self.maximize_window_by_id(id, !is_maximized, geo);
                }
            }
            Action::ToggleTiling => self.toggle_tiling(),
            Action::ToggleFloating => {
                if let Some(id) = self.focused_window {
                    self.toggle_floating_by_id(id);
                }
            }
            Action::SwapMaster => self.swap_with_master(),
            Action::AdjustMaster(delta) => self.adjust_master_ratio(delta),
            Action::FocusNext => self.focus_next_window(),
            Action::FocusPrev => self.focus_prev_window(),
            Action::Quit => {
                tracing::info!("keybinding: quit comphwde");
                self.running.store(false, std::sync::atomic::Ordering::SeqCst);
            }
            Action::ReloadConfig => {
                tracing::info!("keybinding: reloading compositor.toml");
                self.config = crate::config::load_for(self.extern_name.as_deref());
            }
            Action::SwitchWorkspace(n) => self.switch_workspace(n.saturating_sub(1)),
            Action::MoveToWorkspace(n) => {
                if let Some(id) = self.focused_window {
                    self.move_window_to_workspace(id, n.saturating_sub(1));
                }
            }
        }
    }

    /// Focuses whichever mapped window on the active workspace comes after
    /// the currently focused one (wrapping around) - backs the
    /// `focus_next` keybinding action.
    pub fn focus_next_window(&mut self) {
        let ids: Vec<u64> = self
            .windows
            .iter()
            .filter(|w| w.workspace == self.active_workspace && !w.is_minimized)
            .map(|w| w.id)
            .collect();
        if ids.is_empty() {
            return;
        }
        let next_id = match self.focused_window.and_then(|id| ids.iter().position(|&i| i == id)) {
            Some(pos) => ids[(pos + 1) % ids.len()],
            None => ids[0],
        };
        self.focus_window_by_id(next_id);
    }

    /// Same as [`focus_next_window`](Self::focus_next_window) but steps
    /// backward through the active workspace's window list (wrapping
    /// around) - backs the `focus_prev` keybinding action, default
    /// `super+shift+Tab`. v0.2 addition alongside `focus_next` so
    /// alt-tab-style cycling can go both directions.
    pub fn focus_prev_window(&mut self) {
        let ids: Vec<u64> = self
            .windows
            .iter()
            .filter(|w| w.workspace == self.active_workspace && !w.is_minimized)
            .map(|w| w.id)
            .collect();
        if ids.is_empty() {
            return;
        }
        let prev_id = match self.focused_window.and_then(|id| ids.iter().position(|&i| i == id)) {
            Some(pos) => ids[(pos + ids.len() - 1) % ids.len()],
            None => *ids.last().unwrap(),
        };
        self.focus_window_by_id(prev_id);
    }

    /// Toggles whether `id` is individually excluded from master-stack
    /// tiling (v0.2 addition - see `floating_windows` on `HwdeState`).
    /// Floating a window out of an otherwise-tiled workspace leaves it at
    /// its current geometry and re-lays-out the remaining tiled windows to
    /// fill the space it would have occupied; un-floating it hands it back
    /// to the tiler on the next layout pass.
    pub fn toggle_floating_by_id(&mut self, id: u64) {
        if !self.floating_windows.remove(&id) {
            self.floating_windows.insert(id);
        }
        tracing::info!("window {id}: floating {}", self.floating_windows.contains(&id));
        self.apply_tiling_layout();
    }

    /// Sets (rather than toggles) tiling for an arbitrary workspace,
    /// re-laying-out immediately if it's the active one - backs
    /// `IpcRequest::SetTiling`, the explicit version of `toggle_tiling`
    /// used by the Ustawienia → Przestrzenie robocze switch (which needs
    /// to set a specific on/off state, not just flip whatever the current
    /// one is).
    pub fn set_tiling(&mut self, workspace: u32, enabled: bool) {
        if enabled {
            self.tiling_enabled.insert(workspace);
        } else {
            self.tiling_enabled.remove(&workspace);
        }
        if workspace == self.active_workspace {
            self.apply_tiling_layout();
        }
    }

    /// Swaps the focused window with whichever window currently occupies
    /// the master slot (index 0 among tileable windows on the active
    /// workspace) - the `swap_master` keybinding action, default
    /// `super+m`. A no-op if nothing's focused or the focused window
    /// already *is* the master. Implemented as a swap in `self.windows`'
    /// own ordering (the same ordering `apply_tiling_layout` walks to
    /// assign master/stack slots), so this is a real, persistent
    /// reordering - not just a one-off geometry swap that a subsequent
    /// re-tile would undo.
    pub fn swap_with_master(&mut self) {
        let Some(focused_id) = self.focused_window else { return };
        let master_id = self
            .windows
            .iter()
            .find(|w| {
                w.workspace == self.active_workspace
                    && !w.is_minimized
                    && !w.is_maximized
                    && !self.floating_windows.contains(&w.id)
            })
            .map(|w| w.id);
        let Some(master_id) = master_id else { return };
        if master_id == focused_id {
            return;
        }
        let master_pos = self.windows.iter().position(|w| w.id == master_id);
        let focused_pos = self.windows.iter().position(|w| w.id == focused_id);
        if let (Some(a), Some(b)) = (master_pos, focused_pos) {
            self.windows.swap(a, b);
        }
        self.apply_tiling_layout();
    }

    /// Grows/shrinks the master column by `delta` (the
    /// `increase_master`/`decrease_master` keybinding actions, default
    /// `super+l`/`super+h`), clamped to a sane `0.1..=0.9` range so
    /// neither column can be squeezed to nothing.
    pub fn adjust_master_ratio(&mut self, delta: f32) {
        self.config.master_ratio = (self.config.master_ratio + delta).clamp(0.1, 0.9);
        self.apply_tiling_layout();
    }

    /// Turns master-stack tiling on/off for the active workspace (the
    /// `toggle_tiling` keybinding action, default `super+t`) and
    /// immediately re-lays-out if it was just turned on. Turning it off
    /// leaves windows exactly where tiling last put them - "floating" just
    /// means "stop actively managing positions", not "restore whatever
    /// they were before".
    pub fn toggle_tiling(&mut self) {
        if !self.tiling_enabled.remove(&self.active_workspace) {
            self.tiling_enabled.insert(self.active_workspace);
        }
        self.apply_tiling_layout();
        tracing::info!(
            "workspace {}: tiling {}",
            self.active_workspace,
            if self.tiling_enabled.contains(&self.active_workspace) { "on" } else { "off" }
        );
    }

    /// Re-lays-out every eligible window (mapped, not minimized, not
    /// maximized) on the active workspace using a simple master-stack
    /// algorithm - one "master" window filling `master_ratio` of the
    /// width on the left, everything else split evenly in a vertical
    /// stack on the right (the same layout dwm/tinywm-style tiling WMs use
    /// by default). A no-op if the active workspace doesn't have tiling
    /// enabled (see `toggle_tiling`) or has nothing to tile.
    ///
    /// Deliberately called from every place a workspace's window set can
    /// change shape - `place_new_window`, `forget_window_by_surface`,
    /// `minimize_window_by_id`, `focus_window_by_id` (covers
    /// unminimize), `switch_workspace`, `move_window_to_workspace` - so
    /// the layout self-heals without needing a separate "recompute"
    /// keybinding, the same way tiling WMs generally behave.
    pub fn apply_tiling_layout(&mut self) {
        if !self.tiling_enabled.contains(&self.active_workspace) {
            return;
        }

        let output_geo = self.primary_output_geometry();
        let gap = self.config.gaps.max(0);

        let ids: Vec<u64> = self
            .windows
            .iter()
            .filter(|w| {
                w.workspace == self.active_workspace
                    && !w.is_minimized
                    && !w.is_maximized
                    && !self.floating_windows.contains(&w.id)
            })
            .map(|w| w.id)
            .collect();
        let count = ids.len();
        if count == 0 {
            return;
        }

        // Single master window (index 0) takes the left ~55% (configurable
        // via `config.master_ratio`); everything else stacks vertically on
        // the right. With only one window total, the "master" just fills
        // the whole (gapped) output.
        let master_width =
            if count > 1 { ((output_geo.size.w as f32) * self.config.master_ratio) as i32 } else { output_geo.size.w };

        for (index, id) in ids.iter().enumerate() {
            let rect = if index == 0 {
                let half_gap = if count > 1 { gap / 2 } else { 0 };
                Rectangle::new(
                    (output_geo.loc.x + gap, output_geo.loc.y + gap).into(),
                    ((master_width - gap - half_gap).max(1), (output_geo.size.h - gap * 2).max(1)).into(),
                )
            } else {
                let stack_count = (count - 1) as i32;
                let stack_index = (index - 1) as i32;
                let stack_x = output_geo.loc.x + master_width + gap / 2;
                let stack_w = (output_geo.size.w - master_width - gap - gap / 2).max(1);
                let slot_h = ((output_geo.size.h - gap * (stack_count + 1)) / stack_count).max(1);
                let y = output_geo.loc.y + gap + stack_index * (slot_h + gap);
                Rectangle::new((stack_x, y).into(), (stack_w, slot_h).into())
            };
            self.apply_tile_geometry(*id, rect);
        }
    }

    /// Resizes+moves a single window to `rect` - the shared primitive
    /// behind `apply_tiling_layout`, following the same
    /// `with_pending_state`/`send_pending_configure`/`map_element` pattern
    /// `maximize_window_by_id` uses for the Wayland/X11 split.
    fn apply_tile_geometry(&mut self, id: u64, rect: Rectangle<i32, Logical>) {
        let Some(window) = self.windows.iter().find(|w| w.id == id).map(|w| w.window.clone()) else { return };
        match window.underlying_surface() {
            smithay::desktop::WindowSurface::Wayland(toplevel) => {
                toplevel.with_pending_state(|state| {
                    state.size = Some(rect.size);
                });
                toplevel.send_pending_configure();
            }
            #[cfg(feature = "xwayland")]
            smithay::desktop::WindowSurface::X11(x11) => {
                let _ = x11.configure(Some(rect));
            }
        }
        self.space.map_element(window, rect.loc, false);
    }

    /// Drops a destroyed window from both `Space` and our own bookkeeping.
    pub fn forget_window_by_surface(&mut self, surface: &WlSurface) {
        if let Some(pos) = self.windows.iter().position(|w| w.window.wl_surface().as_deref() == Some(surface)) {
            let managed = self.windows.remove(pos);
            self.space.unmap_elem(&managed.window);
            self.floating_windows.remove(&managed.id);
        }
        self.apply_tiling_layout();
    }

    /// Geometry of the first (in the winit backend: only) output - used to
    /// compute the target size/position when maximizing a window.
    pub fn primary_output_geometry(&self) -> Rectangle<i32, Logical> {
        self.space
            .outputs()
            .next()
            .and_then(|o| self.space.output_geometry(o))
            .unwrap_or_else(|| Rectangle::new((0, 0).into(), (1920, 1080).into()))
    }

    pub fn window_id_for_surface(&self, surface: &WlSurface) -> Option<u64> {
        self.windows.iter().find(|w| w.window.wl_surface().as_deref() == Some(surface)).map(|w| w.id)
    }

    /// Records whether `surface`'s window negotiated server-side
    /// decoration - see `handlers/decoration.rs`.
    pub fn set_ssd_by_surface(&mut self, surface: &WlSurface, is_ssd: bool) {
        if let Some(managed) = self.windows.iter_mut().find(|w| w.window.wl_surface().as_deref() == Some(surface)) {
            managed.is_ssd = is_ssd;
        }
    }

    fn find_managed_mut(&mut self, id: u64) -> Option<&mut ManagedWindow> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    pub fn focus_window_by_id(&mut self, id: u64) {
        let Some(managed) = self.find_managed_mut(id) else { return };
        managed.is_minimized = false;
        let window = managed.window.clone();
        self.focused_window = Some(id);
        let loc = self.space.element_location(&window).unwrap_or((0, 0).into());
        self.space.map_element(window.clone(), loc, true);
        self.space.raise_element(&window, true);

        if let Some(surface) = window.wl_surface() {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            if let Some(keyboard) = self.seat.get_keyboard() {
                keyboard.set_focus(self, Some(surface.into_owned()), serial);
            }
        }
        self.apply_tiling_layout();
    }

    pub fn close_window_by_id(&mut self, id: u64) {
        let Some(managed) = self.windows.iter().find(|w| w.id == id) else { return };
        match managed.window.underlying_surface() {
            smithay::desktop::WindowSurface::Wayland(toplevel) => toplevel.send_close(),
            #[cfg(feature = "xwayland")]
            smithay::desktop::WindowSurface::X11(x11) => {
                let _ = x11.close();
            }
        }
    }

    pub fn minimize_window_by_id(&mut self, id: u64) {
        let Some(managed) = self.find_managed_mut(id) else { return };
        if managed.is_minimized {
            return;
        }
        managed.is_minimized = true;
        let window = managed.window.clone();
        self.space.unmap_elem(&window);
        self.apply_tiling_layout();
    }

    pub fn unminimize_window_by_id(&mut self, id: u64) {
        self.focus_window_by_id(id);
    }

    pub fn maximize_window_by_id(&mut self, id: u64, maximize: bool, output_geo: Rectangle<i32, Logical>) {
        // NOTE: we deliberately re-look-up `managed` via short-lived
        // `find_managed_mut` calls below instead of holding one `&mut
        // ManagedWindow` for the whole function. The old version kept that
        // mutable borrow alive across `self.space.element_location(...)`,
        // which needs its own `&self` borrow at the same time - that's a
        // mutable/immutable borrow conflict (E0502), not something a
        // lifetime tweak can fix; the borrows just can't overlap.
        let Some(window) = self.find_managed_mut(id).map(|managed| {
            managed.is_minimized = false;
            managed.window.clone()
        }) else {
            return;
        };

        let default_restore_geo = || Rectangle::new((80, 60).into(), (800, 600).into());

        if maximize {
            let current = self.space.element_location(&window).unwrap_or((0, 0).into());
            let current_size = window.geometry().size;
            if let Some(managed) = self.find_managed_mut(id) {
                managed.saved_geometry = Some(Rectangle::new(current, current_size));
                managed.is_maximized = true;
            }

            match window.underlying_surface() {
                smithay::desktop::WindowSurface::Wayland(toplevel) => {
                    toplevel.with_pending_state(|state| {
                        state.states.set(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized);
                        state.size = Some(output_geo.size);
                    });
                    toplevel.send_pending_configure();
                }
                #[cfg(feature = "xwayland")]
                smithay::desktop::WindowSurface::X11(x11) => {
                    let _ = x11.set_maximized(true);
                    let _ = x11.configure(Some(output_geo));
                }
            }
            self.space.map_element(window, output_geo.loc, true);
        } else {
            let restore = self
                .find_managed_mut(id)
                .map(|managed| {
                    managed.is_maximized = false;
                    managed.saved_geometry.take().unwrap_or_else(default_restore_geo)
                })
                .unwrap_or_else(default_restore_geo);

            match window.underlying_surface() {
                smithay::desktop::WindowSurface::Wayland(toplevel) => {
                    toplevel.with_pending_state(|state| {
                        state.states.unset(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized);
                        state.size = Some(restore.size);
                    });
                    toplevel.send_pending_configure();
                }
                #[cfg(feature = "xwayland")]
                smithay::desktop::WindowSurface::X11(x11) => {
                    let _ = x11.set_maximized(false);
                    let _ = x11.configure(Some(restore));
                }
            }
            self.space.map_element(window, restore.loc, true);
        }
    }

    /// Dismisses any open popup whose geometry does *not* contain
    /// `click_pos` - our (simplified, not using smithay's generic
    /// `PopupGrab` machinery, which would require a custom pointer-focus
    /// enum type) answer to "dismiss popups on outside click".
    pub fn dismiss_popups_outside(&mut self, click_pos: Point<f64, Logical>) {
        let windows: Vec<Window> = self.space.elements().cloned().collect();
        for window in windows {
            let Some(surface) = window.wl_surface() else { continue };
            let win_loc = self.space.element_location(&window).unwrap_or_default();
            for (popup, popup_offset) in PopupManager::popups_for_surface(&surface) {
                let popup_geo = popup.geometry();
                let abs_rect = Rectangle::new(win_loc + popup_offset + popup_geo.loc, popup_geo.size);
                if !abs_rect.contains(click_pos.to_i32_round()) {
                    if let PopupKind::Xdg(xdg) = popup {
                        xdg.send_popup_done();
                    }
                }
            }
        }
    }

    /// Answers `IpcRequest::ListWindows` - every window HWDE currently
    /// knows about, in-shell taskbar-compatible shape.
    pub fn window_summaries(&self) -> Vec<hwde_ipc::WindowSummary> {
        self.windows
            .iter()
            .map(|managed| hwde_ipc::WindowSummary {
                id: managed.id,
                title: managed.window.toplevel().map(with_title).unwrap_or_default(),
                app_id: managed.window.toplevel().map(with_app_id).unwrap_or_default(),
                is_xwayland: {
                    #[cfg(feature = "xwayland")]
                    {
                        managed.window.is_x11()
                    }
                    #[cfg(not(feature = "xwayland"))]
                    {
                        false
                    }
                },
                is_minimized: managed.is_minimized,
                is_maximized: managed.is_maximized,
                is_floating: self.floating_windows.contains(&managed.id),
            })
            .collect()
    }

    /// `sde-ipc`'s equivalent of [`window_summaries`] - a separate method
    /// (rather than converting the `hwde_ipc::WindowSummary`s above)
    /// because `sde_ipc::SdeWindowInfo` additionally carries `workspace`
    /// and `focused`, which `hwde_ipc::WindowSummary` doesn't have (SDE's
    /// panel/dock show per-window workspace + focus state; HWDE's shell
    /// gets that from `ListWorkspaces`/its own focus tracking instead).
    /// Used only from `extern_ipc.rs`.
    pub fn sde_window_summaries(&self) -> Vec<sde_ipc::SdeWindowInfo> {
        self.windows
            .iter()
            .map(|managed| sde_ipc::SdeWindowInfo {
                id: managed.id,
                title: managed.window.toplevel().map(with_title).unwrap_or_default(),
                app_id: managed.window.toplevel().map(with_app_id).unwrap_or_default(),
                workspace: managed.workspace,
                focused: self.focused_window == Some(managed.id),
                minimized: managed.is_minimized,
                maximized: managed.is_maximized,
                floating: self.floating_windows.contains(&managed.id),
            })
            .collect()
    }

    /// Answers `IpcRequest::ListOutputs` (v0.2 addition) - every output
    /// currently mapped into `self.space`, in the shape the shell's
    /// "Wyświetlacze" settings section needs for a real multi-monitor
    /// picker instead of assuming a single fixed-size output. In the
    /// `winit_backend` (the only backend actually wired into `main` today
    /// - see `backend_drm.rs`'s module doc) there is exactly one, but this
    /// is written against `Space::outputs()` generically so it keeps
    /// working unchanged once `drm-experimental` graduates to real
    /// multi-output hardware support.
    pub fn output_summaries(&self) -> Vec<hwde_ipc::OutputSummary> {
        let primary_name = self.space.outputs().next().map(|o| o.name());
        self.space
            .outputs()
            .map(|output| {
                let geo = self.space.output_geometry(output).unwrap_or_else(|| Rectangle::new((0, 0).into(), (1920, 1080).into()));
                let scale = output.current_scale().fractional_scale();
                let refresh_mhz = output.current_mode().map(|m| m.refresh).unwrap_or(0);
                let name = output.name();
                hwde_ipc::OutputSummary {
                    is_primary: primary_name.as_deref() == Some(name.as_str()),
                    name,
                    x: geo.loc.x,
                    y: geo.loc.y,
                    width: geo.size.w,
                    height: geo.size.h,
                    scale,
                    refresh_mhz,
                }
            })
            .collect()
    }
}

pub(crate) fn with_title(t: &ToplevelSurface) -> String {
    smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
        states
            .data_map
            .get::<std::sync::Mutex<smithay::wayland::shell::xdg::XdgToplevelSurfaceRoleAttributes>>()
            .map(|d| d.lock().unwrap().title.clone().unwrap_or_default())
            .unwrap_or_default()
    })
}

pub(crate) fn with_app_id(t: &ToplevelSurface) -> String {
    smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
        states
            .data_map
            .get::<std::sync::Mutex<smithay::wayland::shell::xdg::XdgToplevelSurfaceRoleAttributes>>()
            .map(|d| d.lock().unwrap().app_id.clone().unwrap_or_default())
            .unwrap_or_default()
    })
}

/// Sends a configure requesting `size` for `window`'s toplevel, if it has
/// one (X11/XWayland windows are sized differently and never get pinned in
/// practice - see `place_new_window` - so the no-op fallback there is
/// fine). Used to size pinned shell surfaces (panel/dock) to their full
/// pinned rectangle rather than whatever size the client itself requested.
fn request_size(window: &Window, size: smithay::utils::Size<i32, Logical>) {
    if let smithay::desktop::WindowSurface::Wayland(toplevel) = window.underlying_surface() {
        toplevel.with_pending_state(|state| state.size = Some(size));
        toplevel.send_pending_configure();
    }
}
