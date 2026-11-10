use crate::theme::Theme;
use crate::x11::X11Window;
use crate::gallery::{GalleryState, TagNode, Query, frame_from_time};
use std::collections::{HashMap, HashSet};

use alhazen_core::{Player, PlayerConfig, Source, VideoFrame, PlayerEvent};

pub struct SymtropyApp {
    theme: Theme,
    frames: u32,
    x11: Option<X11Window>,
    last_regions: Vec<[i32; 4]>,
    gallery: GalleryState,

    viewer_open: bool,
    viewer_cache: HashMap<String, Option<egui::TextureHandle>>,
    viewer_player: Option<Player>,
    viewer_loaded_path: Option<String>,
    viewer_zoom: f32,
    viewer_pan: egui::Vec2,
    viewer_anim_frames: Vec<egui::TextureHandle>,
    viewer_anim_durations: Vec<f32>,
    viewer_show_sidebar: bool,
    viewer_sidebar_width: f32,
    viewer_needs_fit: bool,
}

impl SymtropyApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.options_mut(|opt| {
            opt.zoom_with_keyboard = false;
        });

        let theme_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("assets/themes/steel");

        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest.parent().unwrap().parent().unwrap();
        let db_path = workspace_root.join("database/References");

        let gallery = GalleryState::scan_dir(&cc.egui_ctx, &db_path);

        Self {
            theme: Theme::load(&cc.egui_ctx, &theme_path),
            frames: 0,
            x11: None,
            last_regions: Vec::new(),
            gallery,
            viewer_open: false,
            viewer_cache: HashMap::new(),
            viewer_player: None,
            viewer_loaded_path: None,
            viewer_zoom: 1.0,
            viewer_pan: egui::Vec2::ZERO,
            viewer_anim_frames: Vec::new(),
            viewer_anim_durations: Vec::new(),
            viewer_show_sidebar: true,
            viewer_sidebar_width: 320.0,
            viewer_needs_fit: false,
        }
    }
}

fn draw_tag_node(ui: &mut egui::Ui, node: &TagNode, selected_tags: &mut HashSet<String>) {
    if node.name == "Root" {
        for child in &node.children {
            draw_tag_node(ui, child, selected_tags);
        }
        return;
    }
    if !node.children.is_empty() {
        // Parent: just a collapsible header, no checkbox.
        egui::CollapsingHeader::new(&node.name).default_open(false).show(ui, |ui| {
            for child in &node.children {
                draw_tag_node(ui, child, selected_tags);
            }
        });
    } else {
        // Leaf: this is an actual tag, so it gets a checkbox.
        let mut checked = selected_tags.contains(&node.name);
        if ui.checkbox(&mut checked, &node.name).changed() {
            if checked { selected_tags.insert(node.name.clone()); }
            else { selected_tags.remove(&node.name); }
        }
    }
}

impl eframe::App for SymtropyApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let ppp = ctx.pixels_per_point();
        self.theme.sync_scale(ppp);

        self.frames += 1;
        if self.x11.is_none() && self.frames >= 2 && self.frames < 600 {
            if let Some(x) = X11Window::find() {
                x.set_frame_extents((crate::window::overhang(&self.theme) * ppp).round() as u32);
                self.x11 = Some(x);
            }
            ctx.request_repaint();
        }

        let regions = crate::window::show(ctx, &self.theme, "Symtropy", |ui| {
            self.gallery.show(ui, &self.theme);
        });

        if let Some(x) = &self.x11 {
            let px: Vec<[i32; 4]> = regions
            .iter()
            .map(|r| {
                let (x0, y0) = ((r.min.x * ppp).round() as i32, (r.min.y * ppp).round() as i32);
                let (x1, y1) = ((r.max.x * ppp).round() as i32, (r.max.y * ppp).round() as i32);
                [x0, y0, x1 - x0, y1 - y0]
            })
            .collect();
            if px != self.last_regions {
                x.set_input_region(&px);
                self.last_regions = px;
            }
        }

        // Open the viewer if the gallery asked for it.
        if self.gallery.request_open_viewer {
            self.viewer_open = true;
            self.gallery.request_open_viewer = false;
        }

        let viewer_path: Option<String> = if self.viewer_open {
            self.gallery.viewer_target.clone()
            .or_else(|| self.gallery.selected_images.first().cloned())
        } else {
            None
        };

        if let Some(path) = viewer_path {
            let is_new = self.viewer_loaded_path.as_deref() != Some(&path);
            if is_new {
                self.viewer_loaded_path = Some(path.clone());
                self.viewer_zoom = 1.0;
                self.viewer_pan = egui::Vec2::ZERO;
                self.viewer_anim_frames.clear();
                self.viewer_anim_durations.clear();
                self.viewer_player = None;
                self.viewer_needs_fit = true;

                let is_video = path.to_lowercase().ends_with(".mp4")
                || path.to_lowercase().ends_with(".webm")
                || path.to_lowercase().ends_with(".mkv");

                if !is_video {
                    if path.to_lowercase().ends_with(".webp") {
                        if let Ok(bytes) = std::fs::read(&path) {
                            if let Ok(decoder) = webp_animation::Decoder::new(&bytes) {
                                let mut t_frames = Vec::new();
                                let mut timestamps = Vec::new();
                                for frame in decoder {
                                    let (w, h) = frame.dimensions();
                                    let mut img = image::RgbaImage::from_raw(w, h, frame.data().to_vec()).unwrap();
                                    if w.max(h) > 2048 {
                                        img = image::imageops::resize(&img, 2048, 2048, image::imageops::FilterType::Lanczos3);
                                    }
                                    let ci = egui::ColorImage::from_rgba_unmultiplied(
                                        [img.width() as usize, img.height() as usize],
                                                                                      img.as_raw(),
                                    );
                                    t_frames.push(ctx.load_texture(format!("{}_f{}", path, t_frames.len()), ci, egui::TextureOptions::LINEAR));
                                    timestamps.push(frame.timestamp() as f32);
                                }
                                if t_frames.len() > 1 {
                                    let mut t_durs = Vec::new();
                                    for i in 0..timestamps.len() {
                                        let current = timestamps[i];
                                        let next = if i + 1 < timestamps.len() { timestamps[i + 1] } else { current + 100.0 };
                                        let mut dur_ms = next - current;
                                        if i == timestamps.len() - 1 && dur_ms <= 0.0 { dur_ms = 100.0; }
                                        t_durs.push(if dur_ms <= 0.0 { 0.05 } else { dur_ms / 1000.0 });
                                    }
                                    self.viewer_anim_frames = t_frames;
                                    self.viewer_anim_durations = t_durs;
                                } else if t_frames.len() == 1 {
                                    self.viewer_cache.insert(path.clone(), Some(t_frames.remove(0)));
                                }
                            }
                        }
                    }

                    if self.viewer_anim_frames.is_empty() && !self.viewer_cache.contains_key(&path) {
                        let tex_opt = image::open(&path).ok().map(|img| {
                            let img = img.to_rgba8();
                            let size = [img.width() as usize, img.height() as usize];
                            let pixels = img.as_flat_samples();
                            ctx.load_texture(
                                &path,
                                egui::ColorImage::from_rgba_unmultiplied(size, pixels.as_slice()),
                                             egui::TextureOptions::LINEAR,
                            )
                        });
                        self.viewer_cache.insert(path.clone(), tex_opt);
                    }
                }
            }

            let mut still_open = true;
            let viewer_cache = std::mem::take(&mut self.viewer_cache);
            let mut viewer_player = std::mem::take(&mut self.viewer_player);
            let anim_frames = std::mem::take(&mut self.viewer_anim_frames);
            let anim_durs = std::mem::take(&mut self.viewer_anim_durations);
            let mut anim_start = std::mem::take(&mut self.gallery.anim_start);
            let mut zoom = self.viewer_zoom;
            let mut pan = self.viewer_pan;
            let mut show_sidebar = self.viewer_show_sidebar;
            let mut sidebar_width = self.viewer_sidebar_width;
            let mut needs_fit = self.viewer_needs_fit;

            let tag_tree = self.gallery.tag_tree.clone();
            let mut active_tags = self.gallery.active_tags.clone();
            let mut search_query = self.gallery.tag_search_query.clone();
            let selected_count = self.gallery.selected_images.len();

            // Query state (filter box now lives here in the viewer).
            let mut query_input = self.gallery.query_input.clone();
            let mut active_query = self.gallery.active_query.clone();
            let mut query_error = self.gallery.query_error.clone();

            let short_name = path.rsplit('/').next().unwrap_or("").to_string();
            let is_video = path.to_lowercase().ends_with(".mp4")
            || path.to_lowercase().ends_with(".webm")
            || path.to_lowercase().ends_with(".mkv");

            ctx.show_viewport_immediate(
                egui::ViewportId::from_hash_of("media_viewer"),
                                        egui::ViewportBuilder::default()
                                        .with_title(&short_name)
                                        .with_inner_size([1200.0, 800.0])
                                        .with_min_inner_size([600.0, 200.0])
                                        .with_decorations(false)
                                        .with_transparent(true),
                                        |ctx, _class| {
                                            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                                                still_open = false;
                                            }
                                            if ctx.input(|i| i.key_pressed(egui::Key::Tab)) {
                                                show_sidebar = !show_sidebar;
                                            }

                                            egui::CentralPanel::default()
                                            .frame(egui::Frame::none())
                                            .show(ctx, |ui| {
                                                let rect = ui.max_rect();
                                                let actual_sidebar_width = if show_sidebar { sidebar_width } else { 0.0 };
                                                let img_rect = egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x - actual_sidebar_width, rect.max.y));
                                                let sidebar_rect = egui::Rect::from_min_max(egui::pos2(rect.max.x - actual_sidebar_width, rect.min.y), rect.max);

                                                ui.painter().rect_filled(img_rect, 0.0, egui::Color32::from_rgb(15, 15, 20));
                                                if show_sidebar {
                                                    ui.painter().rect_filled(sidebar_rect, 0.0, egui::Color32::from_rgba_unmultiplied(25, 25, 30, 255));
                                                    ui.painter().line_segment(
                                                        [sidebar_rect.min, egui::pos2(sidebar_rect.min.x, sidebar_rect.max.y)],
                                                                              egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(50, 50, 60)),
                                                    );
                                                }

                                                let bg_resp = ui.interact(img_rect, ui.id().with("bg"), egui::Sense::click_and_drag());

                                                let mut drag_resp_opt = None;
                                                if show_sidebar {
                                                    let drag_bar_rect = egui::Rect::from_min_size(
                                                        egui::pos2(sidebar_rect.min.x - 6.0, sidebar_rect.min.y),
                                                                                                  egui::vec2(6.0, sidebar_rect.height()),
                                                    );
                                                    let dr = ui.interact(drag_bar_rect, ui.id().with("sidebar_drag"), egui::Sense::drag());
                                                    if dr.hovered() || dr.dragged() {
                                                        ctx.set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                                                    }
                                                    if dr.dragged() {
                                                        sidebar_width = (sidebar_width - dr.drag_delta().x).clamp(200.0, rect.width() - 400.0);
                                                    }
                                                    drag_resp_opt = Some(dr);
                                                }

                                                let close_rect = egui::Rect::from_min_size(img_rect.max - egui::vec2(56.0, 56.0), egui::vec2(40.0, 40.0));
                                                let close_resp = ui.interact(close_rect, ui.id().with("close_btn"), egui::Sense::click());
                                                let toggle_rect = egui::Rect::from_min_size(img_rect.max - egui::vec2(104.0, 56.0), egui::vec2(40.0, 40.0));
                                                let toggle_resp = ui.interact(toggle_rect, ui.id().with("sidebar_toggle"), egui::Sense::click());

                                                let over_ui = close_resp.hovered()
                                                || toggle_resp.hovered()
                                                || drag_resp_opt.as_ref().map_or(false, |r| r.hovered() || r.dragged());

                                                if bg_resp.dragged_by(egui::PointerButton::Primary) && !over_ui {
                                                    pan += bg_resp.drag_delta();
                                                }
                                                if bg_resp.double_clicked() {
                                                    needs_fit = true;
                                                }

                                                let scroll = ui.input(|i| i.raw_scroll_delta.y);
                                                if scroll != 0.0 && !over_ui {
                                                    let zoom_factor = (scroll * 0.005).exp();
                                                    let new_zoom = (zoom * zoom_factor).clamp(0.1, 20.0);
                                                    let f = new_zoom / zoom;
                                                    let cursor_pos = ui.input(|i| i.pointer.hover_pos()).unwrap_or(img_rect.center());
                                                    pan = (cursor_pos - img_rect.center()) * (1.0 - f) + pan * f;
                                                    zoom = new_zoom;
                                                }

                                                if close_resp.clicked() { still_open = false; }
                                                if toggle_resp.clicked() { show_sidebar = !show_sidebar; }

                                                if is_video {
                                                    if viewer_player.is_none() {
                                                        if let Ok(source) = Source::parse(&path) {
                                                            if let Ok(player) = Player::open(source, PlayerConfig::default()) {
                                                                player.play();
                                                                viewer_player = Some(player);
                                                            }
                                                        }
                                                    }
                                                    if let Some(player) = &mut viewer_player {
                                                        player.set_max_output_size(Some((img_rect.width() as u32, img_rect.height() as u32)));
                                                        if let Some(frame) = player.current_frame() {
                                                            let VideoFrame::Cpu { width, height, bgra, .. } = frame;
                                                            if needs_fit {
                                                                let img_size = egui::vec2(width as f32, height as f32);
                                                                if img_size.x > 0.0 && img_size.y > 0.0 {
                                                                    let scale_x = img_rect.width() / img_size.x;
                                                                    let scale_y = img_rect.height() / img_size.y;
                                                                    zoom = scale_x.min(scale_y) * 0.98;
                                                                    pan = egui::Vec2::ZERO;
                                                                }
                                                                needs_fit = false;
                                                            }
                                                            let mut rgba = Vec::with_capacity(bgra.len());
                                                            for chunk in bgra.chunks_exact(4) {
                                                                rgba.push(chunk[2]); rgba.push(chunk[1]); rgba.push(chunk[0]); rgba.push(chunk[3]);
                                                            }
                                                            let image = egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba);
                                                            let tex = ctx.load_texture("video_frame", image, egui::TextureOptions::LINEAR);
                                                            let img_size = egui::vec2(width as f32, height as f32) * zoom;
                                                            let display_rect = egui::Rect::from_center_size(img_rect.center() + pan, img_size);
                                                            let full_uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                                                            let clipped_painter = ui.painter().with_clip_rect(img_rect);
                                                            clipped_painter.image(tex.id(), display_rect, full_uv, egui::Color32::WHITE);
                                                        }
                                                        ctx.request_repaint();
                                                        for event in player.events().try_iter() {
                                                            if let PlayerEvent::Ended = event { still_open = false; }
                                                        }
                                                    }
                                                } else {
                                                    let mut texture_to_draw = None;
                                                    if !anim_frames.is_empty() {
                                                        let now = ctx.input(|i| i.time);
                                                        let start = *anim_start.entry(path.clone()).or_insert(now);
                                                        let frame_idx = frame_from_time(&anim_durs, now - start);
                                                        ctx.request_repaint();
                                                        texture_to_draw = Some(&anim_frames[frame_idx]);
                                                    } else if let Some(Some(tex)) = viewer_cache.get(&path) {
                                                        texture_to_draw = Some(tex);
                                                    }

                                                    if let Some(tex) = texture_to_draw {
                                                        if needs_fit {
                                                            let img_size = tex.size_vec2();
                                                            if img_size.x > 0.0 && img_size.y > 0.0 {
                                                                let scale_x = img_rect.width() / img_size.x;
                                                                let scale_y = img_rect.height() / img_size.y;
                                                                zoom = scale_x.min(scale_y) * 0.98;
                                                                pan = egui::Vec2::ZERO;
                                                            }
                                                            needs_fit = false;
                                                        }
                                                        let img_size = tex.size_vec2() * zoom;
                                                        let display_rect = egui::Rect::from_center_size(img_rect.center() + pan, img_size);
                                                        let full_uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                                                        let clipped_painter = ui.painter().with_clip_rect(img_rect);
                                                        clipped_painter.image(tex.id(), display_rect, full_uv, egui::Color32::WHITE);
                                                    }
                                                }

                                                let close_bg = if close_resp.hovered() { egui::Color32::from_white_alpha(50) } else { egui::Color32::from_black_alpha(150) };
                                                ui.painter().rect_filled(close_rect, 8.0, close_bg);
                                                ui.painter().text(close_rect.center(), egui::Align2::CENTER_CENTER, "✕", egui::FontId::proportional(20.0), egui::Color32::WHITE);

                                                let toggle_bg = if toggle_resp.hovered() { egui::Color32::from_white_alpha(50) } else { egui::Color32::from_black_alpha(150) };
                                                ui.painter().rect_filled(toggle_rect, 8.0, toggle_bg);
                                                let toggle_icon = if show_sidebar { "▶" } else { "◀" };
                                                ui.painter().text(toggle_rect.center(), egui::Align2::CENTER_CENTER, toggle_icon, egui::FontId::proportional(18.0), egui::Color32::WHITE);

                                                if show_sidebar {
                                                    let mut sidebar_ui = ui.child_ui(sidebar_rect, egui::Layout::top_down(egui::Align::Min));
                                                    sidebar_ui.set_clip_rect(sidebar_rect);

                                                    // --- Query filter (moved here from the masonry view) ---
                                                    sidebar_ui.add_space(8.0);
                                                    sidebar_ui.horizontal(|ui| {
                                                        ui.add_space(8.0);
                                                        let avail = (sidebar_rect.width() - 16.0).max(100.0);
                                                        let resp = ui.add_sized(
                                                            [avail, 22.0],
                                                            egui::TextEdit::singleline(&mut query_input)
                                                            .hint_text("filter: fox + plush")
                                                            .font(egui::TextStyle::Monospace),
                                                        );
                                                        if resp.changed() {
                                                            match Query::parse(&query_input) {
                                                                Ok(q) => {
                                                                    active_query = q;
                                                                    query_error = None;
                                                                }
                                                                Err(e) => {
                                                                    query_error = Some(e);
                                                                }
                                                            }
                                                        }
                                                    });
                                                    if let Some(err) = &query_error {
                                                        sidebar_ui.horizontal(|ui| {
                                                            ui.add_space(8.0);
                                                            ui.label(
                                                                egui::RichText::new(format!("⚠ {}", err))
                                                                .color(egui::Color32::from_rgb(220, 100, 100))
                                                                .font(egui::FontId::monospace(10.0)),
                                                            );
                                                        });
                                                    }
                                                    sidebar_ui.add_space(6.0);
                                                    sidebar_ui.separator();
                                                    sidebar_ui.add_space(6.0);

                                                    // --- Tag editing ---
                                                    sidebar_ui.horizontal(|ui| {
                                                        ui.add_space(12.0);
                                                        ui.heading(egui::RichText::new(format!("Tags ({})", selected_count)).color(egui::Color32::WHITE));
                                                    });
                                                    sidebar_ui.add_space(8.0);
                                                    sidebar_ui.horizontal(|ui| {
                                                        ui.add_space(12.0);
                                                        ui.add_sized([sidebar_rect.width() - 24.0, 24.0], egui::TextEdit::singleline(&mut search_query).hint_text("Search tags..."));
                                                    });
                                                    sidebar_ui.add_space(8.0);
                                                    sidebar_ui.separator();
                                                    sidebar_ui.add_space(4.0);
                                                    egui::ScrollArea::vertical().show(&mut sidebar_ui, |ui| {
                                                        ui.add_space(4.0);
                                                        draw_tag_node(ui, &tag_tree, &mut active_tags);
                                                    });
                                                    sidebar_ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                                                        ui.add_space(12.0);
                                                        if ui.add_sized([sidebar_rect.width() - 24.0, 32.0], egui::Button::new("Apply Tags")).clicked() {
                                                            println!("Applied {:?} to {} images", active_tags, selected_count);
                                                        }
                                                        ui.add_space(12.0);
                                                    });
                                                }
                                            });

                                            if ctx.input(|i| i.viewport().close_requested()) {
                                                still_open = false;
                                            }
                                        },
            );

            // Restore shared state.
            self.viewer_cache = viewer_cache;
            self.viewer_player = viewer_player;
            self.viewer_anim_frames = anim_frames;
            self.viewer_anim_durations = anim_durs;
            self.gallery.anim_start = anim_start;
            self.viewer_zoom = zoom;
            self.viewer_pan = pan;
            self.viewer_needs_fit = needs_fit;
            self.viewer_show_sidebar = show_sidebar;
            self.viewer_sidebar_width = sidebar_width;
            self.gallery.active_tags = active_tags;
            self.gallery.tag_search_query = search_query;
            self.gallery.query_input = query_input;
            self.gallery.active_query = active_query;
            self.gallery.query_error = query_error;

            if !still_open {
                self.viewer_open = false;
                self.viewer_loaded_path = None;
                self.gallery.viewer_target = None;
                self.gallery.selected_images.clear();
                self.viewer_cache.clear();
                self.viewer_player = None;
                self.viewer_anim_frames.clear();
            }
        }
    }
}
