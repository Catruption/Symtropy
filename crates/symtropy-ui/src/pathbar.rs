//! Location bar with live sibling navigation.
//!
//! The full path stays visible, while hovering any segment opens a compact list
//! of that directory's sibling folders. Clicking a segment navigates to that
//! ancestor; clicking a row jumps sideways in one action.
//!
//! Layout: the action buttons (pin / edit / copy / menu) have a fixed slot on
//! the right and never wrap. The path itself lives in a horizontal scroll
//! area that sticks to the right, so a long path scrolls instead of pushing the
//! buttons onto a second row underneath other widgets.

use crate::actions::{self, Action, ContextTarget, MenuOptions};
use egui::{
    Align, Button, Color32, Key, Layout, Pos2, Rect, RichText, ScrollArea, Ui, Window, pos2, vec2,
};
use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Width reserved for the pin / edit / copy / menu buttons.
const ACTIONS_WIDTH: f32 = 150.0;

#[derive(Clone, Debug)]
struct Row {
    path: PathBuf,
    name: String,
    count: usize,
    current: bool,
}

/// Sibling folder list for one parent directory. Read once per dropdown
/// opening, not once per frame.
struct SiblingCache {
    parent: PathBuf,
    show_hidden: bool,
    /// True for the trailing `…` segment, which lists children of the current
    /// directory instead of siblings of an ancestor.
    children: bool,
    dirs: Vec<PathBuf>,
}

pub struct PathBarState {
    open: bool,
    pinned: bool,
    hovered: Option<usize>,
    /// Set after a click navigated away. The pointer is still sitting on the
    /// bar in the new layout; without this latch the dropdown reopens at once
    /// and covers whatever the user is about to click next.
    suppress_hover: bool,
    editing: bool,
    edit_text: String,
    request_focus: bool,
    filter: String,
    counts: Option<HashMap<PathBuf, usize>>,
    sibling_cache: Option<SiblingCache>,
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
            suppress_hover: false,
            editing: false,
            edit_text: String::new(),
            request_focus: false,
            filter: String::new(),
            counts: None,
            sibling_cache: None,
            last_dir: None,
            path_bar_rect: Rect::NOTHING,
            dropdown_rect: Rect::NOTHING,
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
        self.sibling_cache = None;
    }

    fn close(&mut self) {
        self.open = false;
        self.pinned = false;
        self.hovered = None;
        self.filter.clear();
        self.sibling_cache = None;
    }

    /// Close after a navigation click and keep the dropdown shut until the
    /// pointer has left the segments once.
    fn close_after_navigate(&mut self) {
        self.close();
        self.suppress_hover = true;
    }
}

/// Grow `target` to include `other`. `Rect::NOTHING` is the identity, so an
/// untouched accumulator does not drag the union out to the screen origin.
fn grow(target: &mut Rect, other: Rect) {
    *target = target.union(other);
}

/// Make a path absolute without touching symlinks. A relative path (for
/// example from launching with a relative argument) would otherwise produce
/// relative "ancestors" that only resolve from one working directory.
fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

/// Lexically drop `.` and resolve `..` components.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let can_pop = matches!(out.components().next_back(), Some(Component::Normal(_)));
                if can_pop {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push("/");
    }
    out
}

/// Turn text typed into the edit box into an absolute path: `~` expands to
/// home, relative text is resolved against the directory being shown.
fn resolve_typed(text: &str, base: &Path) -> PathBuf {
    let text = text.trim();
    let home = dirs::home_dir();
    let expanded = match (text, &home) {
        ("~", Some(h)) => h.clone(),
        (t, Some(h)) if t.starts_with("~/") => h.join(&t[2..]),
        _ => PathBuf::from(text),
    };
    let absolute = if expanded.is_absolute() { expanded } else { base.join(expanded) };
    normalize(&absolute)
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
        // file_type() is free on most filesystems; only symlinks need a stat.
        let is_dir = match entry.file_type() {
            Ok(t) if t.is_dir() => true,
            Ok(t) if t.is_symlink() => entry.path().is_dir(),
            _ => false,
        };
        if is_dir {
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
    let dir_owned = normalize(&absolutize(dir));
    let dir = dir_owned.as_path();

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
        if pointer.map_or(true, |p| !zone.contains(p)) {
            state.close();
        }
    }

    let segs = segments(dir);
    let current_index = segs.len().saturating_sub(1);
    let mut segment_rects = Vec::with_capacity(segs.len());
    let mut bar_rect = Rect::NOTHING;
    let mut hovered_now = None;
    let mut over_segment = false;

    let mut menu_action = None;
    let menu_target = segs
        .get(state.hovered.unwrap_or(current_index))
        .map(|(_, path)| path.clone())
        .unwrap_or_else(|| dir.to_path_buf());

    let bar_bounds = ui.available_rect_before_wrap();

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

            // A single-line TextEdit gives up focus the moment Enter or Escape
            // is pressed, so `has_focus()` is already false on that frame.
            // `lost_focus()` is the signal that is actually set.
            if response.lost_focus() {
                let (enter, escape) =
                    ui.input(|i| (i.key_pressed(Key::Enter), i.key_pressed(Key::Escape)));
                if enter {
                    let target = resolve_typed(&state.edit_text, dir);
                    if target.is_dir() {
                        actions.push(Action::Navigate(target));
                        state.editing = false;
                    } else {
                        // Not a directory: stay in the box so it can be fixed.
                        response.request_focus();
                    }
                } else if escape {
                    state.editing = false;
                } else {
                    // Clicked somewhere else.
                    state.editing = false;
                }
            }
        } else {
            let row_h = ui.spacing().interact_size.y;
            ui.horizontal(|ui| {
                let path_w = (ui.available_width() - ACTIONS_WIDTH - 8.0).max(60.0);

                ui.allocate_ui_with_layout(
                    vec2(path_w, row_h),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        ScrollArea::horizontal()
                            .auto_shrink([false, true])
                            .stick_to_right(true)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    // One extra trailing segment lets the user step into
                                    // the next directory without first hovering the
                                    // current segment and scanning its sibling list.
                                    for index in 0..=segs.len() {
                                        if index > 0 {
                                            let sep = ui.label(
                                                RichText::new("/").color(Color32::from_gray(120)),
                                            );
                                            grow(&mut bar_rect, sep.rect);
                                        }

                                        let is_child = index == segs.len();
                                        let label = if is_child {
                                            "…"
                                        } else {
                                            segs[index].0.as_str()
                                        };
                                        let path = if is_child {
                                            dir.to_path_buf()
                                        } else {
                                            segs[index].1.clone()
                                        };

                                        let active = state.open && state.hovered == Some(index);
                                        let color = if active {
                                            Color32::from_rgb(255, 220, 140)
                                        } else if index == current_index {
                                            Color32::WHITE
                                        } else {
                                            Color32::from_gray(180)
                                        };
                                        // The label never changes with hover. Swapping
                                        // in a wider "name ▾" label shifted every segment
                                        // to its right mid-click.
                                        let text = if index == 0 { "💽".to_string() } else { label.to_string() };
                                        let response =
                                            ui.add(Button::new(RichText::new(text).color(color)).frame(false))
                                                .on_hover_text(if is_child {
                                                    "Open subfolders of this directory"
                                                } else {
                                                    "Open folders beside this segment"
                                                });

                                        if response.hovered() {
                                            over_segment = true;
                                            if !state.suppress_hover {
                                                // A filter typed for one segment's siblings
                                                // must not follow the pointer to another.
                                                if state.hovered != Some(index) {
                                                    state.filter.clear();
                                                }
                                                hovered_now = Some(index);
                                                state.open = true;
                                                state.hovered = Some(index);
                                            }
                                        }
                                        if response.clicked() && !is_child {
                                            actions.push(Action::Navigate(path));
                                            state.close_after_navigate();
                                        }
                                        grow(&mut bar_rect, response.rect);
                                        segment_rects.push(response.rect);
                                    }
                                });
                            });
                    },
                );

                ui.add_space(8.0);
                // Copy stays visible because it is frequently used. Pin and edit
                // move into the path menu so the location bar has two controls
                // instead of four unexplained icons.
                if ui.small_button("📋 Copy").on_hover_text("Copy this directory path").clicked() {
                    actions.push(Action::CopyPath(dir.to_path_buf()));
                }

                // One menu button for the hovered segment keeps the path compact
                // while still exposing the shared file-manager actions.
                let target = ContextTarget::Background(menu_target.clone());
                let menu = ui.menu_button("Actions", |ui| {
                    if ui.button("✎ Edit path (Ctrl+L)").clicked() {
                        state.begin_edit(dir);
                        ui.close_menu();
                    }
                    if ui.button(if state.pinned { "📌 Unpin folder list" } else { "📌 Keep folder list open" }).clicked() {
                        if state.pinned {
                            state.close();
                        } else {
                            state.pinned = true;
                            state.open = true;
                        }
                        ui.close_menu();
                    }
                    ui.separator();
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

    if !over_segment {
        state.suppress_hover = false;
    }

    if let Some(action) = menu_action {
        actions.push(action);
        state.close();
    }

    // Segments scrolled out of view must not widen the hover zone.
    let bar_rect = if bar_rect.intersects(bar_bounds) {
        bar_rect.intersect(bar_bounds)
    } else {
        bar_rect
    };

    state.segment_rects = segment_rects;
    state.path_bar_rect = bar_rect;

    let Some(index) = hovered_now.or(state.hovered) else {
        state.dropdown_rect = Rect::NOTHING;
        return;
    };
    if !state.open || state.editing {
        state.dropdown_rect = Rect::NOTHING;
        return;
    }
    let is_child = index == segs.len();
    if !is_child && segs.get(index).is_none() {
        state.dropdown_rect = Rect::NOTHING;
        return;
    }

    let (parent, current_path) = if is_child {
        (dir.to_path_buf(), None)
    } else {
        let segment = &segs[index];
        (
            segment.1.parent().unwrap_or(Path::new("/")).to_path_buf(),
            Some(segment.1.clone()),
        )
    };

    // Read the folder list once per opening instead of every frame.
    let stale = state.sibling_cache.as_ref().map_or(true, |c| {
        c.parent != parent || c.show_hidden != show_hidden || c.children != is_child
    });
    if stale {
        state.sibling_cache = Some(SiblingCache {
            parent: parent.clone(),
            show_hidden,
            children: is_child,
            dirs: sibling_dirs(&parent, show_hidden),
        });
    }

    let index_path = crate::paths::cache_dir().join("index.tsv");
    let rows: Vec<Row> = {
        let counts = state.counts.get_or_insert_with(|| {
            crate::gallery::workers::directory_counts(&index_path)
        });
        let dirs = state.sibling_cache.as_ref().map(|c| c.dirs.as_slice()).unwrap_or(&[]);
        dirs.iter()
            .map(|path| {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let count = counts.get(path).copied().unwrap_or(0);
                let current = current_path.as_ref() == Some(path);
                Row { path: path.clone(), name, count, current }
            })
            .collect()
    };

    let anchor = state.segment_rects.get(index).copied().unwrap_or(bar_rect);
    let anchor_pos: Pos2 = pos2(anchor.min.x.max(bar_bounds.left()), anchor.max.y + 2.0);
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
            ScrollArea::vertical()
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
    state.dropdown_rect = match shown {
        Some(response) => response.response.rect,
        None => Rect::NOTHING,
    };

    if let Some(path) = picked {
        actions.push(Action::Navigate(path));
        state.close_after_navigate();
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

    #[test]
    fn normalize_resolves_dots() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert_eq!(normalize(Path::new("/..")), PathBuf::from("/"));
    }

    #[test]
    fn typed_relative_paths_resolve_against_current_dir() {
        assert_eq!(resolve_typed("sub/x", Path::new("/a/b")), PathBuf::from("/a/b/sub/x"));
        assert_eq!(resolve_typed("../c", Path::new("/a/b")), PathBuf::from("/a/c"));
        assert_eq!(resolve_typed("/abs", Path::new("/a/b")), PathBuf::from("/abs"));
    }

    #[test]
    fn absolutize_makes_relative_paths_absolute() {
        assert!(absolutize(Path::new("some/relative")).is_absolute());
    }
}
