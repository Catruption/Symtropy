use egui::{Color32, RichText, ScrollArea, TextEdit, Ui};
use crate::gallery::{TagNode, Sidecar};

#[derive(Default, Clone)]
pub struct FilterQuery {
    pub artists: String,
    pub tags: String,
    pub alt_text: String,
    pub relationships: String,
}

pub fn draw_sidebar(
    ui: &mut Ui,
    sidebar_rect: egui::Rect,
    is_edit_mode: &mut bool,
    staged_sidecar: &mut Option<Sidecar>,
    filter_query: &mut FilterQuery,
    tag_tree: &TagNode,
    active_tags: &mut std::collections::HashSet<String>,
    search_query: &mut String,
) -> (bool, bool) {
    let mut apply = false;
    let mut revert = false;

    let inner_rect = sidebar_rect.shrink(12.0);
    let mut ui = ui.child_ui(inner_rect, egui::Layout::top_down(egui::Align::Min));
    ui.set_clip_rect(inner_rect);
    ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

    ui.horizontal(|ui| {
        ui.selectable_value(is_edit_mode, false, " FILTER ");
        ui.selectable_value(is_edit_mode, true, " EDIT ");
    });
    ui.separator();

    ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("Revert").clicked() { revert = true; }
            ui.add_space(8.0);
            if ui.button(if *is_edit_mode { "Apply Edits" } else { "Apply Filter" }).clicked() { apply = true; }
        });
        ui.separator();

        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
            let tree_width = 180.0;
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(tree_width, ui.available_height()),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        let search_height = 28.0;
                        let scroll_height = ui.available_height() - search_height - 10.0;

                        ui.scope(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0; 
                            ScrollArea::vertical().auto_shrink([false, false]).max_height(scroll_height).show(ui, |ui| {
                                draw_tag_node(ui, tag_tree, active_tags, *is_edit_mode, staged_sidecar, filter_query, &[]);
                            });
                        });

                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(4.0);
                        ui.add_sized([tree_width, 24.0], TextEdit::singleline(search_query).hint_text("Filter tree..."));
                    },
                );

                ui.separator();

                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), ui.available_height()),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                            if *is_edit_mode {
                                if let Some(sc) = staged_sidecar { draw_edit_properties(ui, sc); } else { ui.label("No sidecar loaded."); }
                            } else {
                                draw_filter_properties(ui, filter_query);
                            }
                        });
                    },
                );
            });
        });
    });
    (apply, revert)
}

fn draw_tag_node(
    ui: &mut Ui, node: &TagNode, active_tags: &mut std::collections::HashSet<String>, 
    is_edit_mode: bool, staged_sidecar: &mut Option<Sidecar>, filter_query: &mut FilterQuery, ancestors: &[String],
) {
    if node.name == "Root" {
        for child in &node.children { draw_tag_node(ui, child, active_tags, is_edit_mode, staged_sidecar, filter_query, &[]); }
        return;
    }

    let mut path = ancestors.to_vec();
    path.push(node.name.clone());

    if !node.children.is_empty() {
        egui::CollapsingHeader::new(&node.name).default_open(false).show(ui, |ui| {
            if !is_edit_mode {
                if ui.small_button(format!("Filter by {}", node.name)).clicked() {
                    if !filter_query.tags.is_empty() && !filter_query.tags.ends_with(' ') { filter_query.tags.push_str(" + "); }
                    filter_query.tags.push_str(&node.name);
                }
                ui.separator();
            }
            for child in &node.children { draw_tag_node(ui, child, active_tags, is_edit_mode, staged_sidecar, filter_query, &path); }
        });
    } else {
        if is_edit_mode {
            let mut checked = staged_sidecar.as_ref().map_or(false, |sc| sc.tags.contains(&node.name));
            if ui.checkbox(&mut checked, RichText::new(&node.name).size(13.0)).changed() {
                if let Some(sc) = staged_sidecar {
                    if checked {
                        if !sc.tags.contains(&node.name) { sc.tags.push(node.name.clone()); }
                        let mut prev = "".to_string();
                        for anc in ancestors {
                            if !sc.tags.contains(anc) { sc.tags.push(anc.clone()); }
                            if !prev.is_empty() { sc.tag_parents.insert(anc.clone(), prev.clone()); }
                            prev = anc.clone();
                        }
                        if let Some(last_anc) = ancestors.last() { sc.tag_parents.insert(node.name.clone(), last_anc.clone()); }
                    } else {
                        sc.tags.retain(|t| t != &node.name);
                        sc.tag_parents.remove(&node.name);
                    }
                }
            }
        } else {
            if ui.selectable_label(false, RichText::new(&node.name).size(13.0)).clicked() {
                if !filter_query.tags.is_empty() && !filter_query.tags.ends_with(' ') { filter_query.tags.push_str(" + "); }
                filter_query.tags.push_str(&node.name);
            }
        }
    }
}

fn draw_edit_properties(ui: &mut Ui, sc: &mut Sidecar) {
    ui.add_space(8.0);
    ui.label(RichText::new("Created: 2026-03-24  [512 x 512]").size(10.0).color(Color32::GRAY));
    ui.add_space(8.0);

    ui.label("Artist");
    let mut artist_str = sc.artists.join(", ");
    if ui.add_sized([ui.available_width(), 20.0], TextEdit::singleline(&mut artist_str)).changed() {
        sc.artists = artist_str.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    }
    ui.add_space(8.0);

    ui.label("Tags");
    let mut tag_str = sc.tags.join(" + ");
    if ui.add_sized([ui.available_width(), 40.0], TextEdit::multiline(&mut tag_str)).changed() {
        sc.tags = tag_str.split(|c| c == '+' || c == '|' || c == '-' || c == ' ').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    }
    ui.add_space(8.0);
}

fn draw_filter_properties(ui: &mut Ui, query: &mut FilterQuery) {
    ui.add_space(8.0);
    ui.label("Artist");
    ui.add_sized([ui.available_width(), 20.0], TextEdit::singleline(&mut query.artists).hint_text("Search artists..."));
    ui.add_space(8.0);

    ui.label("Tags");
    ui.add_sized([ui.available_width(), 40.0], TextEdit::multiline(&mut query.tags).hint_text("tag1 + tag2 | tag3 - tag4"));
    ui.add_space(8.0);
}