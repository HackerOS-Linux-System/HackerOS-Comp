use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// One call from SDE to comphwde. `id` is echoed back verbatim in the
/// [`SdeResponse`] so a future multiplexed client (e.g. one issuing calls
/// from more than one shell component concurrently over a shared, kept-
/// open connection) can match responses to requests; today's client
/// ([`call`]) opens one connection per call and doesn't need it, but the
/// field is part of the wire format from day one so that isn't a breaking
/// change later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdeRequest {
    pub id: u64,
    #[serde(flatten)]
    pub call: SdeCall,
}

/// The operations SDE can ask comphwde to perform. Field/variant names are
/// intentionally verbose (`method` + `params`) rather than the compact
/// internally-tagged style `hwde-ipc` uses, because this file is meant to
/// be readable/writable by hand from `sde-shell`'s Slint callback code
/// without generated bindings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum SdeCall {
    /// Liveness check; also how `sde-shell` decides comphwde is up before
    /// doing anything else at startup.
    Ping,

    /// Ask comphwde to spawn a process wired up to this session's Wayland
    /// (and, if enabled, XWayland) display - used by SDE's app launcher
    /// and taskbar "pin" launch action.
    LaunchApp { command: String, args: Vec<String> },

    /// Replace the desktop wallpaper. `path` must be an absolute path to
    /// an image file readable by the compositor process.
    SetWallpaper { path: String },

    ListWindows,
    FocusWindow { id: u64 },
    CloseWindow { id: u64 },
    MinimizeWindow { id: u64 },
    UnminimizeWindow { id: u64 },
    MaximizeWindow { id: u64, maximized: bool },
    ToggleFloatingWindow { id: u64 },

    ListWorkspaces,
    SwitchWorkspace { id: u32 },
    MoveWindowToWorkspace { id: u64, workspace: u32 },
    SetTiling { workspace: u32, enabled: bool },

    /// Register a shell surface (SDE's top panel or bottom dock) so
    /// comphwde pins it to a screen edge instead of placing/tiling it like
    /// a normal window - see `PinnedEdge` and the compositor's
    /// `place_new_window`. `app_id` must match the `app_id` the surface
    /// itself sets (SDE sets `sde-panel` / `sde-dock`).
    PinSurface { app_id: String, edge: PinnedEdge, thickness_px: u32 },

    ReloadConfig,
    ListOutputs,
    Shutdown,

    /// Upgrades this connection into a long-lived event stream instead of
    /// a normal one-shot request/response - see [`SdeEvent`] and the
    /// module docs' "Push events" section. After the [`SdeResponse`]
    /// acknowledging this call, the server writes zero or more
    /// [`SdeEventMessage`] lines to the same connection until the client
    /// disconnects; the client must not send any further requests on it.
    Subscribe,
}

/// Which screen edge a pinned SDE shell surface (panel/dock) is anchored
/// to, and therefore which edge of the usable/tileable area shrinks to
/// make room for it. See `SdeCall::PinSurface`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinnedEdge {
    Top,
    Bottom,
}

/// Response envelope. `id` always matches the [`SdeRequest`] it answers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdeResponse {
    pub id: u64,
    #[serde(flatten)]
    pub outcome: SdeOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SdeOutcome {
    Ok { result: SdeResult },
    Err { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SdeResult {
    None,
    Pong,
    Windows(Vec<SdeWindowInfo>),
    Workspaces(Vec<SdeWorkspaceInfo>),
    Outputs(Vec<SdeOutputInfo>),
}

/// SDE's own, independent window-summary shape (field-compatible in
/// *spirit* with `hwde-ipc`'s `WindowInfo`, but a separate type on
/// purpose - see module docs). `PartialEq` is used by comphwde's
/// diff-tick (see "Push events" above) to detect changes worth
/// broadcasting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SdeWindowInfo {
    pub id: u64,
    pub title: String,
    pub app_id: String,
    pub workspace: u32,
    pub focused: bool,
    pub minimized: bool,
    pub maximized: bool,
    pub floating: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SdeWorkspaceInfo {
    pub id: u32,
    pub name: String,
    pub window_count: u32,
    pub tiling_enabled: bool,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdeOutputInfo {
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub refresh_mhz: i32,
    pub scale: f64,
    pub primary: bool,
}

/// One pushed event on a [`Subscription`] connection (see [`SdeCall::Subscribe`]
/// and the module docs' "Push events" section). Deliberately coarse-
/// grained - a full replacement list rather than a per-window delta -
/// because comphwde's diff tick already has to build the full list to
/// compare against the last snapshot, and the client (`sde-panel`/
/// `sde-dock`) was already written to replace its whole model on every
/// update (from the old polling code), so a delta format would need
/// client-side merge logic for no real benefit at SDE's window counts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum SdeEvent {
    /// The full window list changed (any window's title/workspace/focus/
    /// minimized/maximized/floating state, or the set of windows itself).
    Windows(Vec<SdeWindowInfo>),
    /// The full workspace list changed (active workspace, per-workspace
    /// window counts, or tiling-enabled state).
    Workspaces(Vec<SdeWorkspaceInfo>),
    /// comphwde is shutting down (extern-mode process exiting); the
    /// connection will close right after this. Lets a subscriber show
    /// "compositor offline" immediately instead of waiting for the
    /// socket read to fail.
    ///
    /// **Not currently emitted** by comphwde's `extern_ipc.rs` - shutdown
    /// there happens by the process exiting (closing every subscriber
    /// socket, which `Subscription::recv` already surfaces as an `Err`),
    /// not by a coordinated "send this event to everyone, then exit"
    /// step. The variant exists so a future version can add that without
    /// another protocol change; today, treat a `recv()` error as this.
    CompositorShuttingDown,
}

/// Wire envelope for one [`SdeEvent`] line on a subscription connection -
/// distinct from [`SdeResponse`] (no `id`: events are unsolicited, not
/// answers to a specific request).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdeEventMessage {
    pub event: SdeEvent,
}

#[derive(Debug, thiserror::Error)]
pub enum SdeIpcError {
    #[error("comphwde is not running in --extern-sde mode (no socket at {0})")]
    NotRunning(PathBuf),
    #[error("i/o error talking to comphwde: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed response from comphwde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("comphwde rejected the request: {0}")]
    Rejected(String),
    #[error("response id {got} did not match request id {expected}")]
    IdMismatch { expected: u64, got: u64 },
}

/// `$XDG_RUNTIME_DIR/sde`, falling back to `/tmp/sde-<uid>` if
/// `XDG_RUNTIME_DIR` isn't set (e.g. running outside a full login
/// session). Kept separate from `hwde-ipc::runtime_dir()`'s `.../hwde` on
/// purpose: this crate doesn't depend on `hwde-ipc` at all, by design.
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("sde");
        }
    }
    let uid = unsafe { libc_getuid() };
    PathBuf::from(format!("/tmp/sde-{uid}"))
}

#[cfg(unix)]
unsafe fn libc_getuid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    getuid()
}

/// Path to the control socket comphwde listens on when started as
/// `comphwde --extern-<name>`. SDE itself always uses `extern_name =
/// "sde"`; the parameter exists so a *different* future non-Tauri shell
/// reusing this same crate/protocol (e.g. `--extern-somethingelse`) gets
/// its own, non-colliding socket without touching this file.
pub fn socket_path_for(extern_name: &str) -> PathBuf {
    let slug: String = extern_name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').map(|c| c.to_ascii_lowercase()).collect();
    runtime_dir().join(format!("comphwde-{slug}.sock"))
}

/// SDE's own socket path: shorthand for `socket_path_for("sde")`.
pub fn sde_socket_path() -> PathBuf {
    socket_path_for("sde")
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Sends one request to `comphwde --extern-<extern_name>` and waits (up to
/// `timeout`) for the matching response. Opens a fresh connection per
/// call - see module docs for why that's fine here.
pub fn call(extern_name: &str, request: SdeCall, timeout: Duration) -> Result<SdeResult, SdeIpcError> {
    let path = socket_path_for(extern_name);
    if !path.exists() {
        return Err(SdeIpcError::NotRunning(path));
    }

    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let req = SdeRequest { id, call: request };

    let mut stream = UnixStream::connect(&path)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader.read_line(&mut response_line)?;

    let response: SdeResponse = serde_json::from_str(response_line.trim())?;
    if response.id != id {
        return Err(SdeIpcError::IdMismatch { expected: id, got: response.id });
    }
    match response.outcome {
        SdeOutcome::Ok { result } => Ok(result),
        SdeOutcome::Err { message } => Err(SdeIpcError::Rejected(message)),
    }
}

/// Shorthand for `call("sde", request, timeout)` - what `sde-shell` uses
/// everywhere.
pub fn call_sde(request: SdeCall, timeout: Duration) -> Result<SdeResult, SdeIpcError> {
    call("sde", request, timeout)
}

/// True if a comphwde compositor is currently reachable for `extern_name`.
pub fn is_running(extern_name: &str) -> bool {
    matches!(call(extern_name, SdeCall::Ping, Duration::from_millis(300)), Ok(SdeResult::Pong))
}

/// A live event stream from `comphwde --extern-<extern_name>`, opened via
/// [`subscribe`]. See the module docs' "Push events" section.
///
/// Reading from this is blocking (`recv`/`recv_timeout` block the calling
/// thread on socket I/O) - callers on a UI thread (as `sde-panel`/
/// `sde-dock` are) must run it on a background `std::thread` and hop back
/// to the UI thread to apply updates (e.g. via `slint::invoke_from_event_loop`),
/// same as any other blocking-I/O-on-a-background-thread pattern.
pub struct Subscription {
    reader: BufReader<UnixStream>,
}

impl Subscription {
    /// Blocks until the next event arrives, forever (no read timeout) -
    /// intended for a dedicated background thread whose entire job is
    /// this loop. Returns `Err` when the connection closes (comphwde
    /// exited, crashed, or was never reachable to begin with) or sends
    /// something unparseable; either way the caller should back off
    /// briefly and call [`subscribe`] again - comphwde doesn't remember
    /// missed events across a reconnect, so a fresh `ListWindows`/
    /// `ListWorkspaces` call right after reconnecting (which `sde-panel`/
    /// `sde-dock` already do at startup) is the way to resync state that
    /// changed while disconnected.
    pub fn recv(&mut self) -> Result<SdeEvent, SdeIpcError> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Err(SdeIpcError::Io(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "sde-ipc subscription closed")));
        }
        let msg: SdeEventMessage = serde_json::from_str(line.trim())?;
        Ok(msg.event)
    }
}

/// Opens a [`Subscription`] to `comphwde --extern-<extern_name>`. Sends
/// [`SdeCall::Subscribe`] and consumes its acknowledging [`SdeResponse`]
/// before returning, so a successful return means the server has
/// confirmed the connection is now in event-stream mode.
pub fn subscribe(extern_name: &str) -> Result<Subscription, SdeIpcError> {
    let path = socket_path_for(extern_name);
    if !path.exists() {
        return Err(SdeIpcError::NotRunning(path));
    }

    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let req = SdeRequest { id, call: SdeCall::Subscribe };

    let stream = UnixStream::connect(&path)?;
    stream.set_write_timeout(Some(Duration::from_millis(800)))?;

    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    (&stream).write_all(line.as_bytes())?;
    (&stream).flush()?;

    let mut reader = BufReader::new(stream);
    let mut ack_line = String::new();
    reader.read_line(&mut ack_line)?;
    let ack: SdeResponse = serde_json::from_str(ack_line.trim())?;
    if ack.id != id {
        return Err(SdeIpcError::IdMismatch { expected: id, got: ack.id });
    }
    match ack.outcome {
        SdeOutcome::Ok { .. } => {
            // No read timeout from here on - `Subscription::recv` is meant
            // to block indefinitely on a dedicated thread.
            reader.get_ref().set_read_timeout(None)?;
            Ok(Subscription { reader })
        }
        SdeOutcome::Err { message } => Err(SdeIpcError::Rejected(message)),
    }
}

/// Shorthand for `subscribe("sde")` - what `sde-shell` uses.
pub fn subscribe_sde() -> Result<Subscription, SdeIpcError> {
    subscribe("sde")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sde_socket_is_distinct_from_other_externs() {
        assert_ne!(socket_path_for("sde"), socket_path_for("otherde"));
    }

    #[test]
    fn extern_name_is_normalized() {
        assert_eq!(socket_path_for("SDE"), socket_path_for("sde"));
        assert_eq!(socket_path_for("../../etc/passwd"), socket_path_for("etcpasswd"));
    }

    #[test]
    fn request_roundtrips_through_json() {
        let req = SdeRequest { id: 42, call: SdeCall::FocusWindow { id: 7 } };
        let json = serde_json::to_string(&req).unwrap();
        let back: SdeRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, 42);
        matches!(back.call, SdeCall::FocusWindow { id: 7 });
    }

    #[test]
    fn response_roundtrips_through_json() {
        let resp = SdeResponse { id: 1, outcome: SdeOutcome::Ok { result: SdeResult::Pong } };
        let json = serde_json::to_string(&resp).unwrap();
        let back: SdeResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, 1);
        matches!(back.outcome, SdeOutcome::Ok { result: SdeResult::Pong });
    }

    #[test]
    fn event_message_roundtrips_through_json() {
        let msg = SdeEventMessage { event: SdeEvent::Workspaces(vec![]) };
        let json = serde_json::to_string(&msg).unwrap();
        let back: SdeEventMessage = serde_json::from_str(&json).unwrap();
        matches!(back.event, SdeEvent::Workspaces(v) if v.is_empty());
    }
}
