mod messages;
mod socket;
mod handler;
mod hackerland_ipc;
// `extern_ipc` (the sde-ipc-based protocol for external SDE apps) is
// deliberately disabled for now, along with the `sde` crate it depends
// on — see `sde`'s removal in this same change and the project's own
// "further work" notes: the sde-ipc protocol surface needs a proper
// design pass, not more IPC code layered on top of the current draft.
// `hackerland_ipc` (comphwde's own HackerLand control protocol, which
// only depends on `hwde-ipc`, not `sde`) is unaffected and still wired
// in below.
// mod extern_ipc;

pub use hackerland_ipc::init as init_hackerland_ipc;

// ── Shared peer-credential + process-spawning helpers ───────────────────
// Used by every one of comphwde's Unix-socket IPC surfaces
// (hackerland_ipc.rs directly; extern_ipc.rs's own `peer_pid_of` covers
// the same `SO_PEERCRED` ground independently since it also wants the
// pid, not just the uid) to make sure every socket only accepts
// connections from the same user who owns the compositor process
// itself — these sockets live under `$XDG_RUNTIME_DIR`, which is
// already 0700 and per-user, but checking the peer's uid too costs
// nothing and removes any reliance on directory permissions alone
// being correct on every system this runs on.

/// This process's own real uid, for comparing against a connecting
/// peer's uid (see [`peer_uid`]).
pub fn our_uid() -> libc::uid_t {
    unsafe { libc::getuid() }
}

/// `getsockopt(fd, SOL_SOCKET, SO_PEERCRED, ...)`'s uid field for a
/// `UnixStream`'s peer. `None` on any failure (unsupported platform,
/// already-closed socket, ...) — every caller treats that the same as
/// "reject the connection," never as "assume it's fine."
pub fn peer_uid(stream: &std::os::unix::net::UnixStream) -> Option<libc::uid_t> {
    use std::os::unix::io::AsRawFd;

    let fd = stream.as_raw_fd();
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;

    // SAFETY: identical justification to extern_ipc.rs's `peer_pid_of`,
    // which this mirrors — `fd` is valid for this call's duration,
    // `&mut cred` is a validly-sized/aligned `libc::ucred`, and `len` is
    // pre-set to that struct's exact size as `getsockopt` requires.
    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };

    if ret == 0 { Some(cred.uid) } else { None }
}

/// Spawns `command` with `args` as a detached child process, for
/// `SdeCall::LaunchApp`/HackerLand's `dispatch launch`. Stdio is left
/// attached to the compositor's own (matching how every other
/// `Command::spawn` call in this codebase — e.g. XWayland's own launch —
/// already handles it: a misbehaving launched app's stray stdout/stderr
/// ending up in the compositor's own log is an acceptable, debuggable
/// outcome, versus silently discarding output that might explain why an
/// app failed to start).
pub fn spawn_external_app(
    _state: &mut crate::state::BlueState,
    command: &str,
    args: &[String],
) -> std::io::Result<()> {
    std::process::Command::new(command).args(args).spawn()?;
    Ok(())
}

pub use messages::CompositorMessage;
#[allow(unused_imports)]
pub use messages::ShellMessage;
#[allow(unused_imports)]
pub use messages::GpuInfo;

pub use socket::{init_ipc, broadcast};
#[allow(unused_imports)]
pub use socket::{
    ipc_socket_path, Clients,
    broadcast_workspace_switch, broadcast_start_menu_toggle,
    broadcast_window_opened, broadcast_window_closed, broadcast_idle_changed,
    broadcast_screen_locked,
};
#[allow(unused_imports)]
pub use handler::handle_shell_message;
