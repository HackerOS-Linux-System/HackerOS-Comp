use smithay::backend::input::{
    AbsolutePositionEvent, Axis, ButtonState, Event, InputBackend, InputEvent,
    KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, TouchEvent,
};
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::input::touch::{DownEvent, MotionEvent as TouchMotionEventData, UpEvent};
use smithay::output::Output;
use smithay::utils::SERIAL_COUNTER;

use crate::state::HwdeState;

impl HwdeState {
    pub fn process_input_event<B: InputBackend>(&mut self, event: InputEvent<B>, output: &Output) {
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

            _ => {}
        }
    }
}
