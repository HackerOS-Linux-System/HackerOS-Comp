use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Requests `starthwde` (or any trusted local client) may send to `comphwde`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum IpcRequest {
    /// Liveness check.
    Ping,
    /// Ask the compositor to spawn an "external application" (an arbitrary
    /// binary) as a Wayland or XWayland client of this HWDE session.
    LaunchApp { command: String, args: Vec<String> },
    /// Change the desktop wallpaper at runtime.
    SetWallpaper { path: String },
    /// List currently mapped toplevel surfaces - this is what lets
    /// starthwde's taskbar show "external" (non-shell) windows exactly like
    /// its own in-shell ones.
    ListWindows,
    /// Bring a window (by the id from `ListWindows`) to the front and give
    /// it keyboard focus.
    FocusWindow { id: u64 },
    /// Politely ask a client's window to close (xdg-shell `close` /
    /// XCB `WM_DELETE_WINDOW` as appropriate).
    CloseWindow { id: u64 },
    /// Hide a window without closing it (comphwde tracks this as compositor
    /// state since plain xdg-shell has no "minimize" request of its own).
    MinimizeWindow { id: u64 },
    /// Unhide a previously minimized window.
    UnminimizeWindow { id: u64 },
    /// Toggle/set a window to fill the output.
    MaximizeWindow { id: u64, maximized: bool },
    /// Individually exclude/re-include a window from its workspace's
    /// master-stack tiling, independent of the workspace-wide
    /// `SetTiling` switch - v0.2 addition backing the "Odepnij od
    /// kafelkowania" window-context-menu entry and the `toggle_floating`
    /// keybinding action (default `super+shift+f`). See
    /// `HwdeState::toggle_floating_by_id` on the compositor side.
    ToggleFloatingWindow { id: u64 },
    /// List virtual desktops (id, whether active, window count) - backs
    /// the shell's workspace switcher (Big Picture mode and the top bar).
    ListWorkspaces,
    /// Switch the active virtual desktop.
    SwitchWorkspace { id: u32 },
    /// Move a window (by id from `ListWindows`) to a different workspace.
    MoveWindowToWorkspace { id: u64, workspace: u32 },
    /// Turns master-stack tiling on/off for a specific workspace - what
    /// the Ustawienia → Przestrzenie robocze toggle sends (as opposed to
    /// the `toggle_tiling` keybinding action, which always targets
    /// whichever workspace is currently active).
    SetTiling { workspace: u32, enabled: bool },
    /// Re-read `~/.config/HWDE/compositor.toml` (keybindings, workspace
    /// count, gaps) without restarting comphwde - sent after the shell's
    /// keybindings/workspace settings sections save changes.
    ReloadConfig,
    /// List every output (monitor) currently mapped by the compositor -
    /// v0.2 addition backing a real "Wyświetlacze" settings panel instead
    /// of assuming a single fixed display. See `HwdeState::output_summaries`.
    ListOutputs,
    /// Ask the compositor to end the session cleanly.
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowSummary {
    pub id: u64,
    pub title: String,
    pub app_id: String,
    pub is_xwayland: bool,
    pub is_minimized: bool,
    pub is_maximized: bool,
    /// True if this window is individually excluded from its workspace's
    /// master-stack tiling (v0.2 addition) - see
    /// `IpcRequest::ToggleFloatingWindow`.
    pub is_floating: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSummary {
    /// 0-indexed on the wire; shells typically display `id + 1`.
    pub id: u32,
    pub is_active: bool,
    pub window_count: u32,
    /// Whether master-stack tiling is turned on for this workspace - see
    /// `src/state.rs::apply_tiling_layout`.
    pub is_tiling: bool,
}

/// One compositor output (monitor) - v0.2 addition, see
/// `HwdeState::output_summaries` / `IpcRequest::ListOutputs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputSummary {
    pub name: String,
    /// Logical position/size (already scaled), matching what
    /// `Space::output_geometry` reports and what window placement is
    /// computed against.
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: f64,
    /// Refresh rate in mHz (milli-hertz), Wayland's native unit for this -
    /// divide by 1000 for a human-readable Hz value.
    pub refresh_mhz: i32,
    /// True for the output new windows/maximize target by default (today:
    /// simply the first output `Space::outputs()` yields).
    pub is_primary: bool,
}

/// Responses `comphwde` sends back for a given [`IpcRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum IpcResponse {
    Pong,
    Ok,
    Windows(Vec<WindowSummary>),
    Workspaces(Vec<WorkspaceSummary>),
    Outputs(Vec<OutputSummary>),
    Error(String),
}

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("comphwde socket not found at {0} (is the compositor running?)")]
    NotFound(PathBuf),
    #[error("io error talking to comphwde: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to (de)serialize IPC message: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("comphwde returned an error: {0}")]
    Remote(String),
}

/// Resolves the runtime directory HWDE uses for sockets/state, honoring
/// `XDG_RUNTIME_DIR` and falling back to `/tmp/hwde-<uid>` when unset
/// (e.g. during `starthwde dev` outside of a full login session).
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("hwde");
    }
    let uid = unsafe { libc_getuid() };
    PathBuf::from(format!("/tmp/hwde-{uid}"))
}

/// Path to the comphwde control socket (distinct from the Wayland display
/// socket itself, which Smithay/wayland-server manages separately), for a
/// native HWDE session.
pub fn socket_path() -> PathBuf {
    socket_path_for(None)
}

/// Normalizes an "extern" desktop-environment name (see [`socket_path_for`])
/// to a lowercase, ascii-alphanumeric-and-dash slug, so a caller can't
/// smuggle path separators or mixed casing into a socket/config path.
fn normalize_extern_name(name: &str) -> String {
    name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').map(|c| c.to_ascii_lowercase()).collect()
}

/// Socket path for `comphwde` running either as HWDE's own compositor
/// (`extern_name = None`) or, via `comphwde --extern-<name>`, as the
/// Wayland/XWayland compositor *for a different desktop environment* that
/// reuses comphwde instead of implementing its own from scratch (e.g.
/// `extern_name = Some("swde")` for SWDE) - see `comphwde`'s `main.rs` and
/// SWDE's `swde-ipc` crate, which is a thin wrapper around this one.
///
/// Every extern target gets its own socket (`comphwde-<name>.sock`, next
/// to plain HWDE's `comphwde.sock`) so a native HWDE session and one or
/// more extern sessions never collide, even hypothetically sharing a
/// runtime dir.
pub fn socket_path_for(extern_name: Option<&str>) -> PathBuf {
    match extern_name {
        Some(name) => runtime_dir().join(format!("comphwde-{}.sock", normalize_extern_name(name))),
        None => runtime_dir().join("comphwde.sock"),
    }
}

/// Path to the one-way "something's wrong, reset the shell" push channel -
/// **not** used for normal shell→compositor calls (those go through
/// [`socket_path`]/[`send_request`]). This is the other direction:
/// comphwde → starthwde, fired only when the hardcoded
/// `Ctrl+Alt+Shift+Escape` emergency combo is pressed (see
/// `src/config.rs`'s `EMERGENCY_RESET_KEYBINDING` doc comment).
/// Kept as a dead-simple separate socket rather than extending the
/// request/response protocol, specifically so it has no shared code path
/// with anything a plugin or the normal keybinding config can influence.
pub fn emergency_socket_path() -> PathBuf {
    runtime_dir().join("hwde-emergency-reset.sock")
}

/// Fire-and-forget: connects to [`emergency_socket_path`] and writes a
/// single line, then disconnects. Used exclusively by the compositor's
/// hardcoded emergency-reset keybinding. Errors (most commonly: the shell
/// isn't running, or isn't listening yet) are intentionally swallowed by
/// the caller via `.ok()` - this is a best-effort nudge, not something the
/// compositor should ever block or panic on.
pub fn send_emergency_reset(timeout: Duration) -> Result<(), IpcError> {
    let path = emergency_socket_path();
    if !path.exists() {
        return Err(IpcError::NotFound(path));
    }
    let mut stream = UnixStream::connect(&path)?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(b"RESET\n")?;
    Ok(())
}

/// Binds [`emergency_socket_path`] and spawns a background thread that
/// calls `on_reset` once per connection received on it, forever, until the
/// process exits. Meant to be called once from `starthwde`'s `main()`
/// (see `main.rs`) - `on_reset` there emits a Tauri event to the root
/// window, which is what actually clears an active shell-replacement
/// plugin (`DesktopContext`/`App.tsx`).
///
/// Any stale socket file left over from a previous run (e.g. after a
/// crash) is removed before binding, since `UnixListener::bind` fails on
/// an existing path.
pub fn listen_for_emergency_reset<F>(on_reset: F) -> std::io::Result<()>
where
    F: Fn() + Send + 'static,
{
    use std::os::unix::net::UnixListener;

    let path = emergency_socket_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;

    std::thread::Builder::new()
        .name("hwde-emergency-reset".into())
        .spawn(move || {
            for connection in listener.incoming() {
                match connection {
                    Ok(_stream) => on_reset(),
                    Err(err) => {
                        eprintln!("[hwde-ipc] emergency-reset listener error: {err}");
                    }
                }
            }
        })?;
    Ok(())
}

/// Send a single request to `comphwde` and wait for its response.
///
/// `timeout` bounds both the connection attempt and the read of the
/// response so a wedged compositor can never hang the shell indefinitely.
pub fn send_request(req: &IpcRequest, timeout: Duration) -> Result<IpcResponse, IpcError> {
    send_request_for(None, req, timeout)
}

/// Same as [`send_request`], but targeting the socket for a specific
/// `extern_name` (see [`socket_path_for`]) instead of always the native
/// HWDE one. `swde-ipc` (SWDE's thin wrapper crate) always calls this with
/// `Some("swde")`.
pub fn send_request_for(extern_name: Option<&str>, req: &IpcRequest, timeout: Duration) -> Result<IpcResponse, IpcError> {
    let path = socket_path_for(extern_name);
    if !path.exists() {
        return Err(IpcError::NotFound(path));
    }

    let mut stream = UnixStream::connect(&path)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader.read_line(&mut response_line)?;

    if response_line.trim().is_empty() {
        return Err(IpcError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "comphwde closed the connection without responding",
        )));
    }

    let response: IpcResponse = serde_json::from_str(response_line.trim())?;
    match response {
        IpcResponse::Error(msg) => Err(IpcError::Remote(msg)),
        other => Ok(other),
    }
}

/// Convenience: true if a native-HWDE comphwde compositor is currently
/// reachable.
pub fn is_compositor_running() -> bool {
    is_compositor_running_for(None)
}

/// Same as [`is_compositor_running`], but for a specific `extern_name`
/// (see [`socket_path_for`]).
pub fn is_compositor_running_for(extern_name: Option<&str>) -> bool {
    matches!(
        send_request_for(extern_name, &IpcRequest::Ping, Duration::from_millis(300)),
        Ok(IpcResponse::Pong)
    )
}

// Tiny local shim so this crate doesn't need a `libc` dependency just for
// getuid() when building the fallback runtime-dir path.
#[cfg(unix)]
unsafe fn libc_getuid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    getuid()
}

#[cfg(not(unix))]
unsafe fn libc_getuid() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips every `IpcRequest` variant through `serde_json` and
    /// checks the `Debug` representation survives unchanged - catches
    /// accidental field renames/tag drift between `comphwde` and
    /// `starthwde` since they only ever share this crate, never a live
    /// type-checked call.
    fn roundtrip_request(req: IpcRequest) {
        let json = serde_json::to_string(&req).expect("serialize IpcRequest");
        let back: IpcRequest = serde_json::from_str(&json).expect("deserialize IpcRequest");
        assert_eq!(format!("{req:?}"), format!("{back:?}"), "round-trip mismatch for {json}");
    }

    fn roundtrip_response(res: IpcResponse) {
        let json = serde_json::to_string(&res).expect("serialize IpcResponse");
        let back: IpcResponse = serde_json::from_str(&json).expect("deserialize IpcResponse");
        assert_eq!(format!("{res:?}"), format!("{back:?}"), "round-trip mismatch for {json}");
    }

    #[test]
    fn requests_roundtrip() {
        roundtrip_request(IpcRequest::Ping);
        roundtrip_request(IpcRequest::LaunchApp { command: "kitty".into(), args: vec!["-e".into(), "htop".into()] });
        roundtrip_request(IpcRequest::SetWallpaper { path: "/tmp/wall.png".into() });
        roundtrip_request(IpcRequest::ListWindows);
        roundtrip_request(IpcRequest::FocusWindow { id: 42 });
        roundtrip_request(IpcRequest::CloseWindow { id: 42 });
        roundtrip_request(IpcRequest::MinimizeWindow { id: 42 });
        roundtrip_request(IpcRequest::UnminimizeWindow { id: 42 });
        roundtrip_request(IpcRequest::MaximizeWindow { id: 42, maximized: true });
        roundtrip_request(IpcRequest::ToggleFloatingWindow { id: 42 });
        roundtrip_request(IpcRequest::ListWorkspaces);
        roundtrip_request(IpcRequest::SwitchWorkspace { id: 2 });
        roundtrip_request(IpcRequest::MoveWindowToWorkspace { id: 42, workspace: 3 });
        roundtrip_request(IpcRequest::SetTiling { workspace: 1, enabled: true });
        roundtrip_request(IpcRequest::ReloadConfig);
        roundtrip_request(IpcRequest::ListOutputs);
        roundtrip_request(IpcRequest::Shutdown);
    }

    #[test]
    fn responses_roundtrip() {
        roundtrip_response(IpcResponse::Pong);
        roundtrip_response(IpcResponse::Ok);
        roundtrip_response(IpcResponse::Windows(vec![WindowSummary {
            id: 1,
            title: "Terminal".into(),
            app_id: "kitty".into(),
            is_xwayland: false,
            is_minimized: false,
            is_maximized: true,
            is_floating: false,
        }]));
        roundtrip_response(IpcResponse::Workspaces(vec![
            WorkspaceSummary { id: 0, is_active: true, window_count: 3, is_tiling: true },
            WorkspaceSummary { id: 1, is_active: false, window_count: 0, is_tiling: false },
        ]));
        roundtrip_response(IpcResponse::Outputs(vec![OutputSummary {
            name: "WL-1".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            refresh_mhz: 60000,
            is_primary: true,
        }]));
        roundtrip_response(IpcResponse::Error("coś poszło nie tak".into()));
    }

    /// `#[serde(tag = "type", content = "data")]` means the wire format is
    /// `{"type": "...", "data": ...}` - pin that down explicitly so a
    /// future refactor (e.g. switching to untagged or internally-tagged)
    /// doesn't silently break wire compatibility between the two binaries
    /// without a test failing.
    #[test]
    fn request_wire_format_is_tagged() {
        let json = serde_json::to_value(IpcRequest::SwitchWorkspace { id: 1 }).unwrap();
        assert_eq!(json["type"], "SwitchWorkspace");
        assert_eq!(json["data"]["id"], 1);

        let json = serde_json::to_value(IpcRequest::Ping).unwrap();
        assert_eq!(json["type"], "Ping");
    }

    #[test]
    fn socket_paths_are_distinct() {
        // The emergency-reset channel must never collide with the main
        // request/response socket - they have different delivery
        // semantics (fire-and-forget broadcast vs. request/response) and
        // mixing them up would make comphwde try to JSON-parse a bare
        // "RESET\n" line as an IpcRequest.
        assert_ne!(socket_path(), emergency_socket_path());
    }

    /// The native-HWDE socket, an extern-mode socket, and two *different*
    /// extern-mode sockets must all be distinct paths - this is the whole
    /// point of `socket_path_for` (see its doc comment): comphwde running
    /// for HWDE and comphwde running with `--extern-swde` must never talk
    /// over the same control socket.
    #[test]
    fn extern_socket_paths_are_distinct() {
        assert_ne!(socket_path_for(None), socket_path_for(Some("swde")));
        assert_ne!(socket_path_for(Some("swde")), socket_path_for(Some("other")));
    }

    #[test]
    fn extern_name_is_normalized() {
        // Mixed case / stray characters must not change which file gets
        // opened, and must never let a caller escape `runtime_dir()`.
        assert_eq!(socket_path_for(Some("SWDE")), socket_path_for(Some("swde")));
        assert_eq!(socket_path_for(Some("../../etc/passwd")), socket_path_for(Some("etcpasswd")));
    }
}
