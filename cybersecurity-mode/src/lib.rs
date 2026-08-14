use std::time::{Duration, Instant};

pub use sde_ipc::{
    PinnedEdge, SdeCall, SdeEvent, SdeEventMessage, SdeIpcError, SdeOutcome, SdeOutputInfo,
    SdeRequest, SdeResponse, SdeResult, SdeWindowInfo, SdeWorkspaceInfo, Subscription,
};

/// The `--extern-<n>` name Cybersecurity Mode's session is expected to
/// always use, once it has one - see this crate's module doc. Kept as a
/// named constant for the same readability reasons as
/// `hacker_mode::EXTERN_NAME`.
pub const EXTERN_NAME: &str = "cybersecurity-mode";

/// The `app_id`/`identifier` Cybersecurity Mode's own shell window would
/// report over `ListWindows` - `tauri.conf.json`'s `identifier`
/// (`com.redteam.cybersecuritymode`, see
/// `source-code/backend/tauri.conf.json` in the `cybersec-mode`
/// repository). Used by [`enter_wrapper`]/[`exit_wrapper`] to find "our
/// own" window among comphwde's `ListWindows` result without the caller
/// having to know/pass the id - same pattern as `hacker_mode::SHELL_APP_ID`.
pub const SHELL_APP_ID: &str = "com.redteam.cybersecuritymode";

/// Default timeout for a single request/response round-trip - identical
/// to `hacker_mode::DEFAULT_TIMEOUT`; `LaunchApp` (used both to start the
/// shell itself and to launch tools from the BlackArch store) can
/// legitimately take a bit longer than a pure state query, especially the
/// first time a security tool starts inside the `podman` container.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum CybersecurityModeIpcError {
    #[error(transparent)]
    Sde(#[from] SdeIpcError),
    #[error("nie znaleziono własnego okna powłoki Cybersecurity Mode (app_id={SHELL_APP_ID}) w ListWindows")]
    ShellWindowNotFound,
    #[error("nie wykryto nowego okna w ciągu {0:?} od uruchomienia procesu")]
    NewWindowTimedOut(Duration),
}

/// Path to the control socket for `comphwde --extern-cybersecurity-mode` -
/// shorthand for `sde_ipc::socket_path_for(EXTERN_NAME)`.
pub fn socket_path() -> std::path::PathBuf {
    sde_ipc::socket_path_for(EXTERN_NAME)
}

/// Sends one request to `comphwde --extern-cybersecurity-mode` and waits
/// for its response - shorthand for `sde_ipc::call(EXTERN_NAME, ...)`.
pub fn call(request: SdeCall, timeout: Duration) -> Result<SdeResult, SdeIpcError> {
    sde_ipc::call(EXTERN_NAME, request, timeout)
}

/// True if a `comphwde --extern-cybersecurity-mode` compositor is
/// currently reachable.
pub fn is_running() -> bool {
    sde_ipc::is_running(EXTERN_NAME)
}

/// Opens a live event stream (window/workspace changes) from
/// `comphwde --extern-cybersecurity-mode` - shorthand for
/// `sde_ipc::subscribe(EXTERN_NAME)`.
pub fn subscribe() -> Result<Subscription, SdeIpcError> {
    sde_ipc::subscribe(EXTERN_NAME)
}

/// Asks comphwde to end the `--extern-cybersecurity-mode` session cleanly.
pub fn shutdown_compositor(timeout: Duration) -> Result<(), SdeIpcError> {
    call(SdeCall::Shutdown, timeout).map(|_| ())
}

/// Asks comphwde to spawn `command` (with `args`) as a proper Wayland/
/// XWayland client of this `--extern-cybersecurity-mode` session - i.e.
/// with a correctly-set `WAYLAND_DISPLAY` for *this* compositor instance.
/// Intended use once wired up: launching a GUI security tool installed
/// from the BlackArch store (`blackarch.rs`'s `install_package`/
/// `exec_in_container` in the `cybersec-mode` repository) so it renders
/// as a normal window in this session instead of needing its own X
/// display forwarded out of the `podman` container.
pub fn launch_shell(command: &str, args: &[String]) -> Result<(), SdeIpcError> {
    call(SdeCall::LaunchApp { command: command.to_string(), args: args.to_vec() }, DEFAULT_TIMEOUT).map(|_| ())
}

/// Finds Cybersecurity Mode's own shell window (matching [`SHELL_APP_ID`])
/// in the current `ListWindows` result.
pub fn find_shell_window(timeout: Duration) -> Result<Option<SdeWindowInfo>, SdeIpcError> {
    match call(SdeCall::ListWindows, timeout)? {
        SdeResult::Windows(windows) => Ok(windows.into_iter().find(|w| w.app_id == SHELL_APP_ID)),
        _ => Ok(None),
    }
}

/// Enter "wrapper mode": get Cybersecurity Mode's own shell window out of
/// the way right before spawning a security tool, so the newly-launched
/// window has the screen to itself - same composition as
/// `hacker_mode::enter_wrapper`, over the two primitives comphwde already
/// exposes ([`find_shell_window`] + `MinimizeWindow`) rather than a
/// dedicated IPC call.
pub fn enter_wrapper(timeout: Duration) -> Result<(), CybersecurityModeIpcError> {
    let shell = find_shell_window(timeout)?.ok_or(CybersecurityModeIpcError::ShellWindowNotFound)?;
    call(SdeCall::MinimizeWindow { id: shell.id }, timeout)?;
    Ok(())
}

/// Leave "wrapper mode": restore and focus Cybersecurity Mode's own shell
/// window.
pub fn exit_wrapper(timeout: Duration) -> Result<(), CybersecurityModeIpcError> {
    let shell = find_shell_window(timeout)?.ok_or(CybersecurityModeIpcError::ShellWindowNotFound)?;
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
) -> Result<u64, CybersecurityModeIpcError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let SdeResult::Windows(windows) = call(SdeCall::ListWindows, DEFAULT_TIMEOUT)? {
            if let Some(w) = windows.into_iter().find(|w| !known_ids.contains(&w.id) && w.app_id != SHELL_APP_ID) {
                call(SdeCall::MaximizeWindow { id: w.id, maximized: true }, DEFAULT_TIMEOUT)?;
                return Ok(w.id);
            }
        }
        if Instant::now() >= deadline {
            return Err(CybersecurityModeIpcError::NewWindowTimedOut(timeout));
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
    fn socket_path_is_cybersecurity_mode_specific() {
        assert_eq!(socket_path(), sde_ipc::socket_path_for("cybersecurity-mode"));
        assert_ne!(socket_path(), sde_ipc::socket_path_for("sde"));
        assert_ne!(socket_path(), sde_ipc::socket_path_for("hacker-mode"));
    }
}
