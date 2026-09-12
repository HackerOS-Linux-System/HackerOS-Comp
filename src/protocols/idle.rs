use smithay::reexports::calloop::{LoopHandle, timer::{Timer, TimeoutAction}};
use tracing::info;
use crate::{state::BlueState, ipc};

/// Install the idle timer into the calloop event loop.
/// The timer fires after `state.dpms_timeout`; on activity it is reset.
pub fn init_idle(
    state: &BlueState,
    loop_handle: &LoopHandle<'static, BlueState>,
) {
    let timeout = state.dpms_timeout;
    if timeout.is_zero() {
        info!("DPMS disabled (timeout = 0)");
        return;
    }

    loop_handle.insert_source(
        Timer::from_duration(timeout),
        move |_, _, state: &mut BlueState| {
            on_idle(state);
            // Re-arm for the next cycle (will be cancelled by activity)
            TimeoutAction::ToDuration(state.dpms_timeout)
        },
    ).ok();

    info!("Idle timer armed: {:?}", timeout);
}

fn on_idle(state: &mut BlueState) {
    if state.is_idle { return; }
    // A client holding an idle-inhibitor (idle_inhibit.rs —
    // zwp_idle_inhibit_manager_v1, the protocol a video player,
    // presentation app, or game uses to say "don't blank the screen
    // while I'm the focused/visible content") was never actually
    // checked here despite this exact function being named in that
    // file's own doc comment as the place that should check it. The
    // practical effect: idle-inhibit had zero effect compositor-wide —
    // the screen would still blank during a video or presentation, the
    // one thing that protocol exists to prevent. Doesn't cancel the
    // timer itself, just skips blanking *this* cycle — once the
    // inhibitor is released, the next timeout (already re-armed below
    // regardless of this early return) blanks normally.
    if state.is_idle_inhibited() {
        info!("Idle timeout reached, but a client holds an idle inhibitor — not blanking");
        return;
    }
    state.is_idle = true;
    info!("System idle — blanking outputs");

    // Blank all outputs via DPMS or wlr-output-power-management. Kept
    // as a single `sh -c "... || ..."` spawn (not rewritten to two
    // separate `Command::new(prog).args([...])` calls) deliberately:
    // this runs on a background timer, not in response to a Wayland
    // request, so nothing here can be attacker-supplied the way the
    // shell-injection issues found elsewhere in this project's audit
    // were (`name` comes from DRM/the kernel, not client input) — and
    // spawning a shell is what lets `||` fall back to `xset`
    // asynchronously in the child process without this compositor
    // itself blocking on the first command's exit status to decide
    // whether to also try the second one.
    for output in state.space.outputs() {
        let name = output.name();
        let _ = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "wlr-randr --output {} --off 2>/dev/null || xset dpms force off 2>/dev/null",
                name
            ))
            .spawn();
    }

    // Notify shell via IPC
    let clients = state.clients.clone();
    ipc::broadcast_idle_changed(&clients, true);
}

/// Called from the input handler whenever there is keyboard or pointer activity.
pub fn reset_idle(state: &mut BlueState) {
    if !state.is_idle { return; }
    state.is_idle = false;
    info!("Activity detected — waking outputs");

    for output in state.space.outputs() {
        let name = output.name();
        let _ = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "wlr-randr --output {} --on 2>/dev/null || xset dpms force on 2>/dev/null",
                name
            ))
            .spawn();
    }

    let clients = state.clients.clone();
    ipc::broadcast_idle_changed(&clients, false);
}
