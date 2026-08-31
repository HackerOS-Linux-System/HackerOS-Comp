use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One mapped window, as summarized for `hackerland windows`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowSummary {
    /// Stable-for-this-session id (the underlying `WlSurface`'s
    /// `protocol_id()`, cast to `u64` — see `BlueState::window_id`).
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

/// One workspace, as summarized for `hackerland workspaces`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSummary {
    pub id: usize,
    pub window_count: usize,
    pub is_tiling: bool,
    pub is_active: bool,
}

/// One connected output/monitor, as summarized for `hackerland outputs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputSummary {
    pub name: String,
    /// Logical-pixel position within the compositor's global output
    /// layout (`Space::output_geometry(output).loc`) — where this
    /// output sits relative to every other connected one, not a
    /// physical/EDID coordinate.
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// Refresh rate in millihertz (matches `smithay::output::Mode::refresh`,
    /// which is itself already mHz — no conversion needed at either end
    /// of this wire type).
    pub refresh_mhz: i32,
    pub scale: f64,
    pub is_primary: bool,
}

/// `$XDG_RUNTIME_DIR/hackeros-comp/` — the runtime directory
/// HackerLand's own socket (see `crate::protocol::socket_path`) and the
/// legacy shell socket (`src/ipc/socket.rs` in the main crate) both
/// live under.
pub fn runtime_dir() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
    PathBuf::from(base).join("hackeros-comp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_summary_round_trips_through_json() {
        let w = WindowSummary {
            id: 7, title: "Terminal".into(), app_id: "blue-terminal".into(), workspace: 2,
            is_fullscreen: false, is_minimized: false, is_floating: true,
            is_maximized: false, is_xwayland: false,
        };
        let json = serde_json::to_string(&w).unwrap();
        let back: WindowSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(w, back);
    }

    #[test]
    fn runtime_dir_ends_with_expected_suffix() {
        assert!(runtime_dir().ends_with("hackeros-comp"));
    }
}
