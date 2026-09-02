mod state;
mod config;
mod layout;
mod input;
mod input_emulation;
mod portal;
mod render;
mod ipc;
mod xwayland;
mod protocols;

use std::time::Duration;
use smithay::backend::session::Session;
use tracing::{info, error, warn};
use tracing_subscriber::{EnvFilter, fmt};
use std::fs;

/// Single fixed-path log file, not a fresh timestamped file per run
/// (what this used to do — see the git history) — requested so both
/// `hackeros-comp.log` and `blue-environment.log` (the shell's, see
/// Blue-Environment's `logging.rs`) sit next to each other in one place
/// a user can `tail -f` without hunting for the latest timestamp.
///
/// `/var/log` normally needs root to create/write to, and this
/// compositor runs as the logged-in user (a Wayland session isn't
/// root) — so this tries `/var/log/Blue-Environment/` first and, if
/// that's not writable (the common case unless something has already
/// `chown`'d it to the user, e.g. via a packaged tmpfiles.d rule or
/// install script), falls back to the old per-user cache location
/// instead of crashing the compositor over a log path. Either way the
/// *filename* is now fixed (`hackeros-comp.log`, appended across
/// runs) rather than timestamped.
fn log_file_path() -> std::path::PathBuf {
    let system_dir = std::path::PathBuf::from("/var/log/Blue-Environment");
    if fs::create_dir_all(&system_dir).is_ok() {
        // create_dir_all succeeding doesn't guarantee *this user* can
        // write inside it if the directory already existed owned by
        // someone else — verify with an actual write probe.
        let probe = system_dir.join(".write-test");
        if fs::write(&probe, b"").is_ok() {
            let _ = fs::remove_file(&probe);
            return system_dir;
        }
    }
    let home = dirs::home_dir().expect("Home directory not found");
    let fallback = home.join(".cache/Blue-Environment/compositor/logs");
    fs::create_dir_all(&fallback).ok();
    fallback
}

fn init_logging() -> tracing_appender::non_blocking::WorkerGuard {
    let log_dir = log_file_path();
    let using_system_path = log_dir.starts_with("/var/log");

    let file_appender = tracing_appender::rolling::never(&log_dir, "hackeros-comp.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let subscriber = fmt::Subscriber::builder()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("hackeros_comp=info,smithay=warn")),
        )
        .with_writer(non_blocking)
        .with_ansi(false)
        .finish();

    // Previously this was called *after* `main()`'s own `fmt().init()`
    // had already installed a global subscriber — `set_global_default`
    // fails once a subscriber is set, and the old code silently
    // swallowed that error with `.ok()`, so despite appearances the
    // compositor was actually always logging to stderr only, never to
    // the file this function opens. Fixed by making this the *only*
    // place that installs a subscriber (see `main()`, which no longer
    // calls `fmt().init()` itself) and by not swallowing the error here
    // either — a genuine second-install attempt is a real bug worth
    // panicking on, not silently ignoring again.
    tracing::subscriber::set_global_default(subscriber)
        .expect("no other tracing subscriber should be installed before init_logging() runs");

    if using_system_path {
        info!("Logging to {}/hackeros-comp.log", log_dir.display());
    } else {
        warn!(
            "/var/log/Blue-Environment not writable by this user — logging to {}/hackeros-comp.log instead. \
             To use the system path, pre-create it writable by this user, e.g.: \
             sudo install -d -m 0775 -o $USER -g $USER /var/log/Blue-Environment",
            log_dir.display()
        );
    }

    guard
}

fn write_desktop_file() -> std::io::Result<()> {
    let exe_path = std::env::current_exe()?;
    let desktop_path = dirs::home_dir()
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "No home dir")
        })?
        .join(".local/share/wayland-sessions/blue-environment.desktop");

    fs::create_dir_all(desktop_path.parent().unwrap())?;
    let content = format!(
        "[Desktop Entry]\nName=Blue Environment\nComment=Blue Wayland Compositor\nExec={}\nType=Application\nCategories=System;\n",
        exe_path.display()
    );
    fs::write(desktop_path, content)
}

// ── CLI ──────────────────────────────────────────────────────────────────
// `hackeros-comp --help` for the user-facing version of this list.
//
// Five things this binary can be asked to do:
//   1. (no arguments) — start a normal HWDE session for Blue
//      Environment: autostarts whatever `config.hk`'s `[autostart]`
//      section lists (see `src/config.rs`) once the compositor's
//      socket is up.
//   2. `--extern-cybersecurity-mode` / `--extern-penetration-mode` /
//      `--extern-hacker-mode` — the same kind of session, but reading
//      `config-<name>.hk` instead of `config.hk` and speaking under a
//      separate HackerLand socket name (see `config.rs`'s
//      `config_path_for`) — one of this compositor's three named
//      alternate identities, not an arbitrary `--extern-<anything>`.
//   3. `wm [hackerland-args...]` — **not** a new compositor session.
//      Delegates straight to `hackerland::run` (the `wm` crate's own
//      client-mode CLI — `windows`/`workspaces`/`outputs`/`dispatch
//      <action>`), which talks over HackerLand's control socket to an
//      *already-running* `hackeros-comp` session. Running `hackeros-comp
//      wm` with no compositor already running just fails to connect to
//      that socket, the same as running `hackerland` directly would.
//   4. `one <command> [args...]` — a single-app kiosk session, in the
//      spirit of `cage`: starts a fresh compositor (TTY fullscreen, or
//      nested if already inside a display session, same auto-detection
//      as the default session), spawns exactly `<command> [args...]`
//      as its only client, and exits the whole compositor the moment
//      that process exits — there is no "close the window and keep the
//      session running" for `one`, matching `cage`'s own kiosk
//      semantics.
//   5. `custom <command> [args...]` — a normal, full (not single-app-
//      exiting) compositor session, just autostarting `<command>
//      [args...]` instead of whatever `config.hk`'s `[autostart]`
//      section lists — for running a custom app/tool environment under
//      this compositor without it being a Blue Environment session.
//
// No external argument-parsing crate — this is a small, fixed set of
// shapes, matching the "handrolled `--extern` scan" this replaces
// (which was already too limited to express three of these five
// modes).

const KNOWN_EXTERN_TARGETS: &[&str] = &["cybersecurity-mode", "penetration-mode", "hacker-mode"];

enum CliCommand {
    Help,
    /// Delegates to `hackerland::run` — see this section's doc, item 3.
    Wm(Vec<String>),
    /// A compositor session — the default (`extern_name: None`, `startup:
    /// None`) or one of the three named `--extern-*` targets, `one`, or
    /// `custom` (all of which set `startup` to spawn something other
    /// than `config.hk`'s own `[autostart]` list).
    Session { extern_name: Option<String>, startup: Option<StartupOverride> },
}

/// What `one`/`custom` spawn instead of `config.hk`'s `[autostart]`
/// list, and whether the whole compositor should exit once it does —
/// see this section's doc, items 4 and 5, for the actual difference
/// between the two.
struct StartupOverride {
    command: String,
    args: Vec<String>,
    exit_when_it_exits: bool,
}

fn parse_cli(argv: &[String]) -> CliCommand {
    match argv.first().map(String::as_str) {
        None => CliCommand::Session { extern_name: None, startup: None },
        Some("--help") | Some("-h") => CliCommand::Help,
        Some("wm") => CliCommand::Wm(argv[1..].to_vec()),
        Some("one") | Some("custom") => {
            let exit_when_it_exits = argv[0] == "one";
            let Some(command) = argv.get(1) else {
                eprintln!("usage: hackeros-comp {} <command> [args...]", argv[0]);
                std::process::exit(1);
            };
            CliCommand::Session {
                extern_name: None,
                startup: Some(StartupOverride {
                    command: command.clone(),
                    args: argv[2..].to_vec(),
                    exit_when_it_exits,
                }),
            }
        }
        Some(flag) if flag.starts_with("--extern-") => {
            let name = &flag["--extern-".len()..];
            if !KNOWN_EXTERN_TARGETS.contains(&name) {
                eprintln!(
                    "unknown extern target '{name}' — known targets: {}",
                    KNOWN_EXTERN_TARGETS.join(", ")
                );
                std::process::exit(1);
            }
            CliCommand::Session { extern_name: Some(name.to_string()), startup: None }
        }
        Some(other) => {
            eprintln!("unknown argument: {other}\n\nSee `hackeros-comp --help` for usage.");
            std::process::exit(1);
        }
    }
}

fn print_help() {
    println!(
        r#"hackeros-comp — HackerOS's Wayland compositor for Blue Environment

USAGE:
    hackeros-comp                          Start a normal Blue Environment session
                                            (autostarts config.hk's [autostart] list)
    hackeros-comp --extern-cybersecurity-mode
    hackeros-comp --extern-penetration-mode
    hackeros-comp --extern-hacker-mode     Start a session under one of this
                                            compositor's three named alternate
                                            identities (config-<name>.hk, its own
                                            HackerLand socket name)
    hackeros-comp wm [ARGS...]             Talk to an already-running session's
                                            HackerLand control socket (windows /
                                            workspaces / outputs / dispatch <action>)
                                            — does not start a new session itself
    hackeros-comp one <COMMAND> [ARGS...]  Single-app kiosk session (cage-style):
                                            runs COMMAND as the only client,
                                            fullscreen, exits when it exits
    hackeros-comp custom <COMMAND> [ARGS...]
                                            Normal compositor session, autostarting
                                            COMMAND instead of config.hk's own
                                            [autostart] list

    -h, --help                             Print this help and exit
"#
    );
}

/// Spawns `command args...` and, if `exit_when_it_exits` is set, spawns a
/// background thread that blocks on that one child's exit and then flips
/// `running` to `false` — `run_loop`'s own `state.should_exit()` check
/// (which already ORs in `!running.load(..)` — see `state/mod.rs`) picks
/// that up on its next iteration and shuts the compositor down cleanly,
/// no new plumbing needed beyond the `Arc<AtomicBool>` that already
/// existed for exactly this kind of "something outside the event loop
/// wants to stop it" signal.
fn spawn_one_shot_startup(startup: &StartupOverride, running: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    match std::process::Command::new(&startup.command).args(&startup.args).spawn() {
        Ok(mut child) => {
            info!("Spawned '{}' as the session's client", startup.command);
            if startup.exit_when_it_exits {
                std::thread::spawn(move || {
                    let _ = child.wait();
                    info!("Kiosk client exited — shutting down compositor (hackeros-comp one)");
                    running.store(false, std::sync::atomic::Ordering::Relaxed);
                });
            }
        }
        Err(e) => {
            error!("Failed to spawn '{}': {e}", startup.command);
            if startup.exit_when_it_exits {
                // A kiosk session that failed to launch its one and only
                // client has nothing left to do — exiting immediately
                // (rather than sitting there running a compositor with
                // no client and no way to start one) matches `cage`'s
                // own behavior on a launch failure.
                std::process::exit(1);
            }
        }
    }
}

/// Splits `Config.autostart.commands`' entries (each a full command
/// line, e.g. `"blue-panel --minimal"`) into `(program, args)` — plain
/// whitespace splitting, deliberately: no quoting/escaping support, so
/// an autostart entry needing a literal space inside one argument isn't
/// expressible today. Real, separate follow-up work if that's ever
/// needed (a small shell-word-splitting routine, not a full shell);
/// every default/example `config.hk` this project ships only lists bare
/// program names, which this already handles correctly.
fn split_command_line(line: &str) -> Option<(String, Vec<String>)> {
    let mut parts = line.split_whitespace().map(str::to_string);
    let program = parts.next()?;
    Some((program, parts.collect()))
}

/// Called once the compositor's socket + env vars are ready, in both
/// `run_udev` and `run_winit` — decides what to spawn based on
/// `startup`:
///   - `Some(override_)` (from `hackeros-comp one`/`custom`) — spawns
///     exactly that one command via [`spawn_one_shot_startup`].
///   - `None` (the default session, or an `--extern-*` one) — spawns
///     every entry in `state.config.autostart.commands` (see
///     `src/config.rs`) — previously parsed out of `config.hk` and then
///     never actually used anywhere, which this fixes.
fn launch_startup_clients(state: &state::BlueState, startup: Option<StartupOverride>) {
    match startup {
        Some(override_) => spawn_one_shot_startup(&override_, state.running.clone()),
        None => {
            for line in &state.config.autostart.clone() {
                let Some((program, args)) = split_command_line(line) else { continue };
                match std::process::Command::new(&program).args(&args).spawn() {
                    Ok(_) => info!("Autostarted '{line}'"),
                    Err(e) => warn!("Failed to autostart '{line}': {e}"),
                }
            }
        }
    }
}

fn main() {
    // `init_logging()` is now the only subscriber install — see its
    // doc comment for why this used to double-install (a stderr one
    // here, then a silently-failing file one) and actually only ever
    // logged to stderr despite appearances. The returned guard has to
    // stay alive for the whole process (dropping it flushes and closes
    // the non-blocking writer), hence binding it here instead of
    // discarding it.
    let _log_guard = init_logging();

    let argv: Vec<String> = std::env::args().skip(1).collect();
    match parse_cli(&argv) {
        CliCommand::Help => {
            print_help();
        }
        CliCommand::Wm(wm_args) => {
            // Client mode only — see this file's CLI doc, item 3. Does
            // not touch logging/session setup below at all; a
            // successful `hackerland::run` already printed whatever
            // the person asked for and returns normally.
            if let Err(e) = hackerland::run(&wm_args) {
                eprintln!("hackerland: {e}");
                std::process::exit(1);
            }
        }
        CliCommand::Session { extern_name, startup } => {
            info!("HackerOS-Comp v0.2 starting...");
            if let Some(name) = &extern_name {
                info!("Launching as extern target '{name}'");
            }

            if let Err(e) = write_desktop_file() {
                warn!("Could not write desktop file: {}", e);
            }

            // Detect if we already have a display server
            let has_wayland = std::env::var("WAYLAND_DISPLAY").is_ok();
            let has_x11 = std::env::var("DISPLAY").is_ok();

            if has_wayland || has_x11 {
                warn!(
                    "Existing display session detected (wayland={}, x11={}) - running nested (winit)",
                    has_wayland, has_x11
                );
                run_winit(extern_name, startup);
            } else {
                info!("No display server found - using DRM/KMS backend (TTY mode)");
                run_udev(extern_name, startup);
            }
        }
    }
}

// ── DRM/KMS backend (production, bare-metal / TTY) ─────────────────────────

fn run_udev(extern_name: Option<String>, startup: Option<StartupOverride>) {
    use smithay::backend::session::libseat::LibSeatSession;

    // `LibSeatSession` (this whole function) goes through libseat, which
    // is itself a seat-management abstraction with *two* real backends:
    // systemd-logind (talks to org.freedesktop.login1 over D-Bus — no
    // extra daemon needed, just systemd itself) and seatd (a standalone
    // daemon, for non-systemd systems or systemd setups without a login1
    // session). libseat already auto-selects between them at the C
    // library level — nothing in this function needs to branch on it —
    // but the compositor was previously silent about *which* one it'll
    // get, and its one-size-fits-all failure message ("install seatd")
    // was actively wrong advice on a systemd-logind system where seatd
    // was never the problem in the first place. This detects which path
    // is actually available up front so both the success log line and
    // any failure message are accurate for the system actually running.
    let has_systemd = std::path::Path::new("/run/systemd/system").exists();
    let has_logind_session = has_systemd && std::env::var("XDG_SESSION_ID").is_ok();
    let has_seatd_socket = std::path::Path::new("/run/seatd.sock").exists();

    if has_logind_session {
        info!("systemd-logind session detected (XDG_SESSION_ID set) — libseat will use it, no seatd needed");
    } else if has_seatd_socket {
        info!("No systemd-logind session found — using seatd ({})", "/run/seatd.sock");
    } else if has_systemd {
        info!("systemd is running but no XDG_SESSION_ID is set (not launched from a logind session) \
               and no seatd socket found — session setup will likely fail. \
               Launch from a logind session (e.g. a logind-managed TTY login) or start seatd.");
    } else {
        info!("No systemd detected — this requires seatd (no /run/seatd.sock found yet; is it running?)");
    }

    let (session, notifier) = match LibSeatSession::new() {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create libseat session: {}", e);
            if has_systemd {
                error!("This system has systemd — either:");
                error!("  run from a real logind session (log in via a display/login manager or a TTY logind handles), or");
                error!("  install and enable seatd: sudo systemctl enable --now seatd, then add yourself to its group: sudo usermod -aG seat $USER");
            } else {
                error!("No systemd on this system — seatd is required: sudo systemctl enable --now seatd (or the equivalent for your init system)");
                error!("And add yourself to seat group: sudo usermod -aG seat $USER");
            }
            std::process::exit(1);
        }
    };

    info!("Seat: {}", session.seat());

    let event_loop: calloop::EventLoop<'static, state::BlueState> =
        calloop::EventLoop::try_new().expect("Failed to create event loop");
    let display: wayland_server::Display<state::BlueState> =
        wayland_server::Display::new().expect("Failed to create Wayland display");

    let loop_handle = event_loop.handle();
    let mut st = state::BlueState::new_with_extern_name(&loop_handle, display, extern_name.clone());

    // Register session notifier - relays libseat session events (VT
    // switch, device pause/resume) into the event loop.
    loop_handle
        .insert_source(notifier, |event, _, state| {
            state.handle_session_event(event);
        })
        .expect("Failed to insert session notifier");

    st.init_udev(session, &loop_handle);

    // Start XWayland
    if let Err(e) = st.init_xwayland(&loop_handle) {
        error!("XWayland failed to start: {} - X11 apps will not work", e);
    }

    st.init_ipc(&loop_handle);
    if let Err(e) = crate::ipc::init_hackerland_ipc(&loop_handle) {
        warn!("HackerLand control socket failed to start: {e}");
    }
    // `--extern-<name>` control socket (the protocol `--extern-hacker-mode`/
    // `--extern-cybersecurity-mode`/`--extern-penetration-mode` sessions are
    // actually driven over — see `src/ipc/extern_ipc.rs` and
    // `src/ipc/protocol.rs`). Only opened for an `--extern-*` session, same
    // as `extern_name` itself already only being `Some` in that case — a
    // plain default session has no `--extern-<name>` client to serve this
    // socket to, so there's nothing to open. Not fatal if it fails: a
    // session that fails to expose this socket still has a perfectly usable
    // Wayland compositor underneath it, just without external process
    // control (e.g. `hacker-mode-session`'s `hacker-mode-ipcctl launch`)
    // able to reach it — same "warn, don't exit" treatment already given to
    // `init_hackerland_ipc` and the EIS socket below.
    if let Some(name) = extern_name.clone() {
        if let Err(e) = crate::ipc::init_extern_ipc(&loop_handle, name) {
            warn!("extern-ipc control socket failed to start: {e}");
        }
    }

    // EIS input-emulation socket — see input_emulation module doc for
    // scope. Not fatal if it fails (e.g. no writable XDG_RUNTIME_DIR in
    // some exotic sandbox) — the rest of the compositor works fine
    // without it, this is strictly additive.
    match input_emulation::init(&mut st, &loop_handle) {
        Ok(eis_path) => {
            // D-Bus portal backend — see portal/mod.rs's module doc for
            // exactly what this does and its one real, load-bearing
            // caveat (no consent UI). Only started once the EIS socket
            // itself actually exists, since ConnectToEIS has nothing to
            // bridge to otherwise.
            portal::init(eis_path);
        }
        Err(e) => {
            warn!("EIS input-emulation socket failed to start (remote-input clients won't be able to connect): {e}");
        }
    }

    // Idle / DPMS timer
    protocols::idle::init_idle(&st, &loop_handle);

    let socket = st.socket_name().to_string();
    info!("Compositor ready - WAYLAND_DISPLAY={}", socket);

    // Export environment so spawned apps can find us
    std::env::set_var("WAYLAND_DISPLAY", &socket);
    if let Some(xdisp) = st.x11_display {
        std::env::set_var("DISPLAY", format!(":{}", xdisp));
        info!("XWayland on DISPLAY=:{}", xdisp);
    }

    // Set XDG_SESSION_TYPE so apps use Wayland protocols
    std::env::set_var("XDG_SESSION_TYPE", "wayland");
    std::env::set_var("XDG_CURRENT_DESKTOP", "Blue");

    launch_startup_clients(&st, startup);

    run_loop(event_loop, st);
}

// ── Winit backend (nested, dev/VM) ─────────────────────────────────────────

fn run_winit(extern_name: Option<String>, startup: Option<StartupOverride>) {
    use smithay::backend::winit;

    let event_loop: calloop::EventLoop<'static, state::BlueState> =
        calloop::EventLoop::try_new().expect("Failed to create event loop");
    let display: wayland_server::Display<state::BlueState> =
        wayland_server::Display::new().expect("Failed to create Wayland display");

    let loop_handle = event_loop.handle();
    let mut st = state::BlueState::new_with_extern_name(&loop_handle, display, extern_name.clone());

    // `winit::init` always builds an EGL context against the *host*
    // GL/GLES driver stack (checked against smithay's own
    // `backend::winit` source at the pinned rev — it creates a raw
    // window and immediately wraps it in an `EGLDisplay`/`EGLContext`,
    // there's no non-EGL code path in it at all), so unlike the udev/TTY
    // backend there is no "hand-roll a Pixman-and-dumb-buffers path
    // instead" fallback available here — there's no DRM/KMS device to
    // allocate dumb buffers on in the first place, only a host window.
    // The real, working GPU-less fallback for *this* backend is a
    // software GL driver satisfying the same EGL/GLES2 API winit already
    // asks for — Mesa's llvmpipe, opted into via `LIBGL_ALWAYS_SOFTWARE`
    // (this is the standard, well-known way to get a nested
    // Wayland/X11 compositor running in a GPU-less CI runner or VM; not
    // specific to this codebase).
    //
    // Only kicks in on the first attempt's failure, and only if the
    // variable wasn't already set to something (so an explicit
    // `LIBGL_ALWAYS_SOFTWARE=0` from the person running this to force
    // *real* GPU use and get a clear error instead of a silent
    // llvmpipe fallback is respected, not overridden).
    let (winit_backend, winit_evt) =
        match winit::init::<smithay::backend::renderer::gles::GlesRenderer>() {
            Ok(pair) => pair,
            Err(e) if std::env::var_os("LIBGL_ALWAYS_SOFTWARE").is_none() => {
                warn!(
                    "winit backend init failed ({e}) — retrying with Mesa's llvmpipe \
                     software rasterizer (LIBGL_ALWAYS_SOFTWARE=1); this is expected on \
                     a GPU-less host (CI runner, VM without GPU passthrough) and will \
                     work, just noticeably slower than real GPU acceleration"
                );
                std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
                match winit::init::<smithay::backend::renderer::gles::GlesRenderer>() {
                    Ok(pair) => pair,
                    Err(e2) => {
                        error!(
                            "winit backend init still failed after forcing llvmpipe ({e2}) — \
                             this host likely has no usable GL/GLES driver at all (not even \
                             software), or no host Wayland/X11 display to nest inside \
                             (WAYLAND_DISPLAY/DISPLAY unset?). Original error: {e}"
                        );
                        std::process::exit(1);
                    }
                }
            }
            Err(e) => {
                error!(
                    "winit backend init failed ({e}) and LIBGL_ALWAYS_SOFTWARE is already \
                     set, so not retrying with a different value — unset it to allow an \
                     automatic software-rendering fallback"
                );
                std::process::exit(1);
            }
        };

    st.init_winit(winit_backend, winit_evt, &loop_handle);

    if let Err(e) = st.init_xwayland(&loop_handle) {
        error!("XWayland failed: {} - X11 apps unavailable", e);
    }

    st.init_ipc(&loop_handle);
    if let Err(e) = crate::ipc::init_hackerland_ipc(&loop_handle) {
        warn!("HackerLand control socket failed to start: {e}");
    }
    // `--extern-<name>` control socket — see `run_udev`'s identical block
    // above for the full explanation; nested (winit) sessions need this
    // exactly as much as bare-metal ones do (e.g. a developer running
    // `hacker-mode-session` from inside an existing desktop session for
    // testing — the same `--extern-hacker-mode` client either way).
    if let Some(name) = extern_name.clone() {
        if let Err(e) = crate::ipc::init_extern_ipc(&loop_handle, name) {
            warn!("extern-ipc control socket failed to start: {e}");
        }
    }

    // EIS input-emulation socket — see input_emulation module doc for
    // scope. Not fatal if it fails (e.g. no writable XDG_RUNTIME_DIR in
    // some exotic sandbox) — the rest of the compositor works fine
    // without it, this is strictly additive.
    match input_emulation::init(&mut st, &loop_handle) {
        Ok(eis_path) => {
            // D-Bus portal backend — see portal/mod.rs's module doc for
            // exactly what this does and its one real, load-bearing
            // caveat (no consent UI). Only started once the EIS socket
            // itself actually exists, since ConnectToEIS has nothing to
            // bridge to otherwise.
            portal::init(eis_path);
        }
        Err(e) => {
            warn!("EIS input-emulation socket failed to start (remote-input clients won't be able to connect): {e}");
        }
    }

    // Idle / DPMS timer
    protocols::idle::init_idle(&st, &loop_handle);

    let socket = st.socket_name().to_string();
    info!("Nested compositor ready - WAYLAND_DISPLAY={}", socket);
    std::env::set_var("WAYLAND_DISPLAY", &socket);
    std::env::set_var("XDG_SESSION_TYPE", "wayland");
    std::env::set_var("XDG_CURRENT_DESKTOP", "Blue");

    if let Some(xdisp) = st.x11_display {
        std::env::set_var("DISPLAY", format!(":{}", xdisp));
    }

    launch_startup_clients(&st, startup);

    run_loop(event_loop, st);
}

// ── Main event loop ────────────────────────────────────────────────────────

fn run_loop(
    mut event_loop: calloop::EventLoop<'static, state::BlueState>,
    mut state: state::BlueState,
) {
    loop {
        // 16ms ≈ 60 fps budget for the compositor tick
        if let Err(e) = event_loop.dispatch(Some(Duration::from_millis(16)), &mut state) {
            error!("Event loop dispatch error: {}", e);
            break;
        }

        state.refresh();

        if state.should_exit() {
            info!("Compositor exit requested");
            break;
        }
    }

    info!("HackerOS-Comp stopped");
}
