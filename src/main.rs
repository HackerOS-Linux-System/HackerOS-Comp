mod config;
mod grabs;
mod handlers;
mod input;
mod ipc;
mod render_elements;
mod state;
mod wallpaper;
mod winit_backend;
#[cfg(feature = "xwayland")]
mod xwayland;
// Experimental, feature-gated, NOT wired into `main` yet - see its module
// doc comment for exactly what's missing before this could drive a real
// TTY/SDDM session. Kept compiling (behind a feature flag) so it doesn't
// bit-rot while that work is planned.
#[cfg(feature = "drm-experimental")]
mod backend_drm;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let wallpaper_path = std::env::var("HWDE_WALLPAPER")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(wallpaper::DEFAULT_WALLPAPER));

    tracing::info!("comphwde starting (wallpaper: {})", wallpaper_path.display());

    winit_backend::run(wallpaper_path)
}
