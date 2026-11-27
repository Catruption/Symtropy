//! Shared input vocabulary for the file views.
//!
//! Both the list and the gallery turn raw pointer events into the same
//! [`Click`] values, and both build their right-click menu and drag payload
//! from the same [`ContextTarget`]. Commands are emitted as [`Action`]s and
//! applied in one place by `SymtropyApp`, so adding a new operation means
//! adding one enum variant and one menu entry instead of editing every view.

use crate::file_manager::{SortDirection, SortKey};
use egui::{Color32, DragAndDrop, PointerButton, Rect, Response, Stroke, Ui};
use std::path::{Path, PathBuf};

/// A command emitted by a view and applied by the app.
#[derive(Clone, Debug)]
pub enum Action {
    Navigate(PathBuf),
    /// Move through the existing history without discarding forward/back entries.
    NavigateBack,
    NavigateForward,
    /// Open in the viewer for media, or navigate for directories.
    Open(PathBuf),
    /// Force an external application, bypassing the built-in viewer.
    OpenExternal(PathBuf),
    OpenTerminal(PathBuf),
    Trash(Vec<PathBuf>),

    // Clipboard / file operations
    CopyFiles(Vec<PathBuf>),
    CutFiles(Vec<PathBuf>),
    Paste(PathBuf),
    Duplicate(Vec<PathBuf>),
    Rename(PathBuf),
    RenameCommit { path: PathBuf, name: String },
    NewFolder(PathBuf),
    CreateFolder { dir: PathBuf, name: String },
    NewFile(PathBuf),
    CreateFile { dir: PathBuf, name: String },
    Properties(Vec<PathBuf>),
    MoveFiles { from: Vec<PathBuf>, to: PathBuf, copy: bool },
    /// Files dropped onto the window from another application. Always imported as copies.
    ImportExternal { paths: Vec<PathBuf>, dir: PathBuf },

    // Metadata and discovery
    SetRating { paths: Vec<PathBuf>, rating: u8 },
    /// Clear `display:false` sidecar suppression so a file can be edited again.
    ShowInGallery(Vec<PathBuf>),
    EditTag { paths: Vec<PathBuf>, remove: bool },
    ApplyTag { paths: Vec<PathBuf>, tag: String, remove: bool },
    ApplyTagToSelected { tag: String, remove: bool },
    ApplyTagFilter(String),

    // Tag-tree management (DigiKam-style)
    TagDialog(TagDialog),
    TagCommit { kind: TagDialog, text: String },

    FindDuplicates,
    /// Open the standalone native Tag Manager viewport, optionally targeting a
    /// specific image.
    OpenTagManager(Option<PathBuf>),
    /// Open or close the standalone native Tag Manager viewport.
    ToggleTagManager,
    /// Open the metadata-only export dialog.
    ExportMetadataIndex,
    /// Write the shareable metadata index to an explicit destination.
    ExportMetadataCommit { path: PathBuf },

    CopyPath(PathBuf),
    ToggleBookmark(PathBuf),

    // Undo / redo of reversible file operations
    Undo,
    Redo,

    // View / selection
    SelectAll,
    SelectInverse,
    SelectPaths(Vec<PathBuf>),
    ClearSelection,
    /// Reset the active gallery query and the sidebar's structured filter fields.
    ClearFilters,
    SetSort { key: SortKey, descending: bool },
    ToggleSortDirection,
    ToggleHidden,
    Refresh,
}

/// A taxonomy edit requested from the sidebar tag tree. The dialog collects the
/// new name or target, then emits the corresponding commit action.
#[derive(Clone, Debug)]
pub enum TagDialog {
    New { parent: Option<String> },
    Rename { tag: String },
    Move { tag: String },
    Merge { source: String },
    Delete { tag: String },
}

/// The sidebar tag node a context menu was opened on.
#[derive(Clone, Debug)]
pub struct TagMenuTarget {
    pub name: String,
    pub token: String,
}

/// What a right-click (or right-drag) was aimed at.
#[derive(Clone, Debug)]
pub enum ContextTarget {
    /// One or more file entries. The first path is the "primary" one used by
    /// single-target commands (Open, Copy Path, ...).
    Entries(Vec<PathBuf>),
    /// Empty space in a directory.
    Background(PathBuf),
}

impl ContextTarget {
    pub fn directory(&self) -> PathBuf {
        let primary = match self {
            Self::Entries(paths) => paths.first().map(PathBuf::as_path),
            Self::Background(dir) => Some(dir.as_path()),
        };
        primary
            .and_then(|p| if p.is_dir() { Some(p) } else { p.parent() })
            .map(PathBuf::from)
            .unwrap_or_default()
    }
}

/// Normalised pointer intent, shared by every file view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Click {
    /// Plain primary click: select (or toggle with Ctrl).
    Primary,
    /// Double primary click: open in the viewer / external app.
    Double,
    /// Secondary click: show the context menu.
    Secondary,
    /// Secondary drag: pick up the selection and start a file operation.
    Drag,
    /// Primary drag on an entry: keep egui's normal left-drag scrolling, but arm
    /// the OS drag source so the files can still be dropped outside the app.
    PrimaryDrag,
}

/// Classify a widget's pointer activity. Checked in priority order so a
/// double-click isn't also reported as a plain click, and a right-drag isn't
/// mistaken for a context menu.
pub fn classify(ui: &Ui, response: &Response) -> Option<Click> {
    // Once the compositor owns a native drag, egui may never see the button
    // release. Ignore the stale button state until the drag finishes.
    if crate::wayland_drag::is_active() {
        return None;
    }
    // Read the response flags before taking the input lock. Response helpers
    // such as `double_clicked()` call `ctx.input()` themselves, and nesting
    // those calls while this closure holds the lock can clear the pointer
    // events other widgets still need to see this frame.
    let clicked = response.clicked();
    let double_clicked = response.double_clicked();
    let (double, secondary_click, secondary_drag, primary_drag, primary_click) = ui.input(|i| {
        let p = &i.pointer;
        let pos = p.latest_pos().or_else(|| p.press_origin());
        let in_widget = pos.map_or(false, |pos| response.rect.contains(pos));
        let drag_origin = p
            .press_origin()
            .map_or(in_widget, |origin| response.rect.contains(origin));
        let dragging = p.is_decidedly_dragging() && drag_origin;
        (
            double_clicked && p.button_double_clicked(PointerButton::Primary) && in_widget,
            p.button_clicked(PointerButton::Secondary) && in_widget,
            p.button_down(PointerButton::Secondary)
                && !p.button_down(PointerButton::Primary)
                && dragging,
            p.button_down(PointerButton::Primary)
                && !p.button_down(PointerButton::Secondary)
                && dragging,
            clicked && !p.button_down(PointerButton::Secondary),
        )
    });

    if double {
        Some(Click::Double)
    } else if secondary_click {
        // egui's context-menu layer usually wins the secondary click, so read
        // the raw input instead of relying on `Response::secondary_clicked()`.
        Some(Click::Secondary)
    } else if secondary_drag {
        Some(Click::Drag)
    } else if primary_drag {
        Some(Click::PrimaryDrag)
    } else if primary_click {
        Some(Click::Primary)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardMode {
    Copy,
    Cut,
}

/// The data carried by a right-button drag. Ctrl+drag copies instead of moves,
/// matching the convention used by most file managers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DragPayload {
    pub paths: Vec<PathBuf>,
    pub copy: bool,
}

/// Start carrying `paths` after a right-drag was recognised.
pub fn begin_drag(ui: &Ui, paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    let copy = ui.input(|i| i.modifiers.ctrl);
    // Keep egui's in-process payload for drops inside Symtropy. The native
    // drag only starts once the pointer leaves our viewport, so internal
    // move/copy/trash behavior is unchanged.
    crate::wayland_drag::begin_drag(&paths, true);
    DragAndDrop::set_payload(ui.ctx(), DragPayload { paths, copy });
    ui.ctx().request_repaint();
}

/// Arm the OS drag source for a left-drag on an entry, without creating an
/// egui payload. Left-drag keeps scrolling/selecting normally inside the app;
/// the compositor only takes over if the pointer leaves our windows.
pub fn begin_external_drag(ui: &Ui, paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    crate::wayland_drag::begin_drag(&paths, false);
    ui.ctx().request_repaint();
}

/// Take the drag payload if the pointer was released over `response`.
pub fn take_drag(ui: &Ui, response: &Response) -> Option<DragPayload> {
    if response.contains_pointer && ui.input(|i| i.pointer.any_released()) {
        DragAndDrop::take_payload::<DragPayload>(ui.ctx()).map(|p| (*p).clone())
    } else {
        None
    }
}

fn valid_move(from: &Path, to: &Path) -> bool {
    // Never try to move a directory into itself.
    !to.starts_with(from)
}

/// Handle a drop onto a directory target.
pub fn drop_on_directory(
    ui: &Ui,
    response: &Response,
    dir: &Path,
    actions: &mut Vec<Action>,
) {
    if let Some(payload) = take_drag(ui, response) {
        let to = dir.to_path_buf();
        if payload.paths.iter().any(|from| valid_move(from, &to)) {
            actions.push(Action::MoveFiles {
                from: payload.paths,
                to,
                copy: payload.copy,
            });
        }
    }
}

/// Trash is a valid drop target for right-dragged files.
pub fn drop_on_trash(ui: &Ui, response: &Response) -> Option<Vec<PathBuf>> {
    take_drag(ui, response).map(|p| p.paths)
}

/// Outline a widget while a compatible drag is hovering it.
pub fn paint_drop_target(ui: &Ui, response: &Response) {
    if response.contains_pointer
        && DragAndDrop::has_payload_of_type::<DragPayload>(ui.ctx())
    {
        ui.painter().rect_stroke(
            response.rect,
            2.0,
            Stroke::new(2.0_f32, Color32::from_rgb(96, 184, 255)),
        );
    }
}

/// ScrollArea's built-in drag-to-scroll reacts to every pointer button, which
/// would fight a right-button file drag. Views turn it off and call this for
/// primary-button dragging instead.
pub fn primary_drag_scroll(ui: &Ui, rect: Rect, offset: &mut f32) {
    if crate::wayland_drag::is_active() {
        return;
    }
    let (down, secondary, ctrl, dragging, pos, delta_y) = ui.input(|i| {
        let p = &i.pointer;
        (
            p.primary_down(),
            p.button_down(PointerButton::Secondary),
            i.modifiers.ctrl,
            p.is_decidedly_dragging(),
            p.latest_pos(),
            p.delta().y,
        )
    });
    if down && !secondary && !ctrl && dragging && pos.map_or(false, |p| rect.contains(p)) {
        *offset -= delta_y;
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MenuOptions {
    pub bookmarked: bool,
    pub hidden: bool,
    pub sort: SortKey,
    pub sort_descending: bool,
    pub can_paste: bool,
}

impl Default for MenuOptions {
    fn default() -> Self {
        Self {
            bookmarked: false,
            hidden: false,
            sort: SortKey::Name,
            sort_descending: false,
            can_paste: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct MenuItem {
    pub label: String,
    pub action: Action,
}

#[derive(Clone, Debug)]
pub enum MenuEntry {
    Item(MenuItem),
    Separator,
}

fn item(label: impl Into<String>, action: Action) -> MenuEntry {
    MenuEntry::Item(MenuItem {
        label: label.into(),
        action,
    })
}

fn sort_description(key: SortKey, direction: SortDirection) -> &'static str {
    match (key, direction) {
        (SortKey::Name, SortDirection::Ascending) => "A→Z",
        (SortKey::Name, SortDirection::Descending) => "Z→A",
        (SortKey::Date, SortDirection::Ascending) => "oldest first",
        (SortKey::Date, SortDirection::Descending) => "newest first",
        (SortKey::Size, SortDirection::Ascending) => "smallest first",
        (SortKey::Size, SortDirection::Descending) => "largest first",
    }
}

fn sort_entries(entries: &mut Vec<MenuEntry>, sort: SortKey, sort_descending: bool) {
    let current = if sort_descending {
        SortDirection::Descending
    } else {
        SortDirection::Ascending
    };
    for key in [SortKey::Name, SortKey::Date, SortKey::Size] {
        let direction = if key == sort {
            current
        } else {
            key.default_direction()
        };
        let label = format!(
            "{} Sort by {} ({})",
            if key == sort { "✓" } else { " " },
            key.label(),
            sort_description(key, direction)
        );
        entries.push(item(label, Action::SetSort {
            key,
            descending: direction == SortDirection::Descending,
        }));
    }
    entries.push(item(
        format!("Toggle Sort Direction {}", current.glyph()),
        Action::ToggleSortDirection,
    ));
}

/// Build the menu contents. Kept separate from rendering so it is testable and
/// so alternate renderers (keyboard menu, touch) can reuse it.
pub fn menu_entries(target: &ContextTarget, options: MenuOptions) -> Vec<MenuEntry> {
    let mut entries = Vec::new();
    let bookmark_label = if options.bookmarked {
        "Remove Bookmark"
    } else {
        "Add Bookmark"
    };
    let hidden_label = if options.hidden {
        "Hide Hidden Files"
    } else {
        "Show Hidden Files"
    };

    match target {
        ContextTarget::Entries(paths) => {
            if let Some(primary) = paths.first() {
                entries.push(item("Open", Action::Open(primary.clone())));
                if !primary.is_dir() {
                    entries.push(item(
                        "Open With Default App",
                        Action::OpenExternal(primary.clone()),
                    ));
                }
            }
            let dir = target.directory();
            if !dir.as_os_str().is_empty() {
                entries.push(item("Open Terminal Here", Action::OpenTerminal(dir.clone())));
            }
            entries.push(MenuEntry::Separator);

            entries.push(item("Cut", Action::CutFiles(paths.clone())));
            entries.push(item("Copy", Action::CopyFiles(paths.clone())));
            if let Some(primary) = paths.first() {
                entries.push(item("Copy Path", Action::CopyPath(primary.clone())));
            }
            if paths.len() == 1 {
                if let Some(primary) = paths.first() {
                    entries.push(item("Rename…", Action::Rename(primary.clone())));
                }
            }
            entries.push(item("Duplicate", Action::Duplicate(paths.clone())));
            entries.push(item("Properties", Action::Properties(paths.clone())));
            entries.push(item(
                "Tag Manager",
                Action::OpenTagManager(paths.iter().find(|path| !path.is_dir()).cloned()),
            ));
            entries.push(MenuEntry::Separator);

            let image_paths: Vec<PathBuf> = paths.iter().filter(|p| !p.is_dir()).cloned().collect();
            if !image_paths.is_empty() {
                entries.push(item("Show in Gallery", Action::ShowInGallery(image_paths.clone())));
                entries.push(item("Clear Rating", Action::SetRating { paths: image_paths.clone(), rating: 0 }));
                for rating in 1..=5u8 {
                    entries.push(item(
                        format!("Rate {}", "★".repeat(rating as usize)),
                        Action::SetRating { paths: image_paths.clone(), rating },
                    ));
                }
                entries.push(item("Add Tag…", Action::EditTag { paths: image_paths.clone(), remove: false }));
                entries.push(item("Remove Tag…", Action::EditTag { paths: image_paths, remove: true }));
                entries.push(MenuEntry::Separator);
            }

            if !dir.as_os_str().is_empty() {
                entries.push(item(bookmark_label, Action::ToggleBookmark(dir)));
            }
            let trash_label = if paths.len() > 1 {
                format!("Move {} Items to Trash", paths.len())
            } else {
                "Move to Trash".to_string()
            };
            if !paths.is_empty() {
                entries.push(item(trash_label, Action::Trash(paths.clone())));
            }
            entries.push(MenuEntry::Separator);
        }
        ContextTarget::Background(dir) => {
            entries.push(item("New Folder…", Action::NewFolder(dir.clone())));
            entries.push(item("New File…", Action::NewFile(dir.clone())));
            entries.push(item("Paste", Action::Paste(dir.clone())));
            entries.push(item("Copy Path", Action::CopyPath(dir.clone())));
            entries.push(item("Open Terminal Here", Action::OpenTerminal(dir.clone())));
            entries.push(item(bookmark_label, Action::ToggleBookmark(dir.clone())));
            entries.push(item("Find Duplicates…", Action::FindDuplicates));
            entries.push(item("Tag Manager", Action::OpenTagManager(None)));
            entries.push(item("Export Metadata Index…", Action::ExportMetadataIndex));
            entries.push(MenuEntry::Separator);
        }
    }

    entries.push(MenuEntry::Separator);
    sort_entries(&mut entries, options.sort, options.sort_descending);
    entries.push(MenuEntry::Separator);
    entries.push(item("Select All", Action::SelectAll));
    entries.push(item("Select Inverse", Action::SelectInverse));
    entries.push(item("Clear Selection", Action::ClearSelection));
    entries.push(MenuEntry::Separator);
    entries.push(item("Undo", Action::Undo));
    entries.push(item("Redo", Action::Redo));
    entries.push(MenuEntry::Separator);
    entries.push(item(hidden_label, Action::ToggleHidden));
    entries.push(item("Refresh", Action::Refresh));
    entries
}

/// Draw the shared context menu for a response. Actions are collected rather
/// than applied here to avoid borrowing app state from inside the menu closure.
pub fn show_menu(
    _ui: &mut Ui,
    response: &Response,
    target: impl FnOnce() -> ContextTarget,
    options: MenuOptions,
    actions: &mut Vec<Action>,
) {
    // egui only invokes this closure while the menu for this response is open,
    // so the target is built lazily instead of for every visible item.
    response.context_menu(|ui| {
        let target = target();
        for entry in menu_entries(&target, options) {
            match entry {
                MenuEntry::Separator => {
                    ui.separator();
                }
                MenuEntry::Item(MenuItem { label, action }) => {
                    let enabled = !matches!(action, Action::Paste(_)) || options.can_paste;
                    if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                        actions.push(action);
                        ui.close_menu();
                    }
                }
            }
        }
    });
}

/// DigiKam-style commands for a tag-tree node. Kept here with the file menus so
/// every right-click emits the same `Action` vocabulary.
pub fn tag_menu_entries(target: &TagMenuTarget) -> Vec<MenuEntry> {
    vec![
        item("Filter by This Tag", Action::ApplyTagFilter(target.token.clone())),
        item(
            "Assign Tag to Selected Images",
            Action::ApplyTagToSelected { tag: target.name.clone(), remove: false },
        ),
        item(
            "Remove Tag from Selected Images",
            Action::ApplyTagToSelected { tag: target.name.clone(), remove: true },
        ),
        MenuEntry::Separator,
        item("New Child Tag…", Action::TagDialog(TagDialog::New { parent: Some(target.name.clone()) })),
        item("New Top-Level Tag…", Action::TagDialog(TagDialog::New { parent: None })),
        MenuEntry::Separator,
        item("Rename…", Action::TagDialog(TagDialog::Rename { tag: target.name.clone() })),
        item("Move…", Action::TagDialog(TagDialog::Move { tag: target.name.clone() })),
        item("Merge…", Action::TagDialog(TagDialog::Merge { source: target.name.clone() })),
        item("Delete…", Action::TagDialog(TagDialog::Delete { tag: target.name.clone() })),
    ]
}

/// Attach the tag menu to a single node row. Actions are collected so the
/// viewer closure never mutates gallery state directly.
pub fn show_tag_menu(
    response: &Response,
    target: TagMenuTarget,
    actions: &mut Vec<Action>,
) {
    response.context_menu(|ui| {
        for entry in tag_menu_entries(&target) {
            match entry {
                MenuEntry::Separator => {
                    ui.separator();
                }
                MenuEntry::Item(MenuItem { label, action }) => {
                    if ui.button(label).clicked() {
                        actions.push(action);
                        ui.close_menu();
                    }
                }
            }
        }
    });
}

/// Apply a shift-click range selection to a set of paths.
///
/// `ordered` is the current visible order, and `anchor` is the item where the
/// previous ordinary click started the range. Ctrl+shift extends the existing
/// selection instead of replacing it, matching common file managers.
pub fn select_range<T: Clone + Eq>(
    selected: &mut Vec<T>,
    anchor: &mut Option<T>,
    ordered: &[T],
    clicked: T,
    extend: bool,
) {
    let clicked_index = ordered.iter().position(|p| p == &clicked);
    let anchor_index = anchor
        .as_ref()
        .and_then(|a| ordered.iter().position(|p| p == a));

    let (Some(clicked_index), Some(anchor_index)) = (clicked_index, anchor_index) else {
        *anchor = Some(clicked.clone());
        if !extend {
            selected.clear();
        }
        if !selected.contains(&clicked) {
            selected.push(clicked);
        }
        return;
    };

    let start = clicked_index.min(anchor_index);
    let end = clicked_index.max(anchor_index);
    let range = ordered[start..=end].to_vec();

    if !extend {
        selected.clear();
    }
    for path in range {
        if !selected.contains(&path) {
            selected.push(path);
        }
    }
}

/// Apply a shift-click range selection to a set-style selection.
pub fn select_range_set(
    selected: &mut std::collections::HashSet<PathBuf>,
    anchor: &mut Option<PathBuf>,
    ordered: &[PathBuf],
    clicked: &Path,
    extend: bool,
) {
    let Some(clicked_index) = ordered.iter().position(|p| p == clicked) else {
        *anchor = Some(clicked.to_path_buf());
        if !extend {
            selected.clear();
        }
        selected.insert(clicked.to_path_buf());
        return;
    };
    let Some(anchor_index) = anchor
        .as_ref()
        .and_then(|a| ordered.iter().position(|p| p == a))
    else {
        *anchor = Some(clicked.to_path_buf());
        if !extend {
            selected.clear();
        }
        selected.insert(clicked.to_path_buf());
        return;
    };

    let start = clicked_index.min(anchor_index);
    let end = clicked_index.max(anchor_index);
    if !extend {
        selected.clear();
    }
    for path in &ordered[start..=end] {
        selected.insert(path.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(entries: Vec<MenuEntry>) -> Vec<String> {
        entries
            .into_iter()
            .filter_map(|e| match e {
                MenuEntry::Item(i) => Some(i.label),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn entry_menu_targets_the_selection() {
        let paths = vec![
            PathBuf::from("/tmp/library/a.webp"),
            PathBuf::from("/tmp/library/b.webp"),
        ];
        let target = ContextTarget::Entries(paths);
        assert_eq!(target.directory(), PathBuf::from("/tmp/library"));
        let entries = menu_entries(&target, MenuOptions::default());
        let labels = labels(entries.clone());
        assert!(labels.contains(&"Move 2 Items to Trash".to_string()));
        assert!(labels.contains(&"Add Bookmark".to_string()));
        assert!(labels.contains(&"Select Inverse".to_string()));
        assert!(labels.contains(&"Properties".to_string()));
        assert!(labels.contains(&"Show in Gallery".to_string()));
        assert!(entries.iter().any(|e| matches!(
            e,
            MenuEntry::Item(MenuItem {
                action: Action::OpenTagManager(Some(_)),
                ..
            })
        )));
    }

    #[test]
    fn background_menu_has_no_file_actions() {
        let target = ContextTarget::Background(PathBuf::from("/tmp/library"));
        let entries = menu_entries(&target, MenuOptions {
            bookmarked: true,
            ..MenuOptions::default()
        });
        let labels = labels(entries.clone());
        assert!(labels.contains(&"Remove Bookmark".to_string()));
        assert!(labels.contains(&"New Folder…".to_string()));
        assert!(labels.contains(&"Export Metadata Index…".to_string()));
        assert!(labels.contains(&"Tag Manager".to_string()));
        assert!(!entries.iter().any(|e| matches!(
            e,
            MenuEntry::Item(MenuItem {
                action: Action::Trash(_),
                ..
            })
        )));
    }

    #[test]
    fn context_menu_shows_sort_key_direction_and_toggle() {
        let entries = menu_entries(
            &ContextTarget::Background(PathBuf::from("/tmp/library")),
            MenuOptions {
                sort: SortKey::Date,
                sort_descending: true,
                ..MenuOptions::default()
            },
        );
        let labels = labels(entries.clone());
        assert!(labels.contains(&"✓ Sort by Date (newest first)".to_string()));
        assert!(labels.contains(&"  Sort by Name (A→Z)".to_string()));
        assert!(labels.contains(&"Toggle Sort Direction ↓".to_string()));
        assert!(entries.iter().any(|e| matches!(
            e,
            MenuEntry::Item(MenuItem {
                action: Action::ToggleSortDirection,
                ..
            })
        )));
    }

    #[test]
    fn shift_selects_visible_range_from_anchor() {
        let ordered = vec![
            PathBuf::from("/tmp/a"),
            PathBuf::from("/tmp/b"),
            PathBuf::from("/tmp/c"),
            PathBuf::from("/tmp/d"),
        ];
        let mut selected = vec![PathBuf::from("/tmp/b")];
        let mut anchor = Some(PathBuf::from("/tmp/b"));

        select_range(
            &mut selected,
            &mut anchor,
            &ordered,
            PathBuf::from("/tmp/d"),
            false,
        );
        assert_eq!(
            selected,
            vec![
                PathBuf::from("/tmp/b"),
                PathBuf::from("/tmp/c"),
                PathBuf::from("/tmp/d"),
            ]
        );
    }

    #[test]
    fn ctrl_shift_extends_existing_selection() {
        let ordered = vec![
            PathBuf::from("/tmp/a"),
            PathBuf::from("/tmp/b"),
            PathBuf::from("/tmp/c"),
            PathBuf::from("/tmp/d"),
        ];
        let mut selected = vec![PathBuf::from("/tmp/d")];
        let mut anchor = Some(PathBuf::from("/tmp/d"));

        select_range(
            &mut selected,
            &mut anchor,
            &ordered,
            PathBuf::from("/tmp/b"),
            true,
        );
        assert!(selected.contains(&PathBuf::from("/tmp/b")));
        assert!(selected.contains(&PathBuf::from("/tmp/c")));
        assert!(selected.contains(&PathBuf::from("/tmp/d")));
    }

    #[test]
    fn drag_payload_can_copy() {
        let payload = DragPayload {
            paths: vec![PathBuf::from("/tmp/a")],
            copy: true,
        };
        assert!(payload.copy);
    }
}
