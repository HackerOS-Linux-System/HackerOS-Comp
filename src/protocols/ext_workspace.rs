use smithay::reexports::wayland_server::{
    backend::GlobalId,
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use tracing::info;

#[allow(non_upper_case_globals, non_camel_case_types, unused_imports, clippy::all)]
mod generated {
    use smithay::reexports::wayland_server;
    use smithay::reexports::wayland_server::protocol::wl_output;
    pub mod __interfaces {
        use smithay::reexports::wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols-xml/ext-workspace-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_server_code!("protocols-xml/ext-workspace-v1.xml");
}
use generated::ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1};
use generated::ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1};
use generated::ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1};

use crate::state::BlueState;

struct WorkspaceManagerInstance {
    manager: ExtWorkspaceManagerV1,
    group: ExtWorkspaceGroupHandleV1,
    /// Index-aligned with `BlueState::current_workspace` — `workspaces[i]`
    /// is the handle for workspace number `i`.
    workspaces: Vec<ExtWorkspaceHandleV1>,
}

#[derive(Default)]
pub struct ExtWorkspaceState {
    global: Option<GlobalId>,
    instances: Vec<WorkspaceManagerInstance>,
}

/// Same "return, don't assign into `&mut BlueState`" shape as
/// `dmabuf::init_dmabuf`/`color_management::init_color_management`.
pub fn init_ext_workspace(display_handle: &DisplayHandle) -> ExtWorkspaceState {
    let global = display_handle.create_global::<BlueState, ExtWorkspaceManagerV1, _>(1, ());
    info!("ext_workspace_v1 global registered (external panel/dock workspace listing + switching)");
    ExtWorkspaceState { global: Some(global), instances: Vec::new() }
}

/// Per-workspace-handle user data — just which numbered workspace this
/// particular object represents, for this particular client's instance.
#[derive(Clone, Copy)]
pub struct WorkspaceHandleData {
    index: usize,
}

/// Called from `BlueState::switch_workspace` after `current_workspace`
/// has already been updated — sends the real protocol events every
/// bound client needs to see the switch (`state` on the two affected
/// workspace handles, `done` on the manager to close out the atomic
/// update per the protocol's own `done` event semantics).
pub fn notify_workspace_switched(state: &mut BlueState, old_index: usize, new_index: usize) {
    state.ext_workspace_state.instances.retain(|inst| inst.manager.is_alive());
    for inst in &state.ext_workspace_state.instances {
        // `State` (like `GroupCapabilities`/`WorkspaceCapabilities`) is a
        // `bitfield="true"` enum — wayland-scanner generates those as a
        // `bitflags::bitflags!` struct (verified directly against
        // `wayland-scanner` 0.31.9's `common.rs`), not a plain fieldless
        // enum, so `State::empty()`/`State::Active` (bitflags consts),
        // not a raw `0`/`1 as u32` cast.
        if let Some(handle) = inst.workspaces.get(old_index) {
            handle.state(ext_workspace_handle_v1::State::empty()); // this compositor never sets urgent/hidden, so clearing active means clearing everything
        }
        if let Some(handle) = inst.workspaces.get(new_index) {
            handle.state(ext_workspace_handle_v1::State::Active);
        }
        inst.manager.done();
    }
}

impl GlobalDispatch<ExtWorkspaceManagerV1, ()> for BlueState {
    fn bind(
        state: &mut Self,
        handle: &DisplayHandle,
        client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());

        // `workspace_group`/`workspace` are *server*-initiated `new_id`
        // events, not client requests — there's no `New<T>` for them
        // (that only exists when a *client* sends a request carrying a
        // new_id). The correct API for a server-created object is
        // `Client::create_resource` (verified directly against the
        // `wayland-server` 0.31.13 crate source, `client.rs`'s own doc
        // comment: "create a new Wayland object... immediately sent to
        // the client through an associated event with a new_id
        // argument"), then pass a reference to it into the event method
        // — wayland-scanner's server-side codegen treats a `new_id`
        // event arg identically to a plain `object` arg (checked
        // directly against `wayland-scanner` 0.31.9's `server_gen.rs`:
        // `Type::Object | Type::NewId` share one code path, generating
        // `&super::iface_mod::IfaceType`), it does not auto-generate any
        // `New<T>`/`DataInit` plumbing for events the way it does for
        // requests.
        let Ok(group) = client.create_resource::<ExtWorkspaceGroupHandleV1, (), Self>(handle, manager.version(), ()) else { return };
        manager.workspace_group(&group);

        // One group for every output (see module doc — this compositor
        // has no per-output workspace concept to map onto multiple
        // groups).
        group.capabilities(ext_workspace_group_handle_v1::GroupCapabilities::empty());
        for wl_output in state.outputs.iter().flat_map(|o| o.client_outputs(client)) {
            group.output_enter(&wl_output);
        }

        let mut workspaces = Vec::with_capacity(state.workspace_count);
        for i in 0..state.workspace_count {
            let Ok(ws) = client.create_resource::<ExtWorkspaceHandleV1, WorkspaceHandleData, Self>(handle, manager.version(), WorkspaceHandleData { index: i }) else { continue };
            manager.workspace(&ws);
            ws.name((i + 1).to_string());
            let active = if i == state.current_workspace { ext_workspace_handle_v1::State::Active } else { ext_workspace_handle_v1::State::empty() };
            ws.state(active);
            ws.capabilities(ext_workspace_handle_v1::WorkspaceCapabilities::Activate);
            group.workspace_enter(&ws);
            workspaces.push(ws);
        }

        manager.done();
        state.ext_workspace_state.instances.push(WorkspaceManagerInstance { manager, group, workspaces });
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for BlueState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            // This implementation sends every event eagerly as state
            // actually changes rather than batching — so there's
            // nothing to flush on `commit`; it's accepted as a no-op,
            // which is spec-legal (the protocol requires the *client*
            // send it before relying on atomicity, not that the
            // compositor do anything specific in response beyond having
            // already applied changes atomically, which per-switch
            // `notify_workspace_switched` already does by construction —
            // both affected `state` events go out before the closing
            // `done`).
            ext_workspace_manager_v1::Request::Commit => {}
            ext_workspace_manager_v1::Request::Stop => {
                resource.finished();
                let dead_id = resource.id();
                state.ext_workspace_state.instances.retain(|inst| inst.manager.id() != dead_id);
            }
            // `Request` here is a plain (non-`#[non_exhaustive]`) enum
            // generated by wayland-scanner with exactly these two
            // variants, so a trailing `_ => {}` is dead code (rustc
            // flags it as an unreachable-pattern warning) rather than
            // future-proofing against protocol additions.
        }
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for BlueState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ExtWorkspaceGroupHandleV1,
        request: ext_workspace_group_handle_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            // Capabilities advertised as 0 (no create_workspace) already
            // told a well-behaved client not to send this — per spec
            // ("the compositor will ignore requests it doesn't
            // support"), silently no-op rather than a protocol error.
            ext_workspace_group_handle_v1::Request::CreateWorkspace { .. } => {}
            ext_workspace_group_handle_v1::Request::Destroy => {}
            // Exhaustive already — see the matching note in the
            // manager's `Dispatch::request` above.
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, WorkspaceHandleData> for BlueState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        data: &WorkspaceHandleData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_workspace_handle_v1::Request::Activate => {
                state.switch_workspace(data.index);
            }
            // Advertised `capabilities` only ever include `activate`
            // (see module doc for why deactivate/assign/remove don't
            // map onto this compositor's always-exactly-one-active
            // model) — same "ignore what wasn't advertised" handling as
            // the group's `create_workspace` above.
            ext_workspace_handle_v1::Request::Deactivate => {}
            ext_workspace_handle_v1::Request::Assign { .. } => {}
            ext_workspace_handle_v1::Request::Remove => {}
            ext_workspace_handle_v1::Request::Destroy => {}
            // Exhaustive already — see the matching note in the
            // manager's `Dispatch::request` above.
        }
    }
}
