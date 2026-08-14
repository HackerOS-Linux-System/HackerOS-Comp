# comphwde

Smithay-based Wayland/XWayland compositor for HackerOS.

## Modules

- `state.rs` - `HwdeState` (core struct) + `ManagedWindow` bookkeeping
  (minimize/maximize/focus/close-by-id, `is_ssd` decoration tracking -
  backing the unified taskbar with `starthwde`).
- `handlers/` - one file per Wayland protocol:
  - `compositor.rs` - core compositor/buffer/shm.
  - `xdg_shell.rs` - xdg-shell, incl. interactive move/resize (`grabs.rs`)
    and popup handling.
  - `seat.rs`, `output.rs` - seat/output plumbing.
  - `data_device.rs` - clipboard (`wl_data_device_manager`) **and** primary
    selection ("middle-click paste").
  - `decoration.rs` - xdg-decoration negotiation; tracks `is_ssd` per
    window for `render_elements.rs` to draw.
  - `layer_shell.rs` - wlr-layer-shell protocol + `LayerMap` arrangement.
  - `xwayland_shell.rs` *(feature = "xwayland")* - required by `X11Wm`.
- `render_elements.rs` - the full per-frame render pipeline: wallpaper +
  all four `wlr-layer-shell` layers (via Smithay's `space_render_elements`,
  correctly interleaved in front of/behind the window stack) + windows +
  minimal server-side decorations, assembled back-to-front and driven
  through the lower-level `OutputDamageTracker::render_output` directly
  (bypassing `space::render_output`'s single-group-always-behind
  limitation - see this file's module doc for why).
- `grabs.rs` - interactive move/resize pointer grabs, shared by native
  Wayland (`handlers/xdg_shell.rs`), XWayland (`xwayland.rs`), and SSD grab
  bars (`input.rs`).
- `winit_backend.rs` - the default, wired-into-`main` backend:
  nested/windowed.
- `input.rs` - forwards winit keyboard/pointer/touch events into the seat;
  hit-tests SSD grab bars/close buttons before normal client dispatch;
  triggers popup-dismiss-on-outside-click.
- `wallpaper.rs` - loads HWDE's wallpaper and renders it as a full-screen
  background element every frame; reloadable live via IPC (see below).
- `ipc.rs` - the `hwde-ipc` control socket: external app launching, the
  unified window-list/focus/close/minimize/maximize protocol, **live**
  wallpaper changes (a Settings change now reaches an already-running
  compositor immediately, not just its next restart), shutdown.
- `xwayland.rs` *(feature = "xwayland", default on)* - starts XWayland,
  maps X11 clients into the same desktop space as native Wayland windows,
  real interactive move/resize.
- `backend_drm.rs` *(feature = "drm-experimental", NOT wired into `main`)* -
  session/udev-hotplug/libinput wiring is real, ported from Smithay's own
  `anvil` reference compositor. The atomic-KMS device/connector setup and
  vblank-driven render loop are deliberately left as a precise, sourced
  outline rather than a mechanical port - see the module's doc comment for
  why (short version: that specific ~300 lines of anvil code is exactly
  the kind of GPU-buffer-lifetime-sensitive code that's genuinely unsafe to
  "port and hope" without a real compile-run-fix loop against actual KMS
  hardware).

## What changed most recently

- **wlr-layer-shell now actually renders** (all four layers, not just
  Background/Bottom) - previously the protocol worked but nothing was
  painted.
- **Minimal server-side decorations**: a solid-color grab bar + close
  button for windows that negotiate `Mode::ServerSide` via xdg-decoration -
  drag to move, click the red square to close, title text rasterized on
  the bar via `title_text.rs` (see below).
- **Primary selection** ("middle-click paste") alongside the main
  clipboard.
- **Touch input** forwarded to clients (down/motion/up/cancel/frame).
- **3/4-finger touchpad swipe → workspace switch** (`input.rs`'s
  `GestureSwipe*` handling) - real hardware only (via the DRM backend's
  libinput source); nothing to recognize from the winit backend's
  synthetic events.
- **Live wallpaper reload**: changing the wallpaper in Settings now reaches
  an already-running `comphwde` via IPC instead of only taking effect on
  the compositor's next start.
- **DRM/udev backend**: upgraded from a pure doc-comment plan to a real,
  substantial session/device-discovery/input scaffold - still not the
  default, still needs the atomic-KMS core built against real hardware.
- **SSD title text**: `title_text.rs` rasterizes window titles with
  `fontdue` (pure-Rust, no fontconfig/freetype linkage) against whichever
  system font it finds first (see that module's `FONT_CANDIDATES`),
  cached per-window as a GPU texture and truncated with an ellipsis when
  a window is too narrow for its own title. Degrades gracefully (bar +
  close button still work, title just isn't drawn) if no font is found.
- **`PinSurface` PID fallback**: pinned-shell matching (`sde-panel`/
  `sde-dock` docking to an edge) now also recognizes a window by the pid
  of the process that registered the pin (read via `SO_PEERCRED` on the
  `sde-ipc` connection - see `state.rs`'s `PinnedSurfaceSpec`), not only
  by Wayland `app_id`, so it degrades gracefully if the app_id Slint's
  winit backend sets doesn't end up being what was expected.
- **`zwp_idle_inhibit_manager_v1`**: clients (video players, browsers
  playing fullscreen video, conferencing apps) can now register an idle
  inhibitor instead of hitting a missing global. See
  `handlers/extra_protocols.rs`'s module doc for the honest caveat: this
  compositor has no idle-timeout/lock/DPMS-blanking system of its own
  yet to actually *act* on `HwdeState::is_idle_inhibited()` - this adds
  the missing "clients can ask" half, not the "compositor honors it"
  half.
- **`wp_single_pixel_buffer_manager_v1`**: lets clients (Chromium/CEF-
  based ones in particular) create solid-color buffers without SHM -
  Smithay's GLES renderer imports these natively, so this was a
  two-line, no-handler-needed addition (see `handlers/extra_protocols.rs`).
- **`zwp_virtual_keyboard_manager_v1`**: lets a client (on-screen
  keyboard, IME) synthesize keyboard input through the same
  `KeyboardHandle` real hardware keyboard input already uses - no
  forwarding code needed anywhere else, Smithay wires it straight into
  the seat. **Security note**: registered with a client filter that
  currently allows every client (`|_| true`) - see
  `handlers/extra_protocols.rs`'s module doc for why (this project has no
  trusted/untrusted client concept yet to filter on) and that this is a
  placeholder, not a considered decision, before this runs anywhere with
  untrusted Wayland clients.
- **Real mouse input on the DRM backend, and `zwp_relative_pointer_manager_v1`**:
  `input.rs` never handled `InputEvent::PointerMotion` (relative motion -
  what an ordinary mouse sends, as opposed to `PointerMotionAbsolute`,
  which touchpads/touchscreens and the winit backend's synthetic events
  send) - meaning a plain mouse had no way to move the cursor at all
  outside the winit backend. Fixed, and paired with registering
  `zwp_relative_pointer_manager_v1` so games/3D tools that want raw,
  unaccelerated deltas can ask for them too - see `input.rs`'s
  `PointerMotion` arm and `handlers/extra_protocols.rs`.
- **Idle-dim overlay**: after `config.idle_dim_timeout_secs` (default
  300, `0` disables) of no input with nothing holding an idle inhibitor
  (see the point above), the output dims under a near-opaque overlay -
  `state.rs`'s `idle_dimmed` field, recomputed once per rendered frame in
  `winit_backend.rs`/`backend_drm.rs`, drawn in `render_elements.rs`.
  **This is a screensaver, not a lock screen** - it draws over
  everything but never intercepts a single input event, so whatever's
  focused underneath keeps receiving keystrokes/clicks exactly as before.
  A real lock screen needs authentication (PAM or equivalent) and
  input-interception that's correct under every edge case - deliberately
  not attempted here, since a lock screen that looks secure but isn't is
  worse than not having one. See `HwdeState::idle_dimmed`'s doc comment
  for the full reasoning. A `dim_now` keybinding action can trigger it on
  demand (see `config.rs`'s `Action::DimNow`) - deliberately shipped with
  no default keybinding, since the obvious muscle-memory shortcut for
  that (`super+l`) is what most desktops use for an actual, authenticated
  lock, and binding a non-authenticating dimmer to it risks someone
  genuinely believing they've locked their session.
- **A rendered cursor, at all.** `HwdeState::cursor_status` was tracked
  (set from `SeatHandler::cursor_image`) but nothing ever read it to draw
  anything - on `winit_backend.rs` this was mostly masked by the host
  OS's own cursor showing through the window it renders into, but on
  `backend_drm.rs` - a real, exclusive KMS session with no host cursor to
  fall back on - it meant **no visible pointer at all**. Fixed with a
  synthetic placeholder arrow (five small rectangles, see
  `render_elements.rs`'s `cursor_elements`), not yet the client's actual
  requested cursor surface/theme - see that function's doc comment for
  the deliberately narrower scope and why.
- **Stylus/tablet input** (`zwp_tablet_manager_v2`): proximity, motion
  (with pressure/tilt where the hardware reports them), tip-down/up, and
  buttons, forwarded via `input.rs`'s new `TabletTool*` match arms
  (`handlers/tablet.rs` handles registration and the cursor-image
  callback). **This is genuinely the least-verified addition in this
  codebase** - flagged as "not attempted" across several earlier passes
  specifically because `wlr-tablet-unstable-v2` has more moving parts
  (per-tool *and* per-tablet-device registration, several sub-states)
  than anything else here, and per-line confidence varies a lot within
  the same block - see `input.rs`'s comment on the `TabletToolProximity`
  arm for exactly which calls are more vs. less certain, and
  `TabletDescriptor::from(&event.device())` in particular as the one
  most likely to need adjusting for the generic `InputBackend` bound
  `process_input_event` uses (tablet events realistically only ever come
  from the libinput/DRM backend, never winit's synthetic ones, so a
  conversion that only exists for the concrete libinput device type
  would still cover every real use of this - it just might need the
  function's generic bound narrowed, or the conversion written by hand,
  to actually compile).
- **X11 apps' own maximize/minimize now work.** `XwmHandler`'s
  `maximize_request`/`unmaximize_request`/`minimize_request`/
  `unminimize_request` had no override, so `_NET_WM_STATE`/
  `WM_CHANGE_STATE` requests from an X11 app's own window chrome or
  window-menu were silently swallowed by the trait's default no-op
  bodies - the *Wayland*-native equivalents already worked. Now wired to
  the same `maximize_window_by_id`/`minimize_window_by_id`/
  `unminimize_window_by_id` state functions `handlers/xdg_shell.rs`
  already uses - see `xwayland.rs`.
- **Configurable output transform/scale**: `config.output_transform`
  (`"normal"`/`"90"`/`"180"`/`"270"`/`"flipped"`/`"flipped-90"`/`"flipped-180"`/`"flipped-270"`)
  and `config.output_scale` (fractional HiDPI scaling) now flow into
  `Output::change_current_state` on both backends - see `config.rs`'s
  `parse_transform`. Previously hardcoded to `Transform::Normal` with no
  way to configure either at all (this document used to list that
  specifically as an out-of-scope item, right below).
- **Per-output position config**: `config.outputs` maps a connector name
  (e.g. `"DP-1"`) to an `{x, y}` position, consulted in
  `backend_drm.rs::connector_connected` before falling back to the
  existing automatic left-to-right placement. Only position - each
  output's own transform/scale is still the one global
  `output_transform`/`output_scale` pair above, not per-output; giving
  every output independent transform/scale is a bigger feature than what
  this pass set out to add. (Caught and fixed a real use-after-move bug
  while wiring this up - `output_name` was already moved into
  `Output::new` earlier in the same function before this pass's lookup
  code tried to read it again; fixed by cloning it at the point of the
  original move, not by restructuring the lookup.)
- **Configurable keyboard layout**: `config.xkb_layout`/`xkb_variant`/
  `xkb_model`/`xkb_options` (standard libxkbcommon RMLVO fields) now flow
  into `seat.add_keyboard` on both backends via
  `CompositorConfig::xkb_config()` - e.g. `xkb_options =
  "ctrl:nocaps,grp:alt_shift_toggle"` for caps-lock-as-ctrl plus an
  Alt+Shift layout-switch toggle. Previously hardcoded to
  `Default::default()` (US layout, no options) with no way to configure
  a different layout at all - a real gap for any non-US keyboard.
  `Action::ReloadConfig` also now applies a changed layout live (via
  `KeyboardHandle::set_xkb_config`), not just every other setting - see
  that action's doc comment in `state.rs` for why it needed its own,
  separate fix even after layout config itself already existed (the
  reload path never re-ran `add_keyboard`, so a changed layout was
  invisible until restart without this).
- **Fullscreen support, from scratch** (`ManagedWindow::is_fullscreen`,
  `fullscreen_window_by_id`): this compositor had *no* fullscreen concept
  anywhere before this - not unhandled requests like X11's maximize/
  minimize were, but no field, no state function, nothing. Wired up for
  both Wayland (`handlers/xdg_shell.rs`'s `fullscreen_request`/
  `unfullscreen_request`) and X11 (`xwayland.rs`, mirroring the
  maximize/minimize wiring from the previous pass). Real difference from
  maximize, not just a bigger rectangle: fullscreen also hides the SSD
  grab bar and focus border (`render_elements.rs`) - a fullscreen video
  player showing a drag handle across its top edge would defeat the
  point. Restoring correctly composes with maximize too: un-fullscreening
  a window that's *also* maximized returns it to the maximized rect, not
  all the way back to floating - only un-maximizing after that drops it
  the rest of the way.

## Correcting an earlier claim in this document

Earlier revisions of this README (and this pass's own earlier commentary)
described the DRM/udev backend as needing its "atomic-KMS render core"
built - implying `backend_drm.rs` was mostly scaffolding around a missing
unsafe rendering piece. On closer reading, **that undersold what's
actually there**: `backend_drm.rs` already builds a real
`smithay::backend::drm::compositor::DrmCompositor` per lit connector (see
`connector_connected`), which is Smithay's own safe, tested high-level
abstraction over atomic KMS commits, GBM buffer allocation, and damage
tracking - the same building block real wlroots-family Rust compositors
use, not hand-rolled unsafe `ioctl`/property-setting code. `render_surface`
calls its `render_frame`/`queue_frame`, and the per-device `DrmEvent::VBlank`
handler calls `frame_submitted()` to keep the double-buffered cycle going
- a complete, self-sustaining render loop, kicked off explicitly after a
connector first lights up (`device_added`'s "nothing will vblank until a
first frame has actually been queued" comment) and re-driven on every
subsequent vblank. Hotplug (`device_added`/`device_changed`/
`device_removed`), VT-switch pause/resume, and multi-GPU support are all
real, not stubs.

What's genuinely still missing/uncertain is narrower than "the core":
whether it actually works correctly against real GPU drivers (no hardware
or `cargo check` was available to confirm this end-to-end), a hardware
or client-cursor-surface-aware cursor (see the point above - current
cursor is a synthetic placeholder), and DPMS/real display power control
(the idle-dim overlay above is a rendered dimmer, not actual monitor
power-down - deliberately not attempted, since mixing legacy DPMS
property writes with a surface that's also under active atomic-commit
management via `DrmCompositor` is exactly the kind of driver-specific
interaction that needs real hardware to get right, not more unverified
code stacked on top). This section exists to correct the record rather
than claim new work - nothing in `backend_drm.rs` itself changed as part
of writing it. (Output scale/rotation configuration *was* listed here
too as of this section's first version - that's since been addressed,
see "What's new" above; struck through here rather than silently
deleted, since this section is specifically about correcting the
record.)

## Explicitly out of scope for now

- DPMS/real display power-down tied to `idle_dimmed` (see the correction
  above for why).
- Native Wayland output/display management (`xrandr`-equivalent) beyond
  what already exists (`wlr-output-management`-style reconfiguration of
  mode/position/transform at runtime, not just at connector-connect time).
- Pointer-gesture-recognizer coverage for pinch/rotate (only 3/4-finger
  swipe got a recognizer - see `input.rs` - since nothing in this
  compositor would consume pinch/rotate yet; no zoom/rotate-bindable
  concept exists at the compositor level).
- Pointer constraints (`zwp_pointer_constraints_v1` - lock/confine, the
  usual pairing with `zwp_relative_pointer_manager_v1` above for games
  that want the OS cursor to stay put while reading raw deltas). Not
  attempted this pass; the relative-motion fix and manager registration
  above are useful without it (any client can already ask for relative
  deltas), a locked/confined pointer is the next, separate piece.
- Per-libinput-device output routing for true multi-monitor setups (see
  `run_udev`'s own NOTE comment on this) - every input device is routed
  to whatever output happens to be first, which only matters in practice
  for absolute-position devices (touchscreens) on a system with more than
  one output; not attempted here since correctly resolving "which
  physical output is this input device attached to" needs matching
  libinput device/udev properties that weren't confirmed available in
  the exact form assumed.

None of this is an accident - it's the deliberate "narrow but real" scope
the project asked for, with the bigger/hardware-dependent pieces flagged
for follow-up rather than faked.

