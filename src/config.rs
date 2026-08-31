use std::path::PathBuf;
use std::collections::HashMap;

use hk_parser::{load_hk_file, resolve_interpolations, write_hk_file, HkConfig, HkValue};
use serde::{Deserialize, Serialize};

/// Everything comphwde reads out of `config.hk` at startup (and, via
/// `SdeCall::ReloadConfig` / HackerLand's `reload`, again later without
/// a restart).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub general: GeneralConfig,
    pub appearance: AppearanceConfig,
    pub input: InputConfig,
    pub workspaces: WorkspacesConfig,
    /// `action name -> key combo string` (e.g. `"close_window" ->
    /// "Super+Q"`). Parsed as a plain string map rather than a fixed
    /// struct so a person can bind actions this compositor doesn't ship
    /// a named field for yet without a schema change — `input/mod.rs`'s
    /// keybinding dispatch is the eventual consumer (see this module's
    /// doc for why that wiring isn't done here).
    pub keybindings: HashMap<String, String>,
    /// Commands to spawn once, after the compositor's sockets are all
    /// up (see `main.rs`'s startup sequence) — a panel, a wallpaper
    /// daemon, whatever a person's session needs.
    pub autostart: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeneralConfig {
    pub gaps_px: u32,
    pub border_px: u32,
    pub focus_follows_mouse: bool,
    /// Default tiling state for newly created workspaces (`true` =
    /// master-stack tiling on by default — see `BlueState::set_tiling`).
    pub default_tiling: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self { gaps_px: 8, border_px: 2, focus_follows_mouse: false, default_tiling: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppearanceConfig {
    pub wallpaper: Option<String>,
    pub active_border_color: String,
    pub inactive_border_color: String,
    pub cursor_theme: String,
    pub cursor_size: u32,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            wallpaper: None,
            active_border_color: "#3b82f6".to_string(),
            inactive_border_color: "#334155".to_string(),
            cursor_theme: "default".to_string(),
            cursor_size: 24,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputConfig {
    pub keyboard_layout: String,
    pub keyboard_variant: String,
    pub repeat_rate: u32,
    pub repeat_delay: u32,
    pub natural_scroll: bool,
    pub tap_to_click: bool,
    pub mouse_sensitivity: f64,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            keyboard_layout: "us".to_string(),
            keyboard_variant: String::new(),
            repeat_rate: 25,
            repeat_delay: 600,
            natural_scroll: false,
            tap_to_click: true,
            mouse_sensitivity: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspacesConfig {
    pub count: usize,
    /// Per-workspace names, 0-indexed; a workspace past the end of this
    /// list just has no name (falls back to its 1-based number — see
    /// `BlueState::switch_workspace`'s existing `w.id + 1` convention).
    pub names: Vec<String>,
}

impl Default for WorkspacesConfig {
    fn default() -> Self {
        Self { count: 5, names: Vec::new() }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            appearance: AppearanceConfig::default(),
            input: InputConfig::default(),
            workspaces: WorkspacesConfig::default(),
            keybindings: default_keybindings(),
            autostart: Vec::new(),
        }
    }
}

fn default_keybindings() -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("close_window".to_string(), "Super+Q".to_string());
    m.insert("toggle_fullscreen".to_string(), "Super+F".to_string());
    m.insert("toggle_floating".to_string(), "Super+Space".to_string());
    m.insert("cycle_windows".to_string(), "Super+Tab".to_string());
    m.insert("launch_terminal".to_string(), "Super+Return".to_string());
    m.insert("workspace_1".to_string(), "Super+1".to_string());
    m.insert("workspace_2".to_string(), "Super+2".to_string());
    m.insert("workspace_3".to_string(), "Super+3".to_string());
    m.insert("workspace_4".to_string(), "Super+4".to_string());
    m.insert("workspace_5".to_string(), "Super+5".to_string());
    m
}

/// `~/.config/HackerOS-Comp/config.hk`, or `config-<name>.hk` for an
/// `--extern <name>` session (so multiple extern targets launched
/// side-by-side can each carry their own settings instead of fighting
/// over one shared file). Falls back to
/// `/etc/xdg/HackerOS-Comp/config.hk` (system-wide default) if no
/// per-user file exists yet, matching the XDG base-directory
/// convention every other HackerOS config-reading tool already follows.
pub fn config_path_for(extern_name: Option<&str>) -> PathBuf {
    let file_name = match extern_name {
        Some(name) => format!("config-{name}.hk"),
        None => "config.hk".to_string(),
    };
    let user_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("HackerOS-Comp");
    user_dir.join(file_name)
}

/// Loads and validates the config for the given session (`None` =
/// native HWDE session). Never fails outward — a missing file, a
/// malformed file, or an individually malformed field all fall back to
/// (partial or total) defaults, with the problem logged via `tracing`
/// rather than aborting the compositor over a settings file. A running
/// compositor with default settings is always better than one that
/// refused to start because of one bad config line.
pub fn load_for(extern_name: Option<&str>) -> Config {
    let user_path = config_path_for(extern_name);
    let system_path = PathBuf::from("/etc/xdg/HackerOS-Comp")
        .join(user_path.file_name().unwrap_or_default());

    let path = if user_path.exists() {
        user_path
    } else if system_path.exists() {
        system_path
    } else {
        tracing::info!(
            "no config.hk found at {} (or system default) — using built-in defaults",
            user_path.display()
        );
        return Config::default();
    };

    let mut raw = match load_hk_file(&path) {
        Ok(cfg) => cfg,
        Err(err) => {
            let source = std::fs::read_to_string(&path).unwrap_or_default();
            tracing::warn!(
                "failed to parse {}: falling back to defaults\n{}",
                path.display(),
                err.render(&source)
            );
            return Config::default();
        }
    };

    if let Err(err) = resolve_interpolations(&mut raw) {
        tracing::warn!("interpolation error in {}: {}", path.display(), err);
        // Interpolation failing (a bad `${...}` reference/cycle) doesn't
        // invalidate the rest of the file's already-parsed literal
        // values — only the specific `${...}` strings involved stay
        // unresolved. Continue reading the rest of `raw` as-is rather
        // than discarding a whole valid file over one bad reference.
    }

    Config::from_hk(&raw)
}

/// Writes `config` back out to `path` as `.hk`, for anything that wants
/// to persist a runtime-made settings change (a future Settings-app
/// equivalent, `hackerland dispatch`, ...) — not called anywhere yet
/// (nothing in this codebase currently *writes* settings, only reads
/// them), but real and ready for that follow-up rather than a stub.
pub fn save(config: &Config, extern_name: Option<&str>) -> std::io::Result<()> {
    let path = config_path_for(extern_name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let hk = config.to_hk();
    write_hk_file(&path, &hk)
}

impl Config {
    fn from_hk(raw: &HkConfig) -> Config {
        let mut cfg = Config::default();

        if let Some(section) = raw.get("general").and_then(|v| v.as_map().ok()) {
            let g = &mut cfg.general;
            g.gaps_px = num_or(&section, "gaps_px", g.gaps_px as f64) as u32;
            g.border_px = num_or(&section, "border_px", g.border_px as f64) as u32;
            g.focus_follows_mouse = bool_or(&section, "focus_follows_mouse", g.focus_follows_mouse);
            g.default_tiling = bool_or(&section, "default_tiling", g.default_tiling);
        }

        if let Some(section) = raw.get("appearance").and_then(|v| v.as_map().ok()) {
            let a = &mut cfg.appearance;
            a.wallpaper = str_opt(&section, "wallpaper").or_else(|| a.wallpaper.clone());
            a.active_border_color = str_or(&section, "active_border_color", &a.active_border_color);
            a.inactive_border_color = str_or(&section, "inactive_border_color", &a.inactive_border_color);
            a.cursor_theme = str_or(&section, "cursor_theme", &a.cursor_theme);
            a.cursor_size = num_or(&section, "cursor_size", a.cursor_size as f64) as u32;
        }

        if let Some(section) = raw.get("input").and_then(|v| v.as_map().ok()) {
            let i = &mut cfg.input;
            i.keyboard_layout = str_or(&section, "keyboard_layout", &i.keyboard_layout);
            i.keyboard_variant = str_or(&section, "keyboard_variant", &i.keyboard_variant);
            i.repeat_rate = num_or(&section, "repeat_rate", i.repeat_rate as f64) as u32;
            i.repeat_delay = num_or(&section, "repeat_delay", i.repeat_delay as f64) as u32;
            i.natural_scroll = bool_or(&section, "natural_scroll", i.natural_scroll);
            i.tap_to_click = bool_or(&section, "tap_to_click", i.tap_to_click);
            i.mouse_sensitivity = num_or(&section, "mouse_sensitivity", i.mouse_sensitivity);
        }

        if let Some(section) = raw.get("workspaces").and_then(|v| v.as_map().ok()) {
            let w = &mut cfg.workspaces;
            w.count = num_or(&section, "count", w.count as f64).max(1.0) as usize;
            if let Some(HkValue::Array(items)) = section.get("names") {
                w.names = items.iter().filter_map(|v| v.as_string().ok()).collect();
            }
        }

        if let Some(section) = raw.get("keybindings").and_then(|v| v.as_map().ok()) {
            for (action, value) in section.iter() {
                if let Ok(combo) = value.as_string() {
                    cfg.keybindings.insert(action.clone(), combo);
                }
            }
        }

        if let Some(autostart_section) = raw.get("autostart").and_then(|v| v.as_map().ok()) {
            if let Some(items_val) = autostart_section.get("commands") {
                if let Ok(items) = items_val.as_array() {
                    cfg.autostart = items.iter().filter_map(|v| v.as_string().ok()).collect();
                }
            }
        }

        cfg
    }

    fn to_hk(&self) -> HkConfig {
        use indexmap::IndexMap;

        let mut root: HkConfig = IndexMap::new();

        let mut general = IndexMap::new();
        general.insert("gaps_px".to_string(), HkValue::Number(self.general.gaps_px as f64));
        general.insert("border_px".to_string(), HkValue::Number(self.general.border_px as f64));
        general.insert("focus_follows_mouse".to_string(), HkValue::Bool(self.general.focus_follows_mouse));
        general.insert("default_tiling".to_string(), HkValue::Bool(self.general.default_tiling));
        root.insert("general".to_string(), HkValue::Map(general));

        let mut appearance = IndexMap::new();
        if let Some(wp) = &self.appearance.wallpaper {
            appearance.insert("wallpaper".to_string(), HkValue::String(wp.clone()));
        }
        appearance.insert("active_border_color".to_string(), HkValue::String(self.appearance.active_border_color.clone()));
        appearance.insert("inactive_border_color".to_string(), HkValue::String(self.appearance.inactive_border_color.clone()));
        appearance.insert("cursor_theme".to_string(), HkValue::String(self.appearance.cursor_theme.clone()));
        appearance.insert("cursor_size".to_string(), HkValue::Number(self.appearance.cursor_size as f64));
        root.insert("appearance".to_string(), HkValue::Map(appearance));

        let mut input = IndexMap::new();
        input.insert("keyboard_layout".to_string(), HkValue::String(self.input.keyboard_layout.clone()));
        input.insert("keyboard_variant".to_string(), HkValue::String(self.input.keyboard_variant.clone()));
        input.insert("repeat_rate".to_string(), HkValue::Number(self.input.repeat_rate as f64));
        input.insert("repeat_delay".to_string(), HkValue::Number(self.input.repeat_delay as f64));
        input.insert("natural_scroll".to_string(), HkValue::Bool(self.input.natural_scroll));
        input.insert("tap_to_click".to_string(), HkValue::Bool(self.input.tap_to_click));
        input.insert("mouse_sensitivity".to_string(), HkValue::Number(self.input.mouse_sensitivity));
        root.insert("input".to_string(), HkValue::Map(input));

        let mut workspaces = IndexMap::new();
        workspaces.insert("count".to_string(), HkValue::Number(self.workspaces.count as f64));
        workspaces.insert(
            "names".to_string(),
            HkValue::Array(self.workspaces.names.iter().map(|n| HkValue::String(n.clone())).collect()),
        );
        root.insert("workspaces".to_string(), HkValue::Map(workspaces));

        let mut keybindings = IndexMap::new();
        for (action, combo) in &self.keybindings {
            keybindings.insert(action.clone(), HkValue::String(combo.clone()));
        }
        root.insert("keybindings".to_string(), HkValue::Map(keybindings));

        let mut autostart = IndexMap::new();
        autostart.insert(
            "commands".to_string(),
            HkValue::Array(self.autostart.iter().map(|c| HkValue::String(c.clone())).collect()),
        );
        root.insert("autostart".to_string(), HkValue::Map(autostart));

        root
    }
}

// ── Small typed-getter helpers ───────────────────────────────────────────
// Every one of these treats "key missing" and "key present but wrong
// type" identically: fall back to `default`. See this module's doc
// comment for why a per-field fallback (rather than failing the whole
// section) is the deliberate behavior here.

fn str_or(map: &indexmap::IndexMap<String, HkValue>, key: &str, default: &str) -> String {
    map.get(key).and_then(|v| v.as_string().ok()).unwrap_or_else(|| default.to_string())
}

fn str_opt(map: &indexmap::IndexMap<String, HkValue>, key: &str) -> Option<String> {
    map.get(key).and_then(|v| v.as_string().ok())
}

fn num_or(map: &indexmap::IndexMap<String, HkValue>, key: &str, default: f64) -> f64 {
    map.get(key).and_then(|v| v.as_number().ok()).unwrap_or(default)
}

fn bool_or(map: &indexmap::IndexMap<String, HkValue>, key: &str, default: bool) -> bool {
    map.get(key).and_then(|v| v.as_bool().ok()).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = Config::default();
        assert_eq!(cfg.workspaces.count, 5);
        assert!(cfg.general.default_tiling);
        assert_eq!(cfg.keybindings.get("close_window").map(String::as_str), Some("Super+Q"));
    }

    #[test]
    fn parses_a_real_config_hk() {
        // `r##"..."##` (not `r#"..."#`) — the .hk content below
        // includes a literal `"#22c55e"` color value, whose `"#`
        // sequence would otherwise prematurely close a single-hash raw
        // string right there, silently truncating this test fixture
        // and dumping the rest of it into the surrounding Rust source
        // as if it were real code (which is exactly what a `cargo
        // build` against an earlier, `r#"..."#`-delimited version of
        // this exact test caught, as a cascade of "unknown
        // prefix"/"expected `;`" errors on lines that look nothing
        // like the actual bug).
        let src = r##"
[general]
-> gaps_px             => 12
-> border_px            => 3
-> focus_follows_mouse  => true

[appearance]
-> wallpaper             => /usr/share/wallpapers/HackerOS-Wallpapers/Wallpaper1.png
-> active_border_color   => "#22c55e"
-> cursor_size           => 32

[workspaces]
-> count => 4
-> names => ["code", "web", "chat", "media"]

[keybindings]
-> close_window       => Super+Shift+Q
-> launch_terminal    => Super+Return

[autostart]
-> commands => ["blue-panel", "blue-dock"]
"##;
        let mut raw = hk_parser::parse_hk(src).expect("valid .hk");
        hk_parser::resolve_interpolations(&mut raw).expect("no interpolation errors");
        let cfg = Config::from_hk(&raw);

        assert_eq!(cfg.general.gaps_px, 12);
        assert_eq!(cfg.general.border_px, 3);
        assert!(cfg.general.focus_follows_mouse);
        assert_eq!(cfg.appearance.wallpaper.as_deref(), Some("/usr/share/wallpapers/HackerOS-Wallpapers/Wallpaper1.png"));
        assert_eq!(cfg.appearance.active_border_color, "#22c55e");
        assert_eq!(cfg.appearance.cursor_size, 32);
        assert_eq!(cfg.workspaces.count, 4);
        assert_eq!(cfg.workspaces.names, vec!["code", "web", "chat", "media"]);
        assert_eq!(cfg.keybindings.get("close_window").map(String::as_str), Some("Super+Shift+Q"));
        assert_eq!(cfg.autostart, vec!["blue-panel", "blue-dock"]);
        // Fields absent from `src` keep their compiled-in defaults —
        // confirms per-field fallback, not all-or-nothing parsing.
        assert_eq!(cfg.input.keyboard_layout, "us");
    }

    #[test]
    fn malformed_field_falls_back_to_default_without_failing_the_section() {
        // `not_a_number` isn't a number → `num_or` falls back, doesn't
        // panic, and doesn't invalidate the sibling key in the same
        // section (`focus_follows_mouse` still parses normally).
        let src = r#"
[general]
-> gaps_px            => not_a_number
-> focus_follows_mouse => true
"#;
        let raw = hk_parser::parse_hk(src).expect("still syntactically valid .hk");
        let cfg = Config::from_hk(&raw);
        assert_eq!(cfg.general.gaps_px, GeneralConfig::default().gaps_px);
        assert!(cfg.general.focus_follows_mouse);
    }
}
