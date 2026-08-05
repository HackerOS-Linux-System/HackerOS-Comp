use smithay::input::Seat;
use smithay::reexports::wayland_server::protocol::wl_data_source::WlDataSource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::{SelectionHandler, SelectionSource, SelectionTarget};
use smithay::delegate_data_device;

use crate::state::HwdeState;

impl SelectionHandler for HwdeState {
    type SelectionUserData = ();

    fn new_selection(&mut self, _ty: SelectionTarget, _source: Option<SelectionSource>, _seat: Seat<Self>) {
        // Nothing to do here: smithay's DataDeviceState already brokers the
        // selection between clients. We don't currently mirror it into the
        // shell's own Clipboard app state - the frontend instead polls the
        // browser Clipboard API, which sees the same system clipboard once
        // a client sets it through this protocol.
    }
}

impl DataDeviceHandler for HwdeState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for HwdeState {
    fn started(&mut self, _source: Option<WlDataSource>, icon: Option<WlSurface>, _seat: Seat<Self>) {
        self.dnd_icon = icon;
    }

    fn dropped(&mut self, _target: Option<WlSurface>, _validated: bool, _seat: Seat<Self>) {
        self.dnd_icon = None;
    }
}

impl ServerDndGrabHandler for HwdeState {
    fn send(&mut self, _mime_type: String, _fd: std::os::fd::OwnedFd, _seat: Seat<Self>) {
        // HWDE doesn't originate server-side drag-and-drop sources today.
    }
}

delegate_data_device!(HwdeState);

// "Middle-click paste" - a separate selection buffer from the main
// clipboard that most X11-heritage Linux apps (terminals, GTK, Qt) still
// expect to exist alongside it.
impl smithay::wayland::selection::primary_selection::PrimarySelectionHandler for HwdeState {
    fn primary_selection_state(&self) -> &smithay::wayland::selection::primary_selection::PrimarySelectionState {
        &self.primary_selection_state
    }
}
smithay::delegate_primary_selection!(HwdeState);
