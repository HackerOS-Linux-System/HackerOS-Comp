use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};

use crate::state::HwdeState;

impl XWaylandShellHandler for HwdeState {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}
smithay::delegate_xwayland_shell!(HwdeState);
