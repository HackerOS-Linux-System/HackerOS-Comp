use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ── Requests ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdeRequest {
    pub id: u64,
    #[serde(flatten)]
    pub call: SdeCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum SdeCall {
    /// Liveness check — a bare socket file existing doesn't mean the
    /// listener behind it is actually accepting/answering yet (see
    /// `extern_ipc::init`'s accept loop); this is the real signal.
    Ping,
    /// Spawns `command args...` as a client of *this* `--extern-<name>`
    /// compositor instance (correct `WAYLAND_DISPLAY` already set in its
    /// own environment — see `spawn_external_app`).
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
    SwitchWorkspace { id: usize },
    MoveWindowToWorkspace { id: u64, workspace: usize },
    SetTiling { workspace: usize, enabled: bool },
    PinSurface { app_id: String, edge: PinnedEdge, thickness_px: u32 },
    ReloadConfig,
    ListOutputs,
    /// Ends this `--extern-<name>` session cleanly (the compositor
    /// process exits after finishing its current event-loop tick).
    Shutdown,
    /// Opens a live event stream instead of a single request/response —
    /// see this module's doc comment. Must be the only call ever sent on
    /// its connection.
    Subscribe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinnedEdge {
    Top,
    Bottom,
}

// ── Responses ────────────────────────────────────────────────────────────

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

/// One mapped window, over the wire — field-for-field identical to
/// [`hackerland::summaries::WindowSummary`] (see the `From` impl below):
/// this protocol reports exactly what the compositor already tracks,
/// nothing invented for the wire that isn't queryable state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SdeWindowInfo {
    pub id: u64,
    pub title: String,
    pub app_id: String,
    pub workspace: usize,
    pub is_fullscreen: bool,
    pub is_minimized: bool,
    pub is_floating: bool,
    pub is_maximized: bool,
    pub is_xwayland: bool,
}

impl From<hackerland::summaries::WindowSummary> for SdeWindowInfo {
    fn from(w: hackerland::summaries::WindowSummary) -> Self {
        SdeWindowInfo {
            id: w.id,
            title: w.title,
            app_id: w.app_id,
            workspace: w.workspace,
            is_fullscreen: w.is_fullscreen,
            is_minimized: w.is_minimized,
            is_floating: w.is_floating,
            is_maximized: w.is_maximized,
            is_xwayland: w.is_xwayland,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SdeWorkspaceInfo {
    pub id: usize,
    pub name: String,
    pub window_count: usize,
    pub tiling_enabled: bool,
    pub active: bool,
}

impl From<hackerland::summaries::WorkspaceSummary> for SdeWorkspaceInfo {
    fn from(w: hackerland::summaries::WorkspaceSummary) -> Self {
        SdeWorkspaceInfo {
            id: w.id,
            name: format!("{}", w.id + 1),
            window_count: w.window_count,
            tiling_enabled: w.is_tiling,
            active: w.is_active,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SdeOutputInfo {
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub refresh_mhz: i32,
    pub scale: f64,
    pub primary: bool,
}

impl From<hackerland::summaries::OutputSummary> for SdeOutputInfo {
    fn from(o: hackerland::summaries::OutputSummary) -> Self {
        SdeOutputInfo {
            name: o.name,
            width: o.width,
            height: o.height,
            refresh_mhz: o.refresh_mhz,
            scale: o.scale,
            primary: o.is_primary,
        }
    }
}

// ── Push events (Subscribe) ─────────────────────────────────────────────

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

// ── Socket location ──────────────────────────────────────────────────────

/// `$XDG_RUNTIME_DIR/sde`, falling back to `/tmp/sde-<uid>` when
/// `XDG_RUNTIME_DIR` isn't set (e.g. a bare `--extern-other` instance
/// spawned outside a full login session). Deliberately a different
/// directory from [`hackerland::summaries::runtime_dir`]'s
/// `hackeros-comp/` — this is a distinct protocol/socket namespace, not
/// an alternate transport for the same one. Every hand-vendored client
/// copy of this protocol (`penetration-mode-ipc`, `hacker-mode-ipc`, the
/// vendored copies in each session launcher's own repository, ...) MUST
/// compute this identically, or client and server end up listening on/
/// connecting to different paths — see each copy's own test asserting
/// this.
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("sde");
        }
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/sde-{uid}"))
}

/// Socket path for a given `--extern-<name>` target, e.g.
/// `$XDG_RUNTIME_DIR/sde/hackeros-comp-penetration-mode.sock`.
pub fn socket_path_for(extern_name: &str) -> PathBuf {
    runtime_dir().join(format!("hackeros-comp-{extern_name}.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_is_extern_name_specific() {
        assert_ne!(socket_path_for("penetration-mode"), socket_path_for("hacker-mode"));
        assert!(socket_path_for("penetration-mode").ends_with("hackeros-comp-penetration-mode.sock"));
    }

    #[test]
    fn request_round_trips_through_json() {
        let req = SdeRequest { id: 7, call: SdeCall::FocusWindow { id: 42 } };
        let json = serde_json::to_string(&req).unwrap();
        let back: SdeRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, 7);
        assert!(matches!(back.call, SdeCall::FocusWindow { id: 42 }));
    }

    #[test]
    fn window_summary_converts_field_for_field() {
        let w = hackerland::summaries::WindowSummary {
            id: 1, title: "Terminal".into(), app_id: "blue-terminal".into(), workspace: 0,
            is_fullscreen: false, is_minimized: true, is_floating: false,
            is_maximized: false, is_xwayland: true,
        };
        let sde: SdeWindowInfo = w.clone().into();
        assert_eq!(sde.id, w.id);
        assert_eq!(sde.app_id, w.app_id);
        assert_eq!(sde.is_minimized, w.is_minimized);
        assert_eq!(sde.is_xwayland, w.is_xwayland);
    }
}
