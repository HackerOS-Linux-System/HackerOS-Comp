use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::rc::Rc;
use std::time::Duration;

use sde_ipc::{PinnedEdge as SdePinnedEdge, SdeCall, SdeEvent, SdeEventMessage, SdeOutcome, SdeRequest, SdeResponse, SdeResult};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};

use crate::state::{HwdeState, PinnedEdge};

/// How often the diff tick re-checks window/workspace state for
/// subscribers - see the module doc comment.
const DIFF_TICK: Duration = Duration::from_millis(50);

type Subscribers = Rc<RefCell<Vec<UnixStream>>>;

/// Last-broadcast state, compared against on every [`DIFF_TICK`] to decide
/// whether there's anything new to send. Lives for the lifetime of the
/// diff timer closure (one per `init()` call, i.e. one per comphwde
/// process) - not shared with anything else, so a plain owned struct
/// captured by the timer closure is enough; no `Rc`/`RefCell` needed here
/// (unlike `subscribers`, which the accept-loop closure also touches).
#[derive(Default)]
struct LastBroadcast {
    windows: Vec<sde_ipc::SdeWindowInfo>,
    workspaces: Vec<sde_ipc::SdeWorkspaceInfo>,
}

/// Starts listening on `sde_ipc::socket_path_for(extern_name)`, and starts
/// the [`DIFF_TICK`] event-broadcast timer alongside it. Otherwise
/// identical in shape to `ipc::init` - see that function's comments for
/// the reasoning behind the directory/socket permission handling, which
/// applies here unchanged (extern mode is no less a local-only control
/// channel than native mode is).
pub fn init(handle: &LoopHandle<'static, HwdeState>, extern_name: String) -> std::io::Result<()> {
    let socket_dir = sde_ipc::runtime_dir();
    std::fs::create_dir_all(&socket_dir)?;
    std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700))?;

    let socket_path = sde_ipc::socket_path_for(&extern_name);
    let _ = std::fs::remove_file(&socket_path);

    let listener = UnixListener::bind(&socket_path)?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    tracing::info!("comphwde sde-ipc listening on {} (extern target: {extern_name})", socket_path.display());

    let subscribers: Subscribers = Rc::new(RefCell::new(Vec::new()));

    let subs_for_accept = subscribers.clone();
    let source = Generic::new(listener, Interest::READ, Mode::Level);
    handle
        .insert_source(source, move |_, listener, state| {
            loop {
                match listener.accept() {
                    Ok((stream, _addr)) => handle_connection(stream, state, &subs_for_accept),
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(err) => {
                        tracing::warn!("sde-ipc: accept failed: {err}");
                        break;
                    }
                }
            }
            Ok(PostAction::Continue)
        })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("failed to register sde-ipc listener: {e}")))?;

    let subs_for_timer = subscribers;
    let mut last = LastBroadcast::default();
    handle
        .insert_source(Timer::from_duration(DIFF_TICK), move |_, _, state| {
            broadcast_if_changed(state, &subs_for_timer, &mut last);
            TimeoutAction::ToDuration(DIFF_TICK)
        })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("failed to register sde-ipc diff timer: {e}")))?;

    Ok(())
}

fn handle_connection(stream: UnixStream, state: &mut HwdeState, subscribers: &Subscribers) {
    let _ = stream.set_nonblocking(false);
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!("sde-ipc: failed to clone stream: {err}");
            return;
        }
    });
    let writer = stream;

    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }

    let request = match serde_json::from_str::<SdeRequest>(line.trim()) {
        Ok(req) => req,
        Err(err) => {
            // We don't know the request id if parsing failed entirely -
            // best effort: echo back 0, which no real request should ever
            // use (`sde_ipc::call`'s ids start at 1), so a client that
            // happens to check will at least see a mismatch rather than a
            // false positive match.
            send(&writer, &SdeResponse { id: 0, outcome: SdeOutcome::Err { message: format!("malformed request: {err}") } });
            return;
        }
    };

    if matches!(request.call, SdeCall::Subscribe) {
        // Acknowledge, then keep the connection open as an event stream
        // instead of closing it like every other call does - see the
        // module doc comment and `sde-ipc`'s "Push events" section.
        if !send(&writer, &SdeResponse { id: request.id, outcome: SdeOutcome::Ok { result: SdeResult::None } }) {
            return;
        }
        match writer.try_clone() {
            Ok(subscriber_stream) => {
                let _ = subscriber_stream.set_nonblocking(true);
                subscribers.borrow_mut().push(subscriber_stream);
                tracing::info!("sde-ipc: new subscriber ({} total)", subscribers.borrow().len());
            }
            Err(err) => tracing::warn!("sde-ipc: failed to register subscriber: {err}"),
        }
        // `writer`/`reader` drop here, but the socket itself stays open
        // because `subscribers` holds a cloned file descriptor to it.
        return;
    }

    let outcome = dispatch(request.call, state);
    send(&writer, &SdeResponse { id: request.id, outcome });
}

fn send(writer: &UnixStream, response: &SdeResponse) -> bool {
    let mut w = writer;
    match serde_json::to_string(response) {
        Ok(mut out) => {
            out.push('\n');
            if let Err(err) = w.write_all(out.as_bytes()) {
                tracing::warn!("sde-ipc: failed to write response: {err}");
                false
            } else {
                true
            }
        }
        Err(err) => {
            tracing::warn!("sde-ipc: failed to serialize response: {err}");
            false
        }
    }
}

/// The [`DIFF_TICK`] callback: rebuilds current window/workspace
/// summaries, compares them to `last`, and broadcasts an [`SdeEvent`] per
/// changed list to every subscriber. Skips the (still fairly cheap, but
/// not free) summary rebuild entirely when there are no subscribers.
fn broadcast_if_changed(state: &mut HwdeState, subscribers: &Subscribers, last: &mut LastBroadcast) {
    if subscribers.borrow().is_empty() {
        return;
    }

    let windows = state.sde_window_summaries();
    let workspaces: Vec<sde_ipc::SdeWorkspaceInfo> = state.workspace_summaries().into_iter().map(into_sde_workspace).collect();

    let mut events: Vec<SdeEvent> = Vec::new();
    if windows != last.windows {
        events.push(SdeEvent::Windows(windows.clone()));
        last.windows = windows;
    }
    if workspaces != last.workspaces {
        events.push(SdeEvent::Workspaces(workspaces.clone()));
        last.workspaces = workspaces;
    }
    if events.is_empty() {
        return;
    }

    let lines: Vec<String> = events
        .iter()
        .filter_map(|event| {
            serde_json::to_string(&SdeEventMessage { event: event.clone() })
                .map(|mut s| {
                    s.push('\n');
                    s
                })
                .ok()
        })
        .collect();

    subscribers.borrow_mut().retain_mut(|stream| {
        for line in &lines {
            // Best-effort, non-blocking: a slow/stuck subscriber must
            // never stall the compositor's event loop. A `WouldBlock`
            // (or any other write error) drops that subscriber - its
            // next reconnect + fresh `ListWindows`/`ListWorkspaces` call
            // (which `sde-panel`/`sde-dock` already do on (re)connect)
            // resyncs it. Note: a `WouldBlock` *mid* multi-line write
            // could in principle leave a partial line in the client's
            // read buffer; low-probability for these small payloads over
            // a local socket in practice, and documented here rather
            // than solved with a proper per-subscriber write queue,
            // which would be the correct but more invasive fix.
            match stream.write_all(line.as_bytes()) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(err) => {
                    tracing::debug!("sde-ipc: dropping subscriber: {err}");
                    return false;
                }
            }
        }
        true
    });
}

fn dispatch(call: SdeCall, state: &mut HwdeState) -> SdeOutcome {
    let ok = |result| SdeOutcome::Ok { result };
    match call {
        SdeCall::Ping => ok(SdeResult::Pong),

        SdeCall::LaunchApp { command, args } => match crate::ipc::spawn_external_app(state, &command, &args) {
            Ok(()) => ok(SdeResult::None),
            Err(err) => SdeOutcome::Err { message: err.to_string() },
        },

        SdeCall::SetWallpaper { path } => {
            state.wallpaper.set_path(path.into());
            state.pending_wallpaper_reload = true;
            ok(SdeResult::None)
        }

        SdeCall::ListWindows => ok(SdeResult::Windows(state.sde_window_summaries())),

        SdeCall::FocusWindow { id } => {
            state.focus_window_by_id(id);
            ok(SdeResult::None)
        }
        SdeCall::CloseWindow { id } => {
            state.close_window_by_id(id);
            ok(SdeResult::None)
        }
        SdeCall::MinimizeWindow { id } => {
            state.minimize_window_by_id(id);
            ok(SdeResult::None)
        }
        SdeCall::UnminimizeWindow { id } => {
            state.unminimize_window_by_id(id);
            ok(SdeResult::None)
        }
        SdeCall::MaximizeWindow { id, maximized } => {
            let geo = state.primary_output_geometry();
            state.maximize_window_by_id(id, maximized, geo);
            ok(SdeResult::None)
        }
        SdeCall::ToggleFloatingWindow { id } => {
            state.toggle_floating_by_id(id);
            ok(SdeResult::None)
        }

        SdeCall::ListWorkspaces => ok(SdeResult::Workspaces(state.workspace_summaries().into_iter().map(into_sde_workspace).collect())),

        SdeCall::SwitchWorkspace { id } => {
            state.switch_workspace(id);
            ok(SdeResult::None)
        }
        SdeCall::MoveWindowToWorkspace { id, workspace } => {
            state.move_window_to_workspace(id, workspace);
            ok(SdeResult::None)
        }
        SdeCall::SetTiling { workspace, enabled } => {
            state.set_tiling(workspace, enabled);
            ok(SdeResult::None)
        }

        SdeCall::PinSurface { app_id, edge, thickness_px } => {
            let edge = match edge {
                SdePinnedEdge::Top => PinnedEdge::Top,
                SdePinnedEdge::Bottom => PinnedEdge::Bottom,
            };
            state.pin_surface(app_id, edge, thickness_px);
            ok(SdeResult::None)
        }

        SdeCall::ReloadConfig => {
            state.config = crate::config::load_for(state.extern_name.as_deref());
            tracing::info!("compositor.toml reloaded via sde-ipc");
            ok(SdeResult::None)
        }

        SdeCall::ListOutputs => ok(SdeResult::Outputs(state.output_summaries().into_iter().map(into_sde_output).collect())),

        SdeCall::Shutdown => {
            tracing::info!("shutdown requested via sde-ipc");
            state.running.store(false, std::sync::atomic::Ordering::SeqCst);
            ok(SdeResult::None)
        }

        // Handled entirely in `handle_connection` before `dispatch` is
        // ever called (it needs the raw connection, not just `state`) -
        // this arm only exists so the match stays exhaustive; reaching it
        // would mean `handle_connection`'s `matches!` check above it was
        // bypassed somehow.
        SdeCall::Subscribe => SdeOutcome::Err { message: "Subscribe must be the only call on a connection".to_string() },
    }
}

// hwde_ipc::WorkspaceSummary/OutputSummary -> the sde-ipc equivalents.
// (Windows go through `HwdeState::sde_window_summaries` directly instead -
// see that method's doc comment for why.) `state.rs`'s summary builders
// and `sde-ipc`'s types exist independently of each other (see that
// crate's module docs), so this little adapter layer is the one place
// that has to know both shapes at once - everything else in the
// compositor keeps working exactly as it did before extern mode existed.
fn into_sde_workspace(w: hwde_ipc::WorkspaceSummary) -> sde_ipc::SdeWorkspaceInfo {
    sde_ipc::SdeWorkspaceInfo {
        id: w.id,
        name: format!("{}", w.id + 1),
        window_count: w.window_count,
        tiling_enabled: w.is_tiling,
        active: w.is_active,
    }
}

fn into_sde_output(o: hwde_ipc::OutputSummary) -> sde_ipc::SdeOutputInfo {
    sde_ipc::SdeOutputInfo {
        name: o.name,
        width: o.width,
        height: o.height,
        refresh_mhz: o.refresh_mhz,
        scale: o.scale,
        primary: o.is_primary,
    }
}
