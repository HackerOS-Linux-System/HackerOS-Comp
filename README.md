# comphwde

Smithay-based Wayland/XWayland compositor for HWDE.

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
  drag to move, click the red square to close. No drawn title text yet
  (needs a font-rasterization pipeline this compositor doesn't have).
- **Primary selection** ("middle-click paste") alongside the main
  clipboard.
- **Touch input** forwarded to clients (down/motion/up/cancel/frame).
- **Live wallpaper reload**: changing the wallpaper in Settings now reaches
  an already-running `comphwde` via IPC instead of only taking effect on
  the compositor's next start.
- **DRM/udev backend**: upgraded from a pure doc-comment plan to a real,
  substantial session/device-discovery/input scaffold - still not the
  default, still needs the atomic-KMS core built against real hardware.

## Explicitly out of scope for now

- The DRM/udev backend's atomic-KMS render core (see `backend_drm.rs`).
- Native Wayland output/display management (`xrandr`-equivalent) - blocked
  on the above, since there's no KMS connector/mode info without it.
- SSD title *text* (grab bar has no label, just a drag handle + close
  button) - needs text rendering, which needs a font rasterizer.
- Tablet input and multi-touch gesture *recognition* as a compositor-level
  concept (raw touch points are forwarded; pointer gesture events are
  passed through during an active grab in `grabs.rs`, but there's no
  standalone gesture recognizer).

None of this is an accident - it's the deliberate "narrow but real" scope
the project asked for, with the bigger/hardware-dependent pieces flagged
for follow-up rather than faked.
