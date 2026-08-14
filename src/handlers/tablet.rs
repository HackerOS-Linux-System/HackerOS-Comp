use smithay::wayland::tablet_manager::TabletSeatHandler;
use smithay::delegate_tablet_manager;

use crate::state::HwdeState;

// `TabletToolDescriptor` (needed to override `tablet_tool_image` with a
// real body, mirroring `SeatHandler::cursor_image` for a stylus hovering
// - see `handlers/seat.rs`) turns out to live in
// `wayland::tablet_manager::tablet_tool`, which is `pub(crate)` in this
// Smithay version - confirmed by the compiler (`E0603`) after the
// previous guess at its path. There's no public path to name that type
// from outside the crate at all (checked: not re-exported at
// `tablet_manager`'s top level either - that's the error two rounds
// before this one). Rather than keep guessing at an apparently-
// unreachable type, this relies on `tablet_tool_image` having a default
// (no-op) body on the trait - an empty `impl` block, same pattern
// `handlers/output.rs`'s `impl OutputHandler for HwdeState {}` already
// uses for an all-defaults trait. Cost of this: a stylus hovering over a
// surface won't get its own cursor-image update the way a regular
// pointer does (`cursor_status` simply won't change based on stylus
// proximity) - a minor visual gap, not a functional one; tip/motion/
// proximity/button forwarding in `input.rs` are all unaffected, since
// none of them go through this method.
impl TabletSeatHandler for HwdeState {}
delegate_tablet_manager!(HwdeState);
