use std::collections::HashMap;

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};
use smithay::reexports::wayland_server::backend::{ClientId, GlobalId};
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use crate::state::HwdeState;

/// Highest interface version comphwde advertises. Version 2 added
/// `set_fullscreen`/`unset_fullscreen` (handled below, approximated as
/// maximize - see the module doc's "further work" note on fullscreen);
/// version 3 added the optional `parent` event, which comphwde simply
/// never sends (valid per-spec - it's only sent "whenever the parent...
/// changes", and comphwde doesn't track toplevel parentage today).
const MANAGER_VERSION: u32 = 3;

/// Snapshot of one window's state as this protocol understands it - see
/// `HwdeState::foreign_toplevel_info`, the one place that builds these
/// from a `ManagedWindow`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToplevelInfo {
    pub title: String,
    pub app_id: String,
    pub minimized: bool,
    pub maximized: bool,
    pub activated: bool,
    pub fullscreen: bool,
}

/// Raw `zwlr_foreign_toplevel_handle_v1.state` enum values (protocol
/// constants - the XML defines this as a plain, non-bitfield `array` of
/// `uint`s, so there's no generated Rust helper to encode it; these
/// mirror `wlr-foreign-toplevel-management-unstable-v1.xml`'s `state`
/// enum verbatim).
const STATE_MAXIMIZED: u32 = 0;
const STATE_MINIMIZED: u32 = 1;
const STATE_ACTIVATED: u32 = 2;
const STATE_FULLSCREEN: u32 = 3;

/// Packs `info`'s boolean flags into the little/native-endian `uint[]`
/// payload the `state` event expects (one 4-byte little-endian... in
/// practice native-endian, matching every other `array`-typed Wayland
/// event in this codebase's dependencies - element per active state).
fn encode_states(info: &ToplevelInfo) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 * 4);
    let mut push = |value: u32| buf.extend_from_slice(&value.to_ne_bytes());
    if info.maximized {
        push(STATE_MAXIMIZED);
    }
    if info.minimized {
        push(STATE_MINIMIZED);
    }
    if info.activated {
        push(STATE_ACTIVATED);
    }
    if info.fullscreen {
        push(STATE_FULLSCREEN);
    }
    buf
}

/// Global data for the `zwlr_foreign_toplevel_manager_v1` global. No
/// per-client filtering today (kept as its own zero-sized type, rather
/// than binding `()` directly, purely so a future access-control policy -
/// e.g. restricting this to SDE's own panel/dock `app_id`s - has an
/// obvious place to add a `filter` field, mirroring
/// `WlrLayerShellGlobalData`/`ForeignToplevelListGlobalData` in Smithay
/// itself).
#[derive(Debug, Default)]
pub struct ForeignToplevelManagerGlobalData;

/// State backing the `wlr-foreign-toplevel-management` global - lives on
/// `HwdeState::foreign_toplevels` (`Some` only for `--extern-sde`; see
/// that field's doc comment).
#[derive(Debug)]
pub struct ForeignToplevelManagerState {
    global: GlobalId,
    /// Every currently-bound `zwlr_foreign_toplevel_manager_v1` resource,
    /// across every connected client (in practice: SDE's panel and dock,
    /// each binding it once).
    managers: Vec<ZwlrForeignToplevelManagerV1>,
    /// `window_id -> one zwlr_foreign_toplevel_handle_v1 resource per
    /// currently-bound manager instance representing it`. A window with
    /// two bound managers (panel + dock) has two entries here.
    toplevels: HashMap<u64, Vec<ZwlrForeignToplevelHandleV1>>,
    /// Last-broadcast [`ToplevelInfo`] per window id, compared against on
    /// every diff tick (see `HwdeState::sync_foreign_toplevel_diffs`) to
    /// decide what changed. Also doubles as "is this window id currently
    /// known to this protocol at all" for `toplevel_changed`.
    last_known: HashMap<u64, ToplevelInfo>,
}

impl ForeignToplevelManagerState {
    /// Registers the `zwlr_foreign_toplevel_manager_v1` global. Callers
    /// (`winit_backend.rs`/`backend_drm.rs`) only do this for
    /// `--extern-sde` - see `HwdeState::foreign_toplevels`'s doc comment.
    pub fn new(display: &DisplayHandle) -> Self {
        let global = display.create_global::<HwdeState, ZwlrForeignToplevelManagerV1, ForeignToplevelManagerGlobalData>(
            MANAGER_VERSION,
            ForeignToplevelManagerGlobalData,
        );
        Self { global, managers: Vec::new(), toplevels: HashMap::new(), last_known: HashMap::new() }
    }

    /// The registered global's id, in case a caller ever needs to
    /// `remove_global` it (not currently done anywhere - `--extern-sde`
    /// sessions are one-per-process, same as every other mode, so the
    /// global simply lives for the process's whole lifetime).
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }

    /// Creates and sends a fresh `zwlr_foreign_toplevel_handle_v1` for
    /// `window_id` to every currently-bound manager instance, and records
    /// `info` as the new `last_known` baseline. Called once, right after a
    /// window is mapped (see `HwdeState::sync_foreign_toplevel_created`).
    pub fn toplevel_created(&mut self, dh: &DisplayHandle, output: Option<&Output>, window_id: u64, info: &ToplevelInfo) {
        self.last_known.insert(window_id, info.clone());
        // Cloning the manager list itself (cheap - `Resource` clones are
        // just a reference-counted handle to the same wire object) rather
        // than iterating `&self.managers` directly, so `announce_to_one`
        // below is free to take `&mut self` without a borrow conflict.
        for manager in self.managers.clone() {
            self.announce_to_one(dh, &manager, output, window_id, info);
        }
    }

    /// Diffs `info` against the last-broadcast snapshot for `window_id`
    /// and sends only the events that actually changed (plus a trailing
    /// `done`) to every handle currently representing it - a no-op if
    /// nothing changed, or if `window_id` isn't a window this protocol
    /// currently knows about (e.g. a diff tick that raced a
    /// not-yet-processed `toplevel_created`/`toplevel_closed`). See
    /// `HwdeState::sync_foreign_toplevel_diffs`, the only caller.
    pub fn toplevel_changed(&mut self, window_id: u64, info: &ToplevelInfo) {
        let (changed_title, changed_app_id, changed_state) = match self.last_known.get(&window_id) {
            None => return,
            Some(prev) if prev == info => return,
            Some(prev) => (
                prev.title != info.title,
                prev.app_id != info.app_id,
                (prev.minimized, prev.maximized, prev.activated, prev.fullscreen)
                    != (info.minimized, info.maximized, info.activated, info.fullscreen),
            ),
        };
        self.last_known.insert(window_id, info.clone());

        let Some(handles) = self.toplevels.get(&window_id) else { return };
        for handle in handles {
            if changed_title {
                handle.title(info.title.clone());
            }
            if changed_app_id {
                handle.app_id(info.app_id.clone());
            }
            if changed_state {
                handle.state(encode_states(info));
            }
            handle.done();
        }
    }

    /// Sends `closed` to every handle representing `window_id` and drops
    /// all bookkeeping for it. Called once, right after a window is
    /// unmapped for good (see `HwdeState::sync_foreign_toplevel_closed`).
    /// Harmless (and a no-op) if `window_id` was never announced in the
    /// first place - e.g. a window that was created and destroyed between
    /// two diff ticks without any manager ever having been bound yet.
    pub fn toplevel_closed(&mut self, window_id: u64) {
        self.last_known.remove(&window_id);
        if let Some(handles) = self.toplevels.remove(&window_id) {
            for handle in handles {
                handle.closed();
            }
        }
    }

    /// Creates one `zwlr_foreign_toplevel_handle_v1` for `window_id` on
    /// `manager` specifically, sends its full initial state
    /// (title/app_id/output_enter/state/done, per the protocol's "All
    /// initial details... will be sent immediately after [the toplevel
    /// event]"), and records the new handle in `toplevels`. Shared by
    /// [`Self::toplevel_created`] (existing manager, new window) and
    /// `GlobalDispatch::bind` below (new manager, existing window) - the
    /// two moments this protocol requires a fresh handle to be
    /// constructed and fully synced.
    fn announce_to_one(
        &mut self,
        dh: &DisplayHandle,
        manager: &ZwlrForeignToplevelManagerV1,
        output: Option<&Output>,
        window_id: u64,
        info: &ToplevelInfo,
    ) {
        let Ok(client) = dh.get_client(manager.id()) else { return };
        let Ok(handle) = client.create_resource::<ZwlrForeignToplevelHandleV1, u64, HwdeState>(dh, manager.version(), window_id)
        else {
            return;
        };

        manager.toplevel(&handle);
        handle.title(info.title.clone());
        handle.app_id(info.app_id.clone());
        if let Some(output) = output {
            // See the module doc's "Output tracking" note: sent once at
            // creation time only, never followed by `output_leave` - fine
            // as long as every backend has exactly one output, which is
            // the case for both backends this project ships today.
            for wl_output in output.client_outputs(&client) {
                handle.output_enter(&wl_output);
            }
        }
        handle.state(encode_states(info));
        handle.done();

        self.toplevels.entry(window_id).or_default().push(handle);
    }
}

impl GlobalDispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelManagerGlobalData, HwdeState> for ForeignToplevelManagerState {
    fn bind(
        state: &mut HwdeState,
        dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        _global_data: &ForeignToplevelManagerGlobalData,
        data_init: &mut DataInit<'_, HwdeState>,
    ) {
        let manager = data_init.init(resource, ());

        // Full sync: every window HWDE currently knows about is announced
        // to this newly-bound manager instance right away, per this
        // protocol's contract ("After a client binds the
        // zwlr_foreign_toplevel_manager_v1, each opened toplevel window
        // will be sent via the toplevel event") - mirrors Smithay's own
        // `ext_foreign_toplevel_list_v1` bind-time replay
        // (`foreign_toplevel_list::handlers::bind`).
        let primary_output = state.space.outputs().next().cloned();
        let snapshot: Vec<(u64, ToplevelInfo)> = state.windows.iter().map(|w| (w.id, state.foreign_toplevel_info(w))).collect();

        let Some(fts) = state.foreign_toplevels.as_mut() else {
            // Can't happen in practice: this global is only ever created
            // when `foreign_toplevels` is already `Some` (see
            // `ForeignToplevelManagerState::new`'s callers) - fail safe
            // rather than panic if it somehow does anyway.
            return;
        };
        fts.managers.push(manager.clone());
        for (id, info) in snapshot {
            fts.announce_to_one(dh, &manager, primary_output.as_ref(), id, &info);
            fts.last_known.insert(id, info);
        }
    }

    fn can_view(_client: Client, _global_data: &ForeignToplevelManagerGlobalData) -> bool {
        true
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, (), HwdeState> for ForeignToplevelManagerState {
    fn request(
        _state: &mut HwdeState,
        _client: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, HwdeState>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request {
            // Per protocol: the compositor may still emit further
            // `toplevel` events after `stop`, until it sends `finished`.
            // comphwde doesn't queue any, so this can go straight to
            // `finished` - there's nothing pending to flush first.
            manager.finished();
        }
    }

    fn destroyed(state: &mut HwdeState, _client_id: ClientId, manager: &ZwlrForeignToplevelManagerV1, _data: &()) {
        if let Some(fts) = state.foreign_toplevels.as_mut() {
            fts.managers.retain(|m| m != manager);
        }
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, u64, HwdeState> for ForeignToplevelManagerState {
    fn request(
        state: &mut HwdeState,
        _client: &Client,
        _handle: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        data: &u64,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, HwdeState>,
    ) {
        let window_id = *data;
        match request {
            zwlr_foreign_toplevel_handle_v1::Request::SetMaximized => {
                let geo = state.primary_output_geometry();
                state.maximize_window_by_id(window_id, true, geo);
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMaximized => {
                let geo = state.primary_output_geometry();
                state.maximize_window_by_id(window_id, false, geo);
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetMinimized => {
                state.minimize_window_by_id(window_id);
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMinimized => {
                state.unminimize_window_by_id(window_id);
            }
            zwlr_foreign_toplevel_handle_v1::Request::Activate { .. } => {
                // Single-seat compositor (see `ipc.rs`/`hackerland_ipc.rs`'s
                // identical treatment of "the" seat elsewhere in this
                // codebase) - which `wl_seat` was passed doesn't change
                // which one comphwde actually has.
                state.focus_window_by_id(window_id);
            }
            zwlr_foreign_toplevel_handle_v1::Request::Close => {
                state.close_window_by_id(window_id);
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetFullscreen { .. } => {
                // No fullscreen concept elsewhere in comphwde yet (see
                // this module's doc comment / the project's "further
                // work" notes) - maximizing is the closest approximation
                // a wlr-foreign-toplevel-management dock/taskbar can get
                // today.
                let geo = state.primary_output_geometry();
                state.maximize_window_by_id(window_id, true, geo);
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetFullscreen => {
                let geo = state.primary_output_geometry();
                state.maximize_window_by_id(window_id, false, geo);
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetRectangle { .. } => {
                // Purely an optional hint the client is never required to
                // set (see the protocol doc) - comphwde doesn't currently
                // use it for anything (e.g. minimize-to-taskbar-icon
                // animations), so it's accepted and discarded rather than
                // stored. Candidate for the "further work" list once
                // minimize animations exist.
            }
            zwlr_foreign_toplevel_handle_v1::Request::Destroy => {
                // Handled by the destructor (`destroyed` below).
            }
            _ => {}
        }
    }

    fn destroyed(state: &mut HwdeState, _client_id: ClientId, handle: &ZwlrForeignToplevelHandleV1, data: &u64) {
        if let Some(fts) = state.foreign_toplevels.as_mut() {
            if let Some(handles) = fts.toplevels.get_mut(data) {
                handles.retain(|h| h != handle);
            }
        }
    }
}

smithay::reexports::wayland_server::delegate_global_dispatch!(HwdeState: [
    ZwlrForeignToplevelManagerV1: ForeignToplevelManagerGlobalData
] => ForeignToplevelManagerState);
smithay::reexports::wayland_server::delegate_dispatch!(HwdeState: [
    ZwlrForeignToplevelManagerV1: ()
] => ForeignToplevelManagerState);
smithay::reexports::wayland_server::delegate_dispatch!(HwdeState: [
    ZwlrForeignToplevelHandleV1: u64
] => ForeignToplevelManagerState);
