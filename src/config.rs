use std::path::PathBuf;

use serde::{Deserialize, Serialize};

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// `~/.config/HWDE` in native mode, `~/.config/<NAME>` (upper-cased,
/// e.g. `~/.config/SDE`) in extern mode - see `main.rs`'s `ExternMode`.
/// Kept as a *separate* dir per extern target (rather than reusing HWDE's)
/// so SDE's `workspace_count`/`gaps`/`keybindings`/... can diverge freely
/// from HWDE's without one session's settings editor clobbering the
/// other's file.
pub fn config_dir_for(extern_name: Option<&str>) -> PathBuf {
    match extern_name {
        Some(name) => home_dir().join(".config").join(name.to_uppercase()),
        None => home_dir().join(".config/HWDE"),
    }
}

pub fn config_dir() -> PathBuf {
    config_dir_for(None)
}

pub fn config_file_for(extern_name: Option<&str>) -> PathBuf {
    config_dir_for(extern_name).join("compositor.toml")
}

pub fn config_file() -> PathBuf {
    config_file_for(None)
}

/// One keyboard shortcut. `mods` is a set of `"super"`, `"ctrl"`, `"alt"`,
/// `"shift"` (case-insensitive, order doesn't matter); `key` is an xkb
/// keysym name (`"Return"`, `"q"`, `"1"`, `"F1"`, ...) resolved at match
/// time via `xkbcommon::xkb::keysym_from_name`. `action` is a small DSL
/// parsed by [`Keybinding::parse_action`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keybinding {
    pub mods: Vec<String>,
    pub key: String,
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositorConfig {
    /// Number of virtual desktops. Windows are tagged with a workspace id
    /// in `0..workspace_count`; switching hides/shows accordingly - see
    /// `HwdeState::switch_workspace` in `state.rs`.
    pub workspace_count: u32,
    /// Gap (px, logical) left between tiled windows and screen edges when
    /// a workspace's `tiling` layout is active - see `render_elements.rs`.
    pub gaps: i32,
    /// Border width (px) drawn around the focused window.
    pub border_width: i32,
    /// Fraction of output width the master column takes in
    /// `apply_tiling_layout`'s master-stack layout, `0.1..0.9`. Adjustable
    /// live via the `increase_master`/`decrease_master` keybinding actions
    /// (default `super+l`/`super+h`) - see `state.rs::adjust_master_ratio`.
    pub master_ratio: f32,
    pub keybindings: Vec<Keybinding>,
}

impl Default for CompositorConfig {
    fn default() -> Self {
        Self {
            workspace_count: 4,
            gaps: 8,
            border_width: 2,
            master_ratio: 0.55,
            keybindings: default_keybindings(),
        }
    }
}

fn default_keybindings() -> Vec<Keybinding> {
    let mut v = vec![
        Keybinding { mods: vec!["super".into()], key: "Return".into(), action: "spawn:kitty".into() },
        Keybinding { mods: vec!["super".into()], key: "d".into(), action: "spawn:hwde-launcher".into() },
        Keybinding { mods: vec!["super".into()], key: "q".into(), action: "close_window".into() },
        Keybinding { mods: vec!["super".into()], key: "f".into(), action: "toggle_maximize".into() },
        Keybinding { mods: vec!["super".into()], key: "t".into(), action: "toggle_tiling".into() },
        Keybinding { mods: vec!["super".into()], key: "m".into(), action: "swap_master".into() },
        Keybinding { mods: vec!["super".into()], key: "l".into(), action: "increase_master".into() },
        Keybinding { mods: vec!["super".into()], key: "h".into(), action: "decrease_master".into() },
        Keybinding { mods: vec!["super".into()], key: "Tab".into(), action: "focus_next".into() },
        Keybinding { mods: vec!["super".into(), "shift".into()], key: "Tab".into(), action: "focus_prev".into() },
        Keybinding { mods: vec!["super".into(), "shift".into()], key: "f".into(), action: "toggle_floating".into() },
        Keybinding { mods: vec!["super".into(), "shift".into()], key: "q".into(), action: "quit".into() },
        Keybinding { mods: vec!["super".into(), "shift".into()], key: "r".into(), action: "reload_config".into() },
    ];
    for n in 1..=9u32 {
        v.push(Keybinding { mods: vec!["super".into()], key: n.to_string(), action: format!("workspace_{n}") });
        v.push(Keybinding {
            mods: vec!["super".into(), "shift".into()],
            key: n.to_string(),
            action: format!("move_to_workspace_{n}"),
        });
    }
    v
}

/// Compositor-level action a keybinding can trigger, resolved from a
/// `Keybinding::action` string. Kept intentionally small - anything more
/// elaborate (app launching with args, per-role defaults, ...) already has
/// a much richer home in the shell (`commands/default_apps.rs`) and can be
/// reached via `spawn:` here if it's a plain executable name.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Spawn(String),
    CloseWindow,
    ToggleMaximize,
    ToggleTiling,
    /// Excludes/re-includes the focused window from its workspace's
    /// master-stack tiling without changing whether tiling itself is on -
    /// v0.2 addition, default `super+shift+f`. See
    /// `HwdeState::toggle_floating_by_id`.
    ToggleFloating,
    SwapMaster,
    AdjustMaster(f32),
    FocusNext,
    /// Reverse of `FocusNext` - v0.2 addition, default `super+shift+Tab`.
    FocusPrev,
    Quit,
    ReloadConfig,
    SwitchWorkspace(u32),
    MoveToWorkspace(u32),
}

impl Keybinding {
    pub fn parse_action(&self) -> Option<Action> {
        if let Some(cmd) = self.action.strip_prefix("spawn:") {
            return Some(Action::Spawn(cmd.to_string()));
        }
        if let Some(n) = self.action.strip_prefix("workspace_") {
            return n.parse().ok().map(Action::SwitchWorkspace);
        }
        if let Some(n) = self.action.strip_prefix("move_to_workspace_") {
            return n.parse().ok().map(Action::MoveToWorkspace);
        }
        match self.action.as_str() {
            "close_window" => Some(Action::CloseWindow),
            "toggle_maximize" => Some(Action::ToggleMaximize),
            "toggle_tiling" => Some(Action::ToggleTiling),
            "toggle_floating" => Some(Action::ToggleFloating),
            "swap_master" => Some(Action::SwapMaster),
            "increase_master" => Some(Action::AdjustMaster(0.05)),
            "decrease_master" => Some(Action::AdjustMaster(-0.05)),
            "focus_next" => Some(Action::FocusNext),
            "focus_prev" => Some(Action::FocusPrev),
            "quit" => Some(Action::Quit),
            "reload_config" => Some(Action::ReloadConfig),
            _ => None,
        }
    }

    fn mods_match(&self, mods: &smithay::input::keyboard::ModifiersState) -> bool {
        let wants = |name: &str| self.mods.iter().any(|m| m.eq_ignore_ascii_case(name));
        // Every modifier this binding requires must be held, AND no *extra*
        // modifier we don't ask for should be - otherwise e.g. an
        // unqualified `q` binding would also fire for `super+shift+q`,
        // stepping on a more specific binding for the same key.
        let want_super = wants("super") || wants("logo") || wants("meta");
        let want_ctrl = wants("ctrl") || wants("control");
        let want_alt = wants("alt");
        let want_shift = wants("shift");
        mods.logo == want_super && mods.ctrl == want_ctrl && mods.alt == want_alt && mods.shift == want_shift
    }

    /// True if this binding's `key` (resolved via xkbcommon's keysym name
    /// table) and modifier set match the currently pressed key.
    pub fn matches(&self, keysym: smithay::input::keyboard::xkb::Keysym, mods: &smithay::input::keyboard::ModifiersState) -> bool {
        if !self.mods_match(mods) {
            return false;
        }
        // `keysym_from_name` returns `Keysym::from(0)` ("NoSymbol") for an
        // unrecognized name rather than an `Option` - that's fine here
        // without special-casing it, since `NoSymbol` will never equal a
        // real pressed key, so an unresolvable `key` in the config simply
        // never matches (instead of panicking or silently binding to the
        // wrong key).
        let bound = smithay::input::keyboard::xkb::keysym_from_name(
            &self.key,
            smithay::input::keyboard::xkb::KEYSYM_CASE_INSENSITIVE,
        );
        bound == keysym
    }
}

/// True if `keysym`+`mods` is the hardcoded emergency-reset combo
/// (`Ctrl+Alt+Shift+Escape`, deliberately *not* Super/Logo since that's
/// the modifier every other default binding uses - see the module-level
/// doc comment for why this exists and why it's a plain function rather
/// than a `Keybinding` entry in [`CompositorConfig`]).
pub fn is_emergency_reset(keysym: smithay::input::keyboard::xkb::Keysym, mods: &smithay::input::keyboard::ModifiersState) -> bool {
    let escape = smithay::input::keyboard::xkb::keysym_from_name("Escape", smithay::input::keyboard::xkb::KEYSYM_CASE_INSENSITIVE);
    mods.ctrl && mods.alt && mods.shift && !mods.logo && keysym == escape
}

/// Loads `compositor.toml`, writing out defaults first if the file doesn't
/// exist yet (so there's always something on disk for the shell's
/// keybindings editor to read/round-trip).
pub fn load() -> CompositorConfig {
    load_for(None)
}

/// Same as [`load`], but from/for the extern-mode config dir (see
/// [`config_dir_for`]) when `extern_name` is `Some`.
pub fn load_for(extern_name: Option<&str>) -> CompositorConfig {
    let path = config_file_for(extern_name);
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<CompositorConfig>(&text) {
            Ok(cfg) => cfg,
            Err(err) => {
                tracing::warn!("failed to parse {}: {err} - using defaults", path.display());
                CompositorConfig::default()
            }
        },
        Err(_) => {
            let defaults = CompositorConfig::default();
            if let Err(err) = save_for(extern_name, &defaults) {
                tracing::warn!("failed to write default {}: {err}", path.display());
            }
            defaults
        }
    }
}

pub fn save(cfg: &CompositorConfig) -> std::io::Result<()> {
    save_for(None, cfg)
}

pub fn save_for(extern_name: Option<&str>, cfg: &CompositorConfig) -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir_for(extern_name))?;
    let text = toml::to_string_pretty(cfg).unwrap_or_default();
    std::fs::write(config_file_for(extern_name), text)
}
