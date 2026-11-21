use egui::{Color32, RichText, ScrollArea, TextEdit, Ui};
use crate::gallery::{TagNode, Sidecar};
use std::path::Path;

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct FilterPreset {
    pub name: String,
    pub query: String,
}

#[derive(Default, Clone)]
pub struct FilterQuery {
    pub artists: String,
    pub tags: String,
    pub alt_text: String,
    pub relationships: String,
    pub rating_min: Option<u8>,
    /// Full query-language expression. When set, it takes precedence over the structured fields.
    pub raw_query: String,
    pub preset_name: String,
}

#[derive(Clone, Debug)]
pub enum SidebarAction {
    ApplyQuery(String),
    SavePreset { name: String, query: String },
}

/// Draw a 5-star widget. Returns Some(new_value) if the user clicked a star.
/// Clicking the currently-selected star clears it back to 0.
fn draw_stars(ui: &mut Ui, current: u8) -> Option<u8> {
    let mut result = None;
    ui.horizontal(|ui| {
        for i in 1..=5u8 {
            let filled = current >= i;
            let text = if filled { "★" } else { "☆" };
            let color = if filled {
                Color32::from_rgb(230, 200, 80)
            } else {
                Color32::from_rgb(120, 120, 130)
            };
            let btn = egui::Button::new(RichText::new(text).size(20.0).color(color)).frame(false);
            if ui.add(btn).clicked() {
                result = Some(if current == i { 0 } else { i });
            }
        }
    });
    result
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
    metadata_path: Option<&Path>,
    query_history: &[String],
    presets: &[FilterPreset],
    query_error: Option<&str>,
) -> (bool, bool, Vec<SidebarAction>) {
    let mut apply = false;
    let mut revert = false;
    let mut actions = Vec::new();

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
                                if let Some(sc) = staged_sidecar {
                                    draw_edit_properties(ui, sc, metadata_path);
                                } else {
                                    ui.label("No sidecar loaded.");
                                }
                            } else {
                                draw_filter_properties(ui, filter_query, query_history, presets, query_error, &mut actions);
                            }
                        });
                    },
                );
            });
        });
    });
    (apply, revert, actions)
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

fn labeled_lines(ui: &mut Ui, label: &str, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    ui.label(RichText::new(label).strong());
    for line in lines {
        ui.label(RichText::new(line).size(11.0).color(Color32::LIGHT_GRAY));
    }
    ui.add_space(4.0);
}

fn draw_edit_properties(ui: &mut Ui, sc: &mut Sidecar, path: Option<&Path>) {
    ui.add_space(8.0);
    if let Some(path) = path {
        let meta = std::fs::metadata(path).ok();
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        ui.label(RichText::new("FILE").strong());
        ui.label(RichText::new(path.display().to_string()).size(11.0).color(Color32::LIGHT_GRAY));
        ui.label(RichText::new(format!("{} bytes", size)).size(10.0).color(Color32::GRAY));
        ui.separator();
    }

    let dimensions = match (sc.width, sc.height) {
        (Some(w), Some(h)) => format!("{} × {}", w, h),
        _ => "unknown".to_string(),
    };
    ui.label(RichText::new(format!("Dimensions: {}", dimensions)).size(10.0).color(Color32::GRAY));
    if sc.is_animated == Some(true) {
        ui.label(RichText::new("Animated").size(10.0).color(Color32::GRAY));
    }
    ui.add_space(8.0);

    ui.label("Rating");
    if let Some(new_rating) = draw_stars(ui, sc.rating) {
        sc.rating = new_rating;
    }
    ui.add_space(8.0);

    ui.label("Alt text");
    ui.add_sized([ui.available_width(), 56.0], TextEdit::multiline(&mut sc.alt_text));
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

    ui.collapsing("Properties", |ui| {
        let mut props = sc.properties.iter()
            .map(|(key, value)| format!("{}={}", key, value))
            .collect::<Vec<_>>()
            .join("\n");
        if ui.add_sized([ui.available_width(), 72.0], TextEdit::multiline(&mut props).hint_text("key=value")).changed() {
            sc.properties.clear();
            for line in props.lines() {
                if let Some((key, value)) = line.split_once('=') {
                    let key = key.trim();
                    if !key.is_empty() {
                        sc.properties.insert(key.to_string(), value.trim().to_string());
                    }
                }
            }
        }
    });

    ui.collapsing("Sources", |ui| {
        let sources: Vec<String> = sc.sources.iter().map(|source| {
            let mut parts = Vec::new();
            for (label, value) in [
                ("title", source.title.as_str()),
                ("site", source.site.as_str()),
                ("url", source.url.as_str()),
                ("captured", source.captured_at.as_str()),
                ("description", source.description.as_str()),
            ] {
                if !value.is_empty() {
                    parts.push(format!("{}: {}", label, value));
                }
            }
            parts.join(" · ")
        }).collect();
        labeled_lines(ui, "Sources", &sources);
    });

    ui.collapsing("Relationships", |ui| {
        let relationships: Vec<String> = sc.relationships.iter()
            .map(|r| format!("{} {} {}", r.subject, r.predicate, r.object))
            .collect();
        labeled_lines(ui, "Relationships", &relationships);
    });

    ui.collapsing("Tag parents", |ui| {
        let parents: Vec<String> = sc.tag_parents.iter()
            .map(|(child, parent)| format!("{} → {}", child, parent))
            .collect();
        labeled_lines(ui, "Tag parents", &parents);
    });

    ui.collapsing("Cached analysis", |ui| {
        if let Some(margins) = &sc.margins {
            ui.label(RichText::new(format!("margins: {} {} {} {}", margins.top, margins.bottom, margins.left, margins.right)).size(10.0).color(Color32::GRAY));
        }
        if let Some(colors) = &sc.border_colors {
            ui.label(RichText::new(format!("edges: {}", colors.is_flexible())).size(10.0).color(Color32::GRAY));
        }
        ui.label(RichText::new("Regenerated by the scanner; read-only.").size(10.0).color(Color32::GRAY));
    });
}

fn draw_filter_properties(
    ui: &mut Ui,
    query: &mut FilterQuery,
    history: &[String],
    presets: &[FilterPreset],
    query_error: Option<&str>,
    actions: &mut Vec<SidebarAction>,
) {
    ui.add_space(8.0);

    if let Some(error) = query_error {
        ui.label(RichText::new(format!("Query error: {}", error)).color(Color32::from_rgb(255, 120, 120)).size(11.0));
        ui.add_space(4.0);
    }

    ui.label(RichText::new("Full query").strong());
    ui.add_sized([ui.available_width(), 60.0], TextEdit::multiline(&mut query.raw_query).hint_text("tag + rating:4+ - excluded"));
    ui.horizontal(|ui| {
        if ui.button("Apply Query").clicked() && !query.raw_query.trim().is_empty() {
            actions.push(SidebarAction::ApplyQuery(query.raw_query.trim().to_string()));
        }
        if ui.button("Save Preset").clicked()
            && !query.raw_query.trim().is_empty()
            && !query.preset_name.trim().is_empty()
        {
            actions.push(SidebarAction::SavePreset {
                name: query.preset_name.trim().to_string(),
                query: query.raw_query.trim().to_string(),
            });
        }
    });
    ui.add_sized([ui.available_width(), 20.0], TextEdit::singleline(&mut query.preset_name).hint_text("Preset name"));
    ui.add_space(8.0);

    if !presets.is_empty() {
        ui.label(RichText::new("Presets").strong());
        for preset in presets.iter().rev().take(8) {
            if ui.small_button(&preset.name).clicked() {
                actions.push(SidebarAction::ApplyQuery(preset.query.clone()));
            }
        }
        ui.add_space(4.0);
    }

    if !history.is_empty() {
        ui.label(RichText::new("Recent queries").strong());
        for saved in history.iter().take(8) {
            let label = if saved.len() > 32 { format!("{}…", &saved[..32]) } else { saved.clone() };
            if ui.small_button(label).clicked() {
                actions.push(SidebarAction::ApplyQuery(saved.clone()));
            }
        }
        ui.add_space(4.0);
    }

    ui.separator();
    ui.label("Tags");
    ui.add_sized([ui.available_width(), 40.0], TextEdit::multiline(&mut query.tags).hint_text("tag1 + tag2 | tag3 - tag4"));
    ui.add_space(8.0);

    ui.label("Minimum Rating");
    let cur = query.rating_min.unwrap_or(0);
    if let Some(new_rating) = draw_stars(ui, cur) {
        query.rating_min = if new_rating == 0 { None } else { Some(new_rating) };
    }
    if let Some(min_r) = query.rating_min {
        ui.label(RichText::new(format!("filtering: ≥ {} stars", min_r)).size(10.0).color(Color32::GRAY));
    }
    ui.add_space(8.0);

    ui.label("Artist");
    ui.add_sized([ui.available_width(), 20.0], TextEdit::singleline(&mut query.artists).hint_text("Search artists..."));
    ui.add_space(8.0);

    ui.label("Alt text");
    ui.add_sized([ui.available_width(), 40.0], TextEdit::multiline(&mut query.alt_text).hint_text("Search description..."));
    ui.add_space(8.0);
}
