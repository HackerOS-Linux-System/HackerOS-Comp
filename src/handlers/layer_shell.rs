use smithay::desktop::{layer_map_for_output, LayerSurface};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::wayland::shell::wlr_layer::{Layer, LayerSurface as WlrLayerSurface, WlrLayerShellHandler, WlrLayerShellState};
use smithay::delegate_layer_shell;

use crate::state::HwdeState;

impl WlrLayerShellHandler for HwdeState {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(&mut self, surface: WlrLayerSurface, wl_output: Option<WlOutput>, _layer: Layer, namespace: String) {
        let output = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.space.outputs().next().cloned());

        let Some(output) = output else {
            tracing::warn!("new layer-shell surface '{namespace}' but no output exists yet");
            return;
        };

        let desktop_surface = LayerSurface::new(surface, namespace.clone());

        let mut map = layer_map_for_output(&output);
        if let Err(err) = map.map_layer(&desktop_surface) {
            tracing::warn!("failed to map layer surface '{namespace}': {err}");
            return;
        }
        map.arrange();
        tracing::info!("mapped layer-shell surface '{namespace}'");
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        for output in self.space.outputs().cloned().collect::<Vec<_>>() {
            let mut map = layer_map_for_output(&output);
            let matched = map.layers().find(|l| l.wl_surface() == surface.wl_surface()).cloned();
            if let Some(matched) = matched {
                map.unmap_layer(&matched);
                map.arrange();
                break;
            }
        }
    }
}
delegate_layer_shell!(HwdeState);
