use smithay::desktop::{PopupKind, Window};
use smithay::input::Seat;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_seat;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::Serial;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::{delegate_xdg_shell, wayland::seat::WaylandFocus};

use crate::grabs::{MoveSurfaceGrab, ResizeSurfaceGrab};
use crate::state::HwdeState;

impl XdgShellHandler for HwdeState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface.clone());
        self.place_new_window(&window, true);
        surface.send_configure();
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!("failed to track popup: {err}");
        }
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        let geometry = positioner.get_geometry();
        surface.with_pending_state(|state| {
            state.geometry = geometry;
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
    }

    /// Formal popup grabs (smithay's `PopupManager::grab_popup`) need a
    /// custom pointer-focus type; we instead dismiss popups whenever a
    /// click lands outside them (see `HwdeState::dismiss_popups_outside`,
    /// invoked from `input.rs`'s pointer-button handling), which gets the
    /// same user-visible behavior without that refactor.
    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.forget_window_by_surface(surface.wl_surface());
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let seat: Seat<HwdeState> = Seat::from_resource(&seat).unwrap();
        let Some(pointer) = seat.get_pointer() else { return };
        if !pointer.has_grab(serial) {
            return;
        }
        let Some(start_data) = pointer.grab_start_data() else { return };

        let Some(window) = self.windows.iter().find(|w| w.window.wl_surface().as_deref() == Some(surface.wl_surface())).map(|w| w.window.clone()) else {
            return;
        };

        // Focus must belong to the requesting surface's client.
        match &start_data.focus {
            Some((focus_surface, _)) if focus_surface.id().same_client_as(&surface.wl_surface().id()) => {}
            _ => return,
        }

        let initial_window_location = self.space.element_location(&window).unwrap_or((0, 0).into());

        // Un-maximize before moving, like every other desktop does.
        if let Some(id) = self.window_id_for_surface(surface.wl_surface()) {
            let geo = self.primary_output_geometry();
            self.maximize_window_by_id(id, false, geo);
        }

        let grab = MoveSurfaceGrab { start_data, window, initial_window_location };
        pointer.set_grab(self, grab, serial, smithay::input::pointer::Focus::Clear);
    }

    fn resize_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial, edges: xdg_toplevel::ResizeEdge) {
        let seat: Seat<HwdeState> = Seat::from_resource(&seat).unwrap();
        let Some(pointer) = seat.get_pointer() else { return };
        if !pointer.has_grab(serial) {
            return;
        }
        let Some(start_data) = pointer.grab_start_data() else { return };

        let Some(window) = self.windows.iter().find(|w| w.window.wl_surface().as_deref() == Some(surface.wl_surface())).map(|w| w.window.clone()) else {
            return;
        };

        match &start_data.focus {
            Some((focus_surface, _)) if focus_surface.id().same_client_as(&surface.wl_surface().id()) => {}
            _ => return,
        }

        let initial_window_location = self.space.element_location(&window).unwrap_or((0, 0).into());
        let initial_window_size = window.geometry().size;

        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Resizing);
        });
        surface.send_pending_configure();

        let grab = ResizeSurfaceGrab {
            start_data,
            window,
            edges: edges.into(),
            initial_window_location,
            initial_window_size,
            last_window_size: initial_window_size,
        };
        pointer.set_grab(self, grab, serial, smithay::input::pointer::Focus::Clear);
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.window_id_for_surface(surface.wl_surface()) {
            let geo = self.primary_output_geometry();
            self.maximize_window_by_id(id, true, geo);
        } else {
            surface.send_configure();
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.window_id_for_surface(surface.wl_surface()) {
            let geo = self.primary_output_geometry();
            self.maximize_window_by_id(id, false, geo);
        } else {
            surface.send_configure();
        }
    }
}
delegate_xdg_shell!(HwdeState);
