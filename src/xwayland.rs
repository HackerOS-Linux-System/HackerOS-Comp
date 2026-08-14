use std::process::Stdio;

use smithay::desktop::Window;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::compositor::CompositorHandler;
use smithay::wayland::seat::WaylandFocus;
use smithay::xwayland::xwm::{Reorder, ResizeEdge as X11ResizeEdge, XwmId};
use smithay::xwayland::{X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler};

use crate::state::HwdeState;

pub fn start(state: &mut HwdeState) -> anyhow::Result<()> {
    let (xwayland, client) = XWayland::spawn(
        &state.display_handle,
        None,
        std::iter::empty::<(String, String)>(),
        true,
        Stdio::null(),
        Stdio::null(),
        |_| (),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    state
        .handle
        .insert_source(xwayland, move |event, _, data: &mut HwdeState| match event {
            XWaylandEvent::Ready { x11_socket, display_number } => {
                data.client_compositor_state(&client).set_client_scale(1.0);
                match X11Wm::start_wm(data.handle.clone(), x11_socket, client.clone()) {
                    Ok(wm) => {
                        tracing::info!("XWayland ready on DISPLAY :{display_number}");
                        data.xwm = Some(wm);
                        data.xdisplay = Some(display_number);
                    }
                    Err(err) => tracing::error!("failed to attach X11 window manager: {err}"),
                }
            }
            XWaylandEvent::Error => {
                tracing::warn!("XWayland crashed on startup");
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert XWayland source: {e}"))?;

    Ok(())
}

fn find_element(state: &HwdeState, window: &X11Surface) -> Option<Window> {
    state
        .space
        .elements()
        .find(|e| e.wl_surface().as_deref() == window.wl_surface().as_ref())
        .cloned()
}

impl XwmHandler for HwdeState {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("XWM used before XWayland::Ready")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.set_mapped(true);
        let element = Window::new_x11_window(window.clone());
        self.place_new_window(&element, true);
        if let Some(bbox) = self.space.element_bbox(&element) {
            let _ = window.configure(Some(bbox));
        }
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        let location = window.geometry().loc;
        let element = Window::new_x11_window(window);
        self.space.map_element(element, location, true);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(elem) = find_element(self, &window) {
            self.space.unmap_elem(&elem);
        }
        if !window.is_override_redirect() {
            let _ = window.set_mapped(false);
        }
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(surface) = window.wl_surface() {
            self.forget_window_by_surface(&surface);
        }
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // HWDE (not the client) owns window placement, but we do honor
        // client-requested sizes, mirroring how the SolidJS shell lets
        // in-shell apps request (but not dictate) their own window size.
        let mut geo: Rectangle<i32, Logical> = window.geometry();
        if let Some(w) = w {
            geo.size.w = w as i32;
        }
        if let Some(h) = h {
            geo.size.h = h as i32;
        }
        if let Some(x) = x {
            geo.loc.x = x;
        }
        if let Some(y) = y {
            geo.loc.y = y;
        }
        let _ = window.configure(Some(geo));
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        if let Some(elem) = find_element(self, &window) {
            self.space.map_element(elem, geometry.loc, false);
        }
    }

    fn resize_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32, edges: X11ResizeEdge) {
        let Some(start_data) = self.pointer.grab_start_data() else { return };
        let Some(element) = find_element(self, &window) else { return };

        let geometry = element.geometry();
        let initial_window_location = self.space.element_location(&element).unwrap_or((0, 0).into());

        let grab = crate::grabs::ResizeSurfaceGrab {
            start_data,
            window: element,
            edges: edges.into(),
            initial_window_location,
            initial_window_size: geometry.size,
            last_window_size: geometry.size,
        };
        let pointer = self.pointer.clone();
        pointer.set_grab(self, grab, smithay::utils::SERIAL_COUNTER.next_serial(), smithay::input::pointer::Focus::Clear);
    }

    fn move_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32) {
        let Some(start_data) = self.pointer.grab_start_data() else { return };
        let Some(element) = find_element(self, &window) else { return };

        let mut initial_window_location = self.space.element_location(&element).unwrap_or((0, 0).into());
        if window.is_maximized() {
            let _ = window.set_maximized(false);
            let pos = start_data.location;
            initial_window_location = (pos.x as i32, pos.y as i32).into();
        }

        let grab = crate::grabs::MoveSurfaceGrab { start_data, window: element, initial_window_location };
        let pointer = self.pointer.clone();
        pointer.set_grab(self, grab, smithay::utils::SERIAL_COUNTER.next_serial(), smithay::input::pointer::Focus::Clear);
    }

    // Maximize/minimize - previously unhandled entirely (no override, so
    // `XwmHandler`'s default no-op bodies silently swallowed every
    // request), meaning an X11 app's own maximize button, its window
    // menu's "Minimize", or a window manager hint sent via
    // `_NET_WM_STATE`/`WM_CHANGE_STATE` had no effect at all under this
    // compositor - the *Wayland*-native equivalents already worked (see
    // `handlers/xdg_shell.rs`'s `maximize_request`/`unmaximize_request`),
    // this was purely an XWayland-side gap. Reuses those exact same
    // state functions (`maximize_window_by_id`, `minimize_window_by_id`,
    // `unminimize_window_by_id`) rather than duplicating the maximize/
    // minimize logic itself - an X11 window becoming "managed" already
    // goes through the same `windows: Vec<ManagedWindow>`/
    // `space: Space<Window>` machinery as a Wayland one (see
    // `map_window_request` above using `place_new_window`, the same
    // function `handlers/xdg_shell.rs::new_toplevel` uses), so there's
    // nothing X11-specific left to do beyond finding the window's id and
    // calling the same functions.
    //
    // `X11Surface::set_maximized`/`set_minimized` (the calls that tell
    // the client "yes, your request was granted" and update its own
    // `_NET_WM_STATE`) aren't called here - **unverified**: it wasn't
    // confirmed which of `maximize_window_by_id` (state-side, moves/
    // resizes the window) vs these `X11Surface` setters (client-request-
    // side, updates what the client itself believes its state is) the
    // client actually needs to see change to stop re-requesting: same
    // caveat as the rest of this project (no `cargo check` available).
    // If maximize/minimize visually works but an X11 app's own maximize
    // button doesn't toggle to look "pressed", that's the missing half -
    // a one-line addition per handler, once confirmed.
    fn maximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let Some(surface) = window.wl_surface() else { return };
        let Some(id) = self.window_id_for_surface(&surface) else { return };
        let geo = self.primary_output_geometry();
        self.maximize_window_by_id(id, true, geo);
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let Some(surface) = window.wl_surface() else { return };
        let Some(id) = self.window_id_for_surface(&surface) else { return };
        let geo = self.primary_output_geometry();
        self.maximize_window_by_id(id, false, geo);
    }

    fn minimize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let Some(surface) = window.wl_surface() else { return };
        let Some(id) = self.window_id_for_surface(&surface) else { return };
        self.minimize_window_by_id(id);
    }

    fn unminimize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let Some(surface) = window.wl_surface() else { return };
        let Some(id) = self.window_id_for_surface(&surface) else { return };
        self.unminimize_window_by_id(id);
    }

    // Same story as maximize/minimize above, but for fullscreen - see
    // `fullscreen_window_by_id`'s doc comment in `state.rs`. `XwmHandler`
    // apparently doesn't distinguish "fullscreen request" from
    // "fullscreen-on-this-specific-output request" the way
    // `handlers/xdg_shell.rs`'s Wayland-side `fullscreen_request` takes
    // an `Option<Output>` for - X11's `_NET_WM_STATE_FULLSCREEN` has no
    // per-output targeting concept in the protocol itself, so there's
    // nothing extra to ignore here the way that Wayland-side `_output`
    // parameter is.
    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let Some(surface) = window.wl_surface() else { return };
        let Some(id) = self.window_id_for_surface(&surface) else { return };
        let geo = self.primary_output_geometry();
        self.fullscreen_window_by_id(id, true, geo);
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let Some(surface) = window.wl_surface() else { return };
        let Some(id) = self.window_id_for_surface(&surface) else { return };
        let geo = self.primary_output_geometry();
        self.fullscreen_window_by_id(id, false, geo);
    }
}
