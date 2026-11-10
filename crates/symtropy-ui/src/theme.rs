use egui::{vec2, Vec2};
use serde::Deserialize;

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
    pub user_scale: f32,
    scale: std::cell::Cell<f32>,
    pub squares: Option<egui::TextureHandle>,
    pub triangles: Option<egui::TextureHandle>,
    pub btn_close: Option<egui::TextureHandle>,
    pub btn_close_hl: Option<egui::TextureHandle>,
    pub btn_min: Option<egui::TextureHandle>,
    pub btn_min_hl: Option<egui::TextureHandle>,
    pub btn_max: Option<egui::TextureHandle>,
    pub btn_max_hl: Option<egui::TextureHandle>,
    pub dir_path: std::path::PathBuf,
}

impl Theme {
    pub fn load(ctx: &egui::Context, dir: &std::path::Path) -> Self {
        let config: ThemeConfig = std::fs::read_to_string(dir.join("theme.json"))
            .map_err(|e| eprintln!("theme json: {e}"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).map_err(|e| eprintln!("theme json: {e}")).ok())
            .unwrap_or_default();

        let squares_name = config.image.clone().unwrap_or_else(|| "squares.webp".into());

        Self {
            dir_path: dir.to_path_buf(),
            user_scale: 1.0,
            scale: std::cell::Cell::new(1.0 / ctx.pixels_per_point()),
            squares: load_texture(ctx, &dir.join(squares_name)),
            triangles: load_texture(ctx, &dir.join("triangles.webp")),
            btn_close: load_texture(ctx, &dir.join("close.webp")),
            btn_close_hl: load_texture(ctx, &dir.join("close-highlighted.webp")),
            btn_min: load_texture(ctx, &dir.join("minimize.webp")),
            btn_min_hl: load_texture(ctx, &dir.join("minimize-highlighted.webp")),
            btn_max: load_texture(ctx, &dir.join("maximize.webp")),
            btn_max_hl: load_texture(ctx, &dir.join("maximize-highlighted.webp")),
            config,
        }
    }

    pub fn scale(&self) -> f32 {
        self.scale.get().max(0.01)
    }

    pub fn sync_scale(&self, pixels_per_point: f32) {
        self.scale.set(self.user_scale / pixels_per_point.max(0.1));
    }

    pub fn corner_px(&self) -> Option<Vec2> {
        let img = self.squares.as_ref()?.size_vec2();
        let g = &self.config.images.corners;
        let (cc, cr) = g
            .corner_size
            .split_once('x')
            .and_then(|(a, b)| Some((a.parse::<f32>().ok()?, b.parse::<f32>().ok()?)))
            .unwrap_or((2.0, 2.0));
        Some(vec2(img.x / g.grid.columns * cc, img.y / g.grid.rows * cr) * self.scale())
    }

    pub fn side(&self, win: Vec2) -> f32 {
        if let Some(t) = self.squares.as_ref().or(self.triangles.as_ref()) {
            t.size_vec2().x * 0.125 * self.scale()
        } else {
            (win.x.min(win.y) * 0.08).max(40.0)
        }
    }

    pub fn title_font(&self) -> egui::FontId {
        // FIX: Let egui handle the ppp scaling natively by not dividing by ppp here.
        egui::FontId::proportional(self.config.title_bar.font_size * self.user_scale)
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

fn load_texture(ctx: &egui::Context, path: &std::path::Path) -> Option<egui::TextureHandle> {
    let img = image::open(path).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let pixels = img.as_flat_samples();
    Some(ctx.load_texture(
        path.file_stem()?.to_str()?,
        egui::ColorImage::from_rgba_unmultiplied(size, pixels.as_slice()),
        egui::TextureOptions::LINEAR,
    ))
}