mod config;
mod extern_ipc;
mod grabs;
mod handlers;
mod input;
mod ipc;
mod render_elements;
mod state;
mod wallpaper;
mod winit_backend;
#[cfg(feature = "xwayland")]
mod xwayland;
// Experimental, feature-gated, NOT wired into `main` yet - see its module
// doc comment for exactly what's missing before this could drive a real
// TTY/SDDM session. Kept compiling (behind a feature flag) so it doesn't
// bit-rot while that work is planned.
#[cfg(feature = "drm-experimental")]
mod backend_drm;

/// `comphwde` has two run modes, chosen entirely by the presence of a
/// single CLI flag:
///
/// * `comphwde` (no flag) - **native mode**. Same as always: this is
///   HWDE's own compositor, speaking `hwde-ipc` to `starthwde`'s
///   Tauri/SolidJS frontend on `comphwde.sock`.
/// * `comphwde --extern-<name>` - **extern mode**. comphwde still does all
///   the actual Wayland/XWayland compositing, but now speaks `sde-ipc`
///   (see that crate's module docs) on `comphwde-<name>.sock` instead,
///   and namespaces its config/output/wallpaper-env under `<name>`
///   (upper-cased) instead of `HWDE`. This is how a *different* desktop
///   environment - SDE, a Slint shell, is the one shipped in this
///   repository - reuses comphwde as its compositor without comphwde
///   having to know anything about SDE specifically, and without SDE
///   having to implement a Wayland compositor from scratch.
///
///   SDE's session launcher (`startsde`) always runs `comphwde
///   --extern-sde`; Hacker Mode's session launcher
///   (`hacker-mode-session`, in the Hacker-Mode repo) always runs
///   `comphwde --extern-hacker-mode` the same way, via its own vendored
///   `hacker-mode-ipc` crate (a thin wrapper around the same protocol
///   `sde-ipc` implements - see `hacker-mode/` in this workspace for a
///   reference copy, and that crate's module docs for why Hacker Mode
///   vendors its own separate copy rather than depending on this
///   repository). The mechanism isn't hardcoded to either name, so a
///   third extern target doesn't need any changes here either.
///
/// Native and extern mode are mutually exclusive for a given comphwde
/// process - you get one or the other, never both - so there is exactly
/// one control socket, one config dir and one output name per process,
/// same as before this flag existed.
#[derive(Debug, Clone)]
pub struct ExternMode {
    /// Normalized (lowercase, alnum/dash) name, e.g. `"sde"`.
    pub name: String,
}

fn parse_extern_flag(args: &[String]) -> Option<ExternMode> {
    args.iter().find_map(|arg| {
        arg.strip_prefix("--extern-").map(|name| ExternMode {
            name: name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').map(|c| c.to_ascii_lowercase()).collect(),
        })
    })
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let extern_mode = parse_extern_flag(&args);

    // Wallpaper env var is namespaced the same way config/output are:
    // `HWDE_WALLPAPER` in native mode, `<NAME>_WALLPAPER` in extern mode
    // (e.g. `SDE_WALLPAPER`), falling back to `HWDE_WALLPAPER` too so an
    // extern shell doesn't *have* to set its own var if it's happy
    // sharing HWDE's configured wallpaper.
    let wallpaper_env_var = match &extern_mode {
        Some(mode) => format!("{}_WALLPAPER", mode.name.to_uppercase()),
        None => "HWDE_WALLPAPER".to_string(),
    };
    let wallpaper_path = std::env::var(&wallpaper_env_var)
        .or_else(|_| std::env::var("HWDE_WALLPAPER"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(wallpaper::DEFAULT_WALLPAPER));

    match &extern_mode {
        Some(mode) => tracing::info!(
            "comphwde starting in extern mode for '{}' (wallpaper: {})",
            mode.name,
            wallpaper_path.display()
        ),
        None => tracing::info!("comphwde starting (wallpaper: {})", wallpaper_path.display()),
    }

    winit_backend::run(wallpaper_path, extern_mode)
}
