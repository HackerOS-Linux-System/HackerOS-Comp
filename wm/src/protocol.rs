use std::path::PathBuf;

/// Where HackerLand's control socket lives. Deliberately a different file
/// than any `hwde_ipc::socket_path_for(..)` result (native HWDE's
/// `comphwde.sock`, or any `--extern-<n>`'s `comphwde-<n>.sock`) even
/// though it shares the same runtime directory - a HackerLand session is
/// its own identity speaking its own protocol, not another `--extern-<n>`
/// target riding on sde-ipc.
pub fn socket_path() -> PathBuf {
    hwde_ipc::runtime_dir().join("hackerland.sock")
}

/// Every `dispatch <action> ...` action comphwde's HackerLand server
/// implements, and the argument shape each expects - kept as one list so
/// the CLI's usage text ([`crate::print_usage`]) and the server's "unknown
/// action" error message can't disagree about what exists.
pub const DISPATCH_ACTIONS: &[(&str, &str)] = &[
    ("focuswindow", "<id>"),
    ("closewindow", "<id>"),
    ("minimizewindow", "<id>"),
    ("unminimizewindow", "<id>"),
    ("maximizewindow", "<id> [on|off]"),
    ("togglefloating", "<id>"),
    ("workspace", "<id>"),
    ("movetoworkspace", "<id> <workspace>"),
    ("settiling", "<workspace> <on|off>"),
    ("setwallpaper", "<path>"),
    ("launch", "<command> [args...]"),
    ("reload", ""),
    ("exit", ""),
];

/// A parsed request line, ready to hand to comphwde's dispatcher (or, on
/// the client side, exactly what got typed on the command line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Ping,
    Windows,
    Workspaces,
    Outputs,
    Dispatch { action: String, args: Vec<String> },
}

impl Command {
    /// Parses one line of the wire format. Used on both ends: the server
    /// parses what a client sent; the CLI client parses its own argv into
    /// the same shape before re-serializing it with [`Command::to_wire`] -
    /// keeping exactly one parser/formatter pair for the whole protocol
    /// rather than the client hand-building strings a different way than
    /// the server expects them.
    pub fn parse(line: &str) -> Result<Command, String> {
        let mut parts = line.trim().split_whitespace();
        match parts.next() {
            Some("ping") => Ok(Command::Ping),
            Some("windows") => Ok(Command::Windows),
            Some("workspaces") => Ok(Command::Workspaces),
            Some("outputs") => Ok(Command::Outputs),
            Some("dispatch") => {
                let action = parts
                    .next()
                    .ok_or_else(|| "`dispatch` requires an action, e.g. `dispatch focuswindow 42`".to_string())?
                    .to_string();
                let args = parts.map(str::to_string).collect();
                Ok(Command::Dispatch { action, args })
            }
            Some(other) => Err(format!(
                "unknown command `{other}` (expected: ping, windows, workspaces, outputs, dispatch <action> [args...])"
            )),
            None => Err("empty command".to_string()),
        }
    }

    /// The exact line to write to the socket for this command - the
    /// inverse of [`Command::parse`].
    pub fn to_wire(&self) -> String {
        match self {
            Command::Ping => "ping".to_string(),
            Command::Windows => "windows".to_string(),
            Command::Workspaces => "workspaces".to_string(),
            Command::Outputs => "outputs".to_string(),
            Command::Dispatch { action, args } => {
                if args.is_empty() {
                    format!("dispatch {action}")
                } else {
                    format!("dispatch {action} {}", args.join(" "))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_query() {
        assert_eq!(Command::parse("ping").unwrap(), Command::Ping);
        assert_eq!(Command::parse("  windows  ").unwrap(), Command::Windows);
        assert_eq!(Command::parse("workspaces").unwrap(), Command::Workspaces);
        assert_eq!(Command::parse("outputs").unwrap(), Command::Outputs);
    }

    #[test]
    fn parses_dispatch_with_and_without_args() {
        assert_eq!(
            Command::parse("dispatch reload").unwrap(),
            Command::Dispatch { action: "reload".to_string(), args: vec![] }
        );
        assert_eq!(
            Command::parse("dispatch movetoworkspace 42 3").unwrap(),
            Command::Dispatch { action: "movetoworkspace".to_string(), args: vec!["42".to_string(), "3".to_string()] }
        );
    }

    #[test]
    fn rejects_bare_dispatch_and_unknown_commands() {
        assert!(Command::parse("dispatch").is_err());
        assert!(Command::parse("frobnicate").is_err());
        assert!(Command::parse("").is_err());
    }

    #[test]
    fn to_wire_is_the_inverse_of_parse() {
        for line in ["ping", "windows", "dispatch reload", "dispatch movetoworkspace 42 3"] {
            let cmd = Command::parse(line).unwrap();
            assert_eq!(cmd.to_wire(), line);
        }
    }
}
