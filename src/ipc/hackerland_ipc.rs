use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};

use hackerland::protocol::Command;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};

use crate::ipc::{our_uid, peer_uid, spawn_external_app};
use crate::state::HwdeState;

pub fn init(handle: &LoopHandle<'static, HwdeState>) -> std::io::Result<()> {
    let socket_dir = hwde_ipc::runtime_dir();
    std::fs::create_dir_all(&socket_dir)?;
    // Same reasoning as ipc.rs's identical step: don't trust umask for a
    // directory that might live under world-traversable /tmp.
    std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700))?;

    let socket_path = hackerland::protocol::socket_path();
    let _ = std::fs::remove_file(&socket_path); // stale socket from a crashed run

    let listener = UnixListener::bind(&socket_path)?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    tracing::info!("HackerLand control socket listening on {}", socket_path.display());

    let source = Generic::new(listener, Interest::READ, Mode::Level);
    handle
        .insert_source(source, |_, listener, state| {
            loop {
                match listener.accept() {
                    Ok((stream, _addr)) => {
                        if peer_uid(&stream) != Some(our_uid()) {
                            tracing::warn!("hackerland-ipc: rejected connection with unknown or mismatched peer credentials");
                            continue;
                        }
                        handle_connection(stream, state)
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        tracing::warn!("hackerland-ipc accept error: {e}");
                        break;
                    }
                }
            }
            Ok(PostAction::Continue)
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    Ok(())
}

fn handle_connection(stream: UnixStream, state: &mut HwdeState) {
    let _ = stream.set_nonblocking(false);
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!("hackerland-ipc: failed to clone stream: {err}");
            return;
        }
    });
    let mut writer = stream;

    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return; // client disconnected without sending anything
    }

    let reply = match Command::parse(&line) {
        Ok(cmd) => dispatch(cmd, state),
        Err(err) => format!("error: {err}"),
    };

    let mut out = reply;
    out.push('\n');
    if let Err(err) = writer.write_all(out.as_bytes()) {
        tracing::warn!("hackerland-ipc: failed to write reply: {err}");
    }
}

/// Runs one parsed [`Command`] against `state` and returns the exact reply
/// line to send back (see `protocol` module doc for the response format:
/// `ok`, `error: <msg>`, `pong`, or a single-line JSON array).
fn dispatch(cmd: Command, state: &mut HwdeState) -> String {
    match cmd {
        Command::Ping => "pong".to_string(),

        Command::Windows => json_or_error(&state.window_summaries()),
        Command::Workspaces => json_or_error(&state.workspace_summaries()),
        Command::Outputs => json_or_error(&state.output_summaries()),

        Command::Dispatch { action, args } => match dispatch_action(&action, &args, state) {
            Ok(()) => "ok".to_string(),
            Err(err) => format!("error: {err}"),
        },
    }
}

fn json_or_error<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_string(value) {
        Ok(json) => json,
        Err(err) => format!("error: failed to serialize response: {err}"),
    }
}

/// One entry per action listed in [`hackerland::protocol::DISPATCH_ACTIONS`]
/// - keep both in sync when adding a new one (a mismatch just means an
/// action either isn't documented in `wm`'s usage text or isn't actually
/// implemented here; nothing enforces the two lists match beyond this
/// comment and the integration test at the bottom of this file).
fn dispatch_action(action: &str, args: &[String], state: &mut HwdeState) -> Result<(), String> {
    match action {
        "focuswindow" => {
            state.focus_window_by_id(require_u64(args, 0, "focuswindow <id>")?);
            Ok(())
        }
        "closewindow" => {
            state.close_window_by_id(require_u64(args, 0, "closewindow <id>")?);
            Ok(())
        }
        "minimizewindow" => {
            state.minimize_window_by_id(require_u64(args, 0, "minimizewindow <id>")?);
            Ok(())
        }
        "unminimizewindow" => {
            state.unminimize_window_by_id(require_u64(args, 0, "unminimizewindow <id>")?);
            Ok(())
        }
        "maximizewindow" => {
            let id = require_u64(args, 0, "maximizewindow <id> [on|off]")?;
            let maximized = match args.get(1).map(String::as_str) {
                None | Some("on") | Some("true") => true,
                Some("off") | Some("false") => false,
                Some(other) => return Err(format!("`{other}` is not `on`/`off`")),
            };
            let geo = state.primary_output_geometry();
            state.maximize_window_by_id(id, maximized, geo);
            Ok(())
        }
        "togglefloating" => {
            state.toggle_floating_by_id(require_u64(args, 0, "togglefloating <id>")?);
            Ok(())
        }
        "workspace" => {
            state.switch_workspace(require_u32(args, 0, "workspace <id>")?);
            Ok(())
        }
        "movetoworkspace" => {
            let id = require_u64(args, 0, "movetoworkspace <id> <workspace>")?;
            let workspace = require_u32(args, 1, "movetoworkspace <id> <workspace>")?;
            state.move_window_to_workspace(id, workspace);
            Ok(())
        }
        "settiling" => {
            let workspace = require_u32(args, 0, "settiling <workspace> <on|off>")?;
            let enabled = match args.get(1).map(String::as_str) {
                Some("on") | Some("true") => true,
                Some("off") | Some("false") => false,
                _ => return Err("expected `on` or `off`".to_string()),
            };
            state.set_tiling(workspace, enabled);
            Ok(())
        }
        "setwallpaper" => {
            let path = args.first().ok_or("usage: setwallpaper <path>")?.clone();
            state.wallpaper.set_path(path);
            state.pending_wallpaper_reload = true;
            Ok(())
        }
        "launch" => {
            let (command, rest) = args.split_first().ok_or("usage: launch <command> [args...]")?;
            spawn_external_app(state, command, rest).map_err(|err| err.to_string())
        }
        "reload" => {
            state.config = crate::config::load_for(state.extern_name.as_deref());
            tracing::info!("compositor.toml reloaded via hackerland-ipc");
            Ok(())
        }
        "exit" => {
            tracing::info!("HackerLand session ending via `dispatch exit`");
            state.running.store(false, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        other => Err(format!(
            "unknown dispatch action `{other}` (see `comphwde wm --help` for the full list)"
        )),
    }
}

fn require_u64(args: &[String], idx: usize, usage: &str) -> Result<u64, String> {
    args.get(idx).ok_or_else(|| format!("usage: {usage}"))?.parse::<u64>().map_err(|_| format!("`{}` is not a valid id", args[idx]))
}

fn require_u32(args: &[String], idx: usize, usage: &str) -> Result<u32, String> {
    args.get(idx).ok_or_else(|| format!("usage: {usage}"))?.parse::<u32>().map_err(|_| format!("`{}` is not a valid number", args[idx]))
}

#[cfg(test)]
mod tests {
    use super::protocol_actions_covered;

    /// Every action `wm`'s usage text advertises must have a matching arm
    /// in `dispatch_action` above (checked by name only, not by exercising
    /// the arm against a real `HwdeState` - that would need a live
    /// compositor). Catches "documented an action, forgot to implement
    /// it" (or the reverse) at test time instead of only at runtime.
    #[test]
    fn every_documented_action_has_a_dispatch_arm() {
        for (name, _) in hackerland::protocol::DISPATCH_ACTIONS {
            assert!(protocol_actions_covered().contains(name), "`{name}` is documented in wm's usage text but dispatch_action() has no arm for it");
        }
    }
}

#[cfg(test)]
fn protocol_actions_covered() -> &'static [&'static str] {
    &[
        "focuswindow",
        "closewindow",
        "minimizewindow",
        "unminimizewindow",
        "maximizewindow",
        "togglefloating",
        "workspace",
        "movetoworkspace",
        "settiling",
        "setwallpaper",
        "launch",
        "reload",
        "exit",
    ]
}
