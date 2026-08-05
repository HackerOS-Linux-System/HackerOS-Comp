use smithay::delegate_output;
use smithay::wayland::output::OutputHandler;

use crate::state::HwdeState;

impl OutputHandler for HwdeState {}
delegate_output!(HwdeState);
