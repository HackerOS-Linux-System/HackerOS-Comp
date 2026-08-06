use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{Color32F, ImportMemWl};
use smithay::backend::winit::{self, WinitEvent};
use smithay::desktop::{PopupManager, Space};
use smithay::input::pointer::CursorImageStatus;
use smithay::input::SeatState;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use smithay::reexports::winit::platform::pump_events::PumpStatus;
use smithay::utils::{Clock, Transform};
use smithay::wayland::compositor::CompositorState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::shell::wlr_layer::WlrLayerShellState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::socket::ListeningSocketSource;

use crate::state::{ClientState, HwdeState};
use crate::wallpaper::Wallpaper;

pub const OUTPUT_NAME: &str = "HWDE-1";

pub fn run(wallpaper_path: std::path::PathBuf, extern_mode: Option<crate::ExternMode>) -> anyhow::Result<()> {
    let mut event_loop: EventLoop<HwdeState> = EventLoop::try_new()?;
    let display: Display<HwdeState> = Display::new()?;
    let display_handle = display.handle();

    let (mut backend, mut winit_input) = winit::init::<GlesRenderer>()
        .map_err(|e| anyhow::anyhow!("failed to initialize winit backend: {e}"))?;

    // Native mode keeps the historical "HWDE-1" output name / "HWDE" make
    // string; extern mode uses the extern target's own name (e.g.
    // "SDE-1" / "SDE") so tooling that reads these (e.g. `wlr-randr`,
    // debugging output) doesn't say "HWDE" while actually running SDE.
    let (output_name, output_make) = match &extern_mode {
        Some(mode) => (format!("{}-1", mode.name.to_uppercase()), mode.name.to_uppercase()),
        None => (OUTPUT_NAME.to_string(), "HWDE".to_string()),
    };

    let size = backend.window_size();
    let mode = Mode { size, refresh: 60_000 };
    let output = Output::new(
        output_name,
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: output_make,
            model: "comphwde-winit".into(),
        },
    );
    output.create_global::<HwdeState>(&display_handle);
    output.change_current_state(Some(mode), Some(Transform::Normal), None, Some((0, 0).into()));
    output.set_preferred(mode);

    let mut wallpaper = Wallpaper::new(wallpaper_path);
    wallpaper.load(backend.renderer());

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    let dh = display_handle.clone();
    let socket_source = ListeningSocketSource::new_auto()?;
    let socket_name = socket_source.socket_name().to_string_lossy().into_owned();
    event_loop
        .handle()
        .insert_source(socket_source, |client_stream, _, state: &mut HwdeState| {
            if let Err(err) = state
                .display_handle
                .insert_client(client_stream, Arc::new(ClientState::default()))
            {
                tracing::warn!("failed to add wayland client: {err}");
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to init wayland socket source: {e}"))?;
    tracing::info!("comphwde listening on Wayland socket {socket_name}");

    event_loop
        .handle()
        .insert_source(
            smithay::reexports::calloop::generic::Generic::new(
                display,
                smithay::reexports::calloop::Interest::READ,
                smithay::reexports::calloop::Mode::Level,
            ),
            |_, display, state: &mut HwdeState| {
                // Safety: `display` is kept alive for the lifetime of the event loop.
                unsafe { display.get_mut().dispatch_clients(state)? };
                Ok(smithay::reexports::calloop::PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to init wayland display source: {e}"))?;

    let compositor_state = CompositorState::new::<HwdeState>(&dh);
    let xdg_shell_state = XdgShellState::new::<HwdeState>(&dh);
    let xdg_decoration_state = XdgDecorationState::new::<HwdeState>(&dh);
    let layer_shell_state = WlrLayerShellState::new::<HwdeState>(&dh);
    let data_device_state = DataDeviceState::new::<HwdeState>(&dh);
    let primary_selection_state = PrimarySelectionState::new::<HwdeState>(&dh);
    let shm_state = ShmState::new::<HwdeState>(&dh, vec![]);
    let output_manager_state = OutputManagerState::new_with_xdg_output::<HwdeState>(&dh);

    let mut seat_state = SeatState::<HwdeState>::new();
    let mut seat = seat_state.new_wl_seat(&dh, "hwde-seat".to_string());
    let pointer = seat.add_pointer();
    seat.add_keyboard(Default::default(), 200, 25)?;
    seat.add_touch();

    let mut state = HwdeState {
        display_handle: dh.clone(),
        handle: event_loop.handle(),
        running: Arc::new(AtomicBool::new(true)),
        start_time: std::time::Instant::now(),
        clock: Clock::new(),
        space: Space::default(),
        popups: PopupManager::default(),
        windows: Vec::new(),
        next_window_id: 1,
        compositor_state,
        xdg_shell_state,
        xdg_decoration_state,
        layer_shell_state,
        data_device_state,
        primary_selection_state,
        shm_state,
        output_manager_state,
        seat_state,
        seat,
        pointer,
        cursor_status: CursorImageStatus::default_named(),
        dnd_icon: None,
        wallpaper,
        pending_wallpaper_reload: false,
        socket_name: Some(socket_name),
        config: crate::config::load_for(extern_mode.as_ref().map(|m| m.name.as_str())),
        active_workspace: 0,
        focused_window: None,
        tiling_enabled: std::collections::HashSet::new(),
        floating_windows: std::collections::HashSet::new(),
        pinned_surfaces: std::collections::HashMap::new(),
        extern_name: extern_mode.as_ref().map(|m| m.name.clone()),
        #[cfg(feature = "xwayland")]
        xwm: None,
        #[cfg(feature = "xwayland")]
        xdisplay: None,
        #[cfg(feature = "xwayland")]
        xwayland_shell_state: smithay::wayland::xwayland_shell::XWaylandShellState::new::<HwdeState>(&dh),
    };

    state.space.map_output(&output, (0, 0));
    state.shm_state.update_formats(backend.renderer().shm_formats());

    match &extern_mode {
        // Extern mode speaks sde-ipc (its own protocol - see that crate's
        // module docs) instead of hwde-ipc.
        Some(mode) => crate::extern_ipc::init(&event_loop.handle(), mode.name.clone())?,
        None => crate::ipc::init(&event_loop.handle())?,
    }

    #[cfg(feature = "xwayland")]
    if let Err(err) = crate::xwayland::start(&mut state) {
        tracing::warn!("failed to start XWayland: {err} (X11 apps will not work this session)");
    }

    tracing::info!("HWDE compositor (comphwde) ready - entering main loop");

    while state.running.load(Ordering::SeqCst) {
        let status = winit_input.dispatch_new_events(|event| match event {
            WinitEvent::Resized { size, .. } => {
                let mode = Mode { size, refresh: 60_000 };
                output.change_current_state(Some(mode), None, None, None);
                output.set_preferred(mode);
                state.space.map_output(&output, (0, 0));
            }
            WinitEvent::Input(event) => state.process_input_event(event, &output),
            WinitEvent::CloseRequested => {
                state.running.store(false, Ordering::SeqCst);
            }
            _ => {}
        });

        if let PumpStatus::Exit(_) = status {
            break;
        }

        if state.pending_wallpaper_reload {
            state.wallpaper.load(backend.renderer());
            state.pending_wallpaper_reload = false;
        }

        // --- render ---
        let size = backend.window_size();
        let (renderer, mut framebuffer) = backend.bind()?;

        let elements = crate::render_elements::build_output_elements(&state, renderer, &output, (size.w, size.h));

        let render_result =
            damage_tracker.render_output(renderer, &mut framebuffer, 0, &elements, Color32F::new(0.05, 0.05, 0.08, 1.0));
        drop(framebuffer);

        match render_result {
            Ok(res) => {
                backend.submit(res.damage.map(|d| d.as_slice()))?;
            }
            Err(err) => {
                tracing::error!("render error: {err:?}");
            }
        }

        let now = state.clock.now();
        state.space.elements().for_each(|window| {
            window.send_frame(&output, now, Some(Duration::ZERO), |_, _| Some(output.clone()));
        });
        state.popups.cleanup();

        event_loop.dispatch(Some(Duration::from_millis(16)), &mut state)?;
        state.display_handle.flush_clients()?;
    }

    Ok(())
}
