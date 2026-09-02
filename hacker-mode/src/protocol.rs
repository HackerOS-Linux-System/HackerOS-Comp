use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdeRequest {
    pub id: u64,
    #[serde(flatten)]
    pub call: SdeCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum SdeCall {
    Ping,
    LaunchApp { command: String, args: Vec<String> },
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
    PinSurface { app_id: String, edge: PinnedEdge, thickness_px: u32 },
    ReloadConfig,
    ListOutputs,
    Shutdown,
    Subscribe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinnedEdge {
    Top,
    Bottom,
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum SdeEvent {
    Windows(Vec<SdeWindowInfo>),
    Workspaces(Vec<SdeWorkspaceInfo>),
    CompositorShuttingDown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdeEventMessage {
    pub event: SdeEvent,
}

#[derive(Debug, thiserror::Error)]
pub enum SdeIpcError {
    #[error("hackeros-comp is not running in --extern-{0} mode (no socket at {1:?})")]
    NotRunning(String, PathBuf),
    #[error("i/o error talking to hackeros-comp: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed message from hackeros-comp: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("hackeros-comp rejected the request: {0}")]
    Rejected(String),
    #[error("response id {got} did not match request id {expected}")]
    IdMismatch { expected: u64, got: u64 },
}

/// `$XDG_RUNTIME_DIR/sde`, falling back to `/tmp/sde-<uid>`. MUST match
/// `hackeros-comp`'s own `src/ipc/protocol.rs::runtime_dir()` exactly -
/// see that function's doc comment.
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("sde");
        }
    }
    let uid = unsafe { libc_getuid() };
    PathBuf::from(format!("/tmp/sde-{uid}"))
}

pub fn socket_path_for(extern_name: &str) -> PathBuf {
    runtime_dir().join(format!("hackeros-comp-{extern_name}.sock"))
}

// Deliberately not a `libc` dependency for one syscall in a crate that
// otherwise has none - `getuid()` can never fail and has no meaningful
// error path, so a single `extern "C"` declaration is simpler than a
// whole crate for it. (`hackeros-comp` itself already depends on `libc`
// for far more than this, so its own `protocol.rs` just uses it
// directly instead - see that module.)
extern "C" {
    #[link_name = "getuid"]
    fn c_getuid() -> u32;
}
unsafe fn libc_getuid() -> u32 {
    c_getuid()
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Sends one request to `hackeros-comp --extern-<extern_name>` and waits
/// for its response.
pub fn call(extern_name: &str, request: SdeCall, timeout: Duration) -> Result<SdeResult, SdeIpcError> {
    let path = socket_path_for(extern_name);
    if !path.exists() {
        return Err(SdeIpcError::NotRunning(extern_name.to_string(), path));
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

/// True if a `hackeros-comp --extern-<extern_name>` compositor is
/// currently reachable and answers `Ping` with `Pong`.
pub fn is_running(extern_name: &str) -> bool {
    matches!(call(extern_name, SdeCall::Ping, Duration::from_millis(300)), Ok(SdeResult::Pong))
}

/// A live `Subscribe` event stream, opened by [`subscribe`].
pub struct Subscription {
    reader: BufReader<UnixStream>,
}

impl Subscription {
    /// Blocks until the next event arrives (or the connection drops).
    pub fn recv(&mut self) -> Result<SdeEvent, SdeIpcError> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Err(SdeIpcError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "sde-ipc subscription closed by hackeros-comp",
            )));
        }
        let msg: SdeEventMessage = serde_json::from_str(line.trim())?;
        Ok(msg.event)
    }
}

/// Opens a live event stream (window/workspace changes) from
/// `hackeros-comp --extern-<extern_name>`.
pub fn subscribe(extern_name: &str) -> Result<Subscription, SdeIpcError> {
    let path = socket_path_for(extern_name);
    if !path.exists() {
        return Err(SdeIpcError::NotRunning(extern_name.to_string(), path));
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
            reader.get_ref().set_read_timeout(None)?;
            Ok(Subscription { reader })
        }
        SdeOutcome::Err { message } => Err(SdeIpcError::Rejected(message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_matches_hackeros_comp_naming() {
        // hackeros-comp's own `src/ipc/protocol.rs::socket_path_for`
        // builds `<runtime_dir>/hackeros-comp-<name>.sock` - this MUST
        // match, or a real compositor and this client would each be
        // listening on / connecting to different paths.
        let expected = runtime_dir().join("hackeros-comp-hacker-mode.sock");
        assert_eq!(socket_path_for("hacker-mode"), expected);
    }

    #[test]
    fn request_round_trips_through_json() {
        let req = SdeRequest { id: 7, call: SdeCall::FocusWindow { id: 42 } };
        let json = serde_json::to_string(&req).unwrap();
        let back: SdeRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, 7);
        assert!(matches!(back.call, SdeCall::FocusWindow { id: 42 }));
    }
}
