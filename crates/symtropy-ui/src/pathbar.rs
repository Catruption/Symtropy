//! Location bar with live sibling navigation.
//!
//! The full path stays visible, while hovering any segment opens a compact list
//! of that directory's sibling folders. Clicking a segment navigates to that
//! ancestor; clicking a row jumps sideways in one action.

use crate::actions::{self, Action, ContextTarget, MenuOptions};
use egui::{Button, Color32, Key, Pos2, Rect, RichText, Ui, Vec2, Window, pos2};
use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug)]
struct Row {
    path: PathBuf,
    name: String,
    count: usize,
    current: bool,
}

pub struct PathBarState {
    open: bool,
    pinned: bool,
    hovered: Option<usize>,
    editing: bool,
    edit_text: String,
    request_focus: bool,
    filter: String,
    counts: Option<HashMap<PathBuf, usize>>,
    last_dir: Option<PathBuf>,
    path_bar_rect: Rect,
    dropdown_rect: Rect,
    segment_rects: Vec<Rect>,
}

impl Default for PathBarState {
    fn default() -> Self {
        Self {
            open: false,
            pinned: false,
            hovered: None,
            editing: false,
            edit_text: String::new(),
            request_focus: false,
            filter: String::new(),
            counts: None,
            last_dir: None,
            path_bar_rect: empty_rect(),
            dropdown_rect: empty_rect(),
            segment_rects: Vec::new(),
        }
    }
}

impl PathBarState {
    pub fn begin_edit(&mut self, dir: &Path) {
        self.edit_text = dir.to_string_lossy().into_owned();
        self.editing = true;
        self.request_focus = true;
        self.open = false;
        self.pinned = false;
    }

    pub fn invalidate_counts(&mut self) {
        self.counts = None;
    }

    fn close(&mut self) {
        self.open = false;
        self.pinned = false;
        self.hovered = None;
        self.filter.clear();
    }
}

fn empty_rect() -> Rect {
    Rect::from_min_size(Pos2::ZERO, Vec2::ZERO)
}

fn grow(target: &mut Rect, other: Rect) {
    *target = target.union(other);
}

/// Every clickable component of the path, including `/` as index 0.
fn segments(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let mut current = PathBuf::new();

    for component in dir.components() {
        match component {
            Component::RootDir => {
                current.push(component.as_os_str());
                out.push(("/".to_string(), PathBuf::from("/")));
            }
            Component::Prefix(prefix) => {
                current.push(prefix.as_os_str());
                out.push((prefix.as_os_str().to_string_lossy().into_owned(), current.clone()));
            }
            Component::Normal(name) => {
                current.push(name);
                out.push((name.to_string_lossy().into_owned(), current.clone()));
            }
            Component::CurDir => {}
            Component::ParentDir => {
                current.push("..");
                out.push(("..".to_string(), current.clone()));
            }
        }
    }

    out
}

fn sibling_dirs(parent: &Path, show_hidden: bool) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let Ok(entries) = fs::read_dir(parent) else {
        return dirs;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        if entry.path().is_dir() {
            dirs.push(entry.path());
        }
    }
    dirs.sort_by(|a, b| {
        let a = a.file_name().map(|n| n.to_string_lossy().to_lowercase());
        let b = b.file_name().map(|n| n.to_string_lossy().to_lowercase());
        a.cmp(&b)
    });
    dirs
}

/// Draw the location bar. Emits ordinary file-manager actions so the app can
/// apply navigation, copying, bookmarking, and menus in one place.
pub fn show(
    ui: &mut Ui,
    state: &mut PathBarState,
    dir: &Path,
    show_hidden: bool,
    options: MenuOptions,
    actions: &mut Vec<Action>,
) {
    if state.last_dir.as_deref() != Some(dir) {
        state.last_dir = Some(dir.to_path_buf());
        state.close();
        state.editing = false;
    }

    // Close only once the pointer has deliberately left both the bar and the
    // dropdown. No open delay: hovering a segment responds immediately.
    if state.open && !state.pinned {
        let pointer = ui.ctx().input(|i| i.pointer.latest_pos());
        let zone = state.path_bar_rect.union(state.dropdown_rect).expand(6.0);
        if pointer.map_or(false, |p| !zone.contains(p)) {
            state.open = false;
            state.hovered = None;
        }
    }

    let segs = segments(dir);
    let current_index = segs.len().saturating_sub(1);
    let mut segment_rects = Vec::with_capacity(segs.len());
    let mut bar_rect = empty_rect();
    let mut hovered_now = None;

    let mut menu_action = None;
    let menu_target = segs
        .get(state.hovered.unwrap_or(current_index))
        .map(|(_, path)| path.clone())
        .unwrap_or_else(|| dir.to_path_buf());

    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;

        if state.editing {
            let response = ui.add(
                egui::TextEdit::singleline(&mut state.edit_text)
                    .desired_width(ui.available_width().max(200.0)),
            );
            grow(&mut bar_rect, response.rect);

            if state.request_focus {
                response.request_focus();
                state.request_focus = false;
            }
            if response.has_focus() {
                if ui.input(|i| i.key_pressed(Key::Enter)) {
                    let target = PathBuf::from(&state.edit_text);
                    if target.is_dir() {
                        actions.push(Action::Navigate(target));
                        state.editing = false;
                    }
                } else if ui.input(|i| i.key_pressed(Key::Escape)) {
                    state.editing = false;
                }
            }
        } else {
            ui.horizontal_wrapped(|ui| {
                for (index, (label, path)) in segs.iter().enumerate() {
                    if index > 0 {
                        let sep = ui.label(RichText::new("/").color(Color32::from_gray(120)));
                        grow(&mut bar_rect, sep.rect);
                    }

                    let color = if index == current_index {
                        Color32::WHITE
                    } else {
                        Color32::from_gray(180)
                    };
                    let text = if state.hovered == Some(index) {
                        format!("{} ▾", label)
                    } else if index == 0 {
                        "💽".to_string()
                    } else {
                        label.clone()
                    };
                    let response = ui.add(Button::new(RichText::new(text).color(color)).frame(false));
                    if response.hovered() {
                        hovered_now = Some(index);
                        state.open = true;
                        state.hovered = Some(index);
                    }
                    if response.clicked() {
                        actions.push(Action::Navigate(path.clone()));
                        state.close();
                    }
                    grow(&mut bar_rect, response.rect);
                    while segment_rects.len() <= index {
                        segment_rects.push(response.rect);
                    }
                }

                ui.add_space(8.0);
                if ui.small_button("📌").on_hover_text("Keep this folder list open").clicked() {
                    if state.pinned {
                        state.close();
                    } else {
                        state.pinned = true;
                        state.open = true;
                    }
                }
                if ui.small_button("✎").on_hover_text("Edit path (Ctrl+L)").clicked() {
                    state.begin_edit(dir);
                }
                if ui.small_button("📋").on_hover_text("Copy directory path").clicked() {
                    actions.push(Action::CopyPath(dir.to_path_buf()));
                }

                // One menu button for the hovered segment keeps the path compact
                // while still exposing the shared file-manager actions.
                let target = ContextTarget::Background(menu_target.clone());
                let menu = ui.menu_button("⋯", |ui| {
                    for entry in actions::menu_entries(&target, options) {
                        match entry {
                            actions::MenuEntry::Separator => { ui.separator(); }
                            actions::MenuEntry::Item(item) => {
                                let enabled = !matches!(item.action, Action::Paste(_)) || options.can_paste;
                                if ui.add_enabled(enabled, egui::Button::new(item.label)).clicked() {
                                    menu_action = Some(item.action);
                                    ui.close_menu();
                                }
                            }
                        }
                    }
                });
                grow(&mut bar_rect, menu.response.rect);
            });
        }
    });

    if let Some(action) = menu_action {
        actions.push(action);
        state.close();
    }

    state.segment_rects = segment_rects;
    state.path_bar_rect = bar_rect;

    let Some(index) = hovered_now.or(state.hovered) else {
        state.dropdown_rect = empty_rect();
        return;
    };
    if !state.open || state.editing {
        state.dropdown_rect = empty_rect();
        return;
    }
    let Some(segment) = segs.get(index) else {
        state.dropdown_rect = empty_rect();
        return;
    };

    let parent = segment.1.parent().unwrap_or(Path::new("/"));
    let index_path = crate::paths::cache_dir().join("index.tsv");
    let counts = state.counts.get_or_insert_with(|| {
        crate::gallery::workers::directory_counts(&index_path)
    });

    let rows: Vec<Row> = sibling_dirs(parent, show_hidden)
        .into_iter()
        .map(|path| {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let count = counts.get(&path).copied().unwrap_or(0);
            let current = path == segment.1;
            Row { path, name, count, current }
        })
        .collect();

    let anchor = state.segment_rects.get(index).copied().unwrap_or(bar_rect);
    let anchor_pos = pos2(anchor.min.x, anchor.max.y + 2.0);
    let mut filter = std::mem::take(&mut state.filter);
    let mut picked = None;

    let shown = Window::new("pathbar-siblings")
        .id(egui::Id::new("pathbar-siblings"))
        .title_bar(false)
        .resizable(false)
        .collapsible(false)
        .movable(false)
        .fixed_pos(anchor_pos)
        .show(ui.ctx(), |ui| {
            ui.set_min_width(240.0);
            ui.set_max_width(380.0);
            ui.add_space(2.0);
            ui.add(
                egui::TextEdit::singleline(&mut filter)
                    .hint_text("Filter folders…")
                    .desired_width(ui.available_width()),
            );
            ui.separator();

            let needle = filter.trim().to_lowercase();
            egui::ScrollArea::vertical()
                .max_height(320.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let mut visible = 0;
                    for row in &rows {
                        if !needle.is_empty() && !row.name.to_lowercase().contains(&needle) {
                            continue;
                        }
                        visible += 1;
                        let label = if row.count > 0 {
                            format!("{}   {}", row.name, row.count)
                        } else {
                            row.name.clone()
                        };
                        let response = ui.selectable_label(row.current, label);
                        if response.clicked() {
                            picked = Some(row.path.clone());
                        }
                    }
                    if visible == 0 {
                        ui.label("No matching folders");
                    }
                });
        });

    state.filter = filter;
    if let Some(response) = shown {
        state.dropdown_rect = response.response.rect;
    } else {
        state.dropdown_rect = empty_rect();
    }

    if let Some(path) = picked {
        actions.push(Action::Navigate(path));
        state.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_dirs_lists_directories_only_and_respects_hidden() {
        let root = std::env::temp_dir().join(format!(
            "symtropy-pathbar-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("alpha")).unwrap();
        fs::create_dir_all(root.join("beta")).unwrap();
        fs::create_dir_all(root.join(".secret")).unwrap();
        fs::write(root.join("file.txt"), "x").unwrap();

        let visible = sibling_dirs(&root, false);
        let names: Vec<String> = visible
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["alpha", "beta"]);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn segments_include_root_and_each_component() {
        let dir = Path::new("/home/cashmere/Pictures/Plush");
        let segs = segments(dir);
        let labels: Vec<&str> = segs.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(labels, ["/", "home", "cashmere", "Pictures", "Plush"]);
        assert_eq!(segs.last().unwrap().1, dir);
    }
}
