use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::idle_inhibit::IdleInhibitHandler;
use smithay::{delegate_idle_inhibit, delegate_relative_pointer, delegate_single_pixel_buffer, delegate_virtual_keyboard_manager};

use crate::state::HwdeState;

impl IdleInhibitHandler for HwdeState {
    fn inhibit(&mut self, surface: WlSurface) {
        self.idle_inhibiting_surfaces.insert(surface);
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        self.idle_inhibiting_surfaces.remove(&surface);
    }
}
delegate_idle_inhibit!(HwdeState);
delegate_single_pixel_buffer!(HwdeState);
delegate_relative_pointer!(HwdeState);
delegate_virtual_keyboard_manager!(HwdeState);
