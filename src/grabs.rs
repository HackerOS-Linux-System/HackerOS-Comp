use smithay::desktop::{Window, WindowSurface};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
    GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
    GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab,
    PointerInnerHandle, RelativeMotionEvent,
};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{IsAlive, Logical, Point, Size};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::SurfaceCachedState;
#[cfg(feature = "xwayland")]
use smithay::{utils::Rectangle, xwayland::xwm::ResizeEdge as X11ResizeEdge};

use crate::state::HwdeState;

pub struct MoveSurfaceGrab {
    pub start_data: PointerGrabStartData<HwdeState>,
    pub window: Window,
    pub initial_window_location: Point<i32, Logical>,
}

impl PointerGrab<HwdeState> for MoveSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut HwdeState,
        handle: &mut PointerInnerHandle<'_, HwdeState>,
        _focus: Option<(smithay::reexports::wayland_server::protocol::wl_surface::WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);

        if !self.window.alive() {
            handle.unset_grab(self, data, event.serial, event.time, true);
            return;
        }

        let delta = event.location - self.start_data.location;
        let new_location = self.initial_window_location.to_f64() + delta;
        data.space.map_element(self.window.clone(), new_location.to_i32_round(), true);
    }

    fn relative_motion(
        &mut self,
        data: &mut HwdeState,
        handle: &mut PointerInnerHandle<'_, HwdeState>,
        focus: Option<(smithay::reexports::wayland_server::protocol::wl_surface::WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>, event: &ButtonEvent) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>, details: AxisFrame) {
        handle.axis(data, details)
    }

    fn frame(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>, event: &GestureSwipeBeginEvent) {
        handle.gesture_swipe_begin(data, event);
    }
    fn gesture_swipe_update(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>, event: &GestureSwipeUpdateEvent) {
        handle.gesture_swipe_update(data, event);
    }
    fn gesture_swipe_end(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>, event: &GestureSwipeEndEvent) {
        handle.gesture_swipe_end(data, event);
    }
    fn gesture_pinch_begin(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>, event: &GesturePinchBeginEvent) {
        handle.gesture_pinch_begin(data, event);
    }
    fn gesture_pinch_update(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>, event: &GesturePinchUpdateEvent) {
        handle.gesture_pinch_update(data, event);
    }
    fn gesture_pinch_end(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>, event: &GesturePinchEndEvent) {
        handle.gesture_pinch_end(data, event);
    }
    fn gesture_hold_begin(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>, event: &GestureHoldBeginEvent) {
        handle.gesture_hold_begin(data, event);
    }
    fn gesture_hold_end(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>, event: &GestureHoldEndEvent) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &PointerGrabStartData<HwdeState> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut HwdeState) {}
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ResizeEdge: u32 {
        const TOP    = 0b0001;
        const BOTTOM = 0b0010;
        const LEFT   = 0b0100;
        const RIGHT  = 0b1000;
    }
}

impl From<xdg_toplevel::ResizeEdge> for ResizeEdge {
    #[inline]
    fn from(x: xdg_toplevel::ResizeEdge) -> Self {
        Self::from_bits(x as u32).unwrap_or(ResizeEdge::empty())
    }
}

#[cfg(feature = "xwayland")]
impl From<X11ResizeEdge> for ResizeEdge {
    fn from(edge: X11ResizeEdge) -> Self {
        match edge {
            X11ResizeEdge::Top => ResizeEdge::TOP,
            X11ResizeEdge::Bottom => ResizeEdge::BOTTOM,
            X11ResizeEdge::Left => ResizeEdge::LEFT,
            X11ResizeEdge::Right => ResizeEdge::RIGHT,
            X11ResizeEdge::TopLeft => ResizeEdge::TOP | ResizeEdge::LEFT,
            X11ResizeEdge::TopRight => ResizeEdge::TOP | ResizeEdge::RIGHT,
            X11ResizeEdge::BottomLeft => ResizeEdge::BOTTOM | ResizeEdge::LEFT,
            X11ResizeEdge::BottomRight => ResizeEdge::BOTTOM | ResizeEdge::RIGHT,
        }
    }
}

pub struct ResizeSurfaceGrab {
    pub start_data: PointerGrabStartData<HwdeState>,
    pub window: Window,
    pub edges: ResizeEdge,
    pub initial_window_location: Point<i32, Logical>,
    pub initial_window_size: Size<i32, Logical>,
    pub last_window_size: Size<i32, Logical>,
}

impl PointerGrab<HwdeState> for ResizeSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut HwdeState,
        handle: &mut PointerInnerHandle<'_, HwdeState>,
        _focus: Option<(smithay::reexports::wayland_server::protocol::wl_surface::WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);

        if !self.window.alive() {
            handle.unset_grab(self, data, event.serial, event.time, true);
            return;
        }

        let (mut dx, mut dy) = (event.location - self.start_data.location).into();

        let mut new_width = self.initial_window_size.w;
        let mut new_height = self.initial_window_size.h;

        let left_right = ResizeEdge::LEFT | ResizeEdge::RIGHT;
        let top_bottom = ResizeEdge::TOP | ResizeEdge::BOTTOM;

        if self.edges.intersects(left_right) {
            if self.edges.intersects(ResizeEdge::LEFT) {
                dx = -dx;
            }
            new_width = (self.initial_window_size.w as f64 + dx) as i32;
        }
        if self.edges.intersects(top_bottom) {
            if self.edges.intersects(ResizeEdge::TOP) {
                dy = -dy;
            }
            new_height = (self.initial_window_size.h as f64 + dy) as i32;
        }

        let (min_size, max_size) = if let Some(surface) = smithay::wayland::seat::WaylandFocus::wl_surface(&self.window) {
            with_states(&surface, |states| {
                let mut guard = states.cached_state.get::<SurfaceCachedState>();
                let d = guard.current();
                (d.min_size, d.max_size)
            })
        } else {
            ((0, 0).into(), (0, 0).into())
        };

        let min_w = min_size.w.max(1);
        let min_h = min_size.h.max(1);
        let max_w = if max_size.w == 0 { i32::MAX } else { max_size.w };
        let max_h = if max_size.h == 0 { i32::MAX } else { max_size.h };

        new_width = new_width.max(min_w).min(max_w);
        new_height = new_height.max(min_h).min(max_h);
        self.last_window_size = (new_width, new_height).into();

        match self.window.underlying_surface() {
            WindowSurface::Wayland(xdg) => {
                xdg.with_pending_state(|state| {
                    state.states.set(xdg_toplevel::State::Resizing);
                    state.size = Some(self.last_window_size);
                });
                xdg.send_pending_configure();
            }
            #[cfg(feature = "xwayland")]
            WindowSurface::X11(x11) => {
                let location = data.space.element_location(&self.window).unwrap_or(self.initial_window_location);
                let _ = x11.configure(Some(Rectangle::new(location, self.last_window_size)));
            }
        }
    }

    fn relative_motion(
        &mut self,
        data: &mut HwdeState,
        handle: &mut PointerInnerHandle<'_, HwdeState>,
        focus: Option<(smithay::reexports::wayland_server::protocol::wl_surface::WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>, event: &ButtonEvent) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            if let WindowSurface::Wayland(xdg) = self.window.underlying_surface() {
                xdg.with_pending_state(|state| {
                    state.states.unset(xdg_toplevel::State::Resizing);
                    state.size = Some(self.last_window_size);
                });
                xdg.send_pending_configure();
            }
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>, details: AxisFrame) {
        handle.axis(data, details)
    }

    fn frame(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>, event: &GestureSwipeBeginEvent) {
        handle.gesture_swipe_begin(data, event);
    }
    fn gesture_swipe_update(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>, event: &GestureSwipeUpdateEvent) {
        handle.gesture_swipe_update(data, event);
    }
    fn gesture_swipe_end(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>, event: &GestureSwipeEndEvent) {
        handle.gesture_swipe_end(data, event);
    }
    fn gesture_pinch_begin(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>, event: &GesturePinchBeginEvent) {
        handle.gesture_pinch_begin(data, event);
    }
    fn gesture_pinch_update(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>, event: &GesturePinchUpdateEvent) {
        handle.gesture_pinch_update(data, event);
    }
    fn gesture_pinch_end(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>, event: &GesturePinchEndEvent) {
        handle.gesture_pinch_end(data, event);
    }
    fn gesture_hold_begin(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>, event: &GestureHoldBeginEvent) {
        handle.gesture_hold_begin(data, event);
    }
    fn gesture_hold_end(&mut self, data: &mut HwdeState, handle: &mut PointerInnerHandle<'_, HwdeState>, event: &GestureHoldEndEvent) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &PointerGrabStartData<HwdeState> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut HwdeState) {}
}
