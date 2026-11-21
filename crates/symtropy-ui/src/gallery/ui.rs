use super::*;
use crate::actions::{self, Action, ContextTarget};
use egui::scroll_area::ScrollBarVisibility;
use egui::{Color32, Pos2, Rect, Sense, TextureHandle, Ui, pos2, vec2, Stroke};
use std::f32::consts::{PI, TAU};
use std::path::PathBuf;
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

/// Pick the cell above/below `current` with the smallest vertical gap, breaking ties by
/// horizontal distance. The mosaic layout has no fixed columns, so geometry is the only
/// reliable definition of "next row".
fn vertical_neighbor(cells: &[layout::Cell], current: usize, down: bool) -> Option<usize> {
    let cur = cells.get(current)?;
    let cx = cur.x as f32 + cur.w as f32 / 2.0;
    let cy = cur.y as f32 + cur.h as f32 / 2.0;
    let mut best = None;
    for (idx, c) in cells.iter().enumerate() {
        if idx == current {
            continue;
        }
        let x = c.x as f32 + c.w as f32 / 2.0;
        let y = c.y as f32 + c.h as f32 / 2.0;
        let primary = if down { y - cy } else { cy - y };
        if primary <= 1.0 {
            continue;
        }
        let score = primary + (x - cx).abs() * 0.35;
        if best.map_or(true, |(bs, _)| score < bs) {
            best = Some((score, idx));
        }
    }
    best.map(|(_, idx)| idx)
}

type Tri = [Pos2; 3];

fn radial_point(center: Pos2, angle: f32, radius: f32) -> Pos2 {
    center + vec2(angle.cos(), angle.sin()) * radius
}

/// A five-point star split into five point "slices" plus a small core.
fn star_triangles(center: Pos2, outer_r: f32, inner_r: f32) -> ([Tri; 5], [Tri; 5]) {
    let arms: [Tri; 5] = std::array::from_fn(|i| {
        let angle = -PI / 2.0 + i as f32 * TAU / 5.0;
        [
            radial_point(center, angle - PI / 5.0, inner_r),
            radial_point(center, angle, outer_r),
            radial_point(center, angle + PI / 5.0, inner_r),
        ]
    });

    let core: [Tri; 5] = std::array::from_fn(|i| {
        let a = -PI / 2.0 + (i as f32 + 0.5) * TAU / 5.0;
        let b = -PI / 2.0 + (i as f32 + 1.5) * TAU / 5.0;
        [center, radial_point(center, a, inner_r), radial_point(center, b, inner_r)]
    });

    (arms, core)
}

/// Draw an inverted rating star as a mask over a selected thumbnail.
/// Earned points are bright; the remaining points stay as a faint stencil.
fn draw_selection_star(batch: &mut crate::filter::FilterBatch, rect: Rect, rating: u8) {
    let radius = (rect.width().min(rect.height()) * 0.12).clamp(10.0, 22.0);
    let center = rect.min + vec2(radius + 10.0, radius + 10.0);
    let (arms, core) = star_triangles(center, radius, radius * 0.46);
    let rating = rating.min(5);

    for (i, tri) in arms.iter().enumerate() {
        let alpha = if (i as u8) < rating { 0.88 } else { 0.14 };
        batch.triangle(tri[0], tri[1], tri[2], alpha);
    }

    let core_alpha = if rating > 0 { 0.88 } else { 0.14 };
    for tri in &core {
        batch.triangle(tri[0], tri[1], tri[2], core_alpha);
    }
}

/// Add a small always-visible label to the backdrop-filter mask. Text is drawn
/// into the same invert pass as the selection frame, so it stays legible on both
/// light and dark thumbnails without a separate unfiltered overlay.
fn draw_mask_text(
    batch: &mut crate::filter::FilterBatch,
    painter: &egui::Painter,
    text: String,
    origin: Pos2,
    font: &egui::FontId,
    atlas_size: [u32; 2],
    alpha: f32,
) {
    let galley = painter.layout_no_wrap(text, font.clone(), Color32::WHITE);
    for row in &galley.rows {
        let row_offset = origin + row.rect.min.to_vec2();
        batch.text_mesh(&row.visuals.mesh, row_offset.to_vec2(), atlas_size, alpha);
    }
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
    pub fn show(
        &mut self,
        ui: &mut Ui,
        dir: &Path,
        viewer_open: bool,
        dialog_open: bool,
        options: actions::MenuOptions,
        actions: &mut Vec<Action>,
    ) {
        self.poll(ui.ctx());

        if self.scanning && self.items.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                let n = self.scan_progress.load(std::sync::atomic::Ordering::Relaxed);
                ui.label(format!("Scanning… {} files", n));
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("Phase 1: reading headers & sidecars (fast)")
                    .size(11.0)
                    .color(Color32::from_rgb(140, 140, 150)),
                );
            });
            ui.ctx().request_repaint_after(Duration::from_millis(100));
            return;
        }

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

        let gallery_rect = ui.available_rect_before_wrap();
        self.handle_keyboard_navigation(ui, &filtered, &cache, gallery_rect, dialog_open, viewer_open, actions);

        let mut filter_batch = crate::filter::FilterBatch::default();

        // egui's built-in drag-to-scroll also reacts to the secondary button.
        // Turn it off and scroll on primary drags ourselves so right-drag is a
        // pure file operation.
        let scroll_rect = gallery_rect;
        if !ctrl_down {
            actions::primary_drag_scroll(ui, scroll_rect, &mut self.scroll_offset);
        }
        let scroll = egui::ScrollArea::vertical()
        .drag_to_scroll(false)
        .vertical_scroll_offset(self.scroll_offset)
        .scroll_bar_visibility(ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            // Lowest layer: right-clicking empty space gets a directory menu.
            // Item interactions are registered after this and win on overlap.
            let bg_resp = ui.interact(ui.clip_rect(), ui.id().with("gallery_bg"), Sense::click());
            let bg_dir = dir.to_path_buf();
            actions::show_menu(
                ui,
                &bg_resp,
                move || ContextTarget::Background(bg_dir),
                options,
                actions,
            );

            let origin = ui.cursor().min;
            let (ox, oy) = ((origin.x * ppp).round() as i32, (origin.y * ppp).round() as i32);
            let visible = ui.clip_rect().expand(600.0);
            let ordered_paths: Vec<String> = filtered
                .iter()
                .map(|&i| self.items[i].path.clone())
                .collect();

            for (cell_idx, c) in cache.cells.iter().enumerate() {
                let rect = Rect::from_min_size(
                    pos2((ox + c.x) as f32 / ppp, (oy + c.y) as f32 / ppp),
                                               vec2(c.w as f32, c.h as f32) / ppp,
                );
                if visible.intersects(rect) {
                    let item_idx = filtered[cell_idx];
                    self.draw_item(ui, item_idx, rect, [c.w, c.h], &mut filter_batch, viewer_open, options, &ordered_paths, actions);
                }
            }

            // Runs after the items so a drop on a folder wins over the background.
            actions::drop_on_directory(ui, &bg_resp, dir, actions);

            ui.allocate_space(vec2(available_width, cache.total_h as f32 / ppp));
        });
        self.scroll_offset = scroll.state.offset.y;

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

    /// Arrow/Home/End/Enter navigation for the mosaic gallery. Focus follows the real cell
    /// rectangles, so it works with the irregular block layout instead of assuming a grid.
    fn handle_keyboard_navigation(
        &mut self,
        ui: &mut Ui,
        filtered: &[usize],
        cache: &LayoutCache,
        viewport: Rect,
        dialog_open: bool,
        viewer_open: bool,
        actions: &mut Vec<Action>,
    ) {
        if dialog_open || viewer_open || filtered.is_empty() || ui.memory(|m| m.focused()).is_some() {
            return;
        }

        use egui::Key;
        let (right, left, down, up, home, end, enter, shift) = ui.input(|i| {
            (
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::Home),
                i.key_pressed(Key::End),
                i.key_pressed(Key::Enter),
                i.modifiers.shift,
            )
        });
        if !(right || left || down || up || home || end || enter) {
            return;
        }

        let paths: Vec<String> = filtered.iter().map(|&i| self.items[i].path.clone()).collect();
        let current = self
            .keyboard_focus
            .as_ref()
            .or_else(|| self.selected_images.first())
            .and_then(|p| paths.iter().position(|q| q == p))
            .unwrap_or(0);

        let target = if home {
            0
        } else if end {
            paths.len() - 1
        } else if right {
            (current + 1).min(paths.len() - 1)
        } else if left {
            current.saturating_sub(1)
        } else if up || down {
            vertical_neighbor(&cache.cells, current, down).unwrap_or(current)
        } else {
            current
        };

        let path = paths[target].clone();
        self.keyboard_focus = Some(path.clone());
        if shift {
            actions::select_range(&mut self.selected_images, &mut self.selection_anchor, &paths, path.clone(), false);
        } else if enter || !self.selected_images.contains(&path) {
            self.selected_images = vec![path.clone()];
            self.selection_anchor = Some(path.clone());
        }

        // Keep the focused cell on screen.
        let ppp = ui.ctx().pixels_per_point();
        let c = &cache.cells[target];
        let top = c.y as f32 / ppp;
        let bottom = top + c.h as f32 / ppp;
        let margin = 32.0;
        if top < self.scroll_offset + margin {
            self.scroll_offset = (top - margin).max(0.0);
        } else if bottom > self.scroll_offset + viewport.height() - margin {
            self.scroll_offset = bottom - viewport.height() + margin;
        }

        if enter {
            if self.items[filtered[target]].is_folder {
                actions.push(Action::Navigate(PathBuf::from(&path)));
            } else {
                self.viewer_target = Some(path);
                self.request_open_viewer = true;
            }
        }
    }

    fn draw_item(
        &mut self,
        ui: &mut Ui,
        item_idx: usize,
        rect: Rect,
        cell_px: [i32; 2],
        batch: &mut crate::filter::FilterBatch,
        viewer_open: bool,
        options: actions::MenuOptions,
        ordered_paths: &[String],
        actions: &mut Vec<Action>,
    ) {
        let item_id = self.items[item_idx].id.clone();
        let path = self.items[item_idx].path.clone();
        let border_colors = self.items[item_idx].border_colors.clone();
        let response = ui.interact(rect, ui.id().with(&item_id), Sense::click());
        let painter = ui.painter();
        let ppp = painter.ctx().pixels_per_point();

        let is_selected = self.selected_images.contains(&path);
        let mut has_image = false;

        let is_video = workers::is_video_ext(&self.items[item_idx].ext);
        let is_image = !self.items[item_idx].is_folder;

        if is_image && !self.failed.contains(&path) {
            let aspect = self.items[item_idx].aspect_ratio;
            let (tex_px, uv) = cover(cell_px, aspect);
            self.want(&path, tex_px, workers::video_duration(&self.items[item_idx].sidecar));

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
            let mut icon_drawn = false;
            if let Some(bytes) = crate::theme::Theme::icon_bytes(&icon_name) {
                if let Some(tex) = self.load_icon(ui.ctx(), &icon_name, bytes) {
                    let tex_size = tex.size_vec2();
                    let tex_aspect = tex_size.x / tex_size.y;
                    let (fitted, _) = fit_px(rect, tex_aspect, ppp);
                    let full_uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                    painter.image(tex.id(), fitted, full_uv, Color32::WHITE);
                    icon_drawn = true;
                }
            }
            if !icon_drawn {
                let emoji = if self.items[item_idx].is_folder { "📁" } else { "📄" };
                painter.text(rect.center(), egui::Align2::CENTER_CENTER, emoji, egui::FontId::proportional(48.0), Color32::WHITE);
            }
        }

        if is_selected {
            batch.frame(rect, 3.0, 1.0);
            if !self.items[item_idx].is_folder {
                draw_selection_star(batch, rect, self.items[item_idx].rating);
            }
        } else if response.hovered() {
            batch.frame(rect, 1.5, 0.4);
        }

        // Compact badges are always present so rating, tag count and video length can be
        // scanned without hovering. The full filename stays hover-only to avoid clutter.
        if is_image && rect.width() >= 64.0 && rect.height() >= 48.0 {
            let font = egui::FontId::monospace(11.0);
            let pad = 5.0;
            let atlas = ui.ctx().fonts(|f| f.font_image_size());
            let atlas_size = [atlas[0] as u32, atlas[1] as u32];
            let char_w = painter.ctx().fonts(|f| f.glyph_width(&font, 'M'));
            let item = &self.items[item_idx];

            if item.rating > 0 {
                let text = format!("★{}", item.rating);
                draw_mask_text(batch, painter, text, pos2(rect.min.x + pad, rect.min.y + pad), &font, atlas_size, 0.95);
            }
            if !item.tags.is_empty() {
                let text = format!("#{}", item.tags.len());
                let origin = pos2(rect.max.x - pad - char_w * text.chars().count() as f32, rect.min.y + pad);
                draw_mask_text(batch, painter, text, origin, &font, atlas_size, 0.85);
            }
            if is_video {
                let text = format!("▶ {}", workers::format_duration(workers::video_duration(&item.sidecar)));
                let origin = pos2(
                    rect.max.x - pad - char_w * text.chars().count() as f32,
                    rect.max.y - pad - font.size,
                );
                draw_mask_text(batch, painter, text, origin, &font, atlas_size, 0.95);
            }
        }

        if response.hovered() || viewer_open {
            let font = egui::FontId::monospace(14.0);
            let pad = 6.0;
            let atlas = ui.ctx().fonts(|f| f.font_image_size());
            let atlas_size = [atlas[0] as u32, atlas[1] as u32];

            // Filename at bottom-left
            let name = self.items[item_idx].title.clone();
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
                let galley = painter.layout_no_wrap(truncated, font.clone(), Color32::WHITE);
                let origin = pos2(rect.min.x + pad, rect.max.y - pad - galley.size().y);
                for row in &galley.rows {
                    let row_offset = origin + row.rect.min.to_vec2();
                    batch.text_mesh(&row.visuals.mesh, row_offset.to_vec2(), atlas_size, 1.0);
                }
            }
        }

        match actions::classify(ui, &response) {
            Some(actions::Click::Primary) => {
                self.keyboard_focus = Some(path.clone());
                let modifiers = ui.input(|i| i.modifiers);
                let toggle = modifiers.ctrl || modifiers.alt;
                if modifiers.shift {
                    actions::select_range(
                        &mut self.selected_images,
                        &mut self.selection_anchor,
                        ordered_paths,
                        path.clone(),
                        toggle,
                    );
                } else if toggle {
                    self.selection_anchor = Some(path.clone());
                    if let Some(pos) = self.selected_images.iter().position(|p| p == &path) {
                        self.selected_images.remove(pos);
                    } else {
                        self.selected_images.push(path.clone());
                    }
                } else {
                    self.selection_anchor = Some(path.clone());
                    self.selected_images = vec![path.clone()];
                    self.viewer_target = Some(path.clone());
                }
            }
            Some(actions::Click::Double) => {
                self.keyboard_focus = Some(path.clone());
                if !self.selected_images.contains(&path) {
                    self.selected_images = vec![path.clone()];
                }
                self.viewer_target = Some(path.clone());
                self.request_open_viewer = true;
            }
            Some(actions::Click::Secondary) => {
                self.keyboard_focus = Some(path.clone());
                if !self.selected_images.contains(&path) {
                    self.selection_anchor = Some(path.clone());
                    self.selected_images = vec![path.clone()];
                }
            }
            Some(actions::Click::Drag) => {
                self.keyboard_focus = Some(path.clone());
                let paths: Vec<PathBuf> = if self.selected_images.contains(&path) && self.selected_images.len() > 1 {
                    self.selected_images.iter().map(PathBuf::from).collect()
                } else {
                    vec![PathBuf::from(&path)]
                };
                actions::begin_drag(ui, paths);
            }
            None => {}
        }

        let target_paths: Vec<PathBuf> = if self.selected_images.contains(&path) && self.selected_images.len() > 1 {
            self.selected_images.iter().map(PathBuf::from).collect()
        } else {
            vec![PathBuf::from(&path)]
        };
        let target = ContextTarget::Entries(target_paths);

        if self.items[item_idx].is_folder {
            actions::paint_drop_target(ui, &response);
            if let Some(payload) = actions::take_drag(ui, &response) {
                actions.push(Action::MoveFiles {
                    from: payload.paths,
                    to: PathBuf::from(&path),
                    copy: payload.copy,
                });
            }
        }

        actions::show_menu(ui, &response, move || target, options, actions);
    }

    fn load_icon(&mut self, ctx: &egui::Context, name: &str, bytes: &[u8]) -> Option<TextureHandle> {
        if let Some(cached) = self.icon_cache.get(name) {
            return cached.clone();
        }
        let tex = image::load_from_memory(bytes).ok().map(|img| {
            let img = img.to_rgba8();
            let size = [img.width() as usize, img.height() as usize];
            ctx.load_texture(
                name,
                egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()),
                egui::TextureOptions::LINEAR,
            )
        });
        self.icon_cache.insert(name.to_string(), tex.clone());
        tex
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_is_five_point_slices_plus_core() {
        let center = pos2(50.0, 50.0);
        let (arms, core) = star_triangles(center, 20.0, 9.0);
        assert_eq!(arms.len(), 5);
        assert_eq!(core.len(), 5);

        // The first point starts at the top.
        assert!(arms[0][1].y < center.y);
        for tri in &core {
            assert_eq!(tri[0], center);
        }
    }

    #[test]
    fn selection_star_adds_ten_triangles_to_filter_mask() {
        let mut batch = crate::filter::FilterBatch::default();
        draw_selection_star(&mut batch, Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0)), 3);
        assert_eq!(batch.len(), 10 * 3);
    }

    fn cell(x: i32, y: i32, w: i32, h: i32) -> layout::Cell {
        layout::Cell { x, y, w, h }
    }

    #[test]
    fn vertical_navigation_prefers_the_closest_row() {
        let cells = [
            cell(0, 0, 100, 100),    // current
            cell(120, 0, 100, 100),  // same row
            cell(0, 120, 100, 100),  // nearest below
            cell(400, 400, 100, 100), // far below
        ];
        assert_eq!(vertical_neighbor(&cells, 0, true), Some(2));
        assert_eq!(vertical_neighbor(&cells, 2, false), Some(0));
    }

    #[test]
    fn vertical_navigation_breaks_ties_by_horizontal_distance() {
        let cells = [
            cell(0, 0, 100, 100),    // current
            cell(300, 120, 100, 100), // right candidate
            cell(20, 120, 100, 100),  // left candidate, nearly aligned
        ];
        assert_eq!(vertical_neighbor(&cells, 0, true), Some(2));
        assert_eq!(vertical_neighbor(&cells, 0, false), None);
    }
}
