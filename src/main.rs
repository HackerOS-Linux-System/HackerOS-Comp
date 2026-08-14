mod config;
mod extern_ipc;
mod foreign_toplevel;
mod grabs;
mod handlers;
mod hackerland_ipc;
mod input;
mod ipc;
mod render_elements;
mod sde_toplevel_ipc;
mod state;
mod title_text;
mod wallpaper;
mod winit_backend;
#[cfg(feature = "xwayland")]
mod xwayland;
// Feature-gated real TTY/SDDM session backend (DRM/KMS + udev + libinput +
// libseat), wired into `main()` below. Every call in it is checked against
// Smithay 0.7.0 / drm-rs 0.14.1 source directly (see its module doc
// comment), but hasn't been run against real hardware in this environment -
// kept behind `drm-experimental` so a normal build is unaffected either way.
#[cfg(feature = "drm-experimental")]
mod backend_drm;

/// `comphwde` has two run modes, chosen entirely by the presence of a
/// single CLI flag:
///
/// * `comphwde` (no flag) - **native mode**. Same as always: this is
///   HWDE's own compositor, speaking `hwde-ipc` to `starthwde`'s
///   Tauri/SolidJS frontend on `comphwde.sock`.
/// * `comphwde --extern-<name>` - **extern mode**. comphwde still does all
///   the actual Wayland/XWayland compositing, but now namespaces its
///   config/output/wallpaper-env under `<name>` (upper-cased) instead of
///   `HWDE`, and speaks a control protocol appropriate to `<name>` instead
///   of `hwde-ipc` - see "which protocol, for which target" below. This is
///   how *other* desktop environments/shells - SDE (a Slint shell),
///   Hacker Mode, Cybersecurity Mode (a Tauri/Solid.js shell; see
///   `cybersec-mode/` in its own repository - it doesn't speak to a
///   compositor at all yet, but is already wired up to run as
///   `--extern-cybersecurity-mode` the same way the others do, ready for
///   whenever it starts launching real Wayland/XWayland clients) - reuse
///   comphwde as their compositor without comphwde having to know
///   anything about any of them specifically beyond which protocol their
///   `<name>` maps to, and without any of them having to implement a
///   Wayland compositor from scratch.
///
///   **Which protocol, for which target:**
///
///   - `--extern-sde` speaks `wlr-foreign-toplevel-management-unstable-v1`
///     (see `foreign_toplevel.rs` and `sde_toplevel_ipc.rs`) for window
///     listing/activation/close/minimize/maximize - a real,
///     independently-specified Wayland protocol extension, bound directly
///     by SDE's panel/dock on their existing Wayland connection, *not* a
///     socket. `sde-ipc` (see below) keeps running for SDE too, since
///     that protocol has no equivalent for wallpaper/workspaces/
///     `PinSurface`/`LaunchApp`/`Shutdown`/`ReloadConfig` - see
///     `sde_toplevel_ipc.rs`'s module doc for the full picture, and this
///     project's "further work" notes for retiring `sde-ipc` for SDE
///     entirely once those have a replacement too. SDE's session launcher
///     (`startsde`) always runs `comphwde --extern-sde`.
///   - Every `--extern-<name>` (SDE included, for its non-window-management
///     calls - see above; plus Hacker Mode, Cybersecurity Mode, and any
///     future target) speaks `sde-ipc` (see that crate's module docs) on
///     `comphwde-<name>.sock` - `extern_ipc.rs` is the generic server side
///     for all of them, so a new target beyond these doesn't need any
///     changes here at all. Hacker Mode's
///     session launcher (`hacker-mode-session`, in the Hacker-Mode repo)
///     runs `comphwde --extern-hacker-mode` via its own vendored
///     `hacker-mode-ipc` crate (see `hacker-mode/` in this workspace for a
///     reference copy, and that crate's module docs for why Hacker Mode
///     vendors its own separate copy rather than depending on this
///     repository). Cybersecurity Mode's session launcher is expected to
///     run `comphwde --extern-cybersecurity-mode` the same way, once it
///     has one - see `cybersecurity-mode/` in this workspace for its own
///     reference copy of the client side of this same protocol, modeled
///     directly on `hacker-mode/`.
///
/// Native and extern mode are mutually exclusive for a given comphwde
/// process - you get one or the other, never both - so there is exactly
/// one control channel, one config dir and one output name per process,
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

    // `comphwde wm ...`: HackerLand, comphwde's own window manager
    // identity/protocol (see wm/src/lib.rs's module doc - the crate is
    // named `hackerland`, living in this repo's `wm/` directory).
    //
    // * `comphwde wm` (nothing further) - launch a HackerLand session.
    //   Falls straight through into the exact same startup path as native
    //   HWDE mode / `--extern-<n>` mode below, just with `extern_mode`
    //   forced to `hackerland` - same trick `--extern-<n>` already uses for
    //   config/wallpaper namespacing, except the IPC branch in
    //   winit_backend.rs/backend_drm.rs special-cases this name to install
    //   `hackerland_ipc` (HackerLand's own protocol) instead of the
    //   generic sde-ipc-based `extern_ipc`.
    // * `comphwde wm <subcommand> [args...]` - control an *already
    //   running* HackerLand session instead (list/focus/close windows,
    //   switch workspaces, ...) and exit; never opens a display.
    let extern_mode = if args.get(1).map(String::as_str) == Some("wm") {
        if args.len() > 2 {
            return hackerland::run(&args[2..]);
        }
        Some(ExternMode { name: "hackerland".to_string() })
    } else {
        parse_extern_flag(&args)
    };

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
        Some(mode) if mode.name == "hackerland" => {
            tracing::info!("HackerLand starting (wallpaper: {})", wallpaper_path.display())
        }
        Some(mode) => tracing::info!(
            "comphwde starting in extern mode for '{}' (wallpaper: {})",
            mode.name,
            wallpaper_path.display()
        ),
        None => tracing::info!("comphwde starting (wallpaper: {})", wallpaper_path.display()),
    }

    #[cfg(feature = "drm-experimental")]
    {
        // Only take over the whole display from a bare TTY - if we're
        // already inside someone else's Wayland or X11 session (the
        // overwhelmingly common case during development: running comphwde
        // from a terminal inside your regular desktop), grabbing every
        // `/dev/dri` GPU and every input device out from under it via
        // libseat would fight the session that's already running, not
        // replace it. `winit_backend::run` (nested window) is what that
        // situation wants instead, exactly as before this feature existed.
        let nested = std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
        if !nested {
            tracing::info!("no WAYLAND_DISPLAY/DISPLAY set - starting the DRM/udev backend (real TTY session)");
            return backend_drm::run_udev(wallpaper_path, extern_mode);
        }
        tracing::info!("WAYLAND_DISPLAY/DISPLAY set - running nested via winit instead of taking over the DRM/udev session");
    }

    winit_backend::run(wallpaper_path, extern_mode)
}
