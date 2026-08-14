use smithay::backend::input::{
    AbsolutePositionEvent, Axis, ButtonState, Event, GestureBeginEvent, GestureEndEvent, GestureSwipeUpdateEvent, InputBackend, InputEvent,
    KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent, TabletToolButtonEvent, TabletToolEvent,
    // `TabletToolProximityEvent` kept despite being reported unused
    // against the *previous* attempt (`.proximity_state()`, confirmed
    // wrong) - this pass's `.state()` guess might still resolve through
    // it under a name that wasn't tried yet. Worst case if `.state()`
    // turns out to come from elsewhere too: the compiler re-reports this
    // import as unused (a warning, not an error) and it comes out next
    // round. `TabletToolAxisEvent` was dropped outright since it was
    // confirmed unused by *working* code (`.pressure()`/`.tilt()` calls
    // that type-checked fine), not just a failed guess - safe to remove.
    TabletToolProximityEvent,
    TabletToolTipEvent,
    TouchEvent,
};
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent};
use smithay::input::touch::{DownEvent, MotionEvent as TouchMotionEventData, UpEvent};
use smithay::output::Output;
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::tablet_manager::{TabletDescriptor, TabletSeatTrait};

use crate::state::HwdeState;

impl HwdeState {
    pub fn process_input_event<B: InputBackend>(&mut self, event: InputEvent<B>, output: &Output) {
        // Any input at all counts as activity for `idle_dimmed` - see
        // that field's doc comment in `state.rs`. Deliberately
        // unconditional and before the `match` below (rather than added
        // to each arm) so a future new `InputEvent` variant doesn't need
        // to remember to add this - it's already covered.
        self.last_input_activity = std::time::Instant::now();

        match event {
            InputEvent::Keyboard { event } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = event.time_msec();
                let keycode = event.key_code();
                let key_state = event.state();
                if let Some(keyboard) = self.seat.get_keyboard() {
                    // The filter closure gets `(&mut HwdeState, &ModifiersState,
                    // KeysymHandle)` *before* the event is forwarded to the
                    // focused client - exactly the hook point smithay's own
                    // docs point to for "implement compositor-level key
                    // bindings". `modified_sym()` (rather than the raw/
                    // unmodified variant) matches what a config file's key
                    // names (`"Q"`, `"1"`, ...) naturally mean once combined
                    // with an explicit `mods` list - see `Keybinding::matches`
                    // in `config.rs`. Returning `Intercept` swallows the key
                    // so e.g. a `super+q` binding doesn't *also* deliver a
                    // literal 'q' keystroke to the focused app.
                    keyboard.input::<(), _>(self, keycode, key_state, serial, time, move |data, mods, handle| {
                        if key_state == KeyState::Pressed {
                            let keysym = handle.modified_sym();

                            // Checked first, unconditionally - see
                            // `config.rs::is_emergency_reset`'s doc comment
                            // for why this can never be shadowed by (or
                            // removed via) `compositor.toml`.
                            if crate::config::is_emergency_reset(keysym, mods) {
                                data.trigger_emergency_reset();
                                return FilterResult::Intercept(());
                            }

                            let action = data
                                .config
                                .keybindings
                                .iter()
                                .find(|kb| kb.matches(keysym, mods))
                                .and_then(|kb| kb.parse_action());
                            if let Some(action) = action {
                                data.run_action(action);
                                return FilterResult::Intercept(());
                            }
                        }
                        FilterResult::Forward
                    });
                }
            }

            InputEvent::PointerMotionAbsolute { event } => {
                let output_geo = self
                    .space
                    .output_geometry(output)
                    .unwrap_or_else(|| smithay::utils::Rectangle::from_size((1920, 1080).into()));
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                let serial = SERIAL_COUNTER.next_serial();
                let under = self.surface_under(pos);
                let pointer = self.pointer.clone();
                pointer.motion(
                    self,
                    under,
                    &MotionEvent { location: pos, serial, time: event.time_msec() },
                );
                pointer.frame(self);
            }

            // Relative motion - what an ordinary USB/PS2 mouse (as
            // opposed to a touchpad reporting absolute position, or
            // touch/tablet input) actually sends through libinput. This
            // was entirely unhandled before - meaning a plain mouse
            // plugged into a real DRM session had no way to move the
            // cursor at all, since `PointerMotionAbsolute` above is what
            // touchpads/touchscreens (and the winit backend's synthetic
            // events) send, not what most desktop mice do. Clamps the new
            // position to the output's bounds the same way `anvil`
            // (Smithay's own reference compositor) does, so the cursor
            // can't be pushed off-screen by a large accumulated delta.
            InputEvent::PointerMotion { event } => {
                let output_geo = self
                    .space
                    .output_geometry(output)
                    .unwrap_or_else(|| smithay::utils::Rectangle::from_size((1920, 1080).into()));

                let mut pos = self.pointer.current_location() + event.delta();
                pos.x = pos.x.clamp(output_geo.loc.x as f64, (output_geo.loc.x + output_geo.size.w) as f64 - 1.0);
                pos.y = pos.y.clamp(output_geo.loc.y as f64, (output_geo.loc.y + output_geo.size.h) as f64 - 1.0);

                let serial = SERIAL_COUNTER.next_serial();
                let under = self.surface_under(pos);
                let pointer = self.pointer.clone();

                // Forwarded to `zwp_relative_pointer_v1` clients (games,
                // 3D/CAD tools that want raw, unaccelerated deltas rather
                // than the accelerated/clamped absolute position below) -
                // see `handlers/extra_protocols.rs`'s
                // `relative_pointer_manager_state`. A no-op if no client
                // surface under focus has actually requested a relative
                // pointer object, so this is safe to call unconditionally
                // on every motion event, same as `pointer.frame()` below.
                //
                // **`utime` unverified**: `RelativeMotionEvent::utime`
                // wants microseconds; passed here as `time_msec() * 1000`
                // rather than a dedicated microsecond timestamp, since
                // this backend event type wasn't confirmed to expose one
                // separately (no `cargo check` in the environment this
                // was written in - same caveat as the rest of this
                // project). Millisecond-derived microseconds lose
                // sub-millisecond precision that real hardware timestamps
                // would have, which matters for high-end gaming mice
                // doing their own interpolation, not for anything this
                // compositor itself does with the value (it doesn't read
                // `utime` anywhere, only forwards it).
                pointer.relative_motion(
                    self,
                    under.clone(),
                    &RelativeMotionEvent { delta: event.delta(), delta_unaccel: event.delta_unaccel(), utime: event.time_msec() as u64 * 1000 },
                );

                pointer.motion(self, under, &MotionEvent { location: pos, serial, time: event.time_msec() });
                pointer.frame(self);
            }

            InputEvent::PointerButton { event } => {
                let serial = SERIAL_COUNTER.next_serial();
                let button = event.button_code();
                let button_state = event.state();

                if button_state == ButtonState::Pressed {
                    let location = self.pointer.current_location();
                    self.dismiss_popups_outside(location);

                    match crate::render_elements::ssd_hit_test(self, location) {
                        Some(crate::render_elements::SsdHit::Close(id)) => {
                            self.close_window_by_id(id);
                            return;
                        }
                        Some(crate::render_elements::SsdHit::Bar(id)) => {
                            self.focus_window_by_id(id);
                            if let Some(managed) = self.windows.iter().find(|w| w.id == id) {
                                let window = managed.window.clone();
                                let initial_window_location =
                                    self.space.element_location(&window).unwrap_or((0, 0).into());
                                if let Some(start_data) = self.pointer.grab_start_data() {
                                    let grab = crate::grabs::MoveSurfaceGrab {
                                        start_data,
                                        window,
                                        initial_window_location,
                                    };
                                    let pointer = self.pointer.clone();
                                    pointer.set_grab(self, grab, serial, smithay::input::pointer::Focus::Clear);
                                }
                            }
                            // The bar itself isn't part of any client surface -
                            // don't forward this press to a client.
                            return;
                        }
                        None => {}
                    }

                    if let Some((surface, _)) = self.surface_under(location) {
                        let keyboard = self.seat.get_keyboard();
                        if let Some(keyboard) = keyboard {
                            keyboard.set_focus(self, Some(surface), serial);
                        }
                        if let Some((window, _)) = self.space.element_under(location).map(|(w, l)| (w.clone(), l))
                        {
                            self.space.raise_element(&window, true);
                        }
                    }
                }

                let pointer = self.pointer.clone();
                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: button_state,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }

            InputEvent::PointerAxis { event } => {
                let horizontal = event.amount(Axis::Horizontal).unwrap_or(0.0);
                let vertical = event.amount(Axis::Vertical).unwrap_or(0.0);

                let mut frame = AxisFrame::new(event.time_msec()).source(event.source());
                if horizontal != 0.0 {
                    frame = frame.value(Axis::Horizontal, horizontal);
                }
                if vertical != 0.0 {
                    frame = frame.value(Axis::Vertical, vertical);
                }
                let pointer = self.pointer.clone();
                pointer.axis(self, frame);
                pointer.frame(self);
            }

            InputEvent::TouchDown { event } => {
                let Some(touch) = self.seat.get_touch() else { return };
                let output_geo = self
                    .space
                    .output_geometry(output)
                    .unwrap_or_else(|| smithay::utils::Rectangle::from_size((1920, 1080).into()));
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                let serial = SERIAL_COUNTER.next_serial();
                let under = self.surface_under(pos);
                touch.down(self, under, &DownEvent { slot: event.slot(), location: pos, serial, time: event.time_msec() });
            }

            InputEvent::TouchMotion { event } => {
                let Some(touch) = self.seat.get_touch() else { return };
                let output_geo = self
                    .space
                    .output_geometry(output)
                    .unwrap_or_else(|| smithay::utils::Rectangle::from_size((1920, 1080).into()));
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                let under = self.surface_under(pos);
                touch.motion(self, under, &TouchMotionEventData { slot: event.slot(), location: pos, time: event.time_msec() });
            }

            InputEvent::TouchUp { event } => {
                if let Some(touch) = self.seat.get_touch() {
                    let serial = SERIAL_COUNTER.next_serial();
                    touch.up(self, &UpEvent { slot: event.slot(), serial, time: event.time_msec() });
                }
            }

            InputEvent::TouchCancel { .. } => {
                if let Some(touch) = self.seat.get_touch() {
                    touch.cancel(self);
                }
            }

            InputEvent::TouchFrame { .. } => {
                if let Some(touch) = self.seat.get_touch() {
                    touch.frame(self);
                }
            }

            // Touchpad gesture support: 3- or 4-finger horizontal swipe
            // switches workspace, matching the convention GNOME/KDE
            // touchpad users are already used to. Only swipes are handled
            // (no pinch-to-zoom/rotate binding exists in this compositor
            // yet - nothing to zoom or rotate outside a client surface's
            // own content, which clients already get raw pointer/touch
            // access to independent of this). Only ever fires for real
            // hardware through `backend_drm.rs`'s libinput source -
            // `winit_backend.rs`'s synthetic backend has no gesture
            // events to forward, so this is silently inert (never
            // matched) there, same as it was before this existed.
            InputEvent::GestureSwipeBegin { event } => {
                self.gesture_swipe = Some(GestureSwipeState { fingers: event.fingers(), dx: 0.0 });
            }

            InputEvent::GestureSwipeUpdate { event } => {
                if let Some(gesture) = self.gesture_swipe.as_mut() {
                    gesture.dx += event.delta_x();
                }
            }

            InputEvent::GestureSwipeEnd { event } => {
                if let Some(gesture) = self.gesture_swipe.take() {
                    // 3 or 4 fingers only - a 2-finger swipe is already
                    // meaningful as touchpad scrolling and must not be
                    // reinterpreted here.
                    if !event.cancelled() && matches!(gesture.fingers, 3 | 4) {
                        // Logical pixels of accumulated horizontal motion
                        // before a swipe counts as a deliberate workspace
                        // switch rather than an in-progress gesture the
                        // finger just hasn't lifted from yet. Arrived at
                        // by feel, not measurement (no real touchpad to
                        // test against in the environment this was
                        // written in) - the one number here most worth
                        // revisiting against real hardware first.
                        const SWIPE_THRESHOLD: f64 = 120.0;
                        if gesture.dx <= -SWIPE_THRESHOLD {
                            self.switch_workspace(self.active_workspace.saturating_add(1));
                        } else if gesture.dx >= SWIPE_THRESHOLD && self.active_workspace > 0 {
                            self.switch_workspace(self.active_workspace - 1);
                        }
                    }
                }
            }

            // Stylus/tablet input - the "still unhandled" item this
            // project's README has flagged for several passes now.
            // Structured to mirror `TouchDown`/`TouchMotion`/`TouchUp`/
            // `TouchFrame` above as closely as the two protocols'
            // shapes allow: proximity-in ~ touch-down, axis ~ motion,
            // proximity-out ~ touch-up, tip down/up is tablet-specific
            // (a stylus can hover *and* touch, touch can't).
            //
            // **Overall confidence note**: this is the single least-
            // verified addition in this codebase - `wlr-tablet-unstable-v2`
            // has more moving parts (per-tool AND per-tablet-device
            // registration, several transient sub-states) than anything
            // else added so far, and it was flagged as "not attempted"
            // in earlier passes specifically because of that. Attempting
            // it now that there's a real compile loop on the other end
            // rather than leaving it unimplemented indefinitely - each
            // call below is commented with its own specific confidence
            // level so a compile error points straight at the right
            // line instead of "somewhere in this whole block."
            InputEvent::TabletToolProximity { event } => {
                let output_geo = self
                    .space
                    .output_geometry(output)
                    .unwrap_or_else(|| smithay::utils::Rectangle::from_size((1920, 1080).into()));
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();

                let tablet_seat = self.seat.tablet_seat();
                // Cloned to an owned value up front: `add_tool` below
                // needs `self` itself (confirmed by the compiler:
                // `add_tool`'s first parameter is `&mut D`, i.e.
                // `&mut HwdeState`), and calling that in the same
                // expression as `&self.display_handle` would borrow
                // `self` two incompatible ways at once - `DisplayHandle`
                // is a cheap handle type (Smithay's equivalent of an
                // `Rc`-backed reference), so cloning it here rather than
                // restructuring the borrow is the simpler fix.
                let dh = self.display_handle.clone();
                let tablet_desc = TabletDescriptor::from(&event.device());
                // `add_tablet` - confirmed 2-arg (`dh`, `desc`), no `&mut
                // D` state parameter needed (unlike `add_tool` just
                // below) - presumably because registering a tablet
                // *device* never needs to call back into
                // `TabletSeatHandler` the way registering a *tool* does
                // (see `tablet_tool_image` in `handlers/tablet.rs`, which
                // only ever fires per-tool, never per-tablet).
                let tablet_handle = tablet_seat.add_tablet::<HwdeState>(&dh, &tablet_desc);

                let tool_desc = event.tool();
                // Confirmed 3-arg: `add_tool(state: &mut D, dh: &DisplayHandle, desc)`.
                let tool_handle = tablet_seat.add_tool::<HwdeState>(self, &dh, &tool_desc);

                let serial = SERIAL_COUNTER.next_serial();
                // `event.state()` - **still not fully confirmed**: the
                // first guess (`proximity_state()`) was wrong (confirmed
                // by compile error - the method plainly doesn't exist on
                // this associated type), and this is the next most
                // plausible name by analogy with `PointerButtonEvent::state()`
                // (already used elsewhere in this file, confirmed
                // working) - but that analogy already failed once for
                // this same event, so treat this as a guess, not a
                // correction. If this is also wrong, the compiler's next
                // error should at least list nearby method names on the
                // real trait, which the first error for this exact spot
                // didn't (no "help: a method with a similar name exists"
                // was given for `proximity_state`).
                match event.state() {
                    smithay::backend::input::ProximityState::In => {
                        if let Some((surface, surface_loc)) = self.surface_under(pos) {
                            // Confirmed 5-arg: `proximity_in(location, focus, tablet, serial, time)` - no `&Seat`.
                            tool_handle.proximity_in(pos, (surface, surface_loc), &tablet_handle, serial, event.time_msec());
                        }
                    }
                    smithay::backend::input::ProximityState::Out => {
                        tool_handle.proximity_out(event.time_msec());
                    }
                }
                // No `.frame()` on `TabletToolHandle` - confirmed by the
                // compiler (method doesn't exist on this type at all,
                // unlike `TouchHandle`/`PointerHandle`, which do have
                // one and are called with it elsewhere in this file).
                // Removed from every arm in this block rather than
                // guessed at again.
            }

            InputEvent::TabletToolAxis { event } => {
                let output_geo = self
                    .space
                    .output_geometry(output)
                    .unwrap_or_else(|| smithay::utils::Rectangle::from_size((1920, 1080).into()));
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();

                let tablet_seat = self.seat.tablet_seat();
                let tablet_desc = TabletDescriptor::from(&event.device());
                let Some(tablet_handle) = tablet_seat.get_tablet(&tablet_desc) else { return };
                let tool_desc = event.tool();
                let Some(tool_handle) = tablet_seat.get_tool(&tool_desc) else { return };

                let under = self.surface_under(pos);
                // Confirmed 5-arg: `motion(location, focus, tablet, serial, time)` - no `&Seat`.
                tool_handle.motion(pos, under, &tablet_handle, SERIAL_COUNTER.next_serial(), event.time_msec());

                // Confirmed: `pressure()` returns `f64` directly (not
                // `Option<f64>` as first guessed) - likewise `tilt()`
                // returning `(f64, f64)` directly. Sent unconditionally
                // on every axis event as a result, including ones that
                // didn't actually change pressure/tilt (there's no
                // "did this axis change" flag available without the
                // `Option` this doesn't have) - an accepted, harmless
                // simplification: re-sending an unchanged value is
                // wasted bandwidth on a local Unix/Wayland connection,
                // not a correctness problem.
                tool_handle.pressure(event.pressure());
                let (tilt_x, tilt_y) = event.tilt();
                tool_handle.tilt((tilt_x, tilt_y).into());
            }

            InputEvent::TabletToolTip { event } => {
                let tablet_seat = self.seat.tablet_seat();
                let tablet_desc = TabletDescriptor::from(&event.device());
                // Only used as a validity guard now (confirmed
                // `tip_down`/`tip_up` don't actually take a tablet-handle
                // argument - see below) - still worth bailing out if the
                // tablet itself isn't known, rather than assuming any
                // event this arbitrary is safe to act on.
                if tablet_seat.get_tablet(&tablet_desc).is_none() {
                    return;
                }
                let tool_desc = event.tool();
                let Some(tool_handle) = tablet_seat.get_tool(&tool_desc) else { return };

                let serial = SERIAL_COUNTER.next_serial();
                match event.tip_state() {
                    // Confirmed 2-arg: `tip_down(serial, time)` - no tablet-handle argument.
                    smithay::backend::input::TabletToolTipState::Down => tool_handle.tip_down(serial, event.time_msec()),
                    smithay::backend::input::TabletToolTipState::Up => tool_handle.tip_up(event.time_msec()),
                }
            }

            InputEvent::TabletToolButton { event } => {
                let tablet_seat = self.seat.tablet_seat();
                let tool_desc = event.tool();
                let Some(tool_handle) = tablet_seat.get_tool(&tool_desc) else { return };
                // Confirmed 4-arg: `button(button, state, serial, time)` - no `&Seat`.
                tool_handle.button(event.button(), event.button_state(), SERIAL_COUNTER.next_serial(), event.time_msec());
            }

            _ => {}
        }
    }
}

/// Accumulated state for one in-progress touchpad swipe gesture, from
/// `GestureSwipeBegin` to `GestureSwipeEnd` - see `process_input_event`.
/// Only tracks what workspace-switching needs (finger count, total
/// horizontal delta); vertical motion isn't tracked because nothing in
/// this compositor binds a vertical swipe to anything yet.
pub struct GestureSwipeState {
    pub fingers: u32,
    pub dx: f64,
}
