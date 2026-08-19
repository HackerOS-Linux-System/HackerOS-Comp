use serde::{Deserialize, Serialize};
use crate::state::WindowInfo;

/// Messages FROM compositor TO shell (Tauri frontend).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompositorMessage {
    Ready             { socket: String },
    WindowList        { windows: Vec<WindowInfo> },
    WindowFocused     { id: u64 },
    WindowOpened      { window: WindowInfo },
    WindowClosed      { id: u64 },
    WindowUpdated     { window: WindowInfo },
    ToggleStartMenu,
    ToggleFullscreenMenu,
    WorkspaceSwitched { index: usize, count: usize },
    SwitcherState     { visible: bool, index: usize },
    ScreenLocked      { locked: bool },
    OutputChanged     { name: String, connected: bool, width: u32, height: u32 },
    IdleChanged       { idle: bool },
    ScreenshotReady   { path: String },
    Error             { message: String },
    /// IME candidate-window (popup) visibility/position — informational
    /// only; the candidate *content* is always the IME's own composited
    /// surface (see protocols/input_method.rs), never drawn by the
    /// shell. Lets the shell avoid overlapping that screen region with
    /// its own chrome, or draw a subtle boundary around it.
    ImeCandidateWindow { visible: bool, x: i32, y: i32, width: u32, height: u32 },
    /// HDR state changed for an output — either the user toggled it via
    /// `ShellMessage::SetHdrEnabled`, or (once render/mod.rs's
    /// tone-mapping stub is implemented, see protocols/color_management.rs
    /// module doc) a client's negotiated image description made it
    /// available automatically.
    HdrStateChanged   { output: String, hdr_active: bool },
    /// Real snapshot of what `UdevData::gpu_manager` (render/multigpu.rs)
    /// actually knows: every DRM render node this compositor has opened,
    /// which one is primary, and how many outputs each currently drives.
    /// Sent once after udev backend init finishes (see render/mod.rs's
    /// `init_udev`) and again on every GPU hotplug event
    /// (`UdevEvent::Added`/`Removed`, via `render::mod::
    /// broadcast_gpu_list`) — previously that re-send didn't happen, so
    /// a shell panel open across a hotplug showed a stale snapshot until
    /// the next compositor restart. Only sent under the udev backend;
    /// winit (nested/dev) has no real DRM nodes to report.
    GpuList           { gpus: Vec<GpuInfo> },
}

/// One entry in `CompositorMessage::GpuList`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GpuInfo {
    /// Human-readable node identifier — `DrmNode`'s `Debug` output (e.g.
    /// `DrmNode(...)`), not a raw device path. Good enough to tell GPUs
    /// apart in a settings UI; not meant to be parsed.
    pub node: String,
    pub primary: bool,
    pub output_count: u32,
}

/// Messages FROM shell TO compositor.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShellMessage {
    FocusWindow            { id: u64 },
    CloseWindow            { id: u64 },
    KillWindow             { id: u64 },
    SwitchWorkspace        { index: usize },
    MoveWindowToWorkspace  { id: u64, workspace: usize },
    ToggleMaximize         { id: u64 },
    MinimizeWindow         { id: u64 },
    RestoreWindow          { id: u64 },
    SetFullscreen          { id: u64, fullscreen: bool },
    TileWindow             { id: u64, position: String },
    GetWindowList,
    SetWorkspaceCount      { count: usize },
    SetDpmsTimeout         { seconds: u64 },
    LockScreen,
    TakeScreenshot         { path: String, mode: String },
    SetKeyboardLayout      { layout: String, variant: Option<String> },
    SetCursor              { theme: String, size: u32 },
    ReloadConfig,
    /// User toggled "HDR" in the shell's Monitors settings section
    /// (`MonitorsSection.svelte`). See `HdrStateChanged` for the
    /// compositor's reply and protocols/color_management.rs for how
    /// far the negotiation side of this currently reaches (parametric
    /// only; render-side tone-mapping is still a stub).
    SetHdrEnabled          { output: String, enabled: bool },
}
