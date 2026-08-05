use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::texture::{TextureBuffer, TextureRenderElement};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Physical, Point, Transform};

/// Default wallpaper shipped by the `hackeros-wallpapers` package. This is
/// the one wallpaper HWDE actively supports out of the box today; a proper
/// "Wallpapers" settings panel (multiple images, per-workspace, etc.) is
/// planned but not part of this first cut.
pub const DEFAULT_WALLPAPER: &str = "/usr/share/wallpapers/HackerOS-Wallpapers/Wallpaper23.png";

pub struct Wallpaper {
    path: std::path::PathBuf,
    texture: Option<TextureBuffer<smithay::backend::renderer::gles::GlesTexture>>,
}

impl Wallpaper {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into(), texture: None }
    }

    /// (Re)loads the wallpaper image and uploads it as a GPU texture. Call
    /// once at startup and again if `HWDE_WALLPAPER` / the compositor's
    /// `SetWallpaper` IPC request changes the path.
    pub fn load(&mut self, renderer: &mut GlesRenderer) {
        match image::open(&self.path) {
            Ok(img) => {
                let rgba = img.to_rgba8();
                let (width, height) = rgba.dimensions();
                match TextureBuffer::from_memory(
                    renderer,
                    &rgba,
                    Fourcc::Abgr8888,
                    (width as i32, height as i32),
                    false,
                    1,
                    Transform::Normal,
                    None,
                ) {
                    Ok(texture) => {
                        tracing::info!("loaded wallpaper: {}", self.path.display());
                        self.texture = Some(texture);
                    }
                    Err(err) => {
                        tracing::error!("failed to upload wallpaper texture: {err}");
                        self.texture = None;
                    }
                }
            }
            Err(err) => {
                tracing::error!(
                    "failed to load wallpaper {}: {err} (falling back to solid background)",
                    self.path.display()
                );
                self.texture = None;
            }
        }
    }

    pub fn set_path(&mut self, path: impl Into<std::path::PathBuf>) {
        self.path = path.into();
    }

    /// Builds a render element that fills `output_size` (physical pixels),
    /// scaling the wallpaper to cover the output - or `None` if the
    /// wallpaper failed to load, in which case the caller should just clear
    /// to a solid color instead.
    pub fn render_element(
        &self,
        output_size: (i32, i32),
    ) -> Option<TextureRenderElement<smithay::backend::renderer::gles::GlesTexture>> {
        let texture = self.texture.as_ref()?;
        Some(TextureRenderElement::from_texture_buffer(
            Point::<f64, Physical>::from((0.0, 0.0)),
            texture,
            None,
            None,
            Some(output_size.into()),
            Kind::Unspecified,
        ))
    }
}
