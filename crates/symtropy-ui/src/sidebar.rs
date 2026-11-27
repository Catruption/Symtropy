use egui::{Color32, Id, Key, Response, RichText, ScrollArea, TextEdit, Ui};
use crate::actions::{self, Action, TagDialog, TagMenuTarget};
use crate::gallery::{ClaimScope, ConfidenceRange, Query, TagNode, Sidecar};
use std::path::Path;

/// The FILTER side of the sidebar. Only curated tag-tree/DSL queries are
/// authored here. AI prompt searches live in Tag Manager and arrive here as an
/// already-compiled query carrying a machine claim scope.
#[derive(Clone)]
pub struct FilterQuery {
    pub filter: String,
    pub claim_scope: ClaimScope,
    pub rating_min: Option<u8>,
    pub confidence_min: u8,
    pub confidence_max: u8,
}

impl Default for FilterQuery {
    fn default() -> Self {
        Self {
            filter: String::new(),
            claim_scope: ClaimScope::Curated,
            rating_min: None,
            confidence_min: 100,
            confidence_max: 100,
        }
    }
}

impl FilterQuery {
    pub fn confidence_range(&self) -> ConfidenceRange {
        ConfidenceRange::new(self.confidence_min, self.confidence_max)
    }

    /// Compile the exact tag DSL. The rating control is an additional AND term.
    pub fn compile(&self, _tag_tree: &TagNode) -> Result<Query, String> {
        let base = Query::parse(&self.filter)?;
        Ok(match self.rating_min {
            Some(min) => match base {
                Query::AlwaysTrue => Query::Rating { min, exact: false },
                other => Query::And(vec![other, Query::Rating { min, exact: false }]),
            },
            None => base,
        })
    }

    pub fn compile_for_apply(&self, tag_tree: &TagNode) -> Result<Query, String> {
        self.compile(tag_tree)
    }

    fn normalize_confidence(&mut self) {
        let range = self.confidence_range();
        self.confidence_min = range.min;
        self.confidence_max = range.max;
    }
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

/// Flatten the taxonomy into the names used by the tag tree.
pub(crate) fn tag_catalog(root: &TagNode) -> Vec<String> {
    fn walk(node: &TagNode, out: &mut Vec<String>) {
        if node.name != "Root" {
            out.push(node.name.clone());
        }
        for child in &node.children {
            walk(child, out);
        }
    }

    let mut tags = Vec::new();
    walk(root, &mut tags);
    tags.sort();
    tags.dedup();
    tags
}

fn find_tag_node<'a>(root: &'a TagNode, key: &str) -> Option<&'a TagNode> {
    if root.name != "Root" && query_token(&root.name) == key {
        return Some(root);
    }
    root.children.iter().find_map(|child| find_tag_node(child, key))
}

fn query_tag_ids(query: &Query, out: &mut Vec<String>) {
    match query {
        Query::Tag(tag) => out.push(tag.clone()),
        Query::Not(inner) => query_tag_ids(inner, out),
        Query::And(parts) | Query::Or(parts) | Query::Except(parts) => {
            for part in parts {
                query_tag_ids(part, out);
            }
        }
        Query::Relation(relation) => {
            if let crate::gallery::query::QueryEntity::Tag(tag) = &relation.object {
                out.push(tag.clone());
            }
        }
        Query::Rating { .. } | Query::AlwaysTrue => {}
    }
}

/// Parent tags intentionally match their children. Say so before the user
/// wonders why a file tagged only `whitestorm` appears under `thunderclan`.
fn show_tag_descendant_hint(ui: &mut Ui, tag_tree: &TagNode, tags: &[String]) {
    let mut shown = std::collections::HashSet::new();
    for tag in tags {
        let key = query_token(tag);
        if !shown.insert(key.clone()) {
            continue;
        }
        if let Some(node) = find_tag_node(tag_tree, &key) {
            if !node.children.is_empty() {
                let children = node.children.iter().map(|child| child.name.clone()).collect::<Vec<_>>().join(", ");
                ui.label(
                    RichText::new(format!("\"{}\" also matches child tags: {}", node.name, children))
                        .size(10.0)
                        .color(Color32::GRAY),
                );
            }
        }
    }
}

fn active_query_token(input: &str) -> (usize, String) {
    let is_token_char = |c: char| c.is_alphanumeric() || matches!(c, '_' | ':' | '.' | '-' | '%');
    let start = input
        .char_indices()
        .rev()
        .find_map(|(idx, c)| if is_token_char(c) { None } else { Some(idx + c.len_utf8()) })
        .unwrap_or(0);
    (start, input[start..].to_string())
}

fn active_editor_segment(input: &str) -> (usize, String) {
    let start = input.rfind('+').map_or(0, |idx| idx + 1);
    (start, input[start..].trim().to_string())
}

fn tag_suggestions(catalog: &[String], token: &str, query_mode: bool, exclude: &[String]) -> Vec<String> {
    let needle = if query_mode {
        query_token(token)
    } else {
        token.trim().to_string()
    }
    .to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }

    let mut scored: Vec<(u8, usize, String)> = catalog
        .iter()
        .filter_map(|tag| {
            let candidate = if query_mode { query_token(tag) } else { tag.clone() };
            if exclude.iter().any(|used| used == &candidate) {
                return None;
            }
            let lower = candidate.to_lowercase();
            let score = if lower == needle {
                0
            } else if lower.starts_with(&needle) {
                1
            } else if lower.contains(&needle) {
                2
            } else {
                return None;
            };
            Some((score, candidate.len(), candidate))
        })
        .collect();
    scored.sort();
    scored.into_iter().take(6).map(|(_, _, tag)| tag).collect()
}

fn replace_query_token(text: &mut String, start: usize, suggestion: &str) {
    text.truncate(start);
    text.push_str(suggestion);
}

fn replace_editor_segment(text: &mut String, start: usize, suggestion: &str) {
    let mut prefix = text[..start].trim_end().to_string();
    if prefix.ends_with('+') {
        prefix.push(' ');
    } else if !prefix.is_empty() {
        prefix.push_str(" + ");
    }
    *text = format!("{}{}", prefix, suggestion);
}

/// Grey suggestion list under a tag field. Tab or click accepts the first match.
pub(crate) fn show_tag_suggestions(
    ui: &Ui,
    popup_id: Id,
    response: &Response,
    text: &mut String,
    catalog: &[String],
    query_mode: bool,
    exclude: &[String],
) {
    let (start, token) = if query_mode {
        active_query_token(text)
    } else {
        active_editor_segment(text)
    };
    let suggestions = tag_suggestions(catalog, &token, query_mode, exclude);
    let focused = response.has_focus();

    if focused && !suggestions.is_empty() {
        ui.memory_mut(|m| m.open_popup(popup_id));
    } else {
        ui.memory_mut(|m| {
            if m.is_popup_open(popup_id) {
                m.close_popup();
            }
        });
    }

    let mut picked = None;
    if focused && ui.input(|i| i.key_pressed(Key::Tab)) {
        picked = suggestions.first().cloned();
    }

    let clicked = egui::popup_below_widget(ui, popup_id, response, |ui| {
        for tag in &suggestions {
            let button = egui::Button::new(
                RichText::new(tag.clone()).color(Color32::from_gray(175)).size(12.0),
            )
            .frame(false);
            if ui.add(button).clicked() {
                return Some(tag.clone());
            }
        }
        None
    })
    .flatten();

    if let Some(tag) = picked.or(clicked) {
        if query_mode {
            replace_query_token(text, start, &tag);
        } else {
            replace_editor_segment(text, start, &tag);
        }
    }
}

pub fn draw_sidebar(
    ui: &mut Ui,
    sidebar_rect: egui::Rect,
    is_edit_mode: &mut bool,
    staged_sidecar: &mut Option<Sidecar>,
    filter_query: &mut FilterQuery,
    tag_tree: &TagNode,
    search_query: &mut String,
    show_preserved: &mut bool,
    metadata_path: Option<&Path>,
    query_error: Option<&str>,
    scope_label: &str,
    actions: &mut Vec<Action>,
) -> (bool, bool) {
    let mut apply = false;
    let mut revert = false;

    let inner_rect = sidebar_rect.shrink(12.0);
    let mut ui = ui.child_ui(inner_rect, egui::Layout::top_down(egui::Align::Min));
    ui.set_clip_rect(inner_rect);
    ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

    ui.horizontal(|ui| {
        if ui.selectable_label(!*is_edit_mode, " FILTER ").clicked() {
            *is_edit_mode = false;
        }
        if ui.selectable_label(*is_edit_mode, " EDIT ").clicked() {
            *is_edit_mode = true;
        }
        ui.separator();
        if ui.small_button("TAG MANAGER ↗").on_hover_text(
            "Open the native Tag Manager window",
        ).clicked() {
            actions.push(Action::OpenTagManager(None));
        }
    });
    ui.separator();

    ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("Revert").clicked() { revert = true; }
            ui.add_space(8.0);
            if ui.button(if *is_edit_mode { "Apply Edits" } else { "Apply Filter" }).clicked() {
                apply = true;
            }
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
                        let add_height = 24.0;
                        let scroll_height =
                            (ui.available_height() - search_height - add_height - 14.0).max(40.0);

                        ui.label(
                            RichText::new(if *is_edit_mode {
                                "All known tags · checked = in this file's sidecar"
                            } else {
                                "All known tags · selected = filter criteria"
                            })
                            .size(10.0)
                            .color(Color32::GRAY),
                        );
                        ui.horizontal(|ui| {
                            if ui.small_button("＋ New top-level tag").clicked() {
                                actions.push(Action::TagDialog(TagDialog::New { parent: None }));
                            }
                            if ui
                                .small_button("show preserved")
                                .on_hover_text("Include tags kept in the sidecar but hidden from the active index")
                                .clicked()
                            {
                                *show_preserved = !*show_preserved;
                            }
                        });
                        if *show_preserved {
                            ui.label(
                                RichText::new("Preserved tags are shown for inspection only.")
                                    .size(10.0)
                                    .color(Color32::GRAY),
                            );
                        }

                        ui.scope(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            let selected_tags = if *is_edit_mode {
                                std::collections::HashSet::new()
                            } else {
                                selected_filter_tokens(filter_query)
                            };
                            let search = search_query.trim().to_lowercase();
                            ScrollArea::vertical().auto_shrink([false, false]).max_height(scroll_height).show(ui, |ui| {
                                draw_tag_node(
                                    ui, tag_tree, *is_edit_mode, staged_sidecar, filter_query,
                                    &[], &mut apply, &search, &selected_tags, actions,
                                );
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
                                    draw_edit_properties(ui, sc, metadata_path, tag_tree);
                                } else {
                                    ui.label("No sidecar loaded.");
                                }
                            } else {
                                draw_filter_properties(ui, filter_query, query_error, tag_tree, scope_label, &mut revert);
                            }
                        });
                    },
                );
            });
        });
    });
    (apply, revert)
}

/// One canonical key shared by the tree, suggestions, and the query matcher.
fn query_token(tag: &str) -> String {
    crate::gallery::hierarchy::tag_key(tag)
}

/// Rewrite an exact DSL token when a tag is renamed or removed.
pub fn replace_filter_text(filter: &str, from: &str, to: Option<&str>) -> String {
    let from_key = crate::gallery::hierarchy::tag_key(from);
    let to_key = to.map(crate::gallery::hierarchy::tag_key);
    replace_filter_token(filter, &from_key, to_key.as_deref())
}

/// Rewrite a whole token in a filter expression. `from` is a normalized tag key;
/// `None` removes the tag. Operators and unrelated tokens are left intact.
pub fn replace_filter_token(filter: &str, from: &str, to: Option<&str>) -> String {
    let is_token_char = |c: char| c.is_alphanumeric() || matches!(c, '_' | ':' | '.' | '-' | '%');
    let chars: Vec<char> = filter.chars().collect();
    let mut out = String::with_capacity(filter.len());
    let mut i = 0;
    while i < chars.len() {
        if is_token_char(chars[i]) {
            let start = i;
            while i < chars.len() && is_token_char(chars[i]) {
                i += 1;
            }
            let token: String = chars[start..i].iter().collect();
            if crate::gallery::hierarchy::tag_key(&token) == from {
                if let Some(replacement) = to {
                    out.push_str(replacement);
                }
            } else {
                out.push_str(&token);
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn filter_tokens(filter: &str) -> std::collections::HashSet<String> {
    filter
        .split(|c: char| c.is_whitespace() || "()|!+".contains(c))
        .filter(|token| !token.is_empty() && !token.starts_with("rating:"))
        .map(str::to_string)
        .collect()
}

/// Tags currently referenced by the exact filter, normalized for tree
/// highlighting.
fn selected_filter_tokens(filter: &FilterQuery) -> std::collections::HashSet<String> {
    filter_tokens(&filter.filter)
}

fn append_filter_token(filter: &mut String, token: &str) -> bool {
    if filter_tokens(filter).contains(token) {
        return false;
    }
    let base = filter
        .trim()
        .trim_end_matches(|c| c == '+' || c == '|' || c == '-')
        .trim();
    *filter = if base.is_empty() {
        token.to_string()
    } else {
        format!("({}) + {}", base, token)
    };
    true
}

/// Tree clicks add an exact token; machine queries are never edited here.
fn append_filter_query(
    filter: &mut FilterQuery,
    token: &str,
    selected: &std::collections::HashSet<String>,
) -> bool {
    if filter.claim_scope != ClaimScope::Curated {
        return false;
    }
    let key = crate::gallery::hierarchy::tag_key(token);
    if selected.contains(&key) {
        return false;
    }
    append_filter_token(&mut filter.filter, token)
}

fn node_matches_search(node: &TagNode, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    node.name.to_lowercase().contains(needle)
        || node.children.iter().any(|child| node_matches_search(child, needle))
}

#[allow(clippy::too_many_arguments)]
fn draw_tag_node(
    ui: &mut Ui, node: &TagNode,
    is_edit_mode: bool, staged_sidecar: &mut Option<Sidecar>, filter_query: &mut FilterQuery,
    ancestors: &[String], apply: &mut bool, search: &str,
    selected: &std::collections::HashSet<String>, actions: &mut Vec<Action>,
) {
    if !node_matches_search(node, search) {
        return;
    }

    if node.name == "Root" {
        for child in &node.children {
            draw_tag_node(
                ui, child, is_edit_mode, staged_sidecar, filter_query, &[], apply, search,
                selected, actions,
            );
        }
        return;
    }

    let mut path = ancestors.to_vec();
    path.push(node.name.clone());
    let token = query_token(&node.name);
    let menu_target = TagMenuTarget { name: node.name.clone(), token };

    if !node.children.is_empty() {
        let response = egui::CollapsingHeader::new(&node.name)
            .default_open(!search.is_empty())
            .show(ui, |ui| {
                if !is_edit_mode && ui.small_button(format!("Filter by {}", node.name)).clicked()
                    && append_filter_query(filter_query, &query_token(&node.name), selected)
                {
                    *apply = true;
                }
                if !is_edit_mode {
                    ui.separator();
                }
                for child in &node.children {
                    draw_tag_node(
                        ui, child, is_edit_mode, staged_sidecar, filter_query, &path, apply,
                        search, selected, actions,
                    );
                }
            });
        actions::show_tag_menu(&response.header_response, menu_target, actions);
    } else if is_edit_mode {
        let mut checked = staged_sidecar.as_ref().map_or(false, |sc| sc.tags.contains(&node.name));
        let response = ui.checkbox(&mut checked, RichText::new(&node.name).size(13.0));
        actions::show_tag_menu(&response, menu_target, actions);
        if response.changed() {
            if let Some(sc) = staged_sidecar {
                if checked {
                    if !sc.tags.contains(&node.name) { sc.tags.push(node.name.clone()); }
                    let mut prev = "".to_string();
                    for anc in ancestors {
                        if !sc.tags.contains(anc) { sc.tags.push(anc.clone()); }
                        if !prev.is_empty() { sc.tag_parents.insert(anc.clone(), prev.clone()); }
                        prev = anc.clone();
                    }
                    if let Some(last_anc) = ancestors.last() {
                        sc.tag_parents.insert(node.name.clone(), last_anc.clone());
                    }
                } else {
                    sc.tags.retain(|t| t != &node.name);
                    sc.tag_parents.remove(&node.name);
                }
            }
        }
    } else {
        let response = ui.selectable_label(
            selected.contains(&query_token(&node.name)),
            RichText::new(&node.name).size(13.0),
        );
        actions::show_tag_menu(&response, menu_target, actions);
        if response.clicked() && append_filter_query(filter_query, &query_token(&node.name), selected) {
            *apply = true;
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

fn draw_edit_properties(ui: &mut Ui, sc: &mut Sidecar, path: Option<&Path>, tag_tree: &TagNode) {
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
    let mut suppressed = sc.is_display_suppressed();
    if ui.checkbox(&mut suppressed, "Hide from gallery").changed() {
        if suppressed {
            sc.properties.insert("display".to_string(), "false".to_string());
        } else {
            sc.properties.remove("display");
        }
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
    let mut sorted_tags: Vec<String> = sc.tags.iter().cloned().collect();
    sorted_tags.sort();
    let mut tag_str = sorted_tags.join(" + ");
    let catalog = tag_catalog(tag_tree);
    let tag_response = ui.add_sized(
        [ui.available_width(), 40.0],
        TextEdit::multiline(&mut tag_str).hint_text("tag + tag"),
    );
    show_tag_suggestions(
        ui,
        ui.make_persistent_id("editor_tag_suggestions"),
        &tag_response,
        &mut tag_str,
        &catalog,
        false,
        &sorted_tags,
    );
    sc.tags = tag_str
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
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
    filter: &mut FilterQuery,
    query_error: Option<&str>,
    tag_tree: &TagNode,
    scope_label: &str,
    revert: &mut bool,
) {
    ui.add_space(8.0);
    ui.label(
        RichText::new(format!("Tag filter scope: {}", scope_label))
            .size(10.0)
            .color(Color32::GRAY),
    );
    ui.label(
        RichText::new("The top Search box filters filenames; this field filters tags in the current folder.")
            .size(10.0)
            .color(Color32::GRAY),
    );

    ui.horizontal(|ui| {
        ui.label(RichText::new("Search").strong());
        ui.separator();
        let color = if filter.claim_scope == ClaimScope::Curated {
            Color32::from_rgb(150, 220, 170)
        } else {
            Color32::from_rgb(255, 190, 110)
        };
        ui.label(
            RichText::new(format!("source: {}", filter.claim_scope))
                .size(10.0)
                .color(color),
        );
        if filter.claim_scope != ClaimScope::Curated && ui.small_button("Clear AI filter").clicked() {
            *revert = true;
        }
    });

    if filter.claim_scope != ClaimScope::Curated {
        ui.label(
            RichText::new("Applied from Tag Manager. Rerun the AI search there, or clear it to edit curated tags.")
                .size(10.0)
                .color(Color32::GRAY),
        );
        ui.label(
            RichText::new(format!("Exact: {}", filter.filter))
                .size(10.0)
                .color(Color32::GRAY),
        );
        if let Ok(query) = Query::parse(&filter.filter) {
            let mut tags = Vec::new();
            query_tag_ids(&query, &mut tags);
            show_tag_descendant_hint(ui, tag_tree, &tags);
        }
    } else {
        let catalog = tag_catalog(tag_tree);
        let filter_response = ui.add_sized(
            [ui.available_width(), 60.0],
            TextEdit::multiline(&mut filter.filter)
                .hint_text("cat + fox - plush\nrel:item:this@created_by@tag:artist:kitty"),
        );
        show_tag_suggestions(
            ui,
            ui.make_persistent_id("filter_tag_suggestions"),
            &filter_response,
            &mut filter.filter,
            &catalog,
            true,
            &[],
        );
        ui.label(
            RichText::new("+ all   | any   - not   ! not")
                .size(10.0)
                .color(Color32::GRAY),
        );
        ui.label(
            RichText::new("rel:item:this@predicate@tag:id   (global rules match tagged images)")
                .size(10.0)
                .color(Color32::GRAY),
        );
        if let Ok(query) = Query::parse(&filter.filter) {
            let mut tags = Vec::new();
            query_tag_ids(&query, &mut tags);
            show_tag_descendant_hint(ui, tag_tree, &tags);
        }
    }

    if let Some(error) = query_error {
        ui.label(
            RichText::new(format!("Invalid filter: {}", error))
                .color(Color32::from_rgb(255, 120, 120))
                .size(11.0),
        );
    }
    ui.add_space(8.0);

    ui.label(RichText::new("Tag confidence").strong());
    ui.horizontal(|ui| {
        ui.label("Range");
        ui.add(egui::DragValue::new(&mut filter.confidence_min).speed(1.0).clamp_range(0..=100));
        ui.label("–");
        ui.add(egui::DragValue::new(&mut filter.confidence_max).speed(1.0).clamp_range(0..=100));
        ui.label("%");
    });
    filter.normalize_confidence();
    ui.label(
        RichText::new("100% = accepted/manual · 0–99% = suggestions")
            .size(10.0)
            .color(Color32::GRAY),
    );
    ui.horizontal_wrapped(|ui| {
        if ui.small_button("Accepted").clicked() {
            filter.confidence_min = 100;
            filter.confidence_max = 100;
        }
        if ui.small_button("High 90–99").clicked() {
            filter.confidence_min = 90;
            filter.confidence_max = 99;
        }
        if ui.small_button("Borderline 50–89").clicked() {
            filter.confidence_min = 50;
            filter.confidence_max = 89;
        }
        if ui.small_button("Low 0–49").clicked() {
            filter.confidence_min = 0;
            filter.confidence_max = 49;
        }
        if ui.small_button("All").clicked() {
            let range = ConfidenceRange::all();
            filter.confidence_min = range.min;
            filter.confidence_max = range.max;
        }
    });
    ui.add_space(8.0);

    ui.label("Minimum Rating");
    let cur = filter.rating_min.unwrap_or(0);
    if let Some(new_rating) = draw_stars(ui, cur) {
        filter.rating_min = if new_rating == 0 { None } else { Some(new_rating) };
    }
    if let Some(min_r) = filter.rating_min {
        ui.label(RichText::new(format!("filtering: ≥ {} stars", min_r)).size(10.0).color(Color32::GRAY));
    }
    ui.add_space(8.0);
}

#[cfg(test)]
mod tests {
    use super::{
        active_editor_segment, active_query_token, append_filter_token, filter_tokens,
        node_matches_search, query_token, replace_filter_text, replace_filter_token,
        tag_suggestions, FilterQuery,
    };
    use crate::gallery::{ClaimScope, ConfidenceRange, Query, TagNode};

    #[test]
    fn filter_defaults_to_accepted_only_and_normalizes_ranges() {
        let mut filter = FilterQuery::default();
        assert_eq!(filter.confidence_range(), ConfidenceRange::accepted_only());

        filter.confidence_min = 90;
        filter.confidence_max = 40;
        filter.normalize_confidence();
        assert_eq!(filter.confidence_min, 40);
        assert_eq!(filter.confidence_max, 90);
        assert_eq!(filter.confidence_range(), ConfidenceRange::new(40, 90));
    }

    #[test]
    fn tag_names_with_spaces_become_query_tokens() {
        assert_eq!(query_token("warrior cats"), "warrior_cats");
        assert_eq!(query_token("  house-cat "), "house-cat");
        assert!(crate::gallery::Query::parse(&query_token("warrior cats")).is_ok());
    }

    #[test]
    fn query_suggestions_follow_the_token_being_typed() {
        let catalog = vec!["cat".into(), "warrior cats".into(), "fox".into()];
        let (start, token) = active_query_token("cat + wa");
        assert_eq!(&"cat + wa"[start..], "wa");
        assert_eq!(tag_suggestions(&catalog, &token, true, &[]), vec!["warrior_cats"]);
    }

    #[test]
    fn editor_suggestions_keep_spaced_tag_names() {
        let catalog = vec!["cat".into(), "warrior cats".into(), "fox".into()];
        let (start, token) = active_editor_segment("cat + warrior c");
        assert_eq!(&"cat + warrior c"[..start], "cat +");
        assert_eq!(token, "warrior c");
        assert_eq!(tag_suggestions(&catalog, &token, false, &[]), vec!["warrior cats"]);
    }

    #[test]
    fn tree_clicks_wrap_and_deduplicate_tokens() {
        let mut filter = "fox | plush".to_string();
        assert!(append_filter_token(&mut filter, "derpy_hooves"));
        assert_eq!(filter, "(fox | plush) + derpy_hooves");
        assert!(!append_filter_token(&mut filter, "derpy_hooves"));
        assert_eq!(filter_tokens(&filter).len(), 3);
    }

    #[test]
    fn renaming_a_tag_rewrites_only_that_filter_token() {
        let filter = "(derpy_hooves | cat) + rating:4+ - derpy_hooves";
        let renamed = replace_filter_token(filter, "derpy_hooves", Some("derpy_hoof"));
        assert_eq!(renamed, "(derpy_hoof | cat) + rating:4+ - derpy_hoof");
        let removed = replace_filter_token(filter, "derpy_hooves", None);
        assert_eq!(removed, "( | cat) + rating:4+ - ");
    }

    #[test]
    fn tree_search_keeps_matching_branches() {
        let tree = TagNode {
            name: "Root".into(),
            children: vec![TagNode {
                name: "MLP".into(),
                children: vec![TagNode { name: "derpy hooves".into(), children: vec![] }],
            }],
        };
        assert!(node_matches_search(&tree, "derpy"));
        assert!(!node_matches_search(&tree, "dragon"));
    }

    #[test]
    fn exact_filter_compiles_to_the_shared_query_ast() {
        let tree = TagNode {
            name: "Root".into(),
            children: vec![TagNode { name: "feline".into(), children: vec![] }],
        };
        let mut filter = FilterQuery::default();
        filter.filter = "feature:color:dominant_orange + feline".into();
        filter.rating_min = Some(3);

        assert_eq!(
            filter.compile(&tree).unwrap(),
            Query::And(vec![
                Query::And(vec![
                    Query::Tag("feature:color:dominant_orange".into()),
                    Query::Tag("feline".into()),
                ]),
                Query::Rating { min: 3, exact: false },
            ])
        );
    }

    #[test]
    fn sidebar_filters_default_to_curated_claims() {
        let filter = FilterQuery::default();
        assert_eq!(filter.claim_scope, ClaimScope::Curated);
    }

    #[test]
    fn exact_tag_renames_rewrite_only_the_token() {
        let renamed = replace_filter_text("rainbow_dash + fox", "rainbow dash", Some("rainbow pony"));
        assert_eq!(renamed, "rainbow_pony + fox");
        let removed = replace_filter_text("fox + rainbow_dash", "rainbow dash", None);
        assert_eq!(removed.trim(), "fox +");
    }
}
