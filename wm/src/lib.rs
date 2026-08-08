pub mod protocol;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use protocol::Command;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

/// Entry point for `comphwde wm <args...>` when `args` is non-empty (an
/// empty `args` means "launch a session instead", which `main.rs` handles
/// itself before ever calling in here - see that module's dispatch).
pub fn run(args: &[String]) -> anyhow::Result<()> {
    let (timeout, rest) = parse_global_flags(args)?;

    let Some(subcommand) = rest.first() else {
        print_usage();
        return Ok(());
    };
    let rest_args = &rest[1..];

    if matches!(subcommand.as_str(), "help" | "-h" | "--help") {
        print_usage();
        return Ok(());
    }

    let command = build_command(subcommand, rest_args)?;

    if matches!(command, Command::Dispatch { ref action, .. } if action == "exit") && !rest_args.iter().any(|a| a == "--yes" || a == "-y") {
        anyhow::bail!("this ends the whole HackerLand session - re-run as `wm exit --yes` to confirm");
    }

    let reply = send(&command, timeout)?;
    print_reply(&command, &reply)
}

/// Turns a CLI subcommand + its own args into a [`Command`]. Most
/// subcommands here map 1:1 onto a `dispatch` action of the same name
/// (see [`protocol::DISPATCH_ACTIONS`]); a handful (`windows`,
/// `workspaces`, `outputs`, `ping`) are queries instead.
fn build_command(subcommand: &str, args: &[String]) -> anyhow::Result<Command> {
    match subcommand {
        "ping" => Ok(Command::Ping),
        "windows" | "list" | "ls" => Ok(Command::Windows),
        "workspaces" => Ok(Command::Workspaces),
        "outputs" => Ok(Command::Outputs),

        // Everything else is `dispatch <this-subcommand-name> <its-args>` -
        // a couple of friendlier aliases map onto the actual action name.
        "focus" => dispatch("focuswindow", args, 1, "wm focus <id>"),
        "close" => dispatch("closewindow", args, 1, "wm close <id>"),
        "minimize" => dispatch("minimizewindow", args, 1, "wm minimize <id>"),
        "unminimize" => dispatch("unminimizewindow", args, 1, "wm unminimize <id>"),
        "maximize" => dispatch("maximizewindow", args, 1, "wm maximize <id> [on|off]"),
        "float" | "togglefloating" => dispatch("togglefloating", args, 1, "wm float <id>"),
        "workspace" | "switch" => dispatch("workspace", args, 1, "wm workspace <id>"),
        "move" | "movetoworkspace" => dispatch("movetoworkspace", args, 2, "wm move <id> <workspace>"),
        "tiling" | "settiling" => dispatch("settiling", args, 2, "wm tiling <workspace> <on|off>"),
        "wallpaper" | "setwallpaper" => dispatch("setwallpaper", args, 1, "wm wallpaper <path>"),
        "launch" => {
            if args.is_empty() {
                anyhow::bail!("usage: wm launch <command> [args...]");
            }
            Ok(Command::Dispatch { action: "launch".to_string(), args: args.to_vec() })
        }
        "reload" => Ok(Command::Dispatch { action: "reload".to_string(), args: vec![] }),
        "exit" | "shutdown" => Ok(Command::Dispatch { action: "exit".to_string(), args: vec![] }),

        other => {
            print_usage();
            anyhow::bail!("unknown `wm` subcommand: `{other}`");
        }
    }
}

fn dispatch(action: &str, args: &[String], min_args: usize, usage: &str) -> anyhow::Result<Command> {
    if args.len() < min_args {
        anyhow::bail!("usage: {usage}");
    }
    Ok(Command::Dispatch { action: action.to_string(), args: args.to_vec() })
}

fn send(command: &Command, timeout: Duration) -> anyhow::Result<String> {
    let path = protocol::socket_path();
    if !path.exists() {
        anyhow::bail!("no HackerLand session is running (expected a socket at {}) - start one with `comphwde wm`", path.display());
    }

    let mut stream = UnixStream::connect(&path)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let mut line = command.to_wire();
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    if reply.trim().is_empty() {
        anyhow::bail!("HackerLand closed the connection without responding");
    }
    Ok(reply.trim().to_string())
}

fn print_reply(command: &Command, reply: &str) -> anyhow::Result<()> {
    if let Some(msg) = reply.strip_prefix("error: ") {
        anyhow::bail!("{msg}");
    }

    match command {
        Command::Ping => println!("{reply}"),
        Command::Windows => print_windows_table(reply),
        Command::Workspaces => print_workspaces_table(reply),
        Command::Outputs => print_outputs_table(reply),
        Command::Dispatch { action, .. } => println!("{action}: {reply}"),
    }
    Ok(())
}

fn print_windows_table(json: &str) {
    let Ok(windows) = serde_json::from_str::<Vec<hwde_ipc::WindowSummary>>(json) else {
        println!("{json}");
        return;
    };
    if windows.is_empty() {
        println!("(no mapped windows)");
        return;
    }
    println!("{:>5}  {:<22} {:<22} {}", "ID", "APP_ID", "TITLE", "FLAGS");
    for w in windows {
        let mut flags = Vec::new();
        if w.is_xwayland {
            flags.push("xwayland");
        }
        if w.is_minimized {
            flags.push("minimized");
        }
        if w.is_maximized {
            flags.push("maximized");
        }
        if w.is_floating {
            flags.push("floating");
        }
        println!("{:>5}  {:<22} {:<22} {}", w.id, truncate(&w.app_id, 22), truncate(&w.title, 22), flags.join(","));
    }
}

fn print_workspaces_table(json: &str) {
    let Ok(workspaces) = serde_json::from_str::<Vec<hwde_ipc::WorkspaceSummary>>(json) else {
        println!("{json}");
        return;
    };
    println!("{:>3}  {:<8} {:<9} {}", "ID", "ACTIVE", "TILING", "WINDOWS");
    for ws in workspaces {
        println!(
            "{:>3}  {:<8} {:<9} {}",
            ws.id,
            if ws.is_active { "yes" } else { "" },
            if ws.is_tiling { "on" } else { "off" },
            ws.window_count
        );
    }
}

fn print_outputs_table(json: &str) {
    let Ok(outputs) = serde_json::from_str::<Vec<hwde_ipc::OutputSummary>>(json) else {
        println!("{json}");
        return;
    };
    println!("{:<10} {:>6} {:>6} {:>6} {:>6} {:>7} {:>9}  {}", "NAME", "X", "Y", "W", "H", "SCALE", "REFRESH", "");
    for o in outputs {
        println!(
            "{:<10} {:>6} {:>6} {:>6} {:>6} {:>7.2} {:>7.2}Hz {}",
            o.name,
            o.x,
            o.y,
            o.width,
            o.height,
            o.scale,
            o.refresh_mhz as f64 / 1000.0,
            if o.is_primary { "(primary)" } else { "" }
        );
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// Pulls `--timeout <ms>` out of `args` wherever it appears, returning
/// `(timeout, remaining_positional_args)`.
fn parse_global_flags(args: &[String]) -> anyhow::Result<(Duration, Vec<String>)> {
    let mut timeout = DEFAULT_TIMEOUT;
    let mut rest = Vec::with_capacity(args.len());

    let mut i = 0;
    while i < args.len() {
        if args[i] == "--timeout" {
            let ms = args
                .get(i + 1)
                .ok_or_else(|| anyhow::anyhow!("--timeout requires a value in milliseconds, e.g. `--timeout 500`"))?
                .parse::<u64>()
                .map_err(|_| anyhow::anyhow!("--timeout expects a whole number of milliseconds"))?;
            timeout = Duration::from_millis(ms);
            i += 2;
        } else {
            rest.push(args[i].clone());
            i += 1;
        }
    }

    Ok((timeout, rest))
}

fn print_usage() {
    let mut actions = String::new();
    for (name, args) in protocol::DISPATCH_ACTIONS {
        actions.push_str(&format!("    dispatch {name} {args}\n"));
    }
    println!(
        "\
HackerLand - comphwde's own window manager (comphwde wm)

USAGE:
    comphwde wm                          launch a HackerLand session
    comphwde wm <SUBCOMMAND> [ARGS...]   control an already-running one

SUBCOMMANDS:
    ping                                check that a session is up
    windows | list | ls                 list mapped windows
    focus <id>                          focus a window
    close <id>                          ask a window to close
    minimize <id> / unminimize <id>     hide / unhide a window
    maximize <id> [on|off]              maximize/unmaximize (default: on)
    float <id>                          toggle floating/tiled
    workspaces                          list workspaces
    workspace <id> | switch <id>        switch the active workspace
    move <id> <workspace>               move a window to another workspace
    tiling <workspace> <on|off>         turn master-stack tiling on/off
    wallpaper <path>                    change the wallpaper at runtime
    launch <command> [args...]          spawn an app inside this session
    outputs                             list monitors
    reload                              re-read compositor.toml
    exit --yes                          end the HackerLand session

GLOBAL FLAGS:
    --timeout <ms>   socket timeout in milliseconds (default: 2000)

Every subcommand above (except the queries at the top) is really just
`dispatch <action> [args...]` sent over HackerLand's own control socket -
the raw actions, if you'd rather script against those directly:
{actions}
Window/workspace ids come from `wm windows` / `wm workspaces`."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_friendly_subcommands_onto_dispatch_actions() {
        assert_eq!(
            build_command("focus", &["42".to_string()]).unwrap(),
            Command::Dispatch { action: "focuswindow".to_string(), args: vec!["42".to_string()] }
        );
        assert_eq!(
            build_command("move", &["42".to_string(), "3".to_string()]).unwrap(),
            Command::Dispatch { action: "movetoworkspace".to_string(), args: vec!["42".to_string(), "3".to_string()] }
        );
        assert_eq!(build_command("windows", &[]).unwrap(), Command::Windows);
        assert_eq!(build_command("ping", &[]).unwrap(), Command::Ping);
    }

    #[test]
    fn rejects_missing_required_args() {
        assert!(build_command("focus", &[]).is_err());
        assert!(build_command("move", &["42".to_string()]).is_err());
    }

    #[test]
    fn exit_requires_explicit_confirmation() {
        let err = run(&["exit".to_string()]).unwrap_err();
        assert!(err.to_string().contains("--yes"), "expected a confirmation-required error, got: {err}");
    }

    #[test]
    fn parses_timeout_flag() {
        let args = vec!["--timeout".to_string(), "500".to_string(), "windows".to_string()];
        let (timeout, rest) = parse_global_flags(&args).unwrap();
        assert_eq!(timeout, Duration::from_millis(500));
        assert_eq!(rest, vec!["windows".to_string()]);
    }
}
