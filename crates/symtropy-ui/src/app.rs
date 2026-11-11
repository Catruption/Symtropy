use crate::theme::Theme;
use crate::x11::X11Window;
use crate::gallery::{GalleryState, frame_from_time, Sidecar};
use crate::sidebar::FilterQuery;
use crate::file_manager::{FileManagerState, ViewMode};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use alhazen_core::{Player, PlayerConfig, Source, VideoFrame, PlayerEvent};

pub enum FmAction {
    Navigate(PathBuf),
    OpenSmart(PathBuf),
    OpenTerminal(PathBuf),
    Trash(PathBuf),
}

pub struct SymtropyApp {
    theme: Theme,
    frames: u32,
    x11: Option<X11Window>,
    last_regions: Vec<[i32; 4]>,

    fm: FileManagerState,
    gallery: Option<GalleryState>,

    // Viewer state
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

    // UI State
    is_edit_mode: bool,
    staged_sidecar: Option<Sidecar>,
    filter_query: FilterQuery,
    show_places: bool,
}

impl SymtropyApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.options_mut(|opt| { opt.zoom_with_keyboard = false; });
        let theme_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/themes/steel");
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest.parent().unwrap().parent().unwrap();
        let db_path = workspace_root.join("database/References");

        let mut fm = FileManagerState::new(db_path);
        fm.view_mode = ViewMode::Gallery;
        let gallery = Some(GalleryState::scan_dir(&cc.egui_ctx, &fm.current_dir));

        Self {
            theme: Theme::load(&cc.egui_ctx, &theme_path),
            frames: 0, x11: None, last_regions: Vec::new(),
            fm, gallery,
            viewer_open: false, viewer_cache: HashMap::new(), viewer_player: None,
            viewer_loaded_path: None, viewer_zoom: 1.0, viewer_pan: egui::Vec2::ZERO,
            viewer_anim_frames: Vec::new(), viewer_anim_durations: Vec::new(),
            viewer_show_sidebar: true, viewer_sidebar_width: 500.0, viewer_needs_fit: false,
            is_edit_mode: false, staged_sidecar: None, filter_query: FilterQuery::default(),
            show_places: true,
        }
    }
}

impl eframe::App for SymtropyApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] { [0.0, 0.0, 0.0, 0.0] }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let ppp = ctx.pixels_per_point();
        self.theme.sync_scale(ppp);

        self.frames += 1;
        if self.x11.is_none() && self.frames < 600 {
            if self.frames % 10 == 0 {
                if let Some(x) = X11Window::find() {
                    eprintln!("X11: Found window! Applying frame extents.");
                    x.set_frame_extents((crate::window::overhang(&self.theme) * ppp).round() as u32);
                    self.x11 = Some(x);
                } else if self.frames == 590 {
                    eprintln!("X11: Gave up. Are you running native Wayland?");
                }
            }
            ctx.request_repaint();
        }

        let mut fm_actions = Vec::new();

        let regions = crate::window::show(ctx, &self.theme, "Symtropy", |ui| {
            let max_size = ui.available_size();

            ui.allocate_ui_with_layout(
                max_size,
                egui::Layout::top_down(egui::Align::Min),
                                       |ui| {
                                           // Top Bar
                                           ui.horizontal(|ui| {
                                               if ui.selectable_label(self.show_places, "📁 Places").clicked() {
                                                   self.show_places = !self.show_places;
                                               }
                                               ui.separator();

                                               if ui.button("⬅").clicked() { self.fm.go_back(); self.gallery = None; }
                                               if ui.button("➡").clicked() { self.fm.go_forward(); self.gallery = None; }
                                               if ui.button("⬆").clicked() {
                                                   if let Some(p) = self.fm.current_dir.parent() { self.fm.navigate_to(p.to_path_buf()); self.gallery = None; }
                                               }

                                               let resp = ui.add(egui::TextEdit::singleline(&mut self.fm.search_query).hint_text("Search...").desired_width(150.0));
                                               if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) { self.fm.search_recursive(); }

                                               ui.separator();
                                               ui.selectable_value(&mut self.fm.view_mode, ViewMode::List, "☰ List");
                                               if ui.selectable_value(&mut self.fm.view_mode, ViewMode::Gallery, "🖼️ Gallery").clicked() {
                                                   if self.gallery.is_none() {
                                                       self.gallery = Some(GalleryState::scan_dir(ui.ctx(), &self.fm.current_dir));
                                                   }
                                               }
                                               ui.checkbox(&mut self.fm.show_hidden, "Hidden");
                                           });
                                           ui.separator();

                                           // Split layout
                                           ui.horizontal_top(|ui| {
                                               if self.show_places {
                                                   ui.allocate_ui_with_layout(
                                                       egui::vec2(180.0, ui.available_height()),
                                                                              egui::Layout::top_down(egui::Align::Min),
                                                                              |ui| {
                                                                                  egui::ScrollArea::vertical()
                                                                                  .id_source("places_scroll")
                                                                                  .auto_shrink([false, false])
                                                                                  .show(ui, |ui| {
                                                                                      ui.add_space(8.0);
                                                                                      ui.heading("Places");

                                                                                      let xdg = [
                                                                                          ("🏠 Home", dirs::home_dir()),
                                                                                        ("📥 Downloads", dirs::download_dir()),
                                                                                        ("🖼️ Pictures", dirs::picture_dir()),
                                                                                        ("🗑️ Trash", Some(dirs::home_dir().unwrap().join(".local/share/Trash/files")))
                                                                                      ];
                                                                                      for (name, path) in xdg {
                                                                                          if let Some(p) = path {
                                                                                              if p.exists() && ui.button(name).clicked() { fm_actions.push(FmAction::Navigate(p)); }
                                                                                          }
                                                                                      }

                                                                                      ui.separator();
                                                                                      let mounts = self.fm.mounts.clone();
                                                                                      for mount in mounts {
                                                                                          if ui.button(format!("💽 {}", mount.display())).clicked() { fm_actions.push(FmAction::Navigate(mount)); }
                                                                                      }
                                                                                  });
                                                                              }
                                                   );
                                                   ui.separator();
                                               }

                                               ui.allocate_ui_with_layout(
                                                   egui::vec2(ui.available_width(), ui.available_height()),
                                                                          egui::Layout::top_down(egui::Align::Min),
                                                                          |ui| {
                                                                              if self.fm.view_mode == ViewMode::List {
                                                                                  egui::ScrollArea::vertical()
                                                                                  .id_source("list_scroll")
                                                                                  .auto_shrink([false, false])
                                                                                  .show(ui, |ui| {
                                                                                      let entries = std::mem::take(&mut self.fm.entries);
                                                                                      for entry in &entries {
                                                                                          let icon = if entry.is_dir { "📁" } else { "📄" };
                                                                                          let resp = ui.selectable_label(self.fm.selected.contains(&entry.path), format!("{} {}", icon, entry.name));

                                                                                          if resp.double_clicked() {
                                                                                              if entry.is_dir { fm_actions.push(FmAction::Navigate(entry.path.clone())); }
                                                                                              else { fm_actions.push(FmAction::OpenSmart(entry.path.clone())); }
                                                                                          }

                                                                                          resp.context_menu(|ui| {
                                                                                              if ui.button("Open").clicked() { fm_actions.push(FmAction::OpenSmart(entry.path.clone())); ui.close_menu(); }
                                                                                              if ui.button("Open Terminal Here").clicked() { fm_actions.push(FmAction::OpenTerminal(entry.path.clone())); ui.close_menu(); }
                                                                                              if ui.button("Move to Trash").clicked() { fm_actions.push(FmAction::Trash(entry.path.clone())); ui.close_menu(); }
                                                                                          });
                                                                                      }
                                                                                      self.fm.entries = entries;
                                                                                  });
                                                                              } else if let Some(gallery) = &mut self.gallery {
                                                                                  gallery.show(ui, &self.theme);

                                                                                  if gallery.request_open_viewer {
                                                                                      self.viewer_open = true;
                                                                                      gallery.request_open_viewer = false;
                                                                                  }
                                                                              }
                                                                          }
                                               );
                                           });
                                       }
            );
        });

        // Execute file manager actions
        for action in fm_actions {
            match action {
                FmAction::Navigate(p) => { self.fm.navigate_to(p); self.gallery = None; }
                FmAction::OpenSmart(p) => { self.fm.open_smart(&p); }
                FmAction::OpenTerminal(p) => {
                    let dir = if p.is_dir() { p } else { p.parent().unwrap_or(&p).to_path_buf() };
                    for t in ["konsole", "gnome-terminal", "alacritty", "kitty", "xterm"] {
                        if std::process::Command::new(t).current_dir(&dir).spawn().is_ok() { break; }
                    }
                }
                FmAction::Trash(p) => { let _ = trash::delete(p); self.fm.refresh(); }
            }
        }

        if let Some(x) = &self.x11 {
            let px: Vec<[i32; 4]> = regions.iter().map(|r| {
                let (x0, y0) = ((r.min.x * ppp).round() as i32, (r.min.y * ppp).round() as i32);
                let (x1, y1) = ((r.max.x * ppp).round() as i32, (r.max.y * ppp).round() as i32);
                [x0, y0, x1 - x0, y1 - y0]
            }).collect();
            if px != self.last_regions {
                x.set_input_region(&px);
                self.last_regions = px;
            }
        }

        // --- Viewport Logic ---
        let viewer_path: Option<String> = if self.viewer_open {
            self.gallery.as_ref().and_then(|g| g.viewer_target.clone())
            .or_else(|| self.gallery.as_ref().and_then(|g| g.selected_images.first().cloned()))
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
                self.is_edit_mode = false;

                if let Some(gallery) = &self.gallery {
                    self.staged_sidecar = gallery.items.iter().find(|i| i.path == path).map(|i| i.sidecar.clone());
                    if let Some(g) = &mut self.gallery {
                        g.active_tags = self.staged_sidecar.as_ref().map(|sc| sc.tags.iter().cloned().collect()).unwrap_or_default();
                    }
                }

                let is_video = path.to_lowercase().ends_with(".mp4") || path.to_lowercase().ends_with(".webm") || path.to_lowercase().ends_with(".mkv");

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
                                    let ci = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
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
                            ctx.load_texture(&path, egui::ColorImage::from_rgba_unmultiplied(size, pixels.as_slice()), egui::TextureOptions::LINEAR)
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
            let mut zoom = self.viewer_zoom;
            let mut pan = self.viewer_pan;
            let mut show_sidebar = self.viewer_show_sidebar;
            let mut sidebar_width = self.viewer_sidebar_width;
            let mut needs_fit = self.viewer_needs_fit;

            let mut is_edit_mode = self.is_edit_mode;
            let mut staged_sidecar = self.staged_sidecar.take();
            let mut filter_query = self.filter_query.clone();

            // Capture the gallery's query state so the sidebar can update it
            let mut active_query = self.gallery.as_ref()
            .map(|g| g.active_query.clone())
            .unwrap_or(crate::gallery::Query::AlwaysTrue);
            let mut query_input = self.gallery.as_ref()
            .map(|g| g.query_input.clone())
            .unwrap_or_default();

            let (tag_tree, mut active_tags, mut search_query) = if let Some(g) = &self.gallery {
                (g.tag_tree.clone(), g.active_tags.clone(), g.tag_search_query.clone())
            } else {
                (crate::gallery::TagNode { name: "Root".into(), children: vec![] }, Default::default(), String::new())
            };

            let mut apply_clicked = false;
            let mut revert_clicked = false;
            let short_name = path.rsplit('/').next().unwrap_or("").to_string();
            let is_video = path.to_lowercase().ends_with(".mp4") || path.to_lowercase().ends_with(".webm") || path.to_lowercase().ends_with(".mkv");

            ctx.show_viewport_immediate(
                egui::ViewportId::from_hash_of("media_viewer"),
                                        egui::ViewportBuilder::default()
                                        .with_title(&short_name)
                                        .with_inner_size([1400.0, 900.0])
                                        .with_min_inner_size([800.0, 400.0])
                                        .with_decorations(false)
                                        .with_transparent(true),
                                        |ctx, _class| {
                                            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) { still_open = false; }
                                            if ctx.input(|i| i.key_pressed(egui::Key::Tab)) { show_sidebar = !show_sidebar; }

                                            egui::CentralPanel::default().frame(egui::Frame::none()).show(ctx, |ui| {
                                                let rect = ui.max_rect();
                                                let actual_sidebar_width = if show_sidebar { sidebar_width } else { 0.0 };
                                                let img_rect = egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x - actual_sidebar_width, rect.max.y));
                                                let sidebar_rect = egui::Rect::from_min_max(egui::pos2(rect.max.x - actual_sidebar_width, rect.min.y), rect.max);

                                                ui.painter().rect_filled(img_rect, 0.0, egui::Color32::from_rgb(15, 15, 20));
                                                if show_sidebar {
                                                    ui.painter().rect_filled(sidebar_rect, 0.0, egui::Color32::from_rgba_unmultiplied(25, 25, 30, 255));
                                                    ui.painter().line_segment([sidebar_rect.min, egui::pos2(sidebar_rect.min.x, sidebar_rect.max.y)], egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(50, 50, 60)));
                                                }

                                                let bg_resp = ui.interact(img_rect, ui.id().with("bg"), egui::Sense::click_and_drag());
                                                let mut drag_resp_opt = None;
                                                if show_sidebar {
                                                    let drag_bar_rect = egui::Rect::from_min_size(egui::pos2(sidebar_rect.min.x - 6.0, sidebar_rect.min.y), egui::vec2(6.0, sidebar_rect.height()));
                                                    let dr = ui.interact(drag_bar_rect, ui.id().with("sidebar_drag"), egui::Sense::drag());
                                                    if dr.hovered() || dr.dragged() { ctx.set_cursor_icon(egui::CursorIcon::ResizeHorizontal); }
                                                    if dr.dragged() { sidebar_width = (sidebar_width - dr.drag_delta().x).clamp(400.0, rect.width() - 400.0); }
                                                    drag_resp_opt = Some(dr);
                                                }

                                                let close_rect = egui::Rect::from_min_size(img_rect.max - egui::vec2(56.0, 56.0), egui::vec2(40.0, 40.0));
                                                let close_resp = ui.interact(close_rect, ui.id().with("close_btn"), egui::Sense::click());
                                                let toggle_rect = egui::Rect::from_min_size(img_rect.max - egui::vec2(104.0, 56.0), egui::vec2(40.0, 40.0));
                                                let toggle_resp = ui.interact(toggle_rect, ui.id().with("sidebar_toggle"), egui::Sense::click());

                                                let over_ui = close_resp.hovered() || toggle_resp.hovered() || drag_resp_opt.as_ref().map_or(false, |r| r.hovered() || r.dragged());

                                                if bg_resp.dragged_by(egui::PointerButton::Primary) && !over_ui { pan += bg_resp.drag_delta(); }
                                                if bg_resp.double_clicked() { needs_fit = true; }

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
                                                            for chunk in bgra.chunks_exact(4) { rgba.push(chunk[2]); rgba.push(chunk[1]); rgba.push(chunk[0]); rgba.push(chunk[3]); }
                                                            let image = egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba);
                                                            let tex = ctx.load_texture("video_frame", image, egui::TextureOptions::LINEAR);
                                                            let img_size = egui::vec2(width as f32, height as f32) * zoom;
                                                            let display_rect = egui::Rect::from_center_size(img_rect.center() + pan, img_size);
                                                            let full_uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                                                            ui.painter().with_clip_rect(img_rect).image(tex.id(), display_rect, full_uv, egui::Color32::WHITE);
                                                        }
                                                        ctx.request_repaint();
                                                        for event in player.events().try_iter() { if let PlayerEvent::Ended = event { still_open = false; } }
                                                    }
                                                } else {
                                                    let mut texture_to_draw = None;
                                                    if !anim_frames.is_empty() {
                                                        let now = ctx.input(|i| i.time);
                                                        let start = *self.gallery.as_ref().unwrap().anim_start.get(&path).unwrap_or(&now);
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
                                                        ui.painter().with_clip_rect(img_rect).image(tex.id(), display_rect, full_uv, egui::Color32::WHITE);
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
                                                    let (apply, revert) = crate::sidebar::draw_sidebar(
                                                        ui, sidebar_rect, &mut is_edit_mode, &mut staged_sidecar, &mut filter_query, &tag_tree, &mut active_tags, &mut search_query
                                                    );
                                                    if apply { apply_clicked = true; }
                                                    if revert { revert_clicked = true; }
                                                }
                                            });

                                            if ctx.input(|i| i.viewport().close_requested()) { still_open = false; }
                                        },
            );

            // Apply
            if apply_clicked {
                if is_edit_mode {
                    if let Some(sc) = &staged_sidecar {
                        let mut targets = self.gallery.as_ref().unwrap().selected_images.clone();
                        if targets.is_empty() { targets.push(path.clone()); }
                        for p in targets {
                            if let Some(g) = &mut self.gallery {
                                if let Some(item) = g.items.iter_mut().find(|i| i.path == p) {
                                    item.sidecar = sc.clone();
                                    item.tags = sc.tags.iter().cloned().collect();
                                    item.rating = sc.rating;
                                    let _ = item.sidecar.save(Path::new(&item.path));

                                    let index_path = crate::gallery::workers::cache_dir().join("index.tsv");
                                    crate::gallery::workers::update_index_entry(
                                        &index_path,
                                        &item.path,
                                        &item.sidecar.tags,
                                        item.sidecar.rating,
                                    );
                                }
                            }
                        }
                    }
                } else {
                    // Filter mode: build a Query from the sidebar fields and apply it
                    let mut parts = Vec::new();
                    if !filter_query.tags.trim().is_empty() {
                        parts.push(filter_query.tags.trim().to_string());
                    }

                    let full_query = parts.join(" + ");
                    query_input = full_query.clone();

                    match crate::gallery::query::Query::parse(&full_query) {
                        Ok(q) => {
                            eprintln!("gallery: applied filter '{}'", full_query);
                            active_query = q;
                        }
                        Err(e) => {
                            eprintln!("gallery: filter parse error: {}", e);
                        }
                    }
                }
            }

            // Revert
            if revert_clicked {
                if is_edit_mode {
                    if let Some(g) = &self.gallery {
                        if let Some(item) = g.items.iter().find(|i| i.path == path) {
                            staged_sidecar = Some(item.sidecar.clone());
                            active_tags = item.tags.clone();
                        }
                    }
                } else {
                    filter_query = FilterQuery::default();
                    active_query = crate::gallery::Query::AlwaysTrue;
                    query_input.clear();
                    eprintln!("gallery: filter cleared");
                }
            }

            self.viewer_cache = viewer_cache;
            self.viewer_player = viewer_player;
            self.viewer_anim_frames = anim_frames;
            self.viewer_anim_durations = anim_durs;
            self.viewer_zoom = zoom;
            self.viewer_pan = pan;
            self.viewer_needs_fit = needs_fit;
            self.viewer_show_sidebar = show_sidebar;
            self.viewer_sidebar_width = sidebar_width;
            self.is_edit_mode = is_edit_mode;
            self.staged_sidecar = staged_sidecar;
            self.filter_query = filter_query;
            if let Some(g) = &mut self.gallery {
                g.active_tags = active_tags;
                g.tag_search_query = search_query;
                g.active_query = active_query;
                g.query_input = query_input;
            }

            if !still_open {
                self.viewer_open = false;
                self.viewer_loaded_path = None;
                if let Some(g) = &mut self.gallery { g.viewer_target = None; g.selected_images.clear(); }
                self.viewer_cache.clear();
                self.viewer_player = None;
                self.viewer_anim_frames.clear();
            }
        }
    }
}
