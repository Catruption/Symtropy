use egui::{vec2, Vec2};
use serde::Deserialize;

pub const MIN_FRAME_SCALE: f32 = 0.25;
pub const MAX_FRAME_SCALE: f32 = 6.0;
pub const MIN_BUTTON_SCALE: f32 = 0.5;
pub const MAX_BUTTON_SCALE: f32 = 3.0;

/// Accepts `20` or `"20px"` (the website's theme files use both).
fn px<'de, D>(d: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        N(f32),
        S(String),
    }
    Ok(match V::deserialize(d)? {
        V::N(n) => n,
        V::S(s) => s.trim().trim_end_matches("px").trim().parse().unwrap_or(20.0),
    })
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Grid {
    pub columns: f32,
    pub rows: f32,
}
impl Default for Grid {
    fn default() -> Self {
        Self { columns: 4.0, rows: 4.0 }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct CornersConfig {
    pub grid: Grid,
    pub corner_size: String,
}
impl Default for CornersConfig {
    fn default() -> Self {
        Self { grid: Grid::default(), corner_size: "2x2".into() }
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct ImagesConfig {
    pub corners: CornersConfig,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct TitleBarConfig {
    #[serde(deserialize_with = "px")]
    pub font_size: f32,
    pub position: String,
    pub offset_x: f32,
    pub offset_y: f32,
    pub color: String,
}
impl Default for TitleBarConfig {
    fn default() -> Self {
        Self {
            font_size: 20.0,
            position: "top-center".into(),
            offset_x: 0.0,
            offset_y: 0.0,
            color: "white".into(),
        }
    }
}

/// Decoration metadata for the window buttons. `position`/`offset_x`/`offset_y` are retained for
/// theme compatibility but do not define interactive hit targets; see `window::layout`.
#[derive(Deserialize, Clone, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct ControlsConfig {
    pub position: String,
    pub offset_x: f32,
    pub offset_y: f32,
    pub size: f32,
    pub overlap: f32,
    pub enabled: bool,
}
impl Default for ControlsConfig {
    fn default() -> Self {
        Self {
            position: "top-center".into(),
            offset_x: 0.0,
            offset_y: 0.0,
            size: 80.0,
            overlap: 25.0,
            enabled: true,
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct ThemeConfig {
    pub name: String,
    pub image: Option<String>,
    pub inset: f32,
    pub top_inset_multiplier: f32,
    pub images: ImagesConfig,
    pub title_bar: TitleBarConfig,
    pub controls: ControlsConfig,
}
impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            name: "steel".into(),
            image: None,
            inset: 20.0,
            top_inset_multiplier: 2.0,
            images: Default::default(),
            title_bar: Default::default(),
            controls: Default::default(),
        }
    }
}

pub struct Theme {
    pub config: ThemeConfig,
    /// User preference for the four-part corner texture. The effective value may be larger so the
    /// frame can cover a big (e.g. 4K) screen.
    frame_scale: std::cell::Cell<f32>,
    /// User preference for the window-control buttons, independent from the corner texture.
    button_scale: std::cell::Cell<f32>,
    /// Scale used for title/inset geometry; intentionally independent from the decorative layers.
    scale: std::cell::Cell<f32>,
    effective_frame_scale: std::cell::Cell<f32>,
    pixels_per_point: std::cell::Cell<f32>,
    pub squares: Option<egui::TextureHandle>,
    pub triangles: Option<egui::TextureHandle>,
    pub btn_close: Option<egui::TextureHandle>,
    pub btn_close_hl: Option<egui::TextureHandle>,
    pub btn_min: Option<egui::TextureHandle>,
    pub btn_min_hl: Option<egui::TextureHandle>,
    pub btn_max: Option<egui::TextureHandle>,
    pub btn_max_hl: Option<egui::TextureHandle>,
}

impl Theme {
    /// Theme assets are compiled into the binary so the app is relocatable.
    pub fn load(ctx: &egui::Context) -> Self {
        let config: ThemeConfig = serde_json::from_str(include_str!("../assets/themes/steel/theme.json"))
            .map_err(|e| eprintln!("theme json: {e}"))
            .unwrap_or_default();

        Self {
            frame_scale: std::cell::Cell::new(1.0),
            button_scale: std::cell::Cell::new(1.0),
            scale: std::cell::Cell::new(1.0 / ctx.pixels_per_point()),
            effective_frame_scale: std::cell::Cell::new(1.0),
            pixels_per_point: std::cell::Cell::new(ctx.pixels_per_point().max(0.1)),
            squares: load_texture(ctx, "squares", include_bytes!("../assets/themes/steel/squares.webp")),
            triangles: None,
            btn_close: load_texture(ctx, "close", include_bytes!("../assets/themes/steel/close.webp")),
            btn_close_hl: load_texture(ctx, "close-highlighted", include_bytes!("../assets/themes/steel/close-highlighted.webp")),
            btn_min: load_texture(ctx, "minimize", include_bytes!("../assets/themes/steel/minimize.webp")),
            btn_min_hl: load_texture(ctx, "minimize-highlighted", include_bytes!("../assets/themes/steel/minimize-highlighted.webp")),
            btn_max: load_texture(ctx, "maximize", include_bytes!("../assets/themes/steel/maximize.webp")),
            btn_max_hl: load_texture(ctx, "maximize-highlighted", include_bytes!("../assets/themes/steel/maximize-highlighted.webp")),
            config,
        }
    }

    /// Embedded fallback icon for file types without a thumbnail.
    pub fn icon_bytes(name: &str) -> Option<&'static [u8]> {
        match name {
            "folder" => Some(include_bytes!("../assets/themes/test/icons/folder.webp")),
            "tar" => Some(include_bytes!("../assets/themes/test/icons/tar.png")),
            "txt" => Some(include_bytes!("../assets/themes/test/icons/txt.webp")),
            _ => None,
        }
    }

    /// Scale for title/inset geometry, in points per texture pixel.
    pub fn scale(&self) -> f32 {
        self.scale.get().max(0.01)
    }

    /// Raw saved frame-scale preference, without the automatic large-screen enlargement.
    pub fn frame_scale_setting(&self) -> f32 {
        self.frame_scale.get()
    }

    pub fn button_scale_setting(&self) -> f32 {
        self.button_scale.get()
    }

    pub fn set_frame_scale(&self, scale: f32) {
        self.frame_scale.set(scale.clamp(MIN_FRAME_SCALE, MAX_FRAME_SCALE));
    }

    pub fn set_button_scale(&self, scale: f32) {
        self.button_scale.set(scale.clamp(MIN_BUTTON_SCALE, MAX_BUTTON_SCALE));
    }

    /// Smallest frame scale that can cover `screen` with the four corner blocks.
    pub fn min_frame_scale(&self, screen: Vec2) -> f32 {
        self.required_frame_scale(screen)
            .clamp(MIN_FRAME_SCALE, MAX_FRAME_SCALE)
    }

    /// Largest frame scale that leaves a usable window plate on `screen`.
    pub fn max_frame_scale(&self, screen: Vec2) -> f32 {
        const MIN_PLATE: f32 = 320.0;
        let room = screen.x.min(screen.y) - MIN_PLATE;
        let max = room / (2.0 * crate::window::OVERHANG);
        max.clamp(MIN_FRAME_SCALE, MAX_FRAME_SCALE)
    }

    /// Scale for the corner texture and its transparent overhang, in points per texture pixel.
    pub fn decor_scale(&self) -> f32 {
        self.effective_frame_scale.get() / self.pixels_per_point.get().max(0.1)
    }

    /// Scale for the window-control buttons, in points per texture pixel.
    pub fn button_scale(&self) -> f32 {
        self.button_scale.get() / self.pixels_per_point.get().max(0.1)
    }

    pub fn sync_scale(&self, pixels_per_point: f32, screen: Vec2) {
        let ppp = pixels_per_point.max(0.1);
        self.pixels_per_point.set(ppp);
        self.scale.set(1.0 / ppp);
        let max = self.max_frame_scale(screen);
        let effective = self
            .frame_scale
            .get()
            .max(self.required_frame_scale(screen))
            .clamp(MIN_FRAME_SCALE, max);
        self.effective_frame_scale.set(effective);
    }

    /// The corner quads meet across the frame once `2 * corner_px >= frame_width`. The frame is
    /// wider than the screen by `2 * (side - overhang)`, so solve for the multiplier directly.
    fn required_frame_scale(&self, screen: Vec2) -> f32 {
        let Some(tex) = self.squares.as_ref() else { return 1.0 };
        let g = &self.config.images.corners;
        let (cc, cr) = g
            .corner_size
            .split_once('x')
            .and_then(|(a, b)| Some((a.parse::<f32>().ok()?, b.parse::<f32>().ok()?)))
            .unwrap_or((2.0, 2.0));
        let corner_span = 2.0 * vec2(
            tex.size_vec2().x / g.grid.columns * cc,
            tex.size_vec2().y / g.grid.rows * cr,
        );
        let side = tex.size_vec2().x * 0.125;
        let overhang = crate::window::OVERHANG;
        let cover_span = corner_span - 2.0 * Vec2::splat(side - overhang);
        let needed_x = screen.x / cover_span.x.max(1.0);
        let needed_y = screen.y / cover_span.y.max(1.0);
        needed_x.max(needed_y)
    }

    pub fn corner_px(&self) -> Option<Vec2> {
        let img = self.squares.as_ref()?.size_vec2();
        let g = &self.config.images.corners;
        let (cc, cr) = g
            .corner_size
            .split_once('x')
            .and_then(|(a, b)| Some((a.parse::<f32>().ok()?, b.parse::<f32>().ok()?)))
            .unwrap_or((2.0, 2.0));
        Some(vec2(img.x / g.grid.columns * cc, img.y / g.grid.rows * cr) * self.decor_scale())
    }

    pub fn side(&self, win: Vec2) -> f32 {
        if let Some(t) = self.squares.as_ref().or(self.triangles.as_ref()) {
            t.size_vec2().x * 0.125 * self.decor_scale()
        } else {
            ((win.x.min(win.y) * 0.08).max(40.0)) * self.decor_scale()
        }
    }

    pub fn title_font(&self) -> egui::FontId {
        // Title/title offsets stay on the theme's layout scale, not the decorative scale sliders.
        egui::FontId::proportional(self.config.title_bar.font_size * self.scale() * self.pixels_per_point.get())
    }

    pub fn parse_color(s: &str) -> egui::Color32 {
        match s.trim() {
            "black" => egui::Color32::BLACK,
            h if h.starts_with('#') && h.len() == 7 => {
                let v = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(255);
                egui::Color32::from_rgb(v(1), v(3), v(5))
            }
            _ => egui::Color32::WHITE,
        }
    }
}

fn load_texture(ctx: &egui::Context, name: &str, bytes: &[u8]) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let pixels = img.as_flat_samples();
    Some(ctx.load_texture(
        name,
        egui::ColorImage::from_rgba_unmultiplied(size, pixels.as_slice()),
        egui::TextureOptions::LINEAR,
    ))
}