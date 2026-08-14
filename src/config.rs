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
    /// Seconds of no input activity before the idle-dim overlay kicks in
    /// (see `HwdeState::idle_dimmed` in `state.rs` for exactly what that
    /// does and does not do - short version: a visual dimmer, not a lock
    /// screen). `0` disables it entirely. `#[serde(default = ...)]` so a
    /// `compositor.toml` written before this field existed still loads
    /// instead of falling back to `CompositorConfig::default()` wholesale
    /// (see `load_for`'s `toml::from_str` call, which fails the *entire*
    /// parse on one missing non-defaulted field) - the one field on this
    /// struct that has this annotation, since it's the one added after
    /// the others were already shipping.
    #[serde(default = "default_idle_dim_timeout_secs")]
    pub idle_dim_timeout_secs: u32,
    /// Output rotation/flip: `"normal"`, `"90"`, `"180"`, `"270"`,
    /// `"flipped"`, `"flipped-90"`, `"flipped-180"`, `"flipped-270"` -
    /// the same eight values `wl_output`'s transform enum has, applied
    /// via `Output::change_current_state` at output-creation time
    /// (`winit_backend.rs`/`backend_drm.rs::connector_connected`). An
    /// unrecognized value logs a warning and falls back to `"normal"`
    /// rather than failing to start - see `parse_transform`. Previously
    /// hardcoded to `Transform::Normal` with no way to configure it at
    /// all - see this project's README for that history.
    #[serde(default = "default_output_transform")]
    pub output_transform: String,
    /// Output scale factor (HiDPI). `1.0` is unscaled. Fractional values
    /// (e.g. `1.5`) are valid - `render_elements.rs` already reads
    /// `output.current_scale().fractional_scale()` everywhere it needs
    /// physical-pixel conversion, so nothing else needed to change for
    /// non-integer scales to work through the existing render path.
    #[serde(default = "default_output_scale")]
    pub output_scale: f64,
    /// Per-output position overrides, keyed by connector name (e.g.
    /// `"DP-1"`, `"HDMI-A-1"` - the same `"{interface}-{interface_id}"`
    /// form `backend_drm.rs::connector_connected` already builds for
    /// `Output::new`). An output with no entry here keeps the existing
    /// automatic behavior (`winit_backend.rs`'s single output always at
    /// `(0, 0)`; `backend_drm.rs` places each newly-connected output to
    /// the right of whatever's already mapped, left to right, widths
    /// summed). `#[serde(default)]` (an empty map) rather than a
    /// required field, so a `compositor.toml` written before this
    /// existed still loads instead of failing the whole parse (same
    /// reasoning as `idle_dim_timeout_secs`'s own default annotation).
    #[serde(default)]
    pub outputs: std::collections::HashMap<String, OutputPlacement>,
    /// XKB keyboard layout (e.g. `"pl"`, `"de"`, `"us"`) - empty string
    /// means "let libxkbcommon pick its own default" (effectively US),
    /// same as the `XkbConfig::default()` this replaces. Previously
    /// there was no way to configure this at all -
    /// `seat.add_keyboard(Default::default(), ...)` was hardcoded in
    /// both backends.
    #[serde(default)]
    pub xkb_layout: String,
    /// XKB layout variant (e.g. `"dvorak"`, `"colemak"`, `""` for the
    /// layout's own default).
    #[serde(default)]
    pub xkb_variant: String,
    /// XKB model (e.g. `"pc105"`); empty lets libxkbcommon pick.
    #[serde(default)]
    pub xkb_model: String,
    /// XKB options, comma-separated per libxkbcommon convention (e.g.
    /// `"ctrl:nocaps,grp:alt_shift_toggle"` for caps-lock-as-ctrl plus an
    /// Alt+Shift layout-switch toggle - the two most commonly requested
    /// options users reach for). Empty means none set.
    #[serde(default)]
    pub xkb_options: String,
}

/// Builds Smithay's `XkbConfig` from this config's `xkb_*` fields - the
/// borrowed-`&str` lifetime is why this is a method taking `&self`
/// (returning borrowed data) rather than something computed once and
/// stored, so it has to be called fresh at each `seat.add_keyboard(...)`
/// site rather than cached on `HwdeState` itself.
impl CompositorConfig {
    pub fn xkb_config(&self) -> smithay::input::keyboard::XkbConfig<'_> {
        smithay::input::keyboard::XkbConfig {
            rules: "",
            model: &self.xkb_model,
            layout: &self.xkb_layout,
            variant: &self.xkb_variant,
            options: if self.xkb_options.is_empty() { None } else { Some(self.xkb_options.clone()) },
        }
    }
}

/// One output's configured position - see `CompositorConfig::outputs`.
/// Only position, not mode/transform/scale: those already have their own
/// (currently global, not per-output) config fields
/// (`output_transform`/`output_scale`), and giving every output its own
/// independent transform/scale is a bigger, separate feature than what
/// this pass set out to add (manual *layout*, not full per-output
/// configuration).
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct OutputPlacement {
    pub x: i32,
    pub y: i32,
}

fn default_idle_dim_timeout_secs() -> u32 {
    300
}

fn default_output_transform() -> String {
    "normal".to_string()
}

fn default_output_scale() -> f64 {
    1.0
}

/// Parses `config.output_transform`'s string form into Smithay's
/// `Transform` enum - see that field's doc comment for the accepted
/// values. Falls back to `Transform::Normal` (with a warning, not a
/// startup failure) for anything unrecognized, including simple case/
/// spelling mismatches - a rotated-the-wrong-way or entirely-unrotated
/// output is a recoverable annoyance a user can fix and reload; refusing
/// to start over a typo in this one string would not be proportionate.
pub fn parse_transform(s: &str) -> smithay::utils::Transform {
    use smithay::utils::Transform;
    match s.trim().to_lowercase().as_str() {
        "normal" | "" => Transform::Normal,
        "90" => Transform::_90,
        "180" => Transform::_180,
        "270" => Transform::_270,
        "flipped" => Transform::Flipped,
        "flipped-90" | "flipped90" => Transform::Flipped90,
        "flipped-180" | "flipped180" => Transform::Flipped180,
        "flipped-270" | "flipped270" => Transform::Flipped270,
        other => {
            tracing::warn!("compositor.toml: unrecognized output_transform {other:?}, falling back to \"normal\" - valid values: normal, 90, 180, 270, flipped, flipped-90, flipped-180, flipped-270");
            Transform::Normal
        }
    }
}

impl Default for CompositorConfig {
    fn default() -> Self {
        Self {
            workspace_count: 4,
            gaps: 8,
            border_width: 2,
            master_ratio: 0.55,
            keybindings: default_keybindings(),
            idle_dim_timeout_secs: default_idle_dim_timeout_secs(),
            output_transform: default_output_transform(),
            output_scale: default_output_scale(),
            outputs: std::collections::HashMap::new(),
            xkb_layout: String::new(),
            xkb_variant: String::new(),
            xkb_model: String::new(),
            xkb_options: String::new(),
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
    /// Manually triggers the idle-dim overlay immediately, instead of
    /// waiting for `config.idle_dim_timeout_secs` of inactivity - see
    /// `HwdeState::idle_dimmed`'s doc comment in `state.rs` for what that
    /// overlay is (a screensaver dimmer) and, just as importantly, what
    /// it explicitly is **not** (a lock screen - no authentication, no
    /// input interception). No default keybinding maps to this
    /// deliberately: the conventional muscle-memory shortcut for
    /// "dim/lock now" on most desktops is `super+l`, and binding this
    /// non-authenticating action to that exact combo risks a user
    /// genuinely believing they've locked their session when they
    /// haven't - the same "looks secure but isn't is worse than not
    /// having it" reasoning `idle_dimmed`'s doc comment gives for not
    /// building real authentication in the first place. Available as
    /// `dim_now` in `compositor.toml` for anyone who wants to bind it
    /// themselves, understanding what it actually does.
    DimNow,
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
            "dim_now" => Some(Action::DimNow),
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
