use std::time::{Duration, Instant};

pub mod protocol;

pub use protocol::{
    PinnedEdge, SdeCall, SdeEvent, SdeEventMessage, SdeIpcError, SdeOutcome, SdeOutputInfo,
    SdeRequest, SdeResponse, SdeResult, SdeWindowInfo, SdeWorkspaceInfo, Subscription,
};

/// The `--extern-<n>` name Penetration Mode's session always uses. Not
/// expected to ever be anything else - kept as a named constant (rather
/// than a literal scattered across call sites) purely for readability.
/// Note: `penetration-mode-session` itself also has a throwaway
/// `--extern-other` mode for its bare-tty single-app fallback (see that
/// repository's `session/src/main.rs` module doc) - that path
/// deliberately does NOT go through this constant, since it must never
/// collide with a real curated `--extern-penetration-mode` session.
pub const EXTERN_NAME: &str = "penetration-mode";

/// The `app_id`/`identifier` Penetration Mode's own shell window is
/// expected to report over `ListWindows` - `tauri.conf.json`'s
/// `identifier` (`com.redteam.penetrationmode`). Used by
/// [`enter_wrapper`]/[`exit_wrapper`] to find "our own" window among
/// hackeros-comp's `ListWindows` result without the caller having to
/// know/pass the id.
pub const SHELL_APP_ID: &str = "com.redteam.penetrationmode";

/// Default timeout for a single request/response round-trip. Generous
/// relative to [`is_running`]'s own 300ms liveness-check default because
/// `LaunchApp` (used both to start the shell itself and to launch tools
/// from the BlackArch store) can legitimately take a bit longer than a
/// pure state query, especially the first time a security tool starts
/// inside the `podman` container.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum PenetrationModeIpcError {
    #[error(transparent)]
    Sde(#[from] SdeIpcError),
    #[error("nie znaleziono własnego okna powłoki Penetration Mode (app_id={SHELL_APP_ID}) w ListWindows")]
    ShellWindowNotFound,
    #[error("nie wykryto nowego okna w ciągu {0:?} od uruchomienia procesu")]
    NewWindowTimedOut(Duration),
}

/// Path to the control socket for `hackeros-comp --extern-penetration-mode`
/// - shorthand for `protocol::socket_path_for(EXTERN_NAME)`.
pub fn socket_path() -> std::path::PathBuf {
    protocol::socket_path_for(EXTERN_NAME)
}

/// Sends one request to `hackeros-comp --extern-penetration-mode` and
/// waits for its response - shorthand for `protocol::call(EXTERN_NAME, ...)`.
pub fn call(request: SdeCall, timeout: Duration) -> Result<SdeResult, SdeIpcError> {
    protocol::call(EXTERN_NAME, request, timeout)
}

/// True if a `hackeros-comp --extern-penetration-mode` compositor is
/// currently reachable.
pub fn is_running() -> bool {
    protocol::is_running(EXTERN_NAME)
}

/// Opens a live event stream (window/workspace changes) from
/// `hackeros-comp --extern-penetration-mode` - shorthand for
/// `protocol::subscribe(EXTERN_NAME)`.
pub fn subscribe() -> Result<Subscription, SdeIpcError> {
    protocol::subscribe(EXTERN_NAME)
}

/// Asks hackeros-comp to end the `--extern-penetration-mode` session
/// cleanly.
pub fn shutdown_compositor(timeout: Duration) -> Result<(), SdeIpcError> {
    call(SdeCall::Shutdown, timeout).map(|_| ())
}

/// Asks hackeros-comp to spawn `command` (with `args`) as a proper
/// Wayland/XWayland client of this `--extern-penetration-mode` session -
/// i.e. with a correctly-set `WAYLAND_DISPLAY` for *this* compositor
/// instance. Intended use: launching a GUI security tool installed from
/// the BlackArch store (`blackarch.rs`'s `install_package`/
/// `exec_in_container` in the Penetration Mode repository) so it renders
/// as a normal window in this session instead of needing its own X
/// display forwarded out of the `podman` container.
pub fn launch_shell(command: &str, args: &[String]) -> Result<(), SdeIpcError> {
    call(SdeCall::LaunchApp { command: command.to_string(), args: args.to_vec() }, DEFAULT_TIMEOUT).map(|_| ())
}

/// Finds Penetration Mode's own shell window (matching [`SHELL_APP_ID`])
/// in the current `ListWindows` result.
pub fn find_shell_window(timeout: Duration) -> Result<Option<SdeWindowInfo>, SdeIpcError> {
    match call(SdeCall::ListWindows, timeout)? {
        SdeResult::Windows(windows) => Ok(windows.into_iter().find(|w| w.app_id == SHELL_APP_ID)),
        _ => Ok(None),
    }
}

/// Enter "wrapper mode": get Penetration Mode's own shell window out of
/// the way right before spawning a security tool, so the newly-launched
/// window has the screen to itself - composed over the two primitives
/// hackeros-comp already exposes ([`find_shell_window`] +
/// `MinimizeWindow`) rather than a dedicated IPC call, same as
/// `hacker-mode-ipc::enter_wrapper`.
pub fn enter_wrapper(timeout: Duration) -> Result<(), PenetrationModeIpcError> {
    let shell = find_shell_window(timeout)?.ok_or(PenetrationModeIpcError::ShellWindowNotFound)?;
    call(SdeCall::MinimizeWindow { id: shell.id }, timeout)?;
    Ok(())
}

/// Leave "wrapper mode": restore and focus Penetration Mode's own shell
/// window.
pub fn exit_wrapper(timeout: Duration) -> Result<(), PenetrationModeIpcError> {
    let shell = find_shell_window(timeout)?.ok_or(PenetrationModeIpcError::ShellWindowNotFound)?;
    call(SdeCall::UnminimizeWindow { id: shell.id }, timeout)?;
    call(SdeCall::FocusWindow { id: shell.id }, timeout)?;
    Ok(())
}

/// Polls `ListWindows` (every `poll_interval`, up to `timeout` total) for
/// a window whose id isn't in `known_ids`, and maximizes the first one it
/// finds - useful right after [`launch_shell`]-ing a security tool from
/// the BlackArch store, so it opens full-screen instead of at whatever
/// default size the tool itself picks. Returns the id of the window it
/// maximized, if any.
pub fn maximize_next_new_window(
    known_ids: &[u64],
    timeout: Duration,
    poll_interval: Duration,
) -> Result<u64, PenetrationModeIpcError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let SdeResult::Windows(windows) = call(SdeCall::ListWindows, DEFAULT_TIMEOUT)? {
            if let Some(w) = windows.into_iter().find(|w| !known_ids.contains(&w.id) && w.app_id != SHELL_APP_ID) {
                call(SdeCall::MaximizeWindow { id: w.id, maximized: true }, DEFAULT_TIMEOUT)?;
                return Ok(w.id);
            }
        }
        if Instant::now() >= deadline {
            return Err(PenetrationModeIpcError::NewWindowTimedOut(timeout));
        }
        std::thread::sleep(poll_interval);
    }
}

/// Snapshot of currently-known window ids - pass the result to
/// [`maximize_next_new_window`] as `known_ids` right before spawning a
/// process, so it can tell "new" windows apart from ones that already
/// existed.
pub fn known_window_ids(timeout: Duration) -> Result<Vec<u64>, SdeIpcError> {
    match call(SdeCall::ListWindows, timeout)? {
        SdeResult::Windows(windows) => Ok(windows.into_iter().map(|w| w.id).collect()),
        _ => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_is_penetration_mode_specific() {
        assert_eq!(socket_path(), protocol::socket_path_for("penetration-mode"));
        assert_ne!(socket_path(), protocol::socket_path_for("sde"));
        assert_ne!(socket_path(), protocol::socket_path_for("hacker-mode"));
    }
}
