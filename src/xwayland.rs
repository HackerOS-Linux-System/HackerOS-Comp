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
}
