use smithay::{
    backend::input::{
        Axis, AxisSource, ButtonState, Device, DeviceCapability, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent,
        PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
        PointerMotionAbsoluteEvent,
        GestureSwipeBeginEvent, GestureSwipeUpdateEvent, GestureSwipeEndEvent,
        ProximityState, TabletToolAxisEvent, TabletToolButtonEvent, TabletToolProximityEvent,
        TabletToolTipEvent, TabletToolTipState,
        TouchDownEvent, TouchMotionEvent, TouchUpEvent, TouchCancelEvent,
    },
    desktop::WindowSurfaceType,
    input::{
        keyboard::{FilterResult, Keysym},
        pointer::{
            AxisFrame, ButtonEvent,
            GrabStartData as PointerGrabStartData,
            MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
        },
        touch::{DownEvent as TouchDownData, MotionEvent as TouchMotionData, UpEvent as TouchUpData},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive, Logical, Point, Rectangle, Size, SERIAL_COUNTER},
    wayland::seat::WaylandFocus,
    wayland::tablet_manager::{TabletDescriptor, TabletSeatTrait},
};

pub mod keybind;

use crate::state::BlueState;

pub fn handle_input<B: InputBackend>(state: &mut BlueState, event: InputEvent<B>) {
    state.record_input();

    match event {
        InputEvent::Keyboard { event } => handle_keyboard(state, &event),
        InputEvent::PointerMotion { event } => handle_pointer_motion(state, &event),
        InputEvent::PointerMotionAbsolute { event } => {
            handle_pointer_motion_abs(state, &event)
        }
        InputEvent::PointerButton { event } => handle_pointer_button(state, &event),
        InputEvent::PointerAxis { event } => handle_pointer_axis(state, &event),
        // Touch — was entirely absent before (fell into the wildcard
        // below and was silently dropped, along with the seat never
        // advertising the `touch` capability at all — see
        // `render/mod.rs`'s `add_touch()` calls, added alongside this).
        InputEvent::TouchDown { event } => handle_touch_down(state, &event),
        InputEvent::TouchMotion { event } => handle_touch_motion(state, &event),
        InputEvent::TouchUp { event } => handle_touch_up(state, &event),
        InputEvent::TouchCancel { event } => handle_touch_cancel(state, &event),
        InputEvent::TouchFrame { .. } => {
            if let Some(touch) = state.seat.get_touch() {
                touch.frame(state);
            }
        }
        // Tablet/stylus — `zwp_tablet_manager_v2` has been registered
        // since early in this project (TabletManagerState in
        // state/mod.rs) but, until now, nothing forwarded actual
        // proximity/motion/tip/button events to it — a real graphics
        // tablet's globals were visible to clients but every event from
        // it silently vanished here. Modeled directly on Smithay's own
        // anvil reference compositor (anvil/src/input_handler.rs's
        // on_tablet_tool_*), adapted to this project's existing
        // absolute-position/focus helpers instead of duplicating them.
        InputEvent::TabletToolAxis { event } => handle_tablet_axis(state, &event),
        InputEvent::TabletToolProximity { event } => handle_tablet_proximity(state, &event),
        InputEvent::TabletToolTip { event } => handle_tablet_tip(state, &event),
        InputEvent::TabletToolButton { event } => handle_tablet_button(state, &event),
        // 3/4-finger touchpad swipe → workspace switch. See
        // handle_gesture_swipe_end's doc for the direction convention
        // and why 1/2-finger swipes (ordinary scroll/pointer gestures,
        // reported through this same libinput event stream) are
        // deliberately ignored rather than also switching workspaces.
        InputEvent::GestureSwipeBegin { event } => handle_gesture_swipe_begin(state, &event),
        InputEvent::GestureSwipeUpdate { event } => handle_gesture_swipe_update(state, &event),
        InputEvent::GestureSwipeEnd { event } => handle_gesture_swipe_end(state, &event),
        InputEvent::DeviceAdded { device } => {
            if device.has_capability(DeviceCapability::TabletTool) {
                state
                    .seat
                    .tablet_seat()
                    .add_tablet::<BlueState>(&state.display_handle, &TabletDescriptor::from(&device));
            }
        }
        InputEvent::DeviceRemoved { device } => {
            if device.has_capability(DeviceCapability::TabletTool) {
                let tablet_seat = state.seat.tablet_seat();
                tablet_seat.remove_tablet(&TabletDescriptor::from(&device));
                // No tablets left on the seat — drop tools too, rather
                // than leaving stale tool objects a client could still
                // query state from after the last physical tablet was
                // unplugged.
                if tablet_seat.count_tablets() == 0 {
                    tablet_seat.clear_tools();
                }
            }
        }
        _ => {}
    }
}

// ── Keyboard ──────────────────────────────────────────────────────────────

fn handle_keyboard<B: InputBackend, E: KeyboardKeyEvent<B>>(
    state: &mut BlueState,
    event: &E,
) {
    let serial = SERIAL_COUNTER.next_serial();
    let keyboard = state.seat.get_keyboard().unwrap();

    keyboard.input(
        state,
        event.key_code(),
        event.state(),
        serial,
        event.time_msec(),
        |state, mods, handle| {
            let sym = handle.modified_sym();
            let pressed = event.state() == KeyState::Pressed;

            // ── Alt+Tab (window switcher) ─────────────────────────────────
            if mods.alt && sym == Keysym::Tab && pressed {
                if !state.show_switcher {
                    state.show_switcher = true;
                    state.switcher_index = 0;
                } else {
                    state.cycle_switcher(true);
                }
                return FilterResult::Intercept(());
            }

            // ── Alt+Shift+Tab (backwards switcher) ────────────────────────
            if mods.alt && mods.shift && sym == Keysym::Tab && pressed {
                if state.show_switcher {
                    state.cycle_switcher(false);
                }
                return FilterResult::Intercept(());
            }

            // ── Alt release → commit switcher ─────────────────────────────
            if (sym == Keysym::Alt_L || sym == Keysym::Alt_R)
                && event.state() == KeyState::Released
                && state.show_switcher
            {
                state.apply_switcher_selection();
                return FilterResult::Intercept(());
            }

            // ── Super / Win key ───────────────────────────────────────────
            if sym == Keysym::Super_L || sym == Keysym::Super_R {
                if pressed {
                    state.super_pressed = true;
                    state.super_used = false;
                } else {
                    if state.super_pressed && !state.super_used {
                        state.toggle_start_menu();
                    }
                    state.super_pressed = false;
                    state.super_used = false;
                }
                return FilterResult::Intercept(());
            }

            // ── Win+Tab → full-screen app picker ─────────────────────────
            if mods.logo && sym == Keysym::Tab && pressed {
                state.super_used = true;
                state.toggle_fullscreen_menu();
                return FilterResult::Intercept(());
            }

            // ── Win+1..4 → switch workspace ───────────────────────────────
            if mods.logo && pressed {
                let ws = match sym {
                    Keysym::_1 => Some(0usize),
                    Keysym::_2 => Some(1),
                    Keysym::_3 => Some(2),
                    Keysym::_4 => Some(3),
                    _ => None,
                };
                if let Some(idx) = ws {
                    state.super_used = true;
                    state.switch_workspace(idx);
                    return FilterResult::Intercept(());
                }
            }

            // ── Win+Arrow → workspace ─────────────────────────────────────
            if mods.logo && sym == Keysym::Right && pressed {
                state.super_used = true;
                let next = (state.current_workspace + 1).min(state.workspace_count - 1);
                state.switch_workspace(next);
                return FilterResult::Intercept(());
            }
            if mods.logo && sym == Keysym::Left && pressed {
                state.super_used = true;
                let prev = state.current_workspace.saturating_sub(1);
                state.switch_workspace(prev);
                return FilterResult::Intercept(());
            }

            // ── Win+Up → maximize focused window ─────────────────────────
            if mods.logo && sym == Keysym::Up && pressed {
                state.super_used = true;
                if let Some(surface) = state.seat.get_keyboard().unwrap().current_focus() {
                    if let Some(win) = state.window_by_surface(&surface) {
                        if let Some(t) = win.toplevel() {
                            t.with_pending_state(|s| {
                                if s.states.contains(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized) {
                                    s.states.unset(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized);
                                } else {
                                    s.states.set(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized);
                                }
                            });
                            t.send_configure();
                        }
                    }
                }
                return FilterResult::Intercept(());
            }

            // ── Win+Down → minimize focused window ───────────────────────
            if mods.logo && sym == Keysym::Down && pressed {
                state.super_used = true;
                if let Some(surface) = state.seat.get_keyboard().unwrap().current_focus() {
                    if let Some(win) = state.window_by_surface(&surface) {
                        let id = BlueState::window_id(&win);
                        if let Some(meta) = state.window_meta.get_mut(&id) {
                            meta.is_minimized = true;
                        }
                    }
                }
                return FilterResult::Intercept(());
            }

            // ── Alt+F4 → close focused window ─────────────────────────────
            if mods.alt && sym == Keysym::F4 && pressed {
                if let Some(surface) = state.seat.get_keyboard().unwrap().current_focus() {
                    if let Some(win) = state.window_by_surface(&surface) {
                        if let Some(t) = win.toplevel() {
                            t.send_close();
                        }
                    }
                }
                return FilterResult::Intercept(());
            }

            // ── Ctrl+Alt+T → launch terminal ──────────────────────────────
            if mods.ctrl && mods.alt && sym == Keysym::t && pressed {
                let _ = std::process::Command::new("sh")
                    .args(["-c", "kitty & || alacritty & || gnome-terminal & || xterm &"])
                    .spawn();
                return FilterResult::Intercept(());
            }

            // ── PrintScreen → screenshot ──────────────────────────────────
            if sym == Keysym::Print && pressed {
                let home = dirs::home_dir().unwrap_or_default();
                let path = home
                    .join("Pictures")
                    .join(format!(
                        "screenshot_{}.png",
                        chrono::Local::now().format("%Y%m%d_%H%M%S")
                    ))
                    .to_string_lossy()
                    .to_string();
                let _ = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(format!(
                        "flameshot gui -p '{}' 2>/dev/null || scrot '{}' 2>/dev/null",
                        path, path
                    ))
                    .spawn();
                return FilterResult::Intercept(());
            }

            // ── Escape → close panels / switcher ──────────────────────────
            if sym == Keysym::Escape && pressed {
                if state.show_switcher {
                    state.show_switcher = false;
                    return FilterResult::Intercept(());
                }
            }

            // ── config.hk-driven keybindings ────────────────────────────────
            // Everything above this point is a hardcoded binding. This is
            // the config-driven path: `Config.keybindings` (see
            // `src/config.rs`) is compiled into a `Keybindings` matcher
            // (see `src/input/keybind.rs` for why the parsing/matching
            // logic lives in its own smithay-free module) and consulted
            // here for anything a person has actually configured — see
            // `dispatch_keybind_action` for what each recognized action
            // name does. Runs after every hardcoded binding above so a
            // person can't accidentally shadow Alt+Tab/Alt+F4/Escape by
            // rebinding the same combo to something else in config.hk;
            // making the hardcoded set itself configurable is real,
            // separate follow-up work (see ROADMAP.md), not something
            // this pass changes.
            if pressed {
                if let Some(key_name) = keysym_name(sym) {
                    let modifiers = keybind::Modifiers {
                        super_: mods.logo,
                        shift: mods.shift,
                        ctrl: mods.ctrl,
                        alt: mods.alt,
                    };
                    // Compiled fresh from `state.config.keybindings` on
                    // every keypress rather than cached on `BlueState` —
                    // simpler and correctness-first for now (a person's
                    // typing speed is nowhere near where re-parsing a
                    // handful of short strings per keypress would be
                    // measurable), at the cost of doing more work than
                    // strictly needed per key event. Caching this
                    // (rebuilt only when `SdeCall::ReloadConfig`/
                    // HackerLand's `dispatch reload` actually changes
                    // `state.config`) is real, separate follow-up work —
                    // see ROADMAP.md.
                    let (bindings, _parse_errors) = keybind::Keybindings::from_config(&state.config.keybindings);
                    if let Some(action) = bindings.action_for(modifiers, &key_name) {
                        let action = action.to_string();
                        if dispatch_keybind_action(state, &action) {
                            return FilterResult::Intercept(());
                        }
                    }
                }
            }

            FilterResult::Forward
        },
    );
}

/// Turns a `Keysym` into the canonical lowercase key-name shape
/// `keybind::parse_combo` produces (`"q"`, `"return"`, `"f4"`, `"1"`,
/// ...) — the one part of this feature that genuinely can't be tested
/// without a real `Keysym` value, kept as its own small, easily-audited
/// function precisely so that's the *only* untested part (see
/// `src/input/keybind.rs`'s module doc). Coverage here matches what
/// `config::default_keybindings()` actually uses today
/// (letters/digits/`Return`/`Tab`/`Space`/function keys/arrows) rather
/// than attempting an exhaustive `Keysym -> name` table — extend this
/// as real key combos need more coverage, rather than guessing ahead of
/// demand at every key on a keyboard.
fn keysym_name(sym: Keysym) -> Option<String> {
    let name = match sym {
        Keysym::a => "a", Keysym::b => "b", Keysym::c => "c", Keysym::d => "d",
        Keysym::e => "e", Keysym::f => "f", Keysym::g => "g", Keysym::h => "h",
        Keysym::i => "i", Keysym::j => "j", Keysym::k => "k", Keysym::l => "l",
        Keysym::m => "m", Keysym::n => "n", Keysym::o => "o", Keysym::p => "p",
        Keysym::q => "q", Keysym::r => "r", Keysym::s => "s", Keysym::t => "t",
        Keysym::u => "u", Keysym::v => "v", Keysym::w => "w", Keysym::x => "x",
        Keysym::y => "y", Keysym::z => "z",
        Keysym::_0 => "0", Keysym::_1 => "1", Keysym::_2 => "2", Keysym::_3 => "3",
        Keysym::_4 => "4", Keysym::_5 => "5", Keysym::_6 => "6", Keysym::_7 => "7",
        Keysym::_8 => "8", Keysym::_9 => "9",
        Keysym::F1 => "f1", Keysym::F2 => "f2", Keysym::F3 => "f3", Keysym::F4 => "f4",
        Keysym::F5 => "f5", Keysym::F6 => "f6", Keysym::F7 => "f7", Keysym::F8 => "f8",
        Keysym::F9 => "f9", Keysym::F10 => "f10", Keysym::F11 => "f11", Keysym::F12 => "f12",
        Keysym::Return => "return",
        Keysym::Tab => "tab",
        Keysym::space => "space",
        Keysym::Escape => "escape",
        Keysym::BackSpace => "backspace",
        Keysym::Delete => "delete",
        Keysym::Left => "left",
        Keysym::Right => "right",
        Keysym::Up => "up",
        Keysym::Down => "down",
        Keysym::Print => "print",
        _ => return None,
    };
    Some(name.to_string())
}

/// Executes a config-driven keybinding action by name — the recognized
/// action-name vocabulary `Config.keybindings`'s keys are matched
/// against (see `config::default_keybindings()` in `src/config.rs` for
/// the shipped defaults). Returns `true` if `action` was recognized and
/// handled (so the caller should intercept the keypress rather than
/// forwarding it to the focused client) — an unrecognized action name
/// (a typo in someone's `config.hk`, or a name from a future version)
/// returns `false` rather than panicking, so the keypress just falls
/// through to the focused client as if nothing were bound to it.
fn dispatch_keybind_action(state: &mut BlueState, action: &str) -> bool {
    match action {
        "close_window" => {
            if let Some(surface) = state.seat.get_keyboard().unwrap().current_focus() {
                if let Some(win) = state.window_by_surface(&surface) {
                    let id = BlueState::window_id(&win);
                    state.close_window_by_id(id);
                }
            }
            true
        }
        "toggle_fullscreen" => {
            if let Some(surface) = state.seat.get_keyboard().unwrap().current_focus() {
                if let Some(win) = state.window_by_surface(&surface) {
                    if let Some(t) = win.toplevel() {
                        t.with_pending_state(|s| {
                            if s.states.contains(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen) {
                                s.states.unset(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen);
                            } else {
                                s.states.set(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen);
                            }
                        });
                        t.send_pending_configure();
                    }
                }
            }
            true
        }
        "toggle_floating" => {
            if let Some(surface) = state.seat.get_keyboard().unwrap().current_focus() {
                if let Some(win) = state.window_by_surface(&surface) {
                    let id = BlueState::window_id(&win);
                    state.toggle_floating_by_id(id);
                }
            }
            true
        }
        "cycle_windows" => {
            // Same behavior as the hardcoded Alt+Tab binding above —
            // a person who's rebound this action gets the same window
            // switcher, just under whatever combo they chose instead
            // of Alt+Tab.
            if !state.show_switcher {
                state.show_switcher = true;
                state.switcher_index = 0;
            } else {
                state.cycle_switcher(true);
            }
            true
        }
        "launch_terminal" => {
            let _ = std::process::Command::new("sh")
                .args(["-c", "kitty & || alacritty & || gnome-terminal & || xterm &"])
                .spawn();
            true
        }
        _ => {
            if let Some(n) = action.strip_prefix("workspace_").and_then(|s| s.parse::<usize>().ok()) {
                if n >= 1 {
                    state.switch_workspace(n - 1);
                    return true;
                }
            }
            false
        }
    }
}

// ── Pointer motion ────────────────────────────────────────────────────────

fn handle_pointer_motion<B: InputBackend, E: PointerMotionEvent<B>>(
    state: &mut BlueState,
    event: &E,
) {
    let serial = SERIAL_COUNTER.next_serial();
    let delta = event.delta();

    // Clamp to output bounds
    let (min_x, min_y, max_x, max_y) = output_bounds(state);
    state.pointer_location.x = (state.pointer_location.x + delta.x).clamp(min_x, max_x);
    state.pointer_location.y = (state.pointer_location.y + delta.y).clamp(min_y, max_y);

    update_pointer_focus(state, serial, event.time_msec());
}

fn handle_pointer_motion_abs<B: InputBackend, E: PointerMotionAbsoluteEvent<B>>(
    state: &mut BlueState,
    event: &E,
) {
    let serial = SERIAL_COUNTER.next_serial();
    let size = {
        state
            .space
            .outputs()
            .next()
            .and_then(|o| state.space.output_geometry(o))
            .map(|g| g.size)
            .unwrap_or(Size::from((1920, 1080)))
    };
    state.pointer_location = event.position_transformed(size);
    update_pointer_focus(state, serial, event.time_msec());
}

pub(crate) fn output_bounds(state: &BlueState) -> (f64, f64, f64, f64) {
    state
        .space
        .outputs()
        .next()
        .and_then(|o| state.space.output_geometry(o))
        .map(|g| {
            (
                g.loc.x as f64,
                g.loc.y as f64,
                (g.loc.x + g.size.w) as f64,
                (g.loc.y + g.size.h) as f64,
            )
        })
        .unwrap_or((0.0, 0.0, 1920.0, 1080.0))
}

/// When the session is locked, keyboard/pointer/touch focus must go to
/// the lock surface for whatever output the input is on — never to a
/// regular window underneath it. Without this, `session-lock`'s
/// protocol handshake (protocols/session_lock.rs) is purely decorative:
/// a client can `lock()` and get `is_locked = true`, but a click or
/// keypress would still reach whatever window is under the pointer,
/// same as if nothing were locked at all. This is the security-audit
/// finding this closes — see the compositor security notes' "prawdziwy
/// ekran blokady" section for the fuller writeup.
///
/// Returns `None` (falls through to normal hit-testing) when not
/// locked, or when locked but no lock surface has been created yet for
/// the relevant output (e.g. the brief window between `lock()` being
/// granted and the lock client actually mapping its per-output
/// surfaces) — input is simply dropped on the floor in that gap rather
/// than reaching an unintended window, which is the safe direction to
/// fail in.
fn locked_focus(state: &BlueState, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
    if !state.is_locked {
        return None;
    }
    let output = state.space.output_under(pos).next()?;
    let lock_surface = state.lock_surfaces.get(&output.name())?;
    if !lock_surface.alive() {
        return None;
    }
    let output_loc = state.space.output_geometry(output)?.loc.to_f64();
    Some((lock_surface.wl_surface().clone(), output_loc))
}

pub(crate) fn update_pointer_focus(state: &mut BlueState, serial: smithay::utils::Serial, time: u32) {
    let pointer = state.seat.get_pointer().unwrap();
    let pos = state.pointer_location;

    let focus: Option<(WlSurface, Point<f64, Logical>)> = locked_focus(state, pos).or_else(|| {
        state
            .space
            .element_under(pos)
            .and_then(|(win, win_loc)| {
                let rel = pos - win_loc.to_f64();
                win.surface_under(rel, WindowSurfaceType::ALL)
                    .map(|(s, sp)| (s, (win_loc + sp).to_f64()))
            })
    });

    pointer.motion(
        state,
        focus,
        &MotionEvent {
            location: pos,
            serial,
            time,
        },
    );
    pointer.frame(state);
}

// ── Pointer button ────────────────────────────────────────────────────────

fn handle_pointer_button<B: InputBackend, E: PointerButtonEvent<B>>(
    state: &mut BlueState,
    event: &E,
) {
    let serial = SERIAL_COUNTER.next_serial();
    let pos = state.pointer_location;

    if event.state() == ButtonState::Pressed {
        if state.is_locked {
            // While locked, a click must never raise/focus a regular
            // window — only ever (re-)confirm focus on the lock
            // surface, so a keypress right after this click still goes
            // there too (keyboard focus and pointer focus are set
            // independently in Smithay; without this, clicking during
            // a locked session — even though update_pointer_focus above
            // already keeps *pointer* focus on the lock surface — could
            // still leave stale *keyboard* focus on whatever window had
            // it before locking).
            let keyboard = state.seat.get_keyboard().unwrap();
            match locked_focus(state, pos) {
                Some((surface, _)) => keyboard.set_focus(state, Some(surface), serial),
                None => keyboard.set_focus(state, Option::<WlSurface>::None, serial),
            }
        } else if let Some(window) = state.space.element_under(pos).map(|(w, _)| w.clone()) {
            state.space.raise_element(&window, true);
            let keyboard = state.seat.get_keyboard().unwrap();
            if let Some(surface) = window.wl_surface() {
                keyboard.set_focus(state, Some(surface.into_owned()), serial);
            }
        } else {
            // Click on empty desktop - unfocus
            let keyboard = state.seat.get_keyboard().unwrap();
            keyboard.set_focus(state, Option::<WlSurface>::None, serial);
        }
    }

    let pointer = state.seat.get_pointer().unwrap();
    pointer.button(
        state,
        &ButtonEvent {
            button: event.button_code(),
            state: event.state(),
            serial,
            time: event.time_msec(),
        },
    );
    pointer.frame(state);
}

// ── Pointer axis (scroll) ─────────────────────────────────────────────────

fn handle_pointer_axis<B: InputBackend, E: PointerAxisEvent<B>>(
    state: &mut BlueState,
    event: &E,
) {
    let pointer = state.seat.get_pointer().unwrap();
    let mut frame = AxisFrame::new(event.time_msec()).source(AxisSource::Wheel);

    for axis in [Axis::Horizontal, Axis::Vertical] {
        if let Some(v) = event.amount(axis) {
            frame = frame
                .relative_direction(axis, event.relative_direction(axis))
                .value(axis, v);
            if let Some(d) = event.amount_v120(axis) {
                frame = frame.v120(axis, d as i32);
            }
        }
    }

    pointer.axis(state, frame);
    pointer.frame(state);
}

// ── Move grab ─────────────────────────────────────────────────────────────

pub struct MoveGrab {
    pub start_data: PointerGrabStartData<BlueState>,
    pub window: smithay::desktop::Window,
    pub initial_window_location: Point<i32, Logical>,
}

impl PointerGrab<BlueState> for MoveGrab {
    fn motion(
        &mut self,
        data: &mut BlueState,
        handle: &mut PointerInnerHandle<'_, BlueState>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        let delta = event.location - self.start_data.location;
        let new_loc = self.initial_window_location + delta.to_i32_round();
        data.space.map_element(self.window.clone(), new_loc, true);
    }

    fn relative_motion(
        &mut self,
        data: &mut BlueState,
        handle: &mut PointerInnerHandle<'_, BlueState>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut BlueState,
        handle: &mut PointerInnerHandle<'_, BlueState>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if event.state == ButtonState::Released {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut BlueState,
        handle: &mut PointerInnerHandle<'_, BlueState>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(
        &mut self,
        data: &mut BlueState,
        handle: &mut PointerInnerHandle<'_, BlueState>,
    ) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        _: &mut BlueState,
        _: &mut PointerInnerHandle<'_, BlueState>,
        _: &smithay::input::pointer::GestureSwipeBeginEvent,
    ) {
    }
    fn gesture_swipe_update(
        &mut self,
        _: &mut BlueState,
        _: &mut PointerInnerHandle<'_, BlueState>,
        _: &smithay::input::pointer::GestureSwipeUpdateEvent,
    ) {
    }
    fn gesture_swipe_end(
        &mut self,
        _: &mut BlueState,
        _: &mut PointerInnerHandle<'_, BlueState>,
        _: &smithay::input::pointer::GestureSwipeEndEvent,
    ) {
    }
    fn gesture_pinch_begin(
        &mut self,
        _: &mut BlueState,
        _: &mut PointerInnerHandle<'_, BlueState>,
        _: &smithay::input::pointer::GesturePinchBeginEvent,
    ) {
    }
    fn gesture_pinch_update(
        &mut self,
        _: &mut BlueState,
        _: &mut PointerInnerHandle<'_, BlueState>,
        _: &smithay::input::pointer::GesturePinchUpdateEvent,
    ) {
    }
    fn gesture_pinch_end(
        &mut self,
        _: &mut BlueState,
        _: &mut PointerInnerHandle<'_, BlueState>,
        _: &smithay::input::pointer::GesturePinchEndEvent,
    ) {
    }
    fn gesture_hold_begin(
        &mut self,
        _: &mut BlueState,
        _: &mut PointerInnerHandle<'_, BlueState>,
        _: &smithay::input::pointer::GestureHoldBeginEvent,
    ) {
    }
    fn gesture_hold_end(
        &mut self,
        _: &mut BlueState,
        _: &mut PointerInnerHandle<'_, BlueState>,
        _: &smithay::input::pointer::GestureHoldEndEvent,
    ) {
    }

    fn start_data(&self) -> &PointerGrabStartData<BlueState> {
        &self.start_data
    }

    fn unset(&mut self, _: &mut BlueState) {}
}

pub fn start_move_grab(
    state: &mut BlueState,
    window: smithay::desktop::Window,
    start_data: PointerGrabStartData<BlueState>,
    _serial: smithay::utils::Serial,
) {
    let initial = state
        .space
        .element_location(&window)
        .unwrap_or_default();

    let grab = MoveGrab {
        start_data,
        window,
        initial_window_location: initial,
    };

    state.seat.get_pointer().unwrap().set_grab(
        state,
        grab,
        SERIAL_COUNTER.next_serial(),
        smithay::input::pointer::Focus::Clear,
    );
}

// ── Resize grab ───────────────────────────────────────────────────────────
//
// Previously `resize_request` was a no-op stub for both xdg-shell toplevels
// (state/mod.rs) and XWayland/X11 windows (xwayland/mod.rs) — dragging a
// window's edge/corner from a client-side decoration or the compositor's
// own titlebar did nothing. This mirrors the existing `MoveGrab` pattern.
//
// Note on correctness: for xdg-shell toplevels the "proper" way to handle
// north/west edge resizes is to let the client ack the new size via
// `xdg_surface.configure` and only reposition the window once the new
// buffer has actually committed (otherwise the window can visually jitter
// for a frame or two while the client catches up). This implementation
// takes the simpler approach of resizing eagerly, which is a large
// functional improvement over "resize does nothing at all" and matches
// what many lightweight compositors do, but a follow-up could track
// pending-size-vs-committed-size per window for pixel-perfect behavior.

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResizeEdges {
    pub top: bool,
    pub bottom: bool,
    pub left: bool,
    pub right: bool,
}

impl From<smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge> for ResizeEdges {
    fn from(e: smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge) -> Self {
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge as E;
        match e {
            E::Top => Self { top: true, ..Default::default() },
            E::Bottom => Self { bottom: true, ..Default::default() },
            E::Left => Self { left: true, ..Default::default() },
            E::Right => Self { right: true, ..Default::default() },
            E::TopLeft => Self { top: true, left: true, ..Default::default() },
            E::TopRight => Self { top: true, right: true, ..Default::default() },
            E::BottomLeft => Self { bottom: true, left: true, ..Default::default() },
            E::BottomRight => Self { bottom: true, right: true, ..Default::default() },
            _ => Self::default(),
        }
    }
}

impl From<smithay::xwayland::xwm::ResizeEdge> for ResizeEdges {
    // smithay's X11 `ResizeEdge` variant names have shifted a bit across
    // revisions; matched defensively with a wildcard fallback so this
    // keeps compiling even if the pinned rev's variant set differs
    // slightly (worst case: an unrecognized edge falls back to a
    // bottom-right resize, which is the most common default anyway).
    fn from(e: smithay::xwayland::xwm::ResizeEdge) -> Self {
        use smithay::xwayland::xwm::ResizeEdge as E;
        match e {
            E::Top => Self { top: true, ..Default::default() },
            E::Bottom => Self { bottom: true, ..Default::default() },
            E::Left => Self { left: true, ..Default::default() },
            E::Right => Self { right: true, ..Default::default() },
            E::TopLeft => Self { top: true, left: true, ..Default::default() },
            E::TopRight => Self { top: true, right: true, ..Default::default() },
            E::BottomLeft => Self { bottom: true, left: true, ..Default::default() },
            E::BottomRight => Self { bottom: true, right: true, ..Default::default() },
            #[allow(unreachable_patterns)]
            _ => Self { bottom: true, right: true, ..Default::default() },
        }
    }
}

pub struct ResizeGrab {
    pub start_data: PointerGrabStartData<BlueState>,
    pub window: smithay::desktop::Window,
    pub edges: ResizeEdges,
    pub initial_window_location: Point<i32, Logical>,
    pub initial_window_size: Size<i32, Logical>,
}

const MIN_WINDOW_SIZE: i32 = 32;

impl PointerGrab<BlueState> for ResizeGrab {
    fn motion(
        &mut self,
        data: &mut BlueState,
        handle: &mut PointerInnerHandle<'_, BlueState>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        let delta = (event.location - self.start_data.location).to_i32_round::<i32>();

        let mut new_w = self.initial_window_size.w;
        let mut new_h = self.initial_window_size.h;
        let mut new_x = self.initial_window_location.x;
        let mut new_y = self.initial_window_location.y;

        if self.edges.right {
            new_w = (self.initial_window_size.w + delta.x).max(MIN_WINDOW_SIZE);
        } else if self.edges.left {
            new_w = (self.initial_window_size.w - delta.x).max(MIN_WINDOW_SIZE);
            new_x = self.initial_window_location.x + (self.initial_window_size.w - new_w);
        }
        if self.edges.bottom {
            new_h = (self.initial_window_size.h + delta.y).max(MIN_WINDOW_SIZE);
        } else if self.edges.top {
            new_h = (self.initial_window_size.h - delta.y).max(MIN_WINDOW_SIZE);
            new_y = self.initial_window_location.y + (self.initial_window_size.h - new_h);
        }

        let new_size = Size::from((new_w, new_h));
        let new_loc = Point::from((new_x, new_y));
        apply_resize(data, &self.window, new_loc, new_size, self.edges);
    }

    fn relative_motion(
        &mut self,
        data: &mut BlueState,
        handle: &mut PointerInnerHandle<'_, BlueState>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut BlueState,
        handle: &mut PointerInnerHandle<'_, BlueState>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if event.state == ButtonState::Released {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut BlueState,
        handle: &mut PointerInnerHandle<'_, BlueState>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut BlueState, handle: &mut PointerInnerHandle<'_, BlueState>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(&mut self, _: &mut BlueState, _: &mut PointerInnerHandle<'_, BlueState>, _: &smithay::input::pointer::GestureSwipeBeginEvent) {}
    fn gesture_swipe_update(&mut self, _: &mut BlueState, _: &mut PointerInnerHandle<'_, BlueState>, _: &smithay::input::pointer::GestureSwipeUpdateEvent) {}
    fn gesture_swipe_end(&mut self, _: &mut BlueState, _: &mut PointerInnerHandle<'_, BlueState>, _: &smithay::input::pointer::GestureSwipeEndEvent) {}
    fn gesture_pinch_begin(&mut self, _: &mut BlueState, _: &mut PointerInnerHandle<'_, BlueState>, _: &smithay::input::pointer::GesturePinchBeginEvent) {}
    fn gesture_pinch_update(&mut self, _: &mut BlueState, _: &mut PointerInnerHandle<'_, BlueState>, _: &smithay::input::pointer::GesturePinchUpdateEvent) {}
    fn gesture_pinch_end(&mut self, _: &mut BlueState, _: &mut PointerInnerHandle<'_, BlueState>, _: &smithay::input::pointer::GesturePinchEndEvent) {}
    fn gesture_hold_begin(&mut self, _: &mut BlueState, _: &mut PointerInnerHandle<'_, BlueState>, _: &smithay::input::pointer::GestureHoldBeginEvent) {}
    fn gesture_hold_end(&mut self, _: &mut BlueState, _: &mut PointerInnerHandle<'_, BlueState>, _: &smithay::input::pointer::GestureHoldEndEvent) {}

    fn start_data(&self) -> &PointerGrabStartData<BlueState> {
        &self.start_data
    }

    fn unset(&mut self, _: &mut BlueState) {}
}

/// Pushes a resized geometry to whichever kind of window this is —
/// xdg-shell toplevel (via a new configure) or an XWayland/X11 window
/// (via a direct `configure()`, since X11 has no client-ack round trip).
fn apply_resize(
    state: &mut BlueState,
    window: &smithay::desktop::Window,
    new_loc: Point<i32, Logical>,
    new_size: Size<i32, Logical>,
    edges: ResizeEdges,
) {
    if let Some(toplevel) = window.toplevel() {
        toplevel.with_pending_state(|s| {
            s.size = Some(new_size);
        });
        toplevel.send_configure();
        // Only reposition eagerly for edges that move the window's origin
        // (top/left) — the alternative (waiting for the client's next
        // commit) is more correct but requires per-window pending-state
        // tracking that doesn't exist yet.
        if edges.top || edges.left {
            state.space.map_element(window.clone(), new_loc, false);
        }
    } else if let Some(x11) = window.x11_surface() {
        let geo = Rectangle::new(new_loc, new_size);
        if let Err(e) = x11.configure(geo) {
            tracing::warn!("X11 resize configure failed: {}", e);
        }
        state.space.map_element(window.clone(), new_loc, false);
    }
}

pub fn start_resize_grab(
    state: &mut BlueState,
    window: smithay::desktop::Window,
    start_data: PointerGrabStartData<BlueState>,
    edges: ResizeEdges,
) {
    let initial_window_location = state.space.element_location(&window).unwrap_or_default();
    let initial_window_size = window.geometry().size;

    let grab = ResizeGrab {
        start_data,
        window,
        edges,
        initial_window_location,
        initial_window_size,
    };

    state.seat.get_pointer().unwrap().set_grab(
        state,
        grab,
        SERIAL_COUNTER.next_serial(),
        smithay::input::pointer::Focus::Clear,
    );
}

// ── Touch ────────────────────────────────────────────────────────────────
//
// New — this seat previously never advertised the `touch` capability at
// all (see `render/mod.rs`'s `add_touch()` calls, added alongside this),
// so every `InputEvent::Touch*` variant fell into `handle_input`'s
// wildcard and was silently dropped, regardless of what hardware sent
// them (a touchscreen, or the winit backend's own touch emulation when
// nested inside a host compositor that has one).
//
// Written without a compiler available to verify the exact smithay
// touch API surface at this pinned rev against (same caveat this file's
// pointer/keyboard code doesn't need anymore having presumably been
// fixed against real compile errors already, but genuinely applies here
// since touch is new) — structured to mirror the pointer handlers
// directly above as closely as the wl_touch protocol's actual semantics
// allow, which is the one thing I'm confident about regardless of exact
// method signatures: unlike wl_pointer, focus for a given touch point
// is resolved *once*, at touch-down, from the touch point's position at
// that moment — motion/up for that same touch id then keep going to
// whatever surface was under it at down, even if the finger slides off
// that surface's bounds entirely (real wl_touch protocol behavior, not
// specific to this compositor).

/// Resolves which surface (if any) is under a global point — the same
/// hit-testing `update_pointer_focus` above already does for the
/// pointer, factored out so touch-down can reuse it without duplicating
/// the `space.element_under` + `surface_under` dance.
fn surface_under_point(state: &BlueState, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
    locked_focus(state, pos).or_else(|| {
        state
            .space
            .element_under(pos)
            .and_then(|(win, win_loc)| {
                let rel = pos - win_loc.to_f64();
                win.surface_under(rel, WindowSurfaceType::ALL)
                    .map(|(s, sp)| (s, (win_loc + sp).to_f64()))
            })
    })
}

/// Touch is inherently absolute (a touchscreen's coordinate space maps
/// directly onto the output, same reasoning as
/// `PointerMotionAbsoluteEvent::position_transformed` above) — this
/// mirrors `handle_pointer_motion_abs`'s own output-size lookup exactly
/// rather than introducing a second way to get it.
fn output_size_for_touch(state: &BlueState) -> Size<i32, Logical> {
    state
        .space
        .outputs()
        .next()
        .and_then(|o| state.space.output_geometry(o))
        .map(|g| g.size)
        .unwrap_or(Size::from((1920, 1080)))
}

fn handle_touch_down<B: InputBackend, E: TouchDownEvent<B>>(state: &mut BlueState, event: &E) {
    let Some(touch) = state.seat.get_touch() else { return };
    let serial = SERIAL_COUNTER.next_serial();
    let size = output_size_for_touch(state);
    let position = event.position_transformed(size);
    let focus = surface_under_point(state, position);

    touch.down(
        state,
        focus,
        &TouchDownData {
            slot: event.slot(),
            location: position,
            serial,
            time: event.time_msec(),
        },
    );
}

fn handle_touch_motion<B: InputBackend, E: TouchMotionEvent<B>>(state: &mut BlueState, event: &E) {
    let Some(touch) = state.seat.get_touch() else { return };
    let size = output_size_for_touch(state);
    let position = event.position_transformed(size);
    // Per wl_touch semantics (see this section's own header note): focus
    // for this slot was already fixed at touch-down and isn't
    // re-resolved here — passed as `None` on the theory that smithay's
    // `TouchHandle::motion` looks up the slot's already-established
    // focus internally (mirroring how `PointerHandle::motion` is the
    // one that takes an explicit focus, but touch's per-slot routing is
    // a different enough model that it may not need it passed again).
    // Flagged clearly since this is the least-confident guess in this
    // whole section.
    touch.motion(
        state,
        None,
        &TouchMotionData {
            slot: event.slot(),
            location: position,
            time: event.time_msec(),
        },
    );
}

fn handle_touch_up<B: InputBackend, E: TouchUpEvent<B>>(state: &mut BlueState, event: &E) {
    let Some(touch) = state.seat.get_touch() else { return };
    let serial = SERIAL_COUNTER.next_serial();
    touch.up(
        state,
        &TouchUpData {
            slot: event.slot(),
            serial,
            time: event.time_msec(),
        },
    );
}

fn handle_touch_cancel<B: InputBackend, E: TouchCancelEvent<B>>(state: &mut BlueState, _event: &E) {
    let Some(touch) = state.seat.get_touch() else { return };
    touch.cancel(state);
}

// ── Tablet ────────────────────────────────────────────────────────────────
//
// See the `InputEvent::TabletTool*`/`DeviceAdded`/`DeviceRemoved` match
// arms in `handle_input` above for the registration side of this. All
// four handlers below move the regular pointer too (not just the
// tablet tool) — most tablets are used as an absolute-position mouse
// substitute as much as a pressure-sensitive pen, so the on-screen
// cursor should track the stylus the same way it tracks a touchscreen
// tap, in addition to the tool-specific axis data going to whichever
// client actually asked for `zwp_tablet_manager_v2`.

fn handle_tablet_axis<B: InputBackend, E: TabletToolAxisEvent<B>>(state: &mut BlueState, event: &E) {
    let size = output_size_for_touch(state);
    let pos = event.position_transformed(size);
    state.pointer_location = pos;
    let serial = SERIAL_COUNTER.next_serial();
    update_pointer_focus(state, serial, event.time_msec());

    let tablet_seat = state.seat.tablet_seat();
    let tablet = tablet_seat.get_tablet(&TabletDescriptor::from(&event.device()));
    let tool = tablet_seat.get_tool(&event.tool());
    let Some((tablet, tool)) = tablet.zip(tool) else { return };

    if event.pressure_has_changed() { tool.pressure(event.pressure()); }
    if event.distance_has_changed() { tool.distance(event.distance()); }
    if event.tilt_has_changed() { tool.tilt(event.tilt()); }
    if event.slider_has_changed() { tool.slider_position(event.slider_position()); }
    if event.rotation_has_changed() { tool.rotation(event.rotation()); }
    if event.wheel_has_changed() { tool.wheel(event.wheel_delta(), event.wheel_delta_discrete()); }

    let under = surface_under_point(state, pos);
    tool.motion(pos, under, &tablet, SERIAL_COUNTER.next_serial(), event.time_msec());
}

fn handle_tablet_proximity<B: InputBackend, E: TabletToolProximityEvent<B>>(state: &mut BlueState, event: &E) {
    let size = output_size_for_touch(state);
    let pos = event.position_transformed(size);
    state.pointer_location = pos;
    let serial = SERIAL_COUNTER.next_serial();
    update_pointer_focus(state, serial, event.time_msec());

    let tool_desc = event.tool();
    // Registers the tool with the seat the first time it's seen (a
    // no-op if it's already known) — must happen before `get_tool`
    // below, which only looks up tools that were already added.
    // Split into separate statements (rather than one chained
    // `state.seat.tablet_seat().add_tool(state, ...)` expression) so
    // there's no ambiguity about the transient immutable borrow from
    // `.tablet_seat()` having ended before `state` is borrowed
    // mutably for `add_tool` itself.
    let tablet_seat = state.seat.tablet_seat();
    let dh = state.display_handle.clone();
    tablet_seat.add_tool::<BlueState>(state, &dh, &tool_desc);

    let tablet_seat = state.seat.tablet_seat();
    let tablet = tablet_seat.get_tablet(&TabletDescriptor::from(&event.device()));
    let tool = tablet_seat.get_tool(&tool_desc);
    let under = surface_under_point(state, pos);
    let Some(((tablet, tool), under)) = tablet.zip(tool).zip(under) else { return };

    match event.state() {
        ProximityState::In => {
            tool.proximity_in(pos, under, &tablet, SERIAL_COUNTER.next_serial(), event.time_msec());
        }
        ProximityState::Out => tool.proximity_out(event.time_msec()),
    }
}

fn handle_tablet_tip<B: InputBackend, E: TabletToolTipEvent<B>>(state: &mut BlueState, event: &E) {
    let Some(tool) = state.seat.tablet_seat().get_tool(&event.tool()) else { return };
    match event.tip_state() {
        TabletToolTipState::Down => {
            let serial = SERIAL_COUNTER.next_serial();
            tool.tip_down(serial, event.time_msec());
            // A tip-down is a "click" for focus purposes — same
            // keyboard-focus-follows-click behavior as
            // handle_pointer_button, including respecting a locked
            // session via `locked_focus` (a stylus tap during
            // session-lock must not be able to focus/type into a
            // regular window any more than a mouse click can).
            let keyboard = state.seat.get_keyboard().unwrap();
            match locked_focus(state, state.pointer_location) {
                Some((surface, _)) => keyboard.set_focus(state, Some(surface), serial),
                None => {
                    if let Some((surface, _)) = surface_under_point(state, state.pointer_location) {
                        keyboard.set_focus(state, Some(surface), serial);
                    }
                }
            }
        }
        TabletToolTipState::Up => tool.tip_up(event.time_msec()),
    }
}

fn handle_tablet_button<B: InputBackend, E: TabletToolButtonEvent<B>>(state: &mut BlueState, event: &E) {
    let Some(tool) = state.seat.tablet_seat().get_tool(&event.tool()) else { return };
    tool.button(event.button(), event.button_state(), SERIAL_COUNTER.next_serial(), event.time_msec());
}

// ── Touchpad gestures ────────────────────────────────────────────────────
//
// A 3-or-4-finger horizontal swipe switches workspace. This didn't
// exist anywhere in the compositor before — `handle_input`'s match had
// no `InputEvent::Gesture*` arms at all, despite an earlier pass of
// documentation (now corrected) describing swipe-based workspace
// switching as already working. `Action`/keybinding-style indirection
// (as a config.rs enum a gesture could also target) is intentionally
// not introduced here — `switch_workspace` is the one thing every
// existing workspace-switch entry point already calls directly (IPC
// from the shell, ext-workspace protocol clients), so this follows the
// same pattern rather than inventing a second, gesture-only path to the
// same effect.

/// Total accumulated horizontal movement (logical pixels) a 3/4-finger
/// swipe must cross before it counts as a workspace switch rather than
/// an aborted/too-small gesture. Deliberately generous compared to a
/// typical single-finger-scroll threshold — this is a full-screen
/// gesture users perform somewhat quickly, not a fine pointing motion.
const WORKSPACE_SWIPE_THRESHOLD: f64 = 80.0;

fn handle_gesture_swipe_begin<B: InputBackend, E: GestureSwipeBeginEvent<B>>(state: &mut BlueState, event: &E) {
    // 1/2-finger "swipes" are libinput's term for ordinary scrolling/
    // pointer gestures on some drivers and arrive through this same
    // event stream — only 3+ fingers is an intentional, deliberate
    // "switch workspace" gesture on virtually every desktop environment
    // convention (GNOME, KDE, macOS all reserve 2-finger for scroll).
    state.workspace_swipe = (event.fingers() >= 3).then_some(0.0);
}

fn handle_gesture_swipe_update<B: InputBackend, E: GestureSwipeUpdateEvent<B>>(state: &mut BlueState, event: &E) {
    if let Some(delta) = state.workspace_swipe.as_mut() {
        *delta += event.delta_x();
    }
}

/// Direction convention: swiping left (negative accumulated delta, ~
/// "content moves left, like flipping to the next page") switches to
/// the *next* workspace; swiping right goes to the *previous* one. This
/// matches GNOME's default touchpad convention, which is what most
/// libinput-based distros' users will already have muscle memory for.
fn handle_gesture_swipe_end<B: InputBackend, E: GestureSwipeEndEvent<B>>(state: &mut BlueState, event: &E) {
    let Some(delta) = state.workspace_swipe.take() else { return };
    if event.cancelled() || delta.abs() < WORKSPACE_SWIPE_THRESHOLD {
        return;
    }
    let count = state.workspace_count;
    if count == 0 {
        return;
    }
    let current = state.current_workspace;
    let next = if delta < 0.0 {
        (current + 1) % count
    } else {
        (current + count - 1) % count
    };
    state.switch_workspace(next);
}

#[cfg(test)]
mod resize_edges_tests {
    use super::ResizeEdges;
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge as XdgEdge;

    #[test]
    fn xdg_single_edges_map_correctly() {
        assert_eq!(ResizeEdges::from(XdgEdge::Top), ResizeEdges { top: true, bottom: false, left: false, right: false });
        assert_eq!(ResizeEdges::from(XdgEdge::Bottom), ResizeEdges { top: false, bottom: true, left: false, right: false });
        assert_eq!(ResizeEdges::from(XdgEdge::Left), ResizeEdges { top: false, bottom: false, left: true, right: false });
        assert_eq!(ResizeEdges::from(XdgEdge::Right), ResizeEdges { top: false, bottom: false, left: false, right: true });
    }

    #[test]
    fn xdg_corner_edges_set_two_flags() {
        assert_eq!(ResizeEdges::from(XdgEdge::TopLeft), ResizeEdges { top: true, left: true, bottom: false, right: false });
        assert_eq!(ResizeEdges::from(XdgEdge::TopRight), ResizeEdges { top: true, right: true, bottom: false, left: false });
        assert_eq!(ResizeEdges::from(XdgEdge::BottomLeft), ResizeEdges { bottom: true, left: true, top: false, right: false });
        assert_eq!(ResizeEdges::from(XdgEdge::BottomRight), ResizeEdges { bottom: true, right: true, top: false, left: false });
    }

    #[test]
    fn xdg_none_edge_sets_no_flags() {
        assert_eq!(ResizeEdges::from(XdgEdge::None), ResizeEdges::default());
    }
}
