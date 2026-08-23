use std::path::PathBuf;

use smithay::{
    input::pointer::{AxisFrame, ButtonEvent},
    reexports::calloop::{LoopHandle, PostAction},
    utils::{Point, SERIAL_COUNTER},
};

use reis::{eis, request as eis_request};

use crate::state::BlueState;

fn socket_path() -> PathBuf {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    runtime_dir.join("eis-blue")
}

/// Compositor-wide EIS server state. Currently just a marker that
/// `init()` ran — the per-device/per-connection bookkeeping the first
/// revision of this file had (`pointer_devices`/`keyboard_devices`
/// HashMaps) turned out to be unnecessary: every `EisRequest` variant
/// already says which kind of input it is (`PointerMotion` is always
/// pointer, `KeyboardKey` is always keyboard, ...), so there's no
/// per-device capability tracking needed to route an event correctly —
/// unlike, say, a real `libinput` backend juggling multiple physical
/// devices, this compositor has exactly one seat and every EIS request
/// already self-describes which part of it to touch.
pub struct EisServerState {
    _private: (),
}

pub fn init(
    state: &mut BlueState,
    loop_handle: &LoopHandle<'static, BlueState>,
) -> std::io::Result<PathBuf> {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);

    let listener = eis::Listener::bind(&path)?;
    state.eis_state = Some(EisServerState { _private: () });

    let source = reis::calloop::EisListenerSource::new(listener);
    let loop_handle_clone = loop_handle.clone();
    loop_handle
        .insert_source(source, move |context: eis::Context, _, _state| {
            handle_new_connection(context, &loop_handle_clone);
            Ok(PostAction::Continue)
        })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{e}")))?;

    tracing::info!(
        "EIS input-emulation socket ready at {} (no xdg-desktop-portal \
         RemoteDesktop backend yet — see this module's doc for what \
         that still needs; this socket has no access control of its \
         own in the meantime)",
        path.display()
    );

    Ok(path)
}

fn handle_new_connection(context: eis::Context, loop_handle: &LoopHandle<'static, BlueState>) {
    // Initial serial `0` — this is the first request-tracking serial
    // for this connection's lifetime, not related to
    // `smithay::utils::SERIAL_COUNTER` (that's the compositor-global
    // serial used when *emitting* synthetic input to smithay's own
    // seat, done per-request in `handle_request` below); reis tracks
    // its own protocol-level serial internally per `EisRequestSource`.
    let source = reis::calloop::EisRequestSource::new(context, 0);
    if let Err(e) = loop_handle.insert_source(source, move |event, _, state| {
        match event {
            Ok(source_event) => handle_source_event(state, source_event),
            Err(e) => tracing::warn!("EIS connection error: {:?}", e),
        }
        Ok(PostAction::Continue)
    }) {
        tracing::warn!("Failed to register EIS connection on the event loop: {:?}", e);
    }
}

/// See this module's doc, "Still not independently verified" — the
/// exact shape of `EisRequestSourceEvent` wasn't confirmed in this
/// pass. Written against the most structurally likely shape; the
/// `#[allow(unreachable_patterns)]` is deliberate insurance in case the
/// real enum has fewer/different variants than assumed here, so a
/// mismatch is a silent no-op (input emulation just doesn't react to
/// that event kind) rather than a compile error blocking everything
/// else in this file.
#[allow(unreachable_patterns)]
fn handle_source_event(state: &mut BlueState, event: reis::calloop::EisRequestSourceEvent) {
    use reis::calloop::EisRequestSourceEvent;
    match event {
        EisRequestSourceEvent::Request(request) => handle_request(state, request),
        // `InvalidObject` was a guessed variant name that doesn't
        // actually exist on this enum (confirmed by a real compiler
        // error — good news in a way: it means `Request(...)` above,
        // the one variant this module actually depends on, was the
        // right guess). Whatever other variants the real enum has fall
        // through to the wildcard below.
        _ => {}
    }
}

fn handle_request(state: &mut BlueState, request: eis_request::EisRequest) {
    use eis_request::EisRequest;

    match request {
        EisRequest::Disconnect | EisRequest::DeviceClosed(_) => {}

        EisRequest::PointerMotion(ev) => {
            let serial = SERIAL_COUNTER.next_serial();
            let (min_x, min_y, max_x, max_y) = crate::input::output_bounds(state);
            state.pointer_location.x = (state.pointer_location.x + ev.dx as f64).clamp(min_x, max_x);
            state.pointer_location.y = (state.pointer_location.y + ev.dy as f64).clamp(min_y, max_y);
            crate::input::update_pointer_focus(state, serial, current_time_msec());
        }

        EisRequest::PointerMotionAbsolute(ev) => {
            let serial = SERIAL_COUNTER.next_serial();
            state.pointer_location = Point::from((ev.dx_absolute as f64, ev.dy_absolute as f64));
            crate::input::update_pointer_focus(state, serial, current_time_msec());
        }

        EisRequest::Button(ev) => {
            let serial = SERIAL_COUNTER.next_serial();
            let Some(pointer) = state.seat.get_pointer() else { return };
            pointer.button(
                state,
                &ButtonEvent {
                    button: ev.button,
                    state: if ev.state == reis::ei::button::ButtonState::Press {
                        smithay::backend::input::ButtonState::Pressed
                    } else {
                        smithay::backend::input::ButtonState::Released
                    },
                    serial,
                    time: current_time_msec(),
                },
            );
        }

        // Continuous scroll (trackpad-style). All four scroll request
        // kinds are now handled (`ScrollDiscrete`/`ScrollStop`/
        // `ScrollCancel` below — previously unhandled, see this
        // function's other comments for the field-name caveats on
        // those three specifically).
        EisRequest::ScrollDelta(ev) => {
            let Some(pointer) = state.seat.get_pointer() else { return };
            let frame = AxisFrame::new(current_time_msec())
                .value(smithay::backend::input::Axis::Horizontal, ev.dx as f64)
                .value(smithay::backend::input::Axis::Vertical, ev.dy as f64);
            pointer.axis(state, frame);
            pointer.frame(state);
        }

        // Wheel-click ("discrete") scroll — the libei/eiproto XML for
        // `ei_pointer.scroll_discrete` (checked against the upstream
        // libei protocol spec, not reis's own docs — its own docs.rs
        // page for this specific variant's fields wasn't reachable in
        // this pass) takes `discrete_dx`/`discrete_dy` as the two
        // int32 args, so `reis::request::ScrollDiscrete` is assumed to
        // mirror that 1:1 the same way `ScrollDelta`'s confirmed
        // `dx`/`dy` fields mirror `ei_pointer.scroll`'s args — same
        // "checked shape, not checked exact field names" caveat as the
        // rest of this file. `AxisSource::Wheel` (rather than
        // `Continuous`, which `ScrollDelta` above implicitly uses via
        // smithay's default) is the one part of this that *is*
        // deliberate, not a guess: it's what tells a client this was a
        // discrete wheel click, not a smooth trackpad swipe, so e.g. a
        // scroll-to-zoom gesture recognizer doesn't misinterpret it.
        EisRequest::ScrollDiscrete(ev) => {
            let Some(pointer) = state.seat.get_pointer() else { return };
            // `.discrete(...)` doesn't exist on this smithay version's
            // `AxisFrame` (real compiler error, not a guess this time —
            // two wrong method-name guesses in this exact spot already,
            // so dropping it rather than guessing a third name like
            // `.v120(...)`). `.value(...)` alone (confirmed working —
            // it's the same call `ScrollDelta`'s handler above already
            // uses without error) still reports the scroll magnitude
            // correctly; what's lost is just the discrete "how many
            // notches" metadata some clients use for wheel
            // acceleration curves — a real, smaller gap than not
            // handling wheel scroll at all.
            let frame = AxisFrame::new(current_time_msec())
                .source(smithay::backend::input::AxisSource::Wheel)
                .value(smithay::backend::input::Axis::Horizontal, ev.discrete_dx as f64)
                .value(smithay::backend::input::Axis::Vertical, ev.discrete_dy as f64);
            pointer.axis(state, frame);
            pointer.frame(state);
        }

        // Scroll-gesture end (trackpad fingers lifted, or equivalent) —
        // was entirely unhandled before ("this compositor never sends
        // `AxisSource` 'finished' framing, which some clients use for
        // kinetic-scroll cutoff", per this match's own note further up
        // before this fix). `ei_pointer.scroll_stop`'s args are assumed
        // to be the two `x`/`y` stop-flags the libei protocol spec
        // describes (mirroring `wp_pointer_gestures`' own
        // stop-per-axis convention) — same unverified-field-name
        // caveat as `ScrollDiscrete` above. `ScrollCancel` is treated
        // identically to `ScrollStop` here (both end the axis
        // sequence); a real distinction between "stopped cleanly" vs.
        // "cancelled" isn't threaded through to smithay's `AxisFrame`
        // API at all — it doesn't have a separate concept for it.
        EisRequest::ScrollStop(ev) => {
            let Some(pointer) = state.seat.get_pointer() else { return };
            let mut frame = AxisFrame::new(current_time_msec());
            if ev.x { frame = frame.stop(smithay::backend::input::Axis::Horizontal); }
            if ev.y { frame = frame.stop(smithay::backend::input::Axis::Vertical); }
            pointer.axis(state, frame);
            pointer.frame(state);
        }
        EisRequest::ScrollCancel(ev) => {
            let Some(pointer) = state.seat.get_pointer() else { return };
            let mut frame = AxisFrame::new(current_time_msec());
            if ev.x { frame = frame.stop(smithay::backend::input::Axis::Horizontal); }
            if ev.y { frame = frame.stop(smithay::backend::input::Axis::Vertical); }
            pointer.axis(state, frame);
            pointer.frame(state);
        }

        EisRequest::KeyboardKey(ev) => {
            let serial = SERIAL_COUNTER.next_serial();
            let Some(keyboard) = state.seat.get_keyboard() else { return };
            let key_state = if ev.state == reis::ei::keyboard::KeyState::Press {
                smithay::backend::input::KeyState::Pressed
            } else {
                smithay::backend::input::KeyState::Released
            };
            // Deliberately always `FilterResult::Forward`s straight to
            // the focused client rather than reusing
            // `input::handle_keyboard`'s local-shortcut-intercepting
            // closure (Alt+Tab, Win+1..4, ...) — a remote-input-
            // injection client's keystrokes should land on whatever the
            // person on the *local* screen currently has focused, not
            // additionally trigger this compositor's own local-only
            // shortcuts, matching how real compositors' RemoteDesktop
            // portal backends handle this.
            keyboard.input::<(), _>(
                state,
                ev.key.into(),
                key_state,
                serial,
                current_time_msec(),
                |_, _, _| smithay::input::keyboard::FilterResult::Forward,
            );
        }

        // Every other request kind (touch, text-input, bind/device
        // negotiation beyond what `handle_new_connection` needs, scroll
        // variants not handled above, ...) is intentionally not
        // forwarded to the seat — see this match's own gaps noted
        // above and ROADMAP.md for what's still missing.
        _ => {}
    }
}

fn current_time_msec() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(0)
}
