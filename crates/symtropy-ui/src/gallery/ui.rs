use super::*;
use crate::theme::Theme;
use egui::scroll_area::ScrollBarVisibility;
use egui::{Color32, Rect, Sense, TextureHandle, Ui, pos2, vec2, Stroke};
use std::time::Duration;

fn fit_px(cell: Rect, aspect: f32, ppp: f32) -> (Rect, [u32; 2]) {
    let aspect = if aspect.is_finite() && aspect > 0.0 { aspect } else { 1.0 };
    let cw = cell.width() * ppp;
    let ch = cell.height() * ppp;
    let (w, h) = if aspect > cw / ch {
        (cw.round(), (cw / aspect).round())
    } else {
        ((ch * aspect).round(), ch.round())
    };
    let (w, h) = (w.max(1.0), h.max(1.0));
    let c = cell.center();
    let min = pos2(
        ((c.x * ppp) - w / 2.0).round() / ppp,
                   ((c.y * ppp) - h / 2.0).round() / ppp,
    );
    (Rect::from_min_size(min, vec2(w, h) / ppp), [w as u32, h as u32])
}

fn cover(cell_px: [i32; 2], aspect: f32) -> ([u32; 2], Rect) {
    let (cw, ch) = (cell_px[0].max(1) as f32, cell_px[1].max(1) as f32);
    let aspect = if aspect.is_finite() && aspect > 0.0 { aspect } else { 1.0 };
    if aspect < cw / ch {
        let h = (cw / aspect).ceil().max(ch);
        let off = ((h - ch) / 2.0).floor();
        ([cw as u32, h as u32], Rect::from_min_max(pos2(0.0, off / h), pos2(1.0, (off + ch) / h)))
    } else {
        let w = (ch * aspect).ceil().max(cw);
        let off = ((w - cw) / 2.0).floor();
        ([w as u32, ch as u32], Rect::from_min_max(pos2(off / w, 0.0), pos2((off + cw) / w, 1.0)))
    }
}

impl GalleryState {
    pub fn show(&mut self, ui: &mut Ui, theme: &Theme) {
        self.poll(ui.ctx());

        if self.scanning {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label(format!("Scanning… {} files", self.scan_progress.load(std::sync::atomic::Ordering::Relaxed)));
            });
            ui.ctx().request_repaint_after(Duration::from_millis(100));
            return;
        }

        // --- Zoom ---
        let ctrl_down = ui.input(|i| i.modifiers.ctrl);
        let mut zoom_delta = 0.0;
        if ctrl_down {
            if ui.input(|i| i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals)) {
                zoom_delta += 20.0;
            }
            if ui.input(|i| i.key_pressed(egui::Key::Minus)) {
                zoom_delta -= 20.0;
            }
            let scroll_y = ui.input(|i| i.raw_scroll_delta.y);
            if scroll_y != 0.0 {
                zoom_delta += scroll_y;
                ui.input_mut(|i| {
                    i.raw_scroll_delta = egui::Vec2::ZERO;
                    i.smooth_scroll_delta = egui::Vec2::ZERO;
                });
            }
            if ui.input(|i| i.pointer.primary_down()) {
                let delta = ui.input(|i| i.pointer.delta());
                zoom_delta += (delta.x + delta.y) * 2.0;
            }
        }
        if zoom_delta != 0.0 {
            self.grid_scale = (self.grid_scale + zoom_delta * 0.002).clamp(0.2, 5.0);
        }

        // --- Filter items by active query ---
        let filtered: Vec<usize> = if matches!(self.active_query, Query::AlwaysTrue) {
            (0..self.items.len()).collect()
        } else {
            self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| self.active_query.matches(&item.tags, item.rating, &self.hierarchy))
            .map(|(i, _)| i)
            .collect()
        };

        // --- Layout ---
        let ppp = ui.ctx().pixels_per_point();
        let available_width = ui.available_width();
        let total_px = (available_width * ppp).round().max(1.0) as i32;
        let m_px = (MARGIN * ppp).round().max(0.0) as i32;
        let h0_px = TARGET_ROW_HEIGHT * ppp * self.grid_scale;
        let query_sig = self.active_query.to_string();

        let key = (total_px, m_px, h0_px.round() as i32, filtered.len(), query_sig);
        if self.layout_cache.as_ref().map(|c| &c.key) != Some(&key) {
            let items: Vec<layout::Item> = filtered
            .iter()
            .map(|&i| {
                let it = &self.items[i];
                layout::Item {
                    aspect: if it.is_folder { 1.0 } else { it.aspect_ratio },
                    area: it.width * it.height,
                    height: it.height,
                    seed: seed_of(&it.path),
                 flexible: it.border_colors.is_flexible(),
                }
            })
            .collect();
            let (cells, total_h) = layout::compute(
                &items,
                &layout::Params { width: total_px, target_h: h0_px, margin: m_px },
            );
            self.layout_cache = Some(LayoutCache { key, cells, total_h });
        }
        let cache = self.layout_cache.take().unwrap();

        let mut filter_batch = crate::filter::FilterBatch::default();

        egui::ScrollArea::vertical()
        .scroll_bar_visibility(ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            let origin = ui.cursor().min;
            let (ox, oy) = ((origin.x * ppp).round() as i32, (origin.y * ppp).round() as i32);
            let visible = ui.clip_rect().expand(600.0);

            for (cell_idx, c) in cache.cells.iter().enumerate() {
                let rect = Rect::from_min_size(
                    pos2((ox + c.x) as f32 / ppp, (oy + c.y) as f32 / ppp),
                                               vec2(c.w as f32, c.h as f32) / ppp,
                );
                if visible.intersects(rect) {
                    let item_idx = filtered[cell_idx];
                    self.draw_item(ui, theme, item_idx, rect, [c.w, c.h], &mut filter_batch);
                }
            }

            ui.allocate_space(vec2(available_width, cache.total_h as f32 / ppp));
        });

        self.layout_cache = Some(cache);
        self.flush_requests();

        self.filter_pass.paint(ui, filter_batch, crate::filter::Filter::Invert);

        if self.grid_scale != 1.0 || ctrl_down {
            let text = format!("Scale: {:.0}%", self.grid_scale * 100.0);
            let font = egui::FontId::monospace(14.0);
            let pos = ui.min_rect().min + vec2(16.0, 16.0);
            let galley = ui.painter().layout_no_wrap(text.clone(), font.clone(), Color32::WHITE);
            let bg_rect = egui::Rect::from_min_size(pos - vec2(8.0, 4.0), galley.size() + vec2(16.0, 8.0));
            ui.painter().rect_filled(bg_rect, 4.0, Color32::from_black_alpha(200));
            ui.painter().rect_stroke(bg_rect, 4.0, Stroke::new(1.0_f32, Color32::from_white_alpha(50)));
            ui.painter().text(pos, egui::Align2::LEFT_TOP, text, font, Color32::WHITE);
        }
    }

    fn draw_item(
        &mut self,
        ui: &mut Ui,
        theme: &Theme,
        item_idx: usize,
        rect: Rect,
        cell_px: [i32; 2],
        batch: &mut crate::filter::FilterBatch,
    ) {
        let item_id = self.items[item_idx].id.clone();
        let path = self.items[item_idx].path.clone();
        let border_colors = self.items[item_idx].border_colors.clone();
        let response = ui.interact(rect, ui.id().with(&item_id), Sense::click());
        let painter = ui.painter();
        let ppp = painter.ctx().pixels_per_point();

        let is_selected = self.selected_images.contains(&path);
        let mut has_image = false;

        let is_image = !self.items[item_idx].is_folder
        && self.items[item_idx].ext != "mp4"
        && self.items[item_idx].ext != "webm"
        && self.items[item_idx].ext != "mkv";

        if is_image && !self.failed.contains(&path) {
            let aspect = self.items[item_idx].aspect_ratio;
            let (tex_px, uv) = cover(cell_px, aspect);
            self.want(&path, tex_px);

            let mut anim_start = std::mem::take(&mut self.anim_start);

            if let Some(t) = self.thumbs.get(&path) {
                let now = ui.input(|i| i.time);
                let tex = match &t.content {
                    ThumbContent::Static(tex) => tex,
                    ThumbContent::Animated { frames, durations } => {
                        let start = *anim_start.entry(path.clone()).or_insert(now);
                        let frame_idx = frame_from_time(durations, now - start);
                        ui.ctx().request_repaint();
                        &frames[frame_idx]
                    }
                };

                let mut draw_rect = rect;
                if border_colors.is_flexible() {
                    let true_aspect = self.items[item_idx].aspect_ratio;
                    let cell_aspect = rect.width() / rect.height();
                    if true_aspect > cell_aspect {
                        let h = rect.width() / true_aspect;
                        draw_rect = Rect::from_center_size(rect.center(), vec2(rect.width(), h));
                    } else {
                        let w = rect.height() * true_aspect;
                        draw_rect = Rect::from_center_size(rect.center(), vec2(w, rect.height()));
                    }

                    if draw_rect.min.y > rect.min.y {
                        if let Some(c) = border_colors.top {
                            if c[3] > 0 {
                                let bleed = Rect::from_min_max(rect.min, pos2(rect.max.x, draw_rect.min.y));
                                painter.rect_filled(bleed, 0.0, Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]));
                            }
                        }
                    }
                    if draw_rect.max.y < rect.max.y {
                        if let Some(c) = border_colors.bottom {
                            if c[3] > 0 {
                                let bleed = Rect::from_min_max(pos2(rect.min.x, draw_rect.max.y), rect.max);
                                painter.rect_filled(bleed, 0.0, Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]));
                            }
                        }
                    }
                    if draw_rect.min.x > rect.min.x {
                        if let Some(c) = border_colors.left {
                            if c[3] > 0 {
                                let bleed = Rect::from_min_max(rect.min, pos2(draw_rect.min.x, rect.max.y));
                                painter.rect_filled(bleed, 0.0, Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]));
                            }
                        }
                    }
                    if draw_rect.max.x < rect.max.x {
                        if let Some(c) = border_colors.right {
                            if c[3] > 0 {
                                let bleed = Rect::from_min_max(pos2(draw_rect.max.x, rect.min.y), rect.max);
                                painter.rect_filled(bleed, 0.0, Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]));
                            }
                        }
                    }
                }

                painter.image(tex.id(), draw_rect, uv, Color32::WHITE);
            } else {
                painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(20, 20, 26, 140));
            }
            has_image = true;

            self.anim_start = anim_start;
        }

        if !has_image {
            let icon_name = if self.items[item_idx].is_folder { "folder".to_string() } else { self.items[item_idx].ext.clone() };
            let icon_paths = [
                theme.dir_path.join("icons").join(format!("{}.webp", icon_name)),
                theme.dir_path.join("icons").join(format!("{}.png", icon_name)),
                theme.dir_path.join("icons").join("file.webp"),
            ];
            let mut icon_drawn = false;
            for p in icon_paths {
                if let Some(tex) = self.load_icon(ui.ctx(), &p.to_string_lossy()) {
                    let tex_size = tex.size_vec2();
                    let tex_aspect = tex_size.x / tex_size.y;
                    let (fitted, _) = fit_px(rect, tex_aspect, ppp);
                    let full_uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                    painter.image(tex.id(), fitted, full_uv, Color32::WHITE);
                    icon_drawn = true;
                    break;
                }
            }
            if !icon_drawn {
                let emoji = if self.items[item_idx].is_folder { "📁" } else { "📄" };
                painter.text(rect.center(), egui::Align2::CENTER_CENTER, emoji, egui::FontId::proportional(48.0), Color32::WHITE);
            }
        }

        if is_selected {
            batch.frame(rect, 3.0, 1.0);
        } else if response.hovered() {
            batch.frame(rect, 1.5, 0.4);
        }

        if response.hovered() {
            let name = self.items[item_idx].title.clone();
            let font = egui::FontId::monospace(14.0);
            let pad = 6.0;
            let max_w = rect.width() - 2.0 * pad;
            let mut truncated = String::new();
            let mut w = 0.0;
            for ch in name.chars() {
                let cw = painter.ctx().fonts(|f| f.glyph_width(&font, ch));
                if w + cw > max_w { break; }
                truncated.push(ch);
                w += cw;
            }
            if !truncated.is_empty() {
                let galley = painter.layout_no_wrap(truncated, font, Color32::WHITE);
                let origin = pos2(rect.min.x + pad, rect.max.y - pad - galley.size().y);
                let atlas = ui.ctx().fonts(|f| f.font_image_size());
                let atlas_size = [atlas[0] as u32, atlas[1] as u32];
                for row in &galley.rows {
                    let row_offset = origin + row.rect.min.to_vec2();
                    batch.text_mesh(&row.visuals.mesh, row_offset.to_vec2(), atlas_size, 1.0);
                }
            }
        }

        if response.clicked() {
            if ui.input(|i| i.modifiers.ctrl) {
                if let Some(pos) = self.selected_images.iter().position(|p| p == &path) {
                    self.selected_images.remove(pos);
                } else {
                    self.selected_images.push(path.clone());
                }
            } else {
                self.selected_images = vec![path.clone()];
                self.viewer_target = Some(path.clone());
            }
        }

        if response.double_clicked() {
            if !self.selected_images.contains(&path) {
                self.selected_images = vec![path.clone()];
            }
            self.viewer_target = Some(path.clone());
            self.request_open_viewer = true;
        }
    }

    fn load_icon(&mut self, ctx: &egui::Context, path: &str) -> Option<TextureHandle> {
        if let Some(cached) = self.icon_cache.get(path) {
            return cached.clone();
        }
        let tex = image::open(path).ok().map(|img| {
            let img = img.to_rgba8();
            let size = [img.width() as usize, img.height() as usize];
            ctx.load_texture(
                path,
                egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()),
                             egui::TextureOptions::LINEAR,
            )
        });
        self.icon_cache.insert(path.to_string(), tex.clone());
        tex
    }
}
