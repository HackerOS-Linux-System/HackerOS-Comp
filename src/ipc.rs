use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};

use hwde_ipc::{IpcRequest, IpcResponse};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};

use crate::state::HwdeState;

pub fn init(handle: &LoopHandle<'static, HwdeState>) -> std::io::Result<()> {
    let socket_dir = hwde_ipc::runtime_dir();
    std::fs::create_dir_all(&socket_dir)?;
    // Don't rely on umask: the `$XDG_RUNTIME_DIR/hwde` case is fine either
    // way (the parent is already 0700, owned by us), but the `/tmp/hwde-<uid>`
    // fallback (used outside a full login session, e.g. `starthwde dev`)
    // lives inside world-traversable `/tmp` - a permissive umask would leave
    // *any* local user able to enter this directory and open the socket
    // below, from which they could issue `LaunchApp`/`Shutdown`/etc. as this
    // session's user. Force it closed regardless of umask.
    std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700))?;

    let socket_path = hwde_ipc::socket_path();
    // Stale socket from a previous crashed run - safe to remove, a fresh
    // bind will fail otherwise.
    let _ = std::fs::remove_file(&socket_path);

    let listener = UnixListener::bind(&socket_path)?;
    // Same reasoning as the directory above: force 0600 rather than trust
    // umask, so only our own UID can even open() the socket.
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    tracing::info!("comphwde IPC listening on {}", socket_path.display());

    let source = Generic::new(listener, Interest::READ, Mode::Level);
    handle
        .insert_source(source, |_, listener, state| {
            loop {
                match listener.accept() {
                    Ok((stream, _addr)) => {
                        if let Some(peer_uid) = peer_uid(&stream) {
                            if peer_uid != our_uid() {
                                tracing::warn!(
                                    "hwde-ipc: rejected connection from uid {peer_uid} (we are uid {})",
                                    our_uid()
                                );
                                continue;
                            }
                        } else {
                            // Couldn't determine the peer's credentials (e.g.
                            // non-Linux, or the kernel call failed) - fail
                            // closed rather than silently trusting the
                            // connection, since filesystem permissions alone
                            // aren't a substitute for this check (a
                            // misconfigured umask on the directory/socket
                            // above would otherwise be the only thing
                            // standing between "any local user" and this
                            // session).
                            tracing::warn!("hwde-ipc: rejected connection with unknown peer credentials");
                            continue;
                        }
                        handle_connection(stream, state)
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        tracing::warn!("hwde-ipc accept error: {e}");
                        break;
                    }
                }
            }
            Ok(PostAction::Continue)
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    Ok(())
}

/// Our own UID, for comparing against connecting clients' via
/// [`peer_uid`]. Uses a raw `getuid()` FFI call (same tiny-shim approach
/// `hwde-ipc::runtime_dir` already uses) rather than pulling in a `libc`
/// dependency just for this.
#[cfg(unix)]
pub(crate) fn our_uid() -> u32 {
    unsafe {
        extern "C" {
            fn getuid() -> u32;
        }
        getuid()
    }
}

#[cfg(not(unix))]
pub(crate) fn our_uid() -> u32 {
    0
}

/// The UID of the process on the other end of `stream`, via `SO_PEERCRED`
/// (Linux-specific; this project targets Linux only - see udev/libseat/DRM
/// usage elsewhere). Returns `None` if that can't be determined, which
/// callers must treat as "reject", not "assume trusted".
#[cfg(target_os = "linux")]
pub(crate) fn peer_uid(stream: &UnixStream) -> Option<u32> {
    #[repr(C)]
    struct Ucred {
        pid: i32,
        uid: u32,
        gid: u32,
    }
    const SOL_SOCKET: i32 = 1;
    const SO_PEERCRED: i32 = 17;

    unsafe {
        extern "C" {
            fn getsockopt(sockfd: i32, level: i32, optname: i32, optval: *mut std::ffi::c_void, optlen: *mut u32) -> i32;
        }
        let mut cred = Ucred { pid: 0, uid: 0, gid: 0 };
        let mut len = std::mem::size_of::<Ucred>() as u32;
        let ret = getsockopt(
            stream.as_raw_fd(),
            SOL_SOCKET,
            SO_PEERCRED,
            &mut cred as *mut Ucred as *mut std::ffi::c_void,
            &mut len,
        );
        if ret == 0 {
            Some(cred.uid)
        } else {
            None
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn peer_uid(_stream: &UnixStream) -> Option<u32> {
    None
}

fn handle_connection(stream: UnixStream, state: &mut HwdeState) {
    let _ = stream.set_nonblocking(false);
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!("hwde-ipc: failed to clone stream: {err}");
            return;
        }
    });
    let mut writer = stream;

    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return; // client disconnected without sending anything
    }

    let response = match serde_json::from_str::<IpcRequest>(line.trim()) {
        Ok(req) => dispatch(req, state),
        Err(err) => IpcResponse::Error(format!("malformed request: {err}")),
    };

    if let Ok(mut out) = serde_json::to_string(&response) {
        out.push('\n');
        if let Err(err) = writer.write_all(out.as_bytes()) {
            tracing::warn!("hwde-ipc: failed to write response: {err}");
        }
    }
}

fn dispatch(req: IpcRequest, state: &mut HwdeState) -> IpcResponse {
    match req {
        IpcRequest::Ping => IpcResponse::Pong,

        IpcRequest::LaunchApp { command, args } => match spawn_external_app(state, &command, &args) {
            Ok(()) => IpcResponse::Ok,
            Err(err) => IpcResponse::Error(err.to_string()),
        },

        IpcRequest::SetWallpaper { path } => {
            state.wallpaper.set_path(path);
            // Actual GPU re-upload happens on the next redraw, where the
            // renderer is available; see winit_backend's render loop.
            state.pending_wallpaper_reload = true;
            IpcResponse::Ok
        }

        IpcRequest::ListWindows => IpcResponse::Windows(state.window_summaries()),

        IpcRequest::FocusWindow { id } => {
            state.focus_window_by_id(id);
            IpcResponse::Ok
        }
        IpcRequest::CloseWindow { id } => {
            state.close_window_by_id(id);
            IpcResponse::Ok
        }
        IpcRequest::MinimizeWindow { id } => {
            state.minimize_window_by_id(id);
            IpcResponse::Ok
        }
        IpcRequest::UnminimizeWindow { id } => {
            state.unminimize_window_by_id(id);
            IpcResponse::Ok
        }
        IpcRequest::MaximizeWindow { id, maximized } => {
            let geo = state.primary_output_geometry();
            state.maximize_window_by_id(id, maximized, geo);
            IpcResponse::Ok
        }
        IpcRequest::ToggleFloatingWindow { id } => {
            state.toggle_floating_by_id(id);
            IpcResponse::Ok
        }

        IpcRequest::ListWorkspaces => IpcResponse::Workspaces(state.workspace_summaries()),

        IpcRequest::SwitchWorkspace { id } => {
            state.switch_workspace(id);
            IpcResponse::Ok
        }

        IpcRequest::MoveWindowToWorkspace { id, workspace } => {
            state.move_window_to_workspace(id, workspace);
            IpcResponse::Ok
        }

        IpcRequest::SetTiling { workspace, enabled } => {
            state.set_tiling(workspace, enabled);
            IpcResponse::Ok
        }

        IpcRequest::ReloadConfig => {
            state.config = crate::config::load_for(state.extern_name.as_deref());
            tracing::info!("compositor.toml reloaded via hwde-ipc");
            IpcResponse::Ok
        }

        IpcRequest::ListOutputs => IpcResponse::Outputs(state.output_summaries()),

        IpcRequest::Shutdown => {
            tracing::info!("shutdown requested via hwde-ipc");
            state.running.store(false, std::sync::atomic::Ordering::SeqCst);
            IpcResponse::Ok
        }
    }
}

/// Spawns `command` as a child process configured to connect back to this
/// compositor's Wayland socket (and, if XWayland is up, its X display) -
/// this is the "external application" integration between starthwde and
/// comphwde requested for this milestone.
// pub(crate): also called from `extern_ipc.rs` (SdeCall::LaunchApp) - same
// spawn logic serves both protocols, only the request/response shapes
// wrapping it differ.
pub(crate) fn spawn_external_app(state: &HwdeState, command: &str, args: &[String]) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(command);
    cmd.args(args);

    if let Some(name) = &state.socket_name {
        cmd.env("WAYLAND_DISPLAY", name);
    }
    #[cfg(feature = "xwayland")]
    if let Some(display) = state.xdisplay {
        cmd.env("DISPLAY", format!(":{display}"));
    }
    // comphwde doesn't render its own cursor theme (see project README);
    // propagating XCURSOR_THEME (set on comphwde itself by starthwde at
    // spawn time - see session.rs) at least gets any toolkit that reads
    // this env var directly (GTK, Qt, plain Xlib/X11 apps) to use the
    // configured theme for their own cursor rendering.
    if let Ok(cursor_theme) = std::env::var("XCURSOR_THEME") {
        cmd.env("XCURSOR_THEME", cursor_theme);
    }

    tracing::info!("launching external app: {command} {args:?}");
    cmd.spawn()?;
    Ok(())
}
