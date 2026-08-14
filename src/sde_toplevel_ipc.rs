use std::time::Duration;

use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::LoopHandle;

use crate::state::HwdeState;

/// Same cadence as `extern_ipc.rs`'s own `DIFF_TICK` - see
/// `HwdeState::sync_foreign_toplevel_diffs`'s doc comment for why this
/// exists at all; kept identical so SDE's taskbar/dock feel exactly as
/// responsive over `wlr-foreign-toplevel-management` as they did over
/// `sde-ipc`.
const DIFF_TICK: Duration = Duration::from_millis(50);

/// Starts the diff-tick timer that keeps `wlr-foreign-toplevel-management`
/// current. The global itself is registered earlier, at `HwdeState`
/// construction time (see `HwdeState::foreign_toplevels`), not here -
/// this function only wires up the recurring half of the job, mirroring
/// `extern_ipc::init`'s shape (which also only wires up its `DIFF_TICK`
/// timer here, alongside listening for new socket connections it doesn't
/// have an equivalent of).
pub fn init(handle: &LoopHandle<'static, HwdeState>) -> std::io::Result<()> {
    handle
        .insert_source(Timer::from_duration(DIFF_TICK), |_, _, state| {
            state.sync_foreign_toplevel_diffs();
            TimeoutAction::ToDuration(DIFF_TICK)
        })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("failed to register wlr-foreign-toplevel-management diff timer: {e}")))?;

    tracing::info!("comphwde: wlr-foreign-toplevel-management-unstable-v1 ready (extern target: sde, no sde-ipc socket)");
    Ok(())
}
