use crate::actions::{self, Action, ClipboardMode, ContextTarget, MenuOptions, TagDialog};
use crate::theme::Theme;
use crate::gallery::{
    ClaimScope, ClaimSource, ConfidenceRange, GalleryLayout, GalleryState, Sidecar, TagEvidence,
    frame_from_time,
};
use crate::query_agent::QueryPlanner;
use crate::sidebar::FilterQuery;
use crate::source_search::{self, ReviewOutcome, SourceWorkerMsg};
use crate::tag_review::{ReviewUiState, TagReviewStore};
use crate::tag_manager::{self, AiSearchApply, TagManagerState};
use crate::file_manager::{self, FileManagerState, SortDirection, ViewMode};
use crate::local_vision::{self, LocalVisionWorkerMsg, OnnxUiConfig};
use std::collections::{HashMap, HashSet};
use std::io::{Read as _, Seek as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, UNIX_EPOCH};

use alhazen_core::{Player, PlayerConfig, PlayerState, Source, VideoFrame, PlayerEvent};
use symtropy_tags::AssertionState;

// ---------------------------------------------------------------------------
// Persistent state — remembers folder, view mode, sidebar, hidden toggle
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct VideoPlaybackMemory {
    position_secs: f64,
    duration_secs: Option<f64>,
    was_playing: bool,
    modified_secs: Option<u64>,
    file_len: Option<u64>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct FolderViewMemory {
    view_mode: Option<String>,
    sort: Option<String>,
    sort_descending: Option<bool>,
    show_hidden: Option<bool>,
    grid_scale: Option<f32>,
    list_scroll_offset: Option<f32>,
    gallery_scroll_offset: Option<f32>,
}

struct PendingAiApply {
    token: u64,
    fallback: AiSearchApply,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
struct SavedState {
    current_dir: Option<PathBuf>,
    view_mode: Option<String>,
    sort: Option<String>,
    sort_descending: Option<bool>,
    show_places: Option<bool>,
    show_hidden: Option<bool>,
    sidebar_edit_mode: Option<bool>,
    video_positions: Option<HashMap<String, VideoPlaybackMemory>>,
    video_volume: Option<f32>,
    video_muted: Option<bool>,
    video_loop: Option<bool>,
    frame_scale: Option<f32>,
    button_scale: Option<f32>,
    recent_dirs: Vec<PathBuf>,
    folder_views: HashMap<String, FolderViewMemory>,
    onnx_vision: Option<OnnxUiConfig>,
}

fn fnv1a(bytes: &[u8], seed: u64) -> u64 {
    let mut hash = seed;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn sample_fingerprint(path: &Path) -> Option<(u64, u64)> {
    const SAMPLE: u64 = 64 * 1024;
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut buf = vec![0u8; SAMPLE as usize];
    let mut hash = 0xcbf29ce484222325u64;

    for offset in [0u64, len / 2, len.saturating_sub(SAMPLE)] {
        file.seek(std::io::SeekFrom::Start(offset)).ok()?;
        let mut filled = 0;
        while filled < buf.len() {
            let n = file.read(&mut buf[filled..]).ok()?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        hash = fnv1a(&buf[..filled], hash);
    }
    Some((len, hash))
}

fn files_identical(a: &Path, b: &Path) -> bool {
    let mut left = match std::fs::File::open(a) {
        Ok(file) => file,
        Err(_) => return false,
    };
    let mut right = match std::fs::File::open(b) {
        Ok(file) => file,
        Err(_) => return false,
    };
    if left.metadata().ok().map(|m| m.len()) != right.metadata().ok().map(|m| m.len()) {
        return false;
    }

    let mut left_buf = [0u8; 64 * 1024];
    let mut right_buf = [0u8; 64 * 1024];
    loop {
        let left_read = match left.read(&mut left_buf) {
            Ok(n) => n,
            Err(_) => return false,
        };
        let right_read = match right.read(&mut right_buf) {
            Ok(n) => n,
            Err(_) => return false,
        };
        if left_read != right_read || left_buf[..left_read] != right_buf[..right_read] {
            return false;
        }
        if left_read == 0 {
            return true;
        }
    }
}

/// Group byte-identical files directly inside `dir`. A three-sample fingerprint narrows
/// the candidates; a streaming byte comparison confirms collisions, so no false positives
/// are shown to the user and arbitrarily large files never have to be loaded into memory.
fn find_duplicate_groups(dir: &Path) -> Vec<Vec<PathBuf>> {
    let mut by_fingerprint: HashMap<(u64, u64), Vec<PathBuf>> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if let Some(fingerprint) = sample_fingerprint(&path) {
                by_fingerprint.entry(fingerprint).or_default().push(path);
            }
        }
    }

    let mut groups = Vec::new();
    for (_, paths) in by_fingerprint {
        if paths.len() < 2 {
            continue;
        }
        let mut confirmed: Vec<Vec<PathBuf>> = Vec::new();
        for path in paths {
            if let Some(group) = confirmed.iter_mut().find(|g| files_identical(&g[0], &path)) {
                group.push(path);
            } else {
                confirmed.push(vec![path]);
            }
        }
        groups.extend(confirmed.into_iter().filter(|g| g.len() > 1));
    }
    for group in &mut groups {
        group.sort();
    }
    groups.sort();
    groups
}

/// Absolute, lexically normalised form of a directory path. A relative launch argument or a
/// relative saved path would otherwise leave `current_dir` relative, so the parent button,
/// path bar ancestors and history only resolve from one working directory.
fn absolute_dir(path: PathBuf) -> PathBuf {
    use std::path::Component;
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir().map(|cwd| cwd.join(&path)).unwrap_or(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() { PathBuf::from("/") } else { out }
}

fn folder_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn sort_view_mode(mode: &ViewMode) -> &'static str {
    match mode {
        ViewMode::List => "list",
        ViewMode::Gallery => "gallery",
        ViewMode::Masonry => "masonry",
    }
}

fn sort_name(sort: file_manager::SortKey) -> &'static str {
    match sort {
        file_manager::SortKey::Name => "name",
        file_manager::SortKey::Date => "date",
        file_manager::SortKey::Size => "size",
    }
}

fn parse_sort(name: Option<&str>) -> file_manager::SortKey {
    match name.unwrap_or("").to_ascii_lowercase().as_str() {
        "date" => file_manager::SortKey::Date,
        "size" => file_manager::SortKey::Size,
        _ => file_manager::SortKey::Name,
    }
}

fn sort_direction_bool(direction: SortDirection) -> bool {
    direction == SortDirection::Descending
}

fn parse_direction(descending: Option<bool>, sort: file_manager::SortKey) -> SortDirection {
    match descending {
        Some(true) => SortDirection::Descending,
        Some(false) => SortDirection::Ascending,
        None => sort.default_direction(),
    }
}

fn parse_view_mode(name: Option<&str>) -> Option<ViewMode> {
    match name.unwrap_or("").to_ascii_lowercase().as_str() {
        "list" => Some(ViewMode::List),
        "gallery" => Some(ViewMode::Gallery),
        "masonry" => Some(ViewMode::Masonry),
        _ => None,
    }
}

fn gallery_layout(mode: &ViewMode) -> GalleryLayout {
    match mode {
        ViewMode::Masonry => GalleryLayout::Masonry,
        _ => GalleryLayout::Rows,
    }
}

impl SavedState {
    fn config_path() -> PathBuf {
        crate::paths::state_path()
    }

    fn load() -> Self {
        std::fs::read_to_string(Self::config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
    }

    fn save(&self) {
        if let Ok(s) = serde_json::to_string_pretty(self) {
            if std::fs::read_to_string(Self::config_path()).ok().as_deref() == Some(s.as_str()) {
                return;
            }
            let _ = std::fs::write(Self::config_path(), s);
        }
    }
}

/// One reversible file operation. Destinations are the actual paths produced by the
/// operation (unique names included), not just the requested ones.
#[derive(Clone, Debug)]
enum UndoOp {
    Transfer { pairs: Vec<(PathBuf, PathBuf)>, copy: bool },
    Rename { from: PathBuf, to: PathBuf },
    Create { path: PathBuf, is_dir: bool },
}

/// Filesystem half of undo, kept separate from UI state so it can be tested directly.
fn undo_files(op: &UndoOp) -> Result<String, String> {
    match op {
        UndoOp::Transfer { pairs, copy } => {
            if *copy {
                let dests: Vec<PathBuf> = pairs.iter().map(|(_, to)| to.clone()).collect();
                file_manager::remove_items(&dests).map(|n| format!("Undid {} copied item(s)", n))
            } else {
                let mut done = 0;
                for (from, to) in pairs {
                    if std::fs::symlink_metadata(to).is_err() {
                        continue;
                    }
                    if let Some(parent) = from.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                    }
                    std::fs::rename(to, from).map_err(|e| format!("{}: {}", to.display(), e))?;
                    done += 1;
                }
                Ok(format!("Undid {} move(s)", done))
            }
        }
        UndoOp::Rename { from, to } => {
            let name = from
                .file_name()
                .ok_or_else(|| "undo: invalid source name".to_string())?;
            file_manager::rename(to, &name.to_string_lossy()).map(|_| "Undid rename".to_string())
        }
        UndoOp::Create { path, .. } => {
            file_manager::remove_items(std::slice::from_ref(path)).map(|_| "Undid create".to_string())
        }
    }
}

/// Filesystem half of redo.
fn redo_files(op: &UndoOp) -> Result<String, String> {
    match op {
        UndoOp::Transfer { pairs, copy } => {
            let mut done = 0;
            for (from, to) in pairs {
                let dir = to.parent().unwrap_or_else(|| Path::new("."));
                done += file_manager::transfer(std::slice::from_ref(from), dir, *copy)?.len();
            }
            Ok(format!("Redid {} item(s)", done))
        }
        UndoOp::Rename { from, to } => {
            let name = to
                .file_name()
                .ok_or_else(|| "redo: invalid target name".to_string())?;
            file_manager::rename(from, &name.to_string_lossy()).map(|_| "Redid rename".to_string())
        }
        UndoOp::Create { path, is_dir } => {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            let name = path
                .file_name()
                .ok_or_else(|| "redo: invalid name".to_string())?;
            file_manager::create_item(parent, &name.to_string_lossy(), *is_dir)
                .map(|_| "Redid create".to_string())
        }
    }
}

#[derive(Clone, Debug)]
enum Dialog {
    Rename { path: PathBuf, text: String },
    NewFolder { dir: PathBuf, text: String },
    NewFile { dir: PathBuf, text: String },
    Properties { paths: Vec<PathBuf> },
    EditTag { paths: Vec<PathBuf>, remove: bool, text: String },
    Tag { kind: TagDialog, text: String },
    Duplicates { groups: Vec<Vec<PathBuf>> },
    ExportMetadata { path: String },
}

fn dedup_tags(tags: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    tags.into_iter().filter(|tag| seen.insert(tag.clone())).collect()
}

/// Rewrite one tag's name, its subtree's ancestor chain, and every per-image
/// parent link in a sidecar. `to` is `None` when the tag is deleted.
fn remap_subtree_in_sidecar(
    sc: &mut Sidecar,
    descendants: &HashSet<String>,
    from: &str,
    to: Option<&str>,
    old_chain: &[String],
    new_chain: &[String],
) -> bool {
    if !sc.tags.iter().any(|tag| descendants.contains(tag)) {
        return false;
    }

    let before_tags = sc.tags.clone();
    let before_links = sc.tag_parents.clone();

    let mut tags = Vec::with_capacity(sc.tags.len() + new_chain.len());
    for tag in sc.tags.drain(..) {
        if tag == from {
            if let Some(replacement) = to {
                tags.push(replacement.to_string());
            }
        } else if descendants.contains(&tag)
            || new_chain.contains(&tag)
            || !old_chain.contains(&tag)
        {
            tags.push(tag);
        }
    }
    for ancestor in new_chain {
        if !tags.contains(ancestor) {
            tags.push(ancestor.clone());
        }
    }
    sc.tags = dedup_tags(tags);

    let mut links = std::mem::take(&mut sc.tag_parents);
    links.retain(|child, parent| {
        let child_stale = old_chain.contains(child)
            && !new_chain.contains(child)
            && Some(child.as_str()) != to;
        let parent_stale = old_chain.contains(parent)
            && !new_chain.contains(parent)
            && Some(parent.as_str()) != to;
        !child_stale && !parent_stale && child != parent
    });

    let mut remapped = HashMap::with_capacity(links.len() + new_chain.len() + 1);
    for (child, parent) in links {
        let child = if child == from { to.map(str::to_string) } else { Some(child) };
        let parent = if parent == from { to.map(str::to_string) } else { Some(parent) };
        if let (Some(child), Some(parent)) = (child, parent) {
            if child != parent {
                remapped.insert(child, parent);
            }
        }
    }
    for pair in new_chain.windows(2) {
        remapped.insert(pair[1].clone(), pair[0].clone());
    }
    if let Some(tag) = to {
        if let Some(parent) = new_chain.last() {
            remapped.insert(tag.to_string(), parent.clone());
        } else {
            remapped.remove(tag);
        }
    }
    sc.tag_parents = remapped;

    before_tags != sc.tags || before_links != sc.tag_parents
}

/// Sidecar edits are staged per full image path. The viewer and the Tag
/// Manager can target different files at the same time; a single unkeyed copy
/// would make one window display (and potentially save) the other window's tags.
#[derive(Default)]
struct StagedSidecars {
    entries: HashMap<String, Sidecar>,
}

impl StagedSidecars {
    fn get(&self, item_id: &str) -> Option<&Sidecar> {
        self.entries.get(item_id)
    }

    fn take(&mut self, item_id: &str) -> Option<Sidecar> {
        self.entries.remove(item_id)
    }

    fn insert(&mut self, item_id: impl Into<String>, sidecar: Sidecar) {
        self.entries.insert(item_id.into(), sidecar);
    }

    fn values_mut(&mut self) -> impl Iterator<Item = &mut Sidecar> {
        self.entries.values_mut()
    }
}

pub struct SymtropyApp {
    theme: Theme,
    frames: u32,
    last_persisted_frame_scale: f32,
    last_persisted_button_scale: f32,

    fm: FileManagerState,
    gallery: Option<GalleryState>,

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
    viewer_last_fit_size: Option<egui::Vec2>,
    video_memory: HashMap<String, VideoPlaybackMemory>,
    video_volume: f32,
    video_muted: bool,
    video_loop: bool,

    is_edit_mode: bool,
    tag_manager: TagManagerState,
    query_planner: QueryPlanner,
    pending_ai_apply: Option<PendingAiApply>,
    tag_review: TagReviewStore,
    review_ui: ReviewUiState,
    source_rx: Option<Receiver<SourceWorkerMsg>>,
    local_vision_rx: Option<Receiver<LocalVisionWorkerMsg>>,
    staged_sidecars: StagedSidecars,
    filter_query: FilterQuery,
    /// Toolbar visibility toggle for tag-filter scope. Not persisted: filters
    /// reset on navigation, and the default is always the current folder.
    recursive_tag_search: bool,
    path_bar: crate::pathbar::PathBarState,
    show_places: bool,

    clipboard: Vec<PathBuf>,
    clipboard_mode: ClipboardMode,
    undo_stack: Vec<UndoOp>,
    redo_stack: Vec<UndoOp>,
    recent_dirs: Vec<PathBuf>,
    folder_views: HashMap<String, FolderViewMemory>,
    dialog: Option<Dialog>,
    status: Option<String>,
}

impl SymtropyApp {
    /// Build the effective tag confidences used by the gallery filter. Sidecar
    /// tags are accepted claims; the review store adds accepted decisions and
    /// suggestions below 100%. Rejected and superseded claims are ignored.
    fn build_filter_evidence(&self, review: &TagReviewStore) -> HashMap<String, TagEvidence> {
        let Some(gallery) = &self.gallery else {
            return HashMap::new();
        };
        gallery
            .items
            .iter()
            .map(|item| {
                let mut evidence =
                    TagEvidence::from_tags(item.tags.iter().map(String::as_str)).with_item_id(&item.path);
                for relation in review.store.global_relations() {
                    evidence.insert_relation(relation.clone(), true);
                }
                if let Some(tags) = review.store.item(&item.path) {
                    for assertion in &tags.assertions {
                        if item.sidecar.is_preserved_tag(&assertion.tag) {
                            continue;
                        }
                        let source = ClaimSource::from_provenance(assertion.provenance.kind);
                        match assertion.state {
                            AssertionState::Accepted => {
                                evidence.insert_accepted_with_source(&assertion.tag, source)
                            }
                            AssertionState::Suggested => evidence.insert_suggested_with_source(
                                &assertion.tag,
                                assertion.confidence,
                                source,
                            ),
                            AssertionState::Rejected | AssertionState::Superseded => {}
                        }
                    }
                    for relation in &tags.relations {
                        evidence.insert_relation(relation.clone(), false);
                    }
                }
                (item.path.clone(), evidence)
            })
            .collect()
    }

    pub fn new(
        cc: &eframe::CreationContext<'_>,
        default_library: PathBuf,
        explicit_library: Option<PathBuf>,
    ) -> Self {
        cc.egui_ctx.options_mut(|opt| { opt.zoom_with_keyboard = false; });

        if let Some(p) = &explicit_library {
            if !p.is_dir() {
                eprintln!("app: library '{}' is not a directory, ignoring", p.display());
            }
        }

        // Explicit argument/env > remembered directory > runtime-resolved default.
        let saved = SavedState::load();
        let filter_query = FilterQuery::default();
        let initial_dir = explicit_library
        .filter(|p| p.is_dir())
        .or_else(|| saved.current_dir.clone().filter(|p| p.exists()))
        .unwrap_or(default_library);
        let initial_dir = absolute_dir(initial_dir);

        let mut fm = FileManagerState::new(initial_dir);
        fm.view_mode = parse_view_mode(saved.view_mode.as_deref()).unwrap_or(ViewMode::Gallery);
        fm.show_hidden = saved.show_hidden.unwrap_or(false);
        let saved_sort = parse_sort(saved.sort.as_deref());
        fm.set_sort(saved_sort, parse_direction(saved.sort_descending, saved_sort));
        let initial_view = saved.folder_views.get(&folder_key(&fm.current_dir)).cloned();
        if let Some(view) = &initial_view {
            if let Some(mode) = parse_view_mode(view.view_mode.as_deref()) {
                fm.view_mode = mode;
            }
            if let Some(sort) = &view.sort {
                let sort = parse_sort(Some(sort));
                fm.set_sort(sort, parse_direction(view.sort_descending, sort));
            }
            if let Some(hidden) = view.show_hidden {
                fm.show_hidden = hidden;
                fm.refresh();
            }
        }

        let mut gallery = if fm.view_mode.is_gallery() {
            Some(GalleryState::scan_dir(&cc.egui_ctx, &fm.current_dir))
        } else {
            None
        };
        if let (Some(g), Some(view)) = (gallery.as_mut(), initial_view.as_ref()) {
            if let Some(scale) = view.grid_scale { g.grid_scale = scale; }
            if let Some(offset) = view.gallery_scroll_offset { g.scroll_offset = offset; }
        }
        if let Some(g) = gallery.as_mut() {
            g.set_layout(gallery_layout(&fm.view_mode));
            g.set_sort(fm.sort, fm.sort_direction);
        }

        let theme = Theme::load(&cc.egui_ctx);
        let frame_scale = saved.frame_scale.unwrap_or(1.0);
        let button_scale = saved.button_scale.unwrap_or(1.0);
        theme.set_frame_scale(frame_scale);
        theme.set_button_scale(button_scale);

        let mut app = Self {
            theme,
            frames: 0,
            last_persisted_frame_scale: frame_scale, last_persisted_button_scale: button_scale,
            fm, gallery,
            viewer_open: false, viewer_cache: HashMap::new(), viewer_player: None,
            viewer_loaded_path: None, viewer_zoom: 1.0, viewer_pan: egui::Vec2::ZERO,
            viewer_anim_frames: Vec::new(), viewer_anim_durations: Vec::new(),
            viewer_show_sidebar: true, viewer_sidebar_width: 500.0, viewer_needs_fit: false,
            viewer_last_fit_size: None,
            video_memory: saved.video_positions.unwrap_or_default(),
            video_volume: saved.video_volume.unwrap_or(1.0),
            video_muted: saved.video_muted.unwrap_or(false),
            video_loop: saved.video_loop.unwrap_or(false),
            is_edit_mode: saved.sidebar_edit_mode.unwrap_or(false),
            tag_manager: TagManagerState::default(),
            query_planner: QueryPlanner::default(),
            pending_ai_apply: None,
            tag_review: TagReviewStore::load(),
            review_ui: ReviewUiState {
                onnx: saved.onnx_vision.unwrap_or_default(),
                ..ReviewUiState::default()
            },
            source_rx: None,
            local_vision_rx: None,
            staged_sidecars: StagedSidecars::default(), filter_query,
            recursive_tag_search: false,
            path_bar: crate::pathbar::PathBarState::default(),
            show_places: saved.show_places.unwrap_or(true),
            clipboard: Vec::new(), clipboard_mode: ClipboardMode::Copy,
            undo_stack: Vec::new(), redo_stack: Vec::new(),
            recent_dirs: saved.recent_dirs.into_iter().filter(|p| p.is_dir()).take(20).collect(),
            folder_views: saved.folder_views,
            dialog: None, status: None,
        };
        let current = app.fm.current_dir.clone();
        app.remember_recent_dir(&current);
        if let Ok(target) = std::env::var("SYMTROPY_OPEN_VIEWER") {
            app.viewer_open = true;
            if let Some(g) = app.gallery.as_mut() {
                g.viewer_target = Some(target);
            }
        }
        app
    }

    fn set_status(&mut self, message: impl Into<String>) {
        self.status = Some(message.into());
    }

    /// Persist the staged sidecar after an explicit source import. The import
    /// button is the user decision; this only stores its provenance result.
    fn persist_staged_sidecar(&mut self, item_id: &str) -> bool {
        let Some(sidecar) = self.staged_sidecars.get(item_id).cloned() else { return false };
        let index_path = crate::gallery::workers::cache_dir().join("index.tsv");
        let mut found = false;
        if let Some(g) = &mut self.gallery {
            if let Some(item) = g.items.iter_mut().find(|item| item.path == item_id) {
                item.sidecar = sidecar.clone();
                item.tags = sidecar.active_tags().into_iter().collect();
                item.rating = sidecar.rating;
                let _ = item.sidecar.save(Path::new(&item.path));
                crate::gallery::workers::update_index_entry(&index_path, &item.path, &item.sidecar);
                found = true;
            }
            g.rebuild_tag_tree();
        }
        found
    }

    /// Paths offered by the native Tag Manager target picker. Selection comes
    /// first so a right-click review targets the image the user actually chose.
    fn tag_manager_targets(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut push = |path: &Path| {
            let value = path.to_string_lossy().into_owned();
            if !value.is_empty() && seen.insert(value.clone()) {
                out.push(value);
            }
        };
        for path in self.selected_paths() {
            if !path.is_dir() {
                push(&path);
            }
        }
        if let Some(gallery) = &self.gallery {
            for item in &gallery.items {
                push(Path::new(&item.path));
            }
        } else {
            for entry in &self.fm.entries {
                if !entry.is_dir {
                    push(&entry.path);
                }
            }
        }
        out
    }

    /// Stage an image's sidecar exactly once, keyed by its full path. Existing
    /// edits stay in place; a new path is loaded and imported into the review
    /// store without touching any other staged image.
    fn ensure_staged_sidecar(&mut self, item_id: &str) -> Sidecar {
        if let Some(sidecar) = self.staged_sidecars.get(item_id) {
            return sidecar.clone();
        }
        let sidecar = self.sidecar_for_path(Path::new(item_id));
        self.staged_sidecars.insert(item_id.to_string(), sidecar.clone());
        let _ = self.tag_review.import_sidecar(item_id, &sidecar);
        let _ = self.tag_review.capture_sidecar_sources(item_id, &sidecar);
        sidecar
    }

    /// Temporarily remove one image's staged sidecar for a viewport draw. The
    /// caller must put it back under the same path when the draw finishes.
    fn take_staged_sidecar(&mut self, item_id: &str) -> Option<Sidecar> {
        let _ = self.ensure_staged_sidecar(item_id);
        self.staged_sidecars.take(item_id)
    }

    fn open_tag_manager(&mut self, target: Option<PathBuf>) {
        // Build the 30k-path picker once per open instead of once per frame.
        let targets = self.tag_manager_targets();
        self.tag_manager.targets.set_paths(targets);
        let target = target
            .or_else(|| self.selected_paths().into_iter().find(|path| !path.is_dir()))
            .or_else(|| self.viewer_loaded_path.as_ref().map(PathBuf::from))
            .or_else(|| {
                self.gallery.as_ref().and_then(|g| {
                    g.items
                        .iter()
                        .find(|item| !item.sidecar.tags.is_empty())
                        .or_else(|| g.items.first())
                        .map(|item| PathBuf::from(&item.path))
                })
            })
            .map(|path| path.to_string_lossy().into_owned());
        if let Some(target) = target {
            if self.tag_manager.item.as_deref() != Some(target.as_str()) {
                let _ = self.ensure_staged_sidecar(&target);
                self.tag_manager.item = Some(target.clone());
            }
        }
        self.tag_manager.open = true;
    }

    fn toggle_tag_manager(&mut self) {
        if self.tag_manager.open {
            self.tag_manager.open = false;
        } else {
            self.open_tag_manager(None);
        }
    }

    /// Apply the review window's deferred effects: sidecar writes and worker
    /// launches. The manager itself never starts a worker or writes a sidecar.
    fn apply_review_outcome(
        &mut self,
        ctx: &egui::Context,
        item_id: &str,
        outcome: ReviewOutcome,
        staged_sidecar: Option<Sidecar>,
    ) {
        if outcome.synced || outcome.preservation_changed || outcome.source_attached {
            if let Some(sidecar) = staged_sidecar {
                let index_path = crate::gallery::workers::cache_dir().join("index.tsv");
                if let Some(gallery) = &mut self.gallery {
                    if let Some(item) = gallery.items.iter_mut().find(|item| item.path == item_id) {
                        item.sidecar = sidecar.clone();
                        item.tags = sidecar.active_tags().into_iter().collect();
                        item.rating = sidecar.rating;
                        let _ = item.sidecar.save(Path::new(&item.path));
                        crate::gallery::workers::update_index_entry(&index_path, &item.path, &item.sidecar);
                    }
                    gallery.rebuild_tag_tree();
                }
                self.sync_tag_revision_to_gallery();
            }
            if outcome.source_attached {
                self.set_status("Source attachment saved to sidecar");
            } else if outcome.preservation_changed {
                self.set_status("Preserved tags updated in sidecar");
            } else {
                self.set_status("Accepted tag claims synced to sidecar");
            }
        }

        if let Some(job) = outcome.source_search {
            if self.source_rx.is_none() {
                self.source_rx = Some(source_search::spawn_source_search(ctx, job));
            } else {
                self.review_ui.source_busy = false;
                self.set_status("Another source task is already running");
            }
        }
        if let Some(job) = outcome.source_import {
            if self.source_rx.is_none() {
                self.source_rx = Some(source_search::spawn_source_import(ctx, job));
            } else {
                self.review_ui.source_busy = false;
                self.set_status("Another source task is already running");
            }
        }
        if let Some(job) = outcome.local_vision {
            if self.local_vision_rx.is_none() {
                self.local_vision_rx = Some(local_vision::spawn_local_vision(ctx, job));
            } else {
                self.set_status("Another local vision task is already running");
            }
        }
    }

    /// One gallery cache invalidation path for every tag-store writer (review
    /// window, worker result, or import), instead of viewer-local copies.
    fn sync_tag_revision_to_gallery(&mut self) {
        let revision = self.tag_review.store.revision();
        if self.gallery.as_ref().is_some_and(|gallery| gallery.filter_revision == revision) {
            return;
        }
        let evidence = self.build_filter_evidence(&self.tag_review);
        if let Some(gallery) = &mut self.gallery {
            gallery.filter_evidence = evidence;
            gallery.invalidate_filter_cache();
            gallery.filter_revision = revision;
        }
    }

    /// Materialize descendant items only while a tag filter and the visible
    /// recursive toggle are both active. Rows come from the global index, so
    /// this never reads or writes sidecars.
    fn sync_recursive_tag_search(&mut self) {
        let Some(gallery) = self.gallery.as_mut() else { return };
        if gallery.recursive_tag_search != self.recursive_tag_search {
            gallery.recursive_tag_search = self.recursive_tag_search;
            gallery.clear_recursive_items();
        }
        let wants_descendants = gallery.recursive_tag_search
            && !matches!(gallery.active_query, crate::gallery::Query::AlwaysTrue);
        if !wants_descendants {
            gallery.clear_recursive_items();
            return;
        }
        let root = self.fm.current_dir.clone();
        if gallery.recursive_root.as_deref() == Some(root.as_path()) {
            return;
        }
        let existing: std::collections::HashSet<String> = gallery
            .items
            .iter()
            .map(|item| item.path.clone())
            .collect();
        let extras = crate::gallery::workers::recursive_index_items(
            &crate::paths::cache_dir().join("index.tsv"),
            &root,
            &existing,
        );
        gallery.add_recursive_items(root, extras);
        let evidence = self.build_filter_evidence(&self.tag_review);
        if let Some(gallery) = &mut self.gallery {
            gallery.filter_evidence = evidence;
            gallery.invalidate_filter_cache();
        }
    }

    /// Poll the at-most-one source worker. This runs before the viewer takes
    /// its local metadata snapshot, so results cannot be overwritten by it.
    fn poll_source_workers(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.source_rx.take() else { return };
        match rx.try_recv() {
            Ok(SourceWorkerMsg::Search { item_id, result }) => {
                match result {
                    Ok(report) => {
                        let summary = format!(
                            "source match found {} candidate(s) for {}",
                            report.candidates.len(), item_id
                        );
                        source_search::apply_search_result(
                            &mut self.review_ui,
                            &item_id,
                            report,
                        );
                        self.set_status(summary);
                    }
                    Err(error) => {
                        self.review_ui.source_busy = false;
                        self.review_ui.source_status = format!("Search failed: {error}");
                        self.set_status(format!("Source search failed: {error}"));
                    }
                }
                ctx.request_repaint();
            }
            Ok(SourceWorkerMsg::Import { item_id, item, result }) => {
                let active_target = self.viewer_loaded_path.as_deref() == Some(item_id.as_str())
                    || self.tag_manager.item.as_deref() == Some(item_id.as_str());
                if active_target {
                    let staged = self.staged_sidecars.get(&item_id).cloned()
                        .unwrap_or_else(|| self.sidecar_for_path(Path::new(&item_id)));
                    let mut staged = Some(staged);
                    let cached = source_search::apply_import_result(
                        &mut self.review_ui,
                        &mut staged,
                        &item,
                        result,
                    );
                    if let Some(sidecar) = staged {
                        self.staged_sidecars.insert(item_id.clone(), sidecar);
                    }
                    if !cached {
                        let status = self.review_ui.source_status.clone();
                        self.set_status(status);
                    } else if self.persist_staged_sidecar(&item_id) {
                        self.set_status(format!(
                            "Cached {} from {}; source provenance saved to sidecar",
                            item.remote_id,
                            item.site.as_str()
                        ));
                    } else {
                        self.set_status(format!(
                            "Cached {} from {}; no open sidecar was available for provenance",
                            item.remote_id,
                            item.site.as_str()
                        ));
                    }
                } else {
                    self.review_ui.source_busy = false;
                    self.set_status(format!(
                        "Source image for {item_id} was cached, but that image is no longer open"
                    ));
                }
                ctx.request_repaint();
            }
            Err(TryRecvError::Empty) => self.source_rx = Some(rx),
            Err(TryRecvError::Disconnected) => {
                self.review_ui.source_busy = false;
            }
        }
    }

    /// Poll the at-most-one local vision worker. Results are staged as
    /// suggestions; accepted or locked claims are protected by the store.
    fn poll_local_vision(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.local_vision_rx.take() else { return };
        match rx.try_recv() {
            Ok(LocalVisionWorkerMsg::Suggestions { item_id, result }) => {
                match result {
                    Ok(suggestions) => {
                        let (applied, rejected) =
                            self.tag_review.stage_local_suggestions(&item_id, &suggestions);
                        if applied > 0 {
                            let evidence = self.build_filter_evidence(&self.tag_review);
                            if let Some(gallery) = &mut self.gallery {
                                gallery.filter_evidence = evidence;
                            }
                            self.set_status(format!(
                                "Staged {applied} local suggestion(s) for review ({rejected} rejected)"
                            ));
                        } else {
                            self.set_status(format!(
                                "No new local suggestions ({rejected} rejected)"
                            ));
                        }
                    }
                    Err(error) => self.set_status(format!("Local suggestion failed: {error}")),
                }
                ctx.request_repaint();
            }
            Err(TryRecvError::Empty) => self.local_vision_rx = Some(rx),
            Err(TryRecvError::Disconnected) => {}
        }
    }

    fn poll_query_planner(&mut self) {
        let Some(result) = self.query_planner.poll() else { return };
        let Some(pending) = self.pending_ai_apply.take() else { return };
        if pending.token != result.id {
            self.pending_ai_apply = Some(pending);
            return;
        }
        match result.query {
            Ok(query) => {
                self.tag_manager.ai_search.status =
                    format!("Local model planned: {}", query);
                self.apply_ai_query(pending.fallback, query, "local model");
            }
            Err(error) => {
                let query = pending.fallback.query.clone();
                self.tag_manager.ai_search.status =
                    format!("Local model unavailable ({error}); used literal term: {query}");
                self.apply_ai_query(pending.fallback, query, "literal fallback");
            }
        }
    }

    fn apply_ai_query(&mut self, apply: AiSearchApply, planned: crate::gallery::Query, source: &str) {
        let evidence = self.build_filter_evidence(&self.tag_review);
        let revision = self.tag_review.store.revision();
        let final_query = if apply.mode == tag_manager::AiApplyMode::Add
            && !matches!(planned, crate::gallery::Query::AlwaysTrue)
        {
            match &self.gallery {
                Some(gallery) if !matches!(gallery.active_query, crate::gallery::Query::AlwaysTrue) => {
                    crate::gallery::Query::And(vec![
                        gallery.active_query.clone(),
                        planned.clone(),
                    ])
                }
                _ => planned.clone(),
            }
        } else {
            planned.clone()
        };
        self.filter_query.filter = final_query.to_string();
        self.filter_query.claim_scope = apply.scope;
        self.filter_query.rating_min = None;
        self.filter_query.confidence_min = apply.confidence.min;
        self.filter_query.confidence_max = apply.confidence.max;
        self.viewer_open = false;
        if let Some(gallery) = &mut self.gallery {
            gallery.active_query = final_query.clone();
            gallery.filter_confidence = apply.confidence;
            gallery.filter_scope = apply.scope;
            gallery.filter_evidence = evidence;
            gallery.filter_revision = revision;
            gallery.query_error = None;
            gallery.invalidate_filter_cache();
        }
        let verb = match apply.mode {
            tag_manager::AiApplyMode::Rebuild => "rebuilt",
            tag_manager::AiApplyMode::Add => "extended",
        };
        self.set_status(format!(
            "AI query {verb} via {source}: {} → {} · {} claims",
            apply.prompt, final_query, apply.scope
        ));
    }

    fn persist_state(&self) {
        let mut folder_views = self.folder_views.clone();
        let current = self.fm.current_dir.clone();
        if !current.as_os_str().is_empty() {
            folder_views.insert(folder_key(&current), self.snapshot_current_view());
        }
        SavedState {
            current_dir: Some(self.fm.current_dir.clone()),
            view_mode: Some(match self.fm.view_mode {
                ViewMode::List => "list".into(),
                ViewMode::Gallery => "gallery".into(),
                ViewMode::Masonry => "masonry".into(),
            }),
            sort: Some(sort_name(self.fm.sort).to_string()),
            sort_descending: Some(sort_direction_bool(self.fm.sort_direction)),
            show_places: Some(self.show_places),
            show_hidden: Some(self.fm.show_hidden),
            sidebar_edit_mode: Some(self.is_edit_mode),
            video_positions: Some(self.video_memory.clone()),
            video_volume: Some(self.video_volume),
            video_muted: Some(self.video_muted),
            video_loop: Some(self.video_loop),
            frame_scale: Some(self.theme.frame_scale_setting()),
            button_scale: Some(self.theme.button_scale_setting()),
            recent_dirs: self.recent_dirs.clone(),
            folder_views,
            onnx_vision: Some(self.review_ui.onnx.clone()),
        }
        .save();
    }

    fn remember_current_video(&mut self) {
        let Some(path) = self.viewer_loaded_path.clone() else { return; };
        let Some(player) = self.viewer_player.as_ref() else { return; };
        let (modified_secs, file_len) = video_file_fingerprint(&path);
        let state = player.state();
        let ended = matches!(state, PlayerState::Ended);
        let memory = VideoPlaybackMemory {
            position_secs: if ended { 0.0 } else { player.position().as_secs_f64() },
            duration_secs: player.duration().map(|d| d.as_secs_f64()),
            was_playing: !ended && matches!(state, PlayerState::Playing | PlayerState::Buffering),
            modified_secs,
            file_len,
        };
        if self.video_memory.get(&path) != Some(&memory) {
            self.video_memory.insert(path, memory);
        }
    }

    fn save_video_state(&self) {
        let mut saved = SavedState::load();
        saved.video_positions = Some(self.video_memory.clone());
        saved.video_volume = Some(self.video_volume);
        saved.video_muted = Some(self.video_muted);
        saved.video_loop = Some(self.video_loop);
        saved.save();
    }

    /// Re-read the current directory after a file operation. This is the same
    /// directory, so the query stays, but *every* piece of view state must be
    /// carried over -- replacing the gallery with a pristine one used to reset
    /// zoom, scroll and selection, which then got snapshotted into folder
    /// memory and looked like a stale view when navigating back.
    fn rescan_gallery(&mut self, ctx: &egui::Context) {
        self.path_bar.invalidate_counts();
        if self.fm.view_mode == ViewMode::List {
            return;
        }
        let previous = self.gallery.take();
        let mut gallery = GalleryState::scan_dir(ctx, &self.fm.current_dir);
        if let Some(old) = previous {
            gallery.active_query = old.active_query;
            gallery.query_error = old.query_error;
            gallery.set_filename_query(&old.filename_query);
            gallery.grid_scale = old.grid_scale;
            gallery.scroll_offset = old.scroll_offset;
            gallery.selected_images = old.selected_images;
            gallery.selection_anchor = old.selection_anchor;
            gallery.keyboard_focus = old.keyboard_focus;
            gallery.viewer_target = old.viewer_target;
            gallery.tag_search_query = old.tag_search_query;
        }
        gallery.set_layout(gallery_layout(&self.fm.view_mode));
        gallery.set_sort(self.fm.sort, self.fm.sort_direction);
        self.gallery = Some(gallery);
    }

    fn snapshot_current_view(&self) -> FolderViewMemory {
        let mut view = FolderViewMemory {
            view_mode: Some(sort_view_mode(&self.fm.view_mode).to_string()),
            sort: Some(sort_name(self.fm.sort).to_string()),
            sort_descending: Some(sort_direction_bool(self.fm.sort_direction)),
            show_hidden: Some(self.fm.show_hidden),
            list_scroll_offset: Some(self.fm.list_scroll_offset),
            ..Default::default()
        };
        if let Some(g) = &self.gallery {
            view.grid_scale = Some(g.grid_scale);
            view.gallery_scroll_offset = Some(g.scroll_offset);
        }
        view
    }

    fn remember_recent_dir(&mut self, path: &Path) {
        self.recent_dirs.retain(|p| p != path);
        self.recent_dirs.insert(0, path.to_path_buf());
        self.recent_dirs.truncate(20);
    }

    fn finish_navigation(&mut self, ctx: &egui::Context, saved_view: Option<&FolderViewMemory>) {
        let path = self.fm.current_dir.clone();
        self.remember_recent_dir(&path);
        self.path_bar.invalidate_counts();
        self.viewer_open = false;
        self.status = None;
        // Filters are per-view and deliberately never persisted or carried
        // across folders, including the sidebar's typed expression and the
        // toolbar's recursive filename search.
        self.filter_query = FilterQuery::default();
        let had_filename_search = !self.fm.search_query.is_empty();
        self.fm.search_query.clear();
        self.fm.last_search_query.clear();
        if had_filename_search {
            self.fm.refresh();
        }

        if let Some(view) = saved_view {
            if let Some(mode) = parse_view_mode(view.view_mode.as_deref()) {
                self.fm.view_mode = mode;
            }
            if let Some(sort) = &view.sort {
                let sort = parse_sort(Some(sort));
                self.fm.set_sort(sort, parse_direction(view.sort_descending, sort));
            }
            if let Some(hidden) = view.show_hidden {
                self.fm.show_hidden = hidden;
                self.fm.refresh();
            }
            if let Some(offset) = view.list_scroll_offset {
                self.fm.list_scroll_offset = offset;
            }
        }

        // Filters belong to the folder being viewed, never to the app. Copying
        // the outgoing query here made every path-bar jump inherit the previous
        // folder's filter, so the destination looked empty or "broken".
        self.gallery = if self.fm.view_mode.is_gallery() {
            let mut g = GalleryState::scan_dir(ctx, &self.fm.current_dir);
            if let Some(view) = saved_view {
                if let Some(scale) = view.grid_scale { g.grid_scale = scale; }
                if let Some(offset) = view.gallery_scroll_offset { g.scroll_offset = offset; }
            }
            g.set_layout(gallery_layout(&self.fm.view_mode));
            g.set_sort(self.fm.sort, self.fm.sort_direction);
            Some(g)
        } else {
            None
        };

        self.persist_state();
    }

    /// Navigate once, preserving the view of the folder being left and restoring the
    /// saved view of the destination. All fresh navigation entry points funnel through here.
    fn navigate_to(&mut self, ctx: &egui::Context, path: PathBuf) {
        // Relative targets are resolved against the folder being viewed, never the process's
        // launch directory, and a failed navigation is no longer completely silent.
        let path = if path.is_absolute() {
            path
        } else {
            absolute_dir(self.fm.current_dir.join(&path))
        };
        if !path.is_dir() {
            eprintln!("navigate: '{}' is not a directory", path.display());
            return;
        }
        let from = self.fm.current_dir.clone();
        if !from.as_os_str().is_empty() {
            self.folder_views.insert(folder_key(&from), self.snapshot_current_view());
        }

        self.fm.navigate_to(path);
        let saved_view = self.folder_views.get(&folder_key(&self.fm.current_dir)).cloned();
        self.finish_navigation(ctx, saved_view.as_ref());
    }

    /// Walk the existing history in either direction without truncating it.
    fn navigate_history(&mut self, ctx: &egui::Context, forward: bool) {
        let from = self.fm.current_dir.clone();
        if !from.as_os_str().is_empty() {
            self.folder_views.insert(folder_key(&from), self.snapshot_current_view());
        }
        if forward {
            self.fm.go_forward();
        } else {
            self.fm.go_back();
        }
        if self.fm.current_dir == from {
            return;
        }
        let saved_view = self.folder_views.get(&folder_key(&self.fm.current_dir)).cloned();
        self.finish_navigation(ctx, saved_view.as_ref());
    }

    fn selected_paths(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut push = |p: &Path| {
            if !out.iter().any(|existing| existing == p) {
                out.push(p.to_path_buf());
            }
        };
        if let Some(g) = &self.gallery {
            for p in &g.selected_images {
                push(Path::new(p));
            }
            if let Some(target) = &g.viewer_target {
                push(Path::new(target));
            }
        }
        for p in &self.fm.selected {
            push(p);
        }
        out
    }

    fn sidecar_for_path(&self, path: &Path) -> Sidecar {
        self.gallery
            .as_ref()
            .and_then(|g| g.items.iter().find(|item| Path::new(&item.path) == path))
            .map(|item| item.sidecar.clone())
            .or_else(|| Sidecar::load(path))
            .unwrap_or_default()
    }

    fn sync_sidecar_into_gallery(&mut self, path: &Path, sc: &Sidecar) {
        if let Some(g) = &mut self.gallery {
            let changed = if let Some(item) = g.items.iter_mut().find(|item| Path::new(&item.path) == path) {
                item.sidecar = sc.clone();
                item.tags = sc.active_tags().into_iter().collect();
                item.rating = sc.rating;
                true
            } else {
                false
            };
            if changed {
                g.invalidate_filter_cache();
            }
        }
    }

    fn set_rating_for_paths(&mut self, paths: &[PathBuf], rating: u8) {
        let index_path = crate::gallery::workers::cache_dir().join("index.tsv");
        let mut changed = 0;
        for path in paths {
            if path.is_dir() {
                continue;
            }
            let mut sc = self.sidecar_for_path(path);
            if sc.rating == rating {
                continue;
            }
            sc.rating = rating;
            if sc.save(path).is_err() {
                continue;
            }
            self.sync_sidecar_into_gallery(path, &sc);
            crate::gallery::workers::update_index_entry(&index_path, &path.to_string_lossy(), &sc);
            changed += 1;
        }
        self.set_status(format!("Set rating on {} image(s)", changed));
    }

    fn show_paths_in_gallery(&mut self, ctx: &egui::Context, paths: &[PathBuf]) {
        let index_path = crate::gallery::workers::cache_dir().join("index.tsv");
        let mut changed = 0;
        for path in paths {
            if path.is_dir() {
                continue;
            }
            let mut sc = self.sidecar_for_path(path);
            if !sc.is_display_suppressed() {
                continue;
            }
            sc.properties.remove("display");
            if sc.save(path).is_err() {
                continue;
            }
            self.sync_sidecar_into_gallery(path, &sc);
            crate::gallery::workers::update_index_entry(&index_path, &path.to_string_lossy(), &sc);
            changed += 1;
        }
        if changed > 0 {
            self.rescan_gallery(ctx);
        }
        self.set_status(format!("Restored {} image(s) to the gallery", changed));
    }

    fn apply_tag_for_paths(&mut self, paths: &[PathBuf], tag: &str, remove: bool) {
        let tag = tag.trim();
        if tag.is_empty() {
            return;
        }
        let index_path = crate::gallery::workers::cache_dir().join("index.tsv");
        let mut changed = 0;
        for path in paths {
            if path.is_dir() {
                continue;
            }
            let mut sc = self.sidecar_for_path(path);
            let before = sc.tags.len();
            if remove {
                sc.tags.retain(|t| t != tag);
                sc.tag_parents.remove(tag);
            } else if !sc.tags.iter().any(|t| t == tag) {
                sc.tags.push(tag.to_string());
            }
            if sc.tags.len() == before {
                continue;
            }
            if sc.save(path).is_err() {
                continue;
            }
            self.sync_sidecar_into_gallery(path, &sc);
            crate::gallery::workers::update_index_entry(&index_path, &path.to_string_lossy(), &sc);
            changed += 1;
        }
        self.set_status(format!(
            "{} tag '{}' on {} image(s)",
            if remove { "Removed" } else { "Added" },
            tag,
            changed
        ));
    }

    /// Bulk-rewrite sidecars with the same save + index pattern as tag edits.
    fn rewrite_sidecars<F>(&mut self, transform: F) -> usize
    where
        F: Fn(&mut Sidecar) -> bool,
    {
        let paths: Vec<PathBuf> = self
            .gallery
            .as_ref()
            .map(|g| {
                g.items
                    .iter()
                    .filter(|item| !item.is_folder)
                    .map(|item| PathBuf::from(&item.path))
                    .collect()
            })
            .unwrap_or_default();
        let root = self
            .gallery
            .as_ref()
            .and_then(|g| g.tags_path.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| self.fm.current_dir.clone());
        let index_path = crate::gallery::workers::cache_dir().join("index.tsv");

        let mut local_changed = 0;
        for path in paths {
            let mut sc = self.sidecar_for_path(&path);
            if !transform(&mut sc) {
                continue;
            }
            if sc.save(&path).is_err() {
                continue;
            }
            self.sync_sidecar_into_gallery(&path, &sc);
            crate::gallery::workers::update_index_entry(&index_path, &path.to_string_lossy(), &sc);
            local_changed += 1;
        }

        // Tags are global: rewrite every halo sidecar under the library root so
        // warm launches of other folders do not resurrect the old name.
        let global_changed =
            crate::gallery::workers::rewrite_tag_metadata(&root, &index_path, &transform);
        global_changed.max(local_changed)
    }

    fn refresh_filter_after_tag(&mut self, from: &str, to: Option<&str>) {
        let from_key = crate::gallery::hierarchy::tag_key(from);
        let to_key = to.map(crate::gallery::hierarchy::tag_key);
        let rewritten = crate::sidebar::replace_filter_text(&self.filter_query.filter, from, to);
        if let Some(g) = &mut self.gallery {
            let from_key = from_key.clone();
            let to_key = to_key.clone();
            g.active_query = g.active_query.map_tags(&|tag| {
                if crate::gallery::hierarchy::tag_key(tag) == from_key {
                    to_key.clone()
                } else {
                    Some(tag.to_string())
                }
            });
            g.query_error = None;
            g.invalidate_filter_cache();
            let still_valid = crate::gallery::Query::parse(&rewritten).is_ok();
            if still_valid {
                self.filter_query.filter = rewritten;
            } else {
                // Removing a tag can leave a dangling operator. The active
                // query AST is still valid, so rebuild the text from it.
                self.filter_query.filter = g.active_query.to_string();
                self.filter_query.rating_min = None;
                self.filter_query.claim_scope = ClaimScope::Curated;
                g.filter_scope = ClaimScope::Curated;
            }
        } else {
            self.filter_query.filter = rewritten;
        }
    }

    fn perform_tag_dialog(&mut self, kind: TagDialog, text: String) {
        let Some(g) = self.gallery.as_mut() else {
            self.set_status("Open a gallery folder to manage tags");
            return;
        };

        match kind {
            TagDialog::New { parent } => {
                let name = text.trim().to_string();
                if let Err(error) = g.hierarchy.insert(&name, parent.as_deref()) {
                    self.set_status(format!("Cannot create tag: {}", error));
                    return;
                }
                if let Err(error) = g.commit_tag_hierarchy() {
                    self.set_status(format!("Tag created but tags.txt could not be written: {}", error));
                } else {
                    self.set_status(format!("Created tag '{}'", name));
                }
                return;
            }
            TagDialog::Rename { tag } => {
                let new_name = text.trim().to_string();
                let old_chain = g.hierarchy.ancestors_of(&tag);
                let descendants: HashSet<String> = g.hierarchy.descendants_of(&tag).into_iter().collect();
                if let Err(error) = g.hierarchy.rename(&tag, &new_name) {
                    self.set_status(format!("Cannot rename tag: {}", error));
                    return;
                }
                let changed = self.rewrite_sidecars(|sc| {
                    remap_subtree_in_sidecar(
                        sc, &descendants, &tag, Some(&new_name), &old_chain, &old_chain,
                    )
                });
                for sc in self.staged_sidecars.values_mut() {
                    remap_subtree_in_sidecar(
                        sc, &descendants, &tag, Some(&new_name), &old_chain, &old_chain,
                    );
                }
                self.refresh_filter_after_tag(&tag, Some(&new_name));
                if let Some(g) = &mut self.gallery {
                    if let Err(error) = g.commit_tag_hierarchy() {
                        self.set_status(format!("Tag renamed but tags.txt could not be written: {}", error));
                        return;
                    }
                }
                self.set_status(format!("Renamed '{}' to '{}' on {} image(s)", tag, new_name, changed));
            }
            TagDialog::Move { tag } => {
                let parent_name = text.trim();
                let new_parent = if parent_name.is_empty() { None } else { Some(parent_name) };
                let old_chain = g.hierarchy.ancestors_of(&tag);
                let descendants: HashSet<String> = g.hierarchy.descendants_of(&tag).into_iter().collect();
                let new_chain: Vec<String> = match &new_parent {
                    Some(parent) => {
                        let mut chain = g.hierarchy.ancestors_of(parent);
                        chain.push(parent.to_string());
                        chain
                    }
                    None => Vec::new(),
                };
                if let Err(error) = g.hierarchy.move_tag(&tag, new_parent.as_deref()) {
                    self.set_status(format!("Cannot move tag: {}", error));
                    return;
                }
                let changed = self.rewrite_sidecars(|sc| {
                    remap_subtree_in_sidecar(
                        sc, &descendants, &tag, Some(&tag), &old_chain, &new_chain,
                    )
                });
                for sc in self.staged_sidecars.values_mut() {
                    remap_subtree_in_sidecar(sc, &descendants, &tag, Some(&tag), &old_chain, &new_chain);
                }
                if let Some(g) = &mut self.gallery {
                    if let Err(error) = g.commit_tag_hierarchy() {
                        self.set_status(format!("Tag moved but tags.txt could not be written: {}", error));
                        return;
                    }
                }
                self.set_status(format!(
                    "Moved '{}' under {} on {} image(s)",
                    tag,
                    new_parent.as_deref().unwrap_or("the top level"),
                    changed
                ));
            }
            TagDialog::Merge { source } => {
                let target = text.trim().to_string();
                let old_chain = g.hierarchy.ancestors_of(&source);
                let descendants: HashSet<String> = g.hierarchy.descendants_of(&source).into_iter().collect();
                let new_chain = {
                    let mut chain = g.hierarchy.ancestors_of(&target);
                    chain.push(target.clone());
                    chain
                };
                if let Err(error) = g.hierarchy.merge(&source, &target) {
                    self.set_status(format!("Cannot merge tags: {}", error));
                    return;
                }
                let changed = self.rewrite_sidecars(|sc| {
                    remap_subtree_in_sidecar(
                        sc, &descendants, &source, Some(&target), &old_chain, &new_chain,
                    )
                });
                for sc in self.staged_sidecars.values_mut() {
                    remap_subtree_in_sidecar(
                        sc, &descendants, &source, Some(&target), &old_chain, &new_chain,
                    );
                }
                self.refresh_filter_after_tag(&source, Some(&target));
                if let Some(g) = &mut self.gallery {
                    if let Err(error) = g.commit_tag_hierarchy() {
                        self.set_status(format!("Tags merged but tags.txt could not be written: {}", error));
                        return;
                    }
                }
                self.set_status(format!("Merged '{}' into '{}' on {} image(s)", source, target, changed));
            }
            TagDialog::Delete { tag } => {
                let old_chain = g.hierarchy.ancestors_of(&tag);
                let parent = g.hierarchy.parent_of(&tag);
                let new_chain: Vec<String> = match &parent {
                    Some(p) => {
                        let mut chain = g.hierarchy.ancestors_of(p);
                        chain.push(p.clone());
                        chain
                    }
                    None => Vec::new(),
                };
                let descendants: HashSet<String> = g.hierarchy.descendants_of(&tag).into_iter().collect();
                if let Err(error) = g.hierarchy.delete(&tag) {
                    self.set_status(format!("Cannot delete tag: {}", error));
                    return;
                }
                let changed = self.rewrite_sidecars(|sc| {
                    remap_subtree_in_sidecar(sc, &descendants, &tag, None, &old_chain, &new_chain)
                });
                for sc in self.staged_sidecars.values_mut() {
                    remap_subtree_in_sidecar(sc, &descendants, &tag, None, &old_chain, &new_chain);
                }
                self.refresh_filter_after_tag(&tag, None);
                if let Some(g) = &mut self.gallery {
                    if let Err(error) = g.commit_tag_hierarchy() {
                        self.set_status(format!("Tag deleted but tags.txt could not be written: {}", error));
                        return;
                    }
                }
                self.set_status(format!("Deleted '{}' from {} image(s)", tag, changed));
            }
        }
    }

    fn record_op(&mut self, op: UndoOp) {
        self.undo_stack.push(op);
        if self.undo_stack.len() > 100 {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
    }

    fn finish_history_op(&mut self, ctx: &egui::Context) {
        self.fm.selected.clear();
        self.fm.selection_anchor = None;
        if let Some(g) = &mut self.gallery {
            g.selected_images.clear();
            g.selection_anchor = None;
            g.keyboard_focus = None;
        }
        self.fm.refresh();
        self.rescan_gallery(ctx);
    }

    fn undo(&mut self, ctx: &egui::Context) {
        let Some(op) = self.undo_stack.pop() else {
            self.set_status("Nothing to undo");
            return;
        };
        match undo_files(&op) {
            Ok(message) => {
                self.redo_stack.push(op);
                self.finish_history_op(ctx);
                self.set_status(message);
            }
            Err(e) => {
                self.undo_stack.push(op);
                self.set_status(format!("Undo failed: {}", e));
            }
        }
    }

    fn redo(&mut self, ctx: &egui::Context) {
        let Some(op) = self.redo_stack.pop() else {
            self.set_status("Nothing to redo");
            return;
        };
        match redo_files(&op) {
            Ok(message) => {
                self.undo_stack.push(op);
                self.finish_history_op(ctx);
                self.set_status(message);
            }
            Err(e) => {
                self.redo_stack.push(op);
                self.set_status(format!("Redo failed: {}", e));
            }
        }
    }

    fn forget_paths(&mut self, paths: &[PathBuf]) {
        if let Some(g) = &mut self.gallery {
            g.items.retain(|item| !paths.iter().any(|p| p == Path::new(&item.path)));
            g.selected_images.retain(|s| !paths.iter().any(|p| p == Path::new(s)));
            g.invalidate_filter_cache();
            let viewer_gone = g
                .viewer_target
                .as_ref()
                .map_or(false, |t| paths.iter().any(|p| p == Path::new(t)));
            if viewer_gone {
                g.viewer_target = None;
            }
        }
        self.fm.selected.retain(|p| !paths.contains(p));
    }

    /// The single place where view commands are turned into state changes.
    fn apply_action(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::Undo => self.undo(ctx),
            Action::Redo => self.redo(ctx),
            Action::Navigate(p) => self.navigate_to(ctx, p),
            Action::NavigateBack => self.navigate_history(ctx, false),
            Action::NavigateForward => self.navigate_history(ctx, true),
            Action::Open(p) => {
                if p.is_dir() {
                    self.navigate_to(ctx, p);
                } else {
                    self.fm.open_smart(&p);
                }
            }
            Action::OpenExternal(p) => self.fm.open_smart(&p),
            Action::OpenTerminal(p) => {
                let dir = if p.is_dir() { p } else { p.parent().unwrap_or(&p).to_path_buf() };
                for t in ["konsole", "gnome-terminal", "alacritty", "kitty", "xterm"] {
                    if std::process::Command::new(t).current_dir(&dir).spawn().is_ok() { break; }
                }
            }
            Action::Trash(paths) => {
                self.redo_stack.clear();
                let mut done = 0;
                for p in &paths {
                    if trash::delete(p).is_ok() {
                        done += 1;
                    }
                }
                self.forget_paths(&paths);
                self.fm.refresh();
                self.set_status(format!("Moved {} item(s) to Trash", done));
            }
            Action::CopyFiles(paths) => {
                self.clipboard = paths;
                self.clipboard_mode = ClipboardMode::Copy;
                self.set_status(format!("Copied {} item(s) — paste with the background menu", self.clipboard.len()));
            }
            Action::CutFiles(paths) => {
                self.clipboard = paths;
                self.clipboard_mode = ClipboardMode::Cut;
                self.set_status(format!("Cut {} item(s) — paste with the background menu", self.clipboard.len()));
            }
            Action::Paste(dir) => {
                if self.clipboard.is_empty() {
                    self.set_status("Nothing to paste");
                } else {
                    let paths = self.clipboard.clone();
                    let copy = self.clipboard_mode == ClipboardMode::Copy;
                    match file_manager::transfer(&paths, &dir, copy) {
                        Ok(pairs) => {
                            let n = pairs.len();
                            if !copy {
                                self.clipboard.clear();
                            }
                            self.fm.refresh();
                            if n == 0 {
                                self.set_status("Already in place");
                            } else {
                                self.record_op(UndoOp::Transfer { pairs, copy });
                                if !copy { self.forget_paths(&paths); }
                                self.rescan_gallery(ctx);
                                self.set_status(format!("{} {} item(s)", if copy { "Copied" } else { "Moved" }, n));
                            }
                        }
                        Err(e) => self.set_status(format!("Paste failed: {}", e)),
                    }
                }
            }
            Action::Duplicate(paths) => {
                match file_manager::duplicate(&paths) {
                    Ok(pairs) => {
                        let n = pairs.len();
                        if n > 0 {
                            self.record_op(UndoOp::Transfer { pairs, copy: true });
                        }
                        self.fm.refresh();
                        self.rescan_gallery(ctx);
                        self.set_status(format!("Duplicated {} item(s)", n));
                    }
                    Err(e) => self.set_status(format!("Duplicate failed: {}", e)),
                }
            }
            Action::Rename(path) => {
                let text = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.dialog = Some(Dialog::Rename { path, text });
            }
            Action::RenameCommit { path, name } => {
                match file_manager::rename(&path, &name) {
                    Ok(to) => {
                        self.record_op(UndoOp::Rename { from: path.clone(), to: to.clone() });
                        self.forget_paths(&[path]);
                        self.fm.refresh();
                        self.rescan_gallery(ctx);
                        self.set_status(format!("Renamed to {}", to.display()));
                    }
                    Err(e) => self.set_status(format!("Rename failed: {}", e)),
                }
            }
            Action::NewFolder(dir) => self.dialog = Some(Dialog::NewFolder { dir, text: String::new() }),
            Action::CreateFolder { dir, name } => self.create_from_dialog(ctx, &dir, &name, true),
            Action::NewFile(dir) => self.dialog = Some(Dialog::NewFile { dir, text: String::new() }),
            Action::CreateFile { dir, name } => self.create_from_dialog(ctx, &dir, &name, false),
            Action::Properties(paths) => self.dialog = Some(Dialog::Properties { paths }),
            Action::MoveFiles { from, to, copy } => {
                let valid: Vec<PathBuf> = from
                    .into_iter()
                    .filter(|p| p != &to && !to.starts_with(p))
                    .collect();
                if valid.is_empty() {
                    self.set_status("Cannot move an item into itself");
                } else {
                    match file_manager::transfer(&valid, &to, copy) {
                        Ok(pairs) => {
                            let n = pairs.len();
                            self.fm.refresh();
                            if n == 0 {
                                self.set_status("Already in place");
                            } else {
                                self.record_op(UndoOp::Transfer { pairs, copy });
                                if !copy { self.forget_paths(&valid); }
                                self.rescan_gallery(ctx);
                                let verb = if copy { "Copied" } else { "Moved" };
                                self.set_status(format!("{} {} item(s) to {}", verb, n, to.display()));
                            }
                        }
                        Err(e) => self.set_status(format!("Move failed: {}", e)),
                    }
                }
            }
            Action::ImportExternal { paths, dir } => {
                let valid: Vec<PathBuf> = paths.into_iter().filter(|p| p.exists()).collect();
                if valid.is_empty() {
                    self.set_status("No files to import");
                } else {
                    match file_manager::transfer(&valid, &dir, true) {
                        Ok(pairs) => {
                            let n = pairs.len();
                            if n == 0 {
                                self.set_status("Nothing new to import");
                            } else {
                                self.record_op(UndoOp::Transfer { pairs, copy: true });
                                self.fm.refresh();
                                self.rescan_gallery(ctx);
                                self.set_status(format!("Imported {} item(s)", n));
                            }
                        }
                        Err(e) => self.set_status(format!("Import failed: {}", e)),
                    }
                }
            }
            Action::SetRating { paths, rating } => self.set_rating_for_paths(&paths, rating),
            Action::ShowInGallery(paths) => self.show_paths_in_gallery(ctx, &paths),
            Action::EditTag { paths, remove } => {
                self.dialog = Some(Dialog::EditTag { paths, remove, text: String::new() });
            }
            Action::ApplyTag { paths, tag, remove } => self.apply_tag_for_paths(&paths, &tag, remove),
            Action::ApplyTagToSelected { tag, remove } => {
                let paths = self.selected_paths();
                self.apply_tag_for_paths(&paths, &tag, remove);
            }
            Action::ApplyTagFilter(token) => {
                match crate::gallery::Query::parse(&token) {
                    Ok(parsed) => {
                        self.filter_query.filter = token.clone();
                        self.filter_query.claim_scope = ClaimScope::Curated;
                        self.filter_query.rating_min = None;
                        let confidence = self.filter_query.confidence_range();
                        let evidence = self.build_filter_evidence(&self.tag_review);
                        let revision = self.tag_review.store.revision();
                        if let Some(g) = &mut self.gallery {
                            g.active_query = parsed;
                            g.filter_confidence = confidence;
                            g.filter_scope = ClaimScope::Curated;
                            g.filter_revision = revision;
                            g.filter_evidence = evidence;
                            g.query_error = None;
                            g.invalidate_filter_cache();
                        }
                        self.viewer_open = false;
                                        self.set_status(format!("Filter applied: {}", token));
                    }
                    Err(error) => self.set_status(format!("Invalid filter: {}", error)),
                }
            }
            Action::TagDialog(kind) => {
                let text = match &kind {
                    TagDialog::Rename { tag } => tag.clone(),
                    TagDialog::Delete { tag } => tag.clone(),
                    _ => String::new(),
                };
                self.dialog = Some(Dialog::Tag { kind, text });
            }
            Action::TagCommit { kind, text } => self.perform_tag_dialog(kind, text),
            Action::FindDuplicates => {
                let groups = find_duplicate_groups(&self.fm.current_dir);
                if groups.is_empty() {
                    self.set_status("No duplicate files in this folder");
                } else {
                    let count: usize = groups.iter().map(Vec::len).sum();
                    self.dialog = Some(Dialog::Duplicates { groups });
                    self.set_status(format!("Found {} duplicate file(s)", count));
                }
            }
            Action::OpenTagManager(target) => self.open_tag_manager(target),
            Action::ToggleTagManager => self.toggle_tag_manager(),
            Action::ExportMetadataIndex => {
                let default_path = crate::paths::config_dir().join("symtropy-metadata-index.json");
                self.dialog = Some(Dialog::ExportMetadata { path: default_path.to_string_lossy().into_owned() });
            }
            Action::ExportMetadataCommit { path } => {
                let items = self.gallery.as_ref().map(|g| g.items.as_slice()).unwrap_or_default();
                match crate::metadata_export::write_metadata_index(items, &path) {
                    Ok(count) => self.set_status(format!("Exported metadata for {} item(s) to {}", count, path.display())),
                    Err(error) => self.set_status(format!("Export failed: {error}")),
                }
            }
            Action::CopyPath(p) => {
                ctx.output_mut(|o| o.copied_text = p.to_string_lossy().into_owned());
                self.set_status("Path copied to clipboard");
            }
            Action::ToggleBookmark(p) => {
                if let Some(i) = self.fm.bookmarks.paths.iter().position(|b| b == &p) {
                    self.fm.bookmarks.paths.remove(i);
                } else {
                    self.fm.bookmarks.paths.push(p);
                }
                self.fm.bookmarks.save();
            }
            Action::SelectAll => {
                if self.fm.view_mode.is_gallery() {
                    if let Some(g) = &mut self.gallery {
                        self.fm.selected = g.items.iter().map(|i| PathBuf::from(&i.path)).collect();
                        g.selected_images = g.items.iter().map(|i| i.path.clone()).collect();
                        g.selection_anchor = None;
                    }
                } else {
                    self.fm.selected = self.fm.entries.iter().map(|e| e.path.clone()).collect();
                }
                self.fm.selection_anchor = None;
            }
            Action::SelectInverse => {
                if self.fm.view_mode.is_gallery() {
                    if let Some(g) = &mut self.gallery {
                        let all: Vec<(PathBuf, String)> = g
                            .items
                            .iter()
                            .map(|i| (PathBuf::from(&i.path), i.path.clone()))
                            .collect();
                        g.selected_images.clear();
                        g.selection_anchor = None;
                        for (path, path_string) in all {
                            if self.fm.selected.contains(&path) {
                                self.fm.selected.remove(&path);
                            } else {
                                self.fm.selected.insert(path);
                                g.selected_images.push(path_string);
                            }
                        }
                    }
                } else {
                    let mut inverse = std::collections::HashSet::new();
                    for entry in &self.fm.entries {
                        if !self.fm.selected.contains(&entry.path) {
                            inverse.insert(entry.path.clone());
                        }
                    }
                    self.fm.selected = inverse;
                }
            }
            Action::SelectPaths(paths) => {
                self.fm.selected = paths.iter().cloned().collect();
                self.fm.selection_anchor = paths.first().cloned();
                if let Some(g) = &mut self.gallery {
                    g.selected_images = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
                    g.selection_anchor = g.selected_images.first().cloned();
                }
            }
            Action::ClearSelection => {
                self.fm.selected.clear();
                self.fm.selection_anchor = None;
                if let Some(g) = &mut self.gallery {
                    g.selected_images.clear();
                    g.selection_anchor = None;
                }
            }
            Action::ClearFilters => {
                self.filter_query = FilterQuery::default();
                self.filter_query.claim_scope = ClaimScope::Curated;
                if !self.fm.search_query.is_empty() {
                    self.fm.search_query.clear();
                    self.fm.last_search_query.clear();
                    self.fm.refresh();
                }
                if let Some(g) = &mut self.gallery {
                    g.clear_filters();
                }
                self.set_status("Filters cleared");
            }
            Action::SetSort { key, descending } => {
                let direction = if descending {
                    SortDirection::Descending
                } else {
                    SortDirection::Ascending
                };
                self.fm.set_sort(key, direction);
                if let Some(g) = &mut self.gallery {
                    g.set_sort(key, direction);
                }
                self.persist_state();
            }
            Action::ToggleSortDirection => {
                self.fm.toggle_sort_direction();
                if let Some(g) = &mut self.gallery {
                    g.set_sort(self.fm.sort, self.fm.sort_direction);
                }
                self.persist_state();
            }
            Action::ToggleHidden => {
                self.fm.show_hidden = !self.fm.show_hidden;
                self.fm.refresh();
                self.rescan_gallery(ctx);
            }
            Action::Refresh => {
                self.path_bar.invalidate_counts();
                self.fm.refresh();
                self.rescan_gallery(ctx);
                self.set_status("Refreshed");
            }
        }
    }

    fn create_from_dialog(&mut self, ctx: &egui::Context, dir: &Path, name: &str, is_dir: bool) {
        match file_manager::create_item(dir, name, is_dir) {
            Ok(path) => {
                self.record_op(UndoOp::Create { path: path.clone(), is_dir });
                self.fm.refresh();
                self.rescan_gallery(ctx);
                self.set_status(format!("Created {}", path.display()));
            }
            Err(e) => self.set_status(format!("Create failed: {}", e)),
        }
    }

    fn show_dialog(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let Some(mut dialog) = self.dialog.take() else { return; };
        let title = match &dialog {
            Dialog::Rename { .. } => "Rename",
            Dialog::NewFolder { .. } => "New Folder",
            Dialog::NewFile { .. } => "New File",
            Dialog::Properties { .. } => "Properties",
            Dialog::EditTag { remove, .. } => if *remove { "Remove Tag" } else { "Add Tag" },
            Dialog::Tag { kind, .. } => match kind {
                TagDialog::New { parent: Some(_) } => "New Child Tag",
                TagDialog::New { parent: None } => "New Top-Level Tag",
                TagDialog::Rename { .. } => "Rename Tag",
                TagDialog::Move { .. } => "Move Tag",
                TagDialog::Merge { .. } => "Merge Tags",
                TagDialog::Delete { .. } => "Delete Tag",
            },
            Dialog::Duplicates { .. } => "Duplicate Files",
            Dialog::ExportMetadata { .. } => "Export Metadata Index",
        };
        let mut close = false;
        let mut submit: Option<Action> = None;
        let new_is_dir = matches!(dialog, Dialog::NewFolder { .. });
        let tag_catalog: Vec<String> = self
            .gallery
            .as_ref()
            .map(|g| crate::sidebar::tag_catalog(&g.tag_tree))
            .unwrap_or_default();

        let response = egui::Window::new(title)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                match &mut dialog {
                    Dialog::Rename { path, text } => {
                        ui.label(format!("Rename {}", path.display()));
                        let resp = ui.text_edit_singleline(text);
                        ui.horizontal(|ui| {
                            if ui.button("Rename").clicked() || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                                submit = Some(Action::RenameCommit { path: path.clone(), name: text.clone() });
                            }
                            if ui.button("Cancel").clicked() { close = true; }
                        });
                    }
                    Dialog::NewFolder { dir, text } | Dialog::NewFile { dir, text } => {
                        ui.label(format!("Create in {}", dir.display()));
                        let resp = ui.text_edit_singleline(text);
                        ui.horizontal(|ui| {
                            if ui.button("Create").clicked() || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                                let action = if new_is_dir {
                                    Action::CreateFolder { dir: dir.clone(), name: text.clone() }
                                } else {
                                    Action::CreateFile { dir: dir.clone(), name: text.clone() }
                                };
                                submit = Some(action);
                            }
                            if ui.button("Cancel").clicked() { close = true; }
                        });
                    }
                    Dialog::Properties { paths } => {
                        egui::Grid::new("properties_grid").show(ui, |ui| {
                            for path in paths.iter() {
                                let meta = std::fs::metadata(path.as_path()).ok();
                                let kind = if path.is_dir() { "Folder" } else { "File" };
                                let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                                ui.label("Name:");
                                ui.label(path.file_name().unwrap_or_default().to_string_lossy());
                                ui.end_row();
                                ui.label("Type:");
                                ui.label(kind);
                                ui.end_row();
                                ui.label("Size:");
                                ui.label(format!("{} bytes", size));
                                ui.end_row();
                                ui.label("Location:");
                                ui.label(path.parent().unwrap_or(path).display().to_string());
                                ui.end_row();
                                ui.separator();
                                ui.separator();
                                ui.end_row();
                            }
                        });
                        if ui.button("Close").clicked() { close = true; }
                    }
                    Dialog::EditTag { paths, remove, text } => {
                        ui.label(if *remove { "Tag to remove from all selected images:" } else { "Tag to add to all selected images:" });
                        let resp = ui.text_edit_singleline(text);
                        ui.horizontal(|ui| {
                            let commit = ui.button(if *remove { "Remove" } else { "Add" }).clicked()
                                || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                            if commit && !text.trim().is_empty() {
                                submit = Some(Action::ApplyTag {
                                    paths: paths.clone(),
                                    tag: text.trim().to_string(),
                                    remove: *remove,
                                });
                            }
                            if ui.button("Cancel").clicked() { close = true; }
                        });
                    }
                    Dialog::Tag { kind, text } => {
                        let commit_kind = kind.clone();
                        match &commit_kind {
                            TagDialog::Delete { tag } => {
                                ui.label(format!("Delete tag '{}'?", tag));
                                ui.label(
                                    egui::RichText::new("Its child tags will be promoted; images will lose this tag.")
                                        .small()
                                        .color(egui::Color32::GRAY),
                                );
                                ui.horizontal(|ui| {
                                    if ui.button("Delete").clicked() {
                                        submit = Some(Action::TagCommit {
                                            kind: commit_kind.clone(),
                                            text: text.clone(),
                                        });
                                    }
                                    if ui.button("Cancel").clicked() { close = true; }
                                });
                            }
                            other => {
                                let (label, hint, button) = match other {
                                    TagDialog::New { parent: Some(parent) } => (
                                        format!("New child tag under '{}':", parent),
                                        "tag name",
                                        "Create",
                                    ),
                                    TagDialog::New { parent: None } => (
                                        "New top-level tag:".to_string(),
                                        "tag name",
                                        "Create",
                                    ),
                                    TagDialog::Rename { tag } => (
                                        format!("Rename '{}' to:", tag),
                                        "new tag name",
                                        "Rename",
                                    ),
                                    TagDialog::Move { tag } => (
                                        format!("Move '{}' under:", tag),
                                        "parent tag (blank = top level)",
                                        "Move",
                                    ),
                                    TagDialog::Merge { source } => (
                                        format!("Merge '{}' into:", source),
                                        "target tag",
                                        "Merge",
                                    ),
                                    TagDialog::Delete { .. } => unreachable!(),
                                };
                                ui.label(label);
                                if matches!(other, TagDialog::Merge { .. }) {
                                    ui.label(
                                        egui::RichText::new("Images tagged with the source will be retagged with the target.")
                                            .small()
                                            .color(egui::Color32::GRAY),
                                    );
                                }
                                let response = ui.add_sized(
                                    [ui.available_width().max(260.0), 22.0],
                                    egui::TextEdit::singleline(text).hint_text(hint),
                                );
                                crate::sidebar::show_tag_suggestions(
                                    ui,
                                    ui.make_persistent_id("tag_dialog_suggestions"),
                                    &response,
                                    text,
                                    &tag_catalog,
                                    false,
                                    &[],
                                );
                                ui.horizontal(|ui| {
                                    let commit = ui.button(button).clicked()
                                        || (response.lost_focus()
                                            && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                                    // A blank parent is a valid Move target (top level).
                                    let has_text = !text.trim().is_empty();
                                    let valid = has_text || matches!(other, TagDialog::Move { .. });
                                    if commit && valid {
                                        submit = Some(Action::TagCommit {
                                            kind: commit_kind.clone(),
                                            text: text.trim().to_string(),
                                        });
                                    }
                                    if ui.button("Cancel").clicked() { close = true; }
                                });
                            }
                        }
                    }
                    Dialog::ExportMetadata { path } => {
                        ui.label("Write tags, ratings, sources, relationships, and notes to a JSON file.");
                        ui.label(egui::RichText::new("No image pixels or cached analysis are included.").size(11.0).color(egui::Color32::GRAY));
                        let resp = ui.text_edit_singleline(path);
                        ui.horizontal(|ui| {
                            if ui.button("Export").clicked()
                                || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                            {
                                if path.trim().is_empty() {
                                    self.set_status("Choose an export path");
                                } else {
                                    submit = Some(Action::ExportMetadataCommit {
                                        path: PathBuf::from(path.trim()),
                                    });
                                }
                            }
                            if ui.button("Cancel").clicked() { close = true; }
                        });
                    }
                    Dialog::Duplicates { groups } => {
                        ui.label(format!("{} groups of identical files", groups.len()));
                        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                            for (index, group) in groups.iter().enumerate() {
                                ui.label(egui::RichText::new(format!("Group {} — {} files", index + 1, group.len())).strong());
                                for path in group {
                                    ui.label(egui::RichText::new(path.display().to_string()).monospace().size(11.0));
                                }
                                ui.horizontal(|ui| {
                                    if ui.small_button("Select group").clicked() {
                                        submit = Some(Action::SelectPaths(group.clone()));
                                    }
                                    if group.len() > 1 && ui.small_button("Trash extras").clicked() {
                                        submit = Some(Action::Trash(group[1..].to_vec()));
                                    }
                                });
                                ui.separator();
                            }
                        });
                        if ui.button("Close").clicked() { close = true; }
                    }
                }
            });

        if response.is_none() {
            close = true;
        }
        if let Some(action) = submit {
            actions.push(action);
            close = true;
        }
        if !close {
            self.dialog = Some(dialog);
        }
    }
}

impl eframe::App for SymtropyApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] { [0.0, 0.0, 0.0, 0.0] }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let ppp = ctx.pixels_per_point();
        self.theme.sync_scale(ppp, ctx.screen_rect().size());
        crate::wayland_drag::init_from_frame(frame);
        crate::wayland_drag::tick(ctx);
        self.poll_source_workers(ctx);
        self.poll_local_vision(ctx);
        self.poll_query_planner();
        self.sync_recursive_tag_search();

        if ctx.input(|i| i.key_pressed(egui::Key::L) && i.modifiers.ctrl) {
            let dir = self.fm.current_dir.clone();
            self.path_bar.begin_edit(&dir);
        }

        self.frames += 1;
        if self.frames == 1 {
            eprintln!("startup: first frame / window ready @ {}ms", crate::startup_ms());
        }

        // Persist state on close
        if ctx.input(|i| i.viewport().close_requested()) {
            self.persist_state();
        }

        let mut fm_actions = Vec::new();
        let mut pathbar_actions = Vec::new();
        let status_text = self.status.clone();

        // OS-level drops are separate from egui's internal DragPayload. Import them as copies
        // into the folder currently being viewed. winit handles this on X11; our Wayland
        // data-device target covers native Wayland clients.
        let mut dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw.dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .filter(|path| path.exists())
                .collect()
        });
        dropped.extend(crate::wayland_drag::take_incoming_drops());
        dropped.retain(|path| path.exists());
        if !dropped.is_empty() {
            fm_actions.push(Action::ImportExternal { paths: dropped, dir: self.fm.current_dir.clone() });
        }

        // No-icon keyboard shortcuts. They stay out of the way of text fields, dialogs and the
        // viewer (which has its own key bindings in its own viewport).
        if self.dialog.is_none() && !self.viewer_open && ctx.memory(|m| m.focused()).is_none() {
            use egui::Key;
            let (undo, redo, all, invert, clear, rating, back, forward, manager) = ctx.input(|i| {
                let ctrl = i.modifiers.ctrl;
                let alt = i.modifiers.alt;
                let plain = !ctrl && !alt && !i.modifiers.shift;
                let rating_keys = [
                    Key::Num0, Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5,
                ];
                (
                    ctrl && i.key_pressed(Key::Z) && !i.modifiers.shift,
                    ctrl && (i.key_pressed(Key::Y) || (i.key_pressed(Key::Z) && i.modifiers.shift)),
                    ctrl && i.key_pressed(Key::A),
                    ctrl && i.key_pressed(Key::I),
                    i.key_pressed(Key::Escape),
                    if plain {
                        rating_keys.iter().position(|key| i.key_pressed(*key)).map(|n| n as u8)
                    } else {
                        None
                    },
                    alt && !ctrl && i.key_pressed(Key::ArrowLeft),
                    alt && !ctrl && i.key_pressed(Key::ArrowRight),
                    ctrl && i.key_pressed(Key::T),
                )
            });
            if undo { fm_actions.push(Action::Undo); }
            if redo { fm_actions.push(Action::Redo); }
            if all { fm_actions.push(Action::SelectAll); }
            if invert { fm_actions.push(Action::SelectInverse); }
            if clear { fm_actions.push(Action::ClearSelection); }
            if back { fm_actions.push(Action::NavigateBack); }
            if forward { fm_actions.push(Action::NavigateForward); }
            if manager { fm_actions.push(Action::ToggleTagManager); }
            if let Some(rating) = rating {
                let paths = self.selected_paths();
                if !paths.is_empty() {
                    fm_actions.push(Action::SetRating { paths, rating });
                }
            }
        }

        let mut filename_search_switched_view = false;
        crate::window::show(ctx, &self.theme, "Symtropy", |ui| {
            let max_size = ui.available_size();

            ui.allocate_ui_with_layout(
                max_size,
                egui::Layout::top_down(egui::Align::Min),
                                       |ui| {
                                           ui.horizontal(|ui| {
                                               if ui.selectable_label(self.show_places, "📁 Places").clicked() {
                                                   self.show_places = !self.show_places;
                                               }
                                               ui.separator();

                                               if ui.add_enabled(self.fm.history_idx > 0, egui::Button::new("⬅"))
                                                   .on_hover_text("Back (Alt+←)")
                                                   .clicked()
                                               {
                                                   fm_actions.push(Action::NavigateBack);
                                               }
                                               if ui.add_enabled(self.fm.history_idx + 1 < self.fm.history.len(), egui::Button::new("➡"))
                                                   .on_hover_text("Forward (Alt+→)")
                                                   .clicked()
                                               {
                                                   fm_actions.push(Action::NavigateForward);
                                               }
                                               let undo_hint = if self.undo_stack.is_empty() {
                                                   "Nothing to undo yet (Ctrl+Z)"
                                               } else {
                                                   "Undo last file operation (Ctrl+Z)"
                                               };
                                               let redo_hint = if self.redo_stack.is_empty() {
                                                   "Nothing to redo yet (Ctrl+Y)"
                                               } else {
                                                   "Redo last file operation (Ctrl+Y)"
                                               };
                                               if ui.add_enabled(!self.undo_stack.is_empty(), egui::Button::new("Undo"))
                                                   .on_hover_text(undo_hint)
                                                   .clicked()
                                               {
                                                   fm_actions.push(Action::Undo);
                                               }
                                               if ui.add_enabled(!self.redo_stack.is_empty(), egui::Button::new("Redo"))
                                                   .on_hover_text(redo_hint)
                                                   .clicked()
                                               {
                                                   fm_actions.push(Action::Redo);
                                               }

                                               ui.label("Find files:");
                                               let resp = ui.add(
                                                   egui::TextEdit::singleline(&mut self.fm.search_query)
                                                       .hint_text("filename…")
                                                       .desired_width(180.0),
                                               ).on_hover_text("Recursive filename search — not tag search");
                                               if self.fm.search_query != self.fm.last_search_query {
                                                   // The default view is the image gallery, which cannot show source
                                                   // files. A generic filename search must land in the file list or
                                                   // it looks like nothing happened (and the tag search gets the blame).
                                                   if self.fm.last_search_query.is_empty()
                                                       && !self.fm.search_query.is_empty()
                                                       && self.fm.view_mode.is_gallery()
                                                   {
                                                       self.fm.view_mode = ViewMode::List;
                                                       filename_search_switched_view = true;
                                                   }
                                                   self.fm.search_recursive();
                                               }
                                                if resp.lost_focus()
                                                    || (resp.has_focus()
                                                        && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                                                {
                                                    self.fm.search_recursive();
                                                }
                                               if !self.fm.search_query.is_empty() {
                                                   ui.label(format!("{} found", self.fm.entries.len()));
                                                   if ui.small_button("✕").on_hover_text("Clear filename search").clicked() {
                                                       self.fm.search_query.clear();
                                                       self.fm.last_search_query.clear();
                                                       self.fm.refresh();
                                                   }
                                               }
                                               if let Some(gallery) = self.gallery.as_mut() {
                                                   gallery.set_filename_query(&self.fm.search_query);
                                               }

                                               ui.separator();
                                               // One Dolphin-style control cycles the three views.
                                               // "Gallery" is the cheap uniform-row mode; "Masonry"
                                               // keeps the old variable-height aspect mosaic.
                                               let (current_label, target_mode) = match self.fm.view_mode {
                                                   ViewMode::List => ("List", ViewMode::Gallery),
                                                   ViewMode::Gallery => ("Gallery", ViewMode::Masonry),
                                                   ViewMode::Masonry => ("Masonry", ViewMode::List),
                                               };
                                               let next_label = match target_mode {
                                                   ViewMode::List => "List",
                                                   ViewMode::Gallery => "Gallery",
                                                   ViewMode::Masonry => "Masonry",
                                               };
                                               if ui.button(current_label).on_hover_text(
                                                   format!("Current view: {current_label}. Click for {next_label}."),
                                               ).clicked() {
                                                   if target_mode.is_gallery() && self.gallery.is_none() {
                                                       let mut gallery = GalleryState::scan_dir(ui.ctx(), &self.fm.current_dir);
                                                       gallery.set_layout(gallery_layout(&target_mode));
                                                       gallery.set_sort(self.fm.sort, self.fm.sort_direction);
                                                       self.gallery = Some(gallery);
                                                   } else if let Some(gallery) = self.gallery.as_mut() {
                                                       gallery.set_layout(gallery_layout(&target_mode));
                                                   }
                                                   self.fm.view_mode = target_mode;
                                               }

                                               // Scope is directory-bound, so this stays visible
                                               // instead of hiding inside a menu with the filters.
                                               let recursive_label = if self.recursive_tag_search {
                                                   "Recursive tags: On"
                                               } else {
                                                   "Recursive tags: Off"
                                               };
                                               if ui.button(recursive_label).on_hover_text(
                                                   "Tag filters: current folder only, or this folder and all subfolders",
                                               ).clicked() {
                                                   self.recursive_tag_search = !self.recursive_tag_search;
                                               }
                                           });
                                           ui.separator();

                                           let path_options = MenuOptions {
                                                bookmarked: self.fm.bookmarks.paths.contains(&self.fm.current_dir),
                                                hidden: self.fm.show_hidden,
                                                sort: self.fm.sort,
                                                sort_descending: sort_direction_bool(self.fm.sort_direction),
                                                can_paste: !self.clipboard.is_empty(),
                                            };
                                            crate::pathbar::show(
                                                ui,
                                                &mut self.path_bar,
                                                &self.fm.current_dir,
                                                self.fm.show_hidden,
                                                path_options,
                                                &mut pathbar_actions,
                                            );
                                            ui.separator();

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
                                                                                              if !p.exists() { continue; }
                                                                                              let resp = ui.button(name);
                                                                                              if resp.clicked() { fm_actions.push(Action::Navigate(p.clone())); }
                                                                                              if name.starts_with('🗑') {
                                                                                                  actions::paint_drop_target(ui, &resp);
                                                                                                  if let Some(paths) = actions::drop_on_trash(ui, &resp) {
                                                                                                      fm_actions.push(Action::Trash(paths));
                                                                                                  }
                                                                                              } else {
                                                                                                  actions::drop_on_directory(ui, &resp, &p, &mut fm_actions);
                                                                                              }
                                                                                          }
                                                                                      }

                                                                                      ui.separator();
                                                                                      let mounts = self.fm.mounts.clone();
                                                                                      for mount in mounts {
                                                                                          let resp = ui.button(format!("💽 {}", mount.display()));
                                                                                          if resp.clicked() { fm_actions.push(Action::Navigate(mount.clone())); }
                                                                                          actions::drop_on_directory(ui, &resp, &mount, &mut fm_actions);
                                                                                      }
                                                                                      let recent = self.recent_dirs.clone();
                                                                                      if !recent.is_empty() {
                                                                                          ui.separator();
                                                                                          ui.label("Recent");
                                                                                          for path in recent.iter().take(8) {
                                                                                              let name = path.file_name().map(|n| n.to_string_lossy().into_owned())
                                                                                                  .unwrap_or_else(|| path.display().to_string());
                                                                                              if ui.button(format!("🕘 {}", name)).clicked() {
                                                                                                  fm_actions.push(Action::Navigate(path.clone()));
                                                                                              }
                                                                                          }
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
                                                                                  let scroll_rect = ui.available_rect_before_wrap();
                                                                                  actions::primary_drag_scroll(ui, scroll_rect, &mut self.fm.list_scroll_offset);
                                                                                  let list_scroll = egui::ScrollArea::vertical()
                                                                                  .id_source("list_scroll")
                                                                                  .drag_to_scroll(false)
                                                                                  .vertical_scroll_offset(self.fm.list_scroll_offset)
                                                                                  .auto_shrink([false, false])
                                                                                  .show(ui, |ui| {
                                                                                      // Bind the background to the list viewport. egui keeps the inherited parent
                                                                                      // clip when the content is too short to scroll; using that rect here would
                                                                                      // cover the toolbar/path bar and swallow their clicks in an empty folder.
                                                                                      let bg_rect = ui.clip_rect().intersect(scroll_rect);
                                                                                      let bg_resp = ui.interact(bg_rect, ui.id().with("list_bg"), egui::Sense::click());
                                                                                      let bg_dir = self.fm.current_dir.clone();
                                                                                      let options = MenuOptions {
                                                                                          bookmarked: self.fm.bookmarks.paths.contains(&bg_dir),
                                                                                          hidden: self.fm.show_hidden,
                                                                                          sort: self.fm.sort,
                                                                                          sort_descending: sort_direction_bool(self.fm.sort_direction),
                                                                                          can_paste: !self.clipboard.is_empty(),
                                                                                      };
                                                                                      actions::show_menu(ui, &bg_resp, move || ContextTarget::Background(bg_dir.clone()), options, &mut fm_actions);
                                                                                      let entries = std::mem::take(&mut self.fm.entries);
                                                                                      let ordered_paths: Vec<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();
                                                                                      for entry in &entries {
                                                                                          let icon = if entry.is_dir { "📁" } else { "📄" };
                                                                                          let resp = ui.selectable_label(self.fm.selected.contains(&entry.path), format!("{} {}", icon, entry.name));

                                                                                          if entry.is_dir {
                                                                                              actions::paint_drop_target(ui, &resp);
                                                                                              if let Some(payload) = actions::take_drag(ui, &resp) {
                                                                                                  fm_actions.push(Action::MoveFiles {
                                                                                                      from: payload.paths,
                                                                                                      to: entry.path.clone(),
                                                                                                      copy: payload.copy,
                                                                                                  });
                                                                                              }
                                                                                          }

                                                                                          match actions::classify(ui, &resp) {
                                                                                              Some(actions::Click::Primary) => {
                                                                                                  let modifiers = ui.input(|i| i.modifiers);
                                                                                                  let toggle = modifiers.ctrl || modifiers.alt;
                                                                                                  if modifiers.shift {
                                                                                                      actions::select_range_set(
                                                                                                          &mut self.fm.selected,
                                                                                                          &mut self.fm.selection_anchor,
                                                                                                          &ordered_paths,
                                                                                                          &entry.path,
                                                                                                          toggle,
                                                                                                      );
                                                                                                  } else if toggle {
                                                                                                      self.fm.selection_anchor = Some(entry.path.clone());
                                                                                                      if !self.fm.selected.remove(&entry.path) {
                                                                                                          self.fm.selected.insert(entry.path.clone());
                                                                                                      }
                                                                                                  } else {
                                                                                                      self.fm.selection_anchor = Some(entry.path.clone());
                                                                                                      self.fm.selected.clear();
                                                                                                      self.fm.selected.insert(entry.path.clone());
                                                                                                  }
                                                                                              }
                                                                                              Some(actions::Click::Double) => {
                                                                                                  if entry.is_dir { fm_actions.push(Action::Navigate(entry.path.clone())); }
                                                                                                  else { fm_actions.push(Action::Open(entry.path.clone())); }
                                                                                              }
                                                                                              Some(actions::Click::Secondary) => {
                                                                                                  if !self.fm.selected.contains(&entry.path) {
                                                                                                      self.fm.selection_anchor = Some(entry.path.clone());
                                                                                                      self.fm.selected.clear();
                                                                                                      self.fm.selected.insert(entry.path.clone());
                                                                                                  }
                                                                                              }
                                                                                              Some(actions::Click::Drag) => {
                                                                                                  let paths = if self.fm.selected.contains(&entry.path) && self.fm.selected.len() > 1 {
                                                                                                      entries.iter()
                                                                                                      .filter(|e| self.fm.selected.contains(&e.path))
                                                                                                      .map(|e| e.path.clone())
                                                                                                      .collect()
                                                                                                  } else {
                                                                                                      vec![entry.path.clone()]
                                                                                                  };
                                                                                                  actions::begin_drag(ui, paths);
                                                                                              }
                                                                                              Some(actions::Click::PrimaryDrag) => {
                                                                                                  let paths = if self.fm.selected.contains(&entry.path) && self.fm.selected.len() > 1 {
                                                                                                      entries.iter()
                                                                                                      .filter(|e| self.fm.selected.contains(&e.path))
                                                                                                      .map(|e| e.path.clone())
                                                                                                      .collect()
                                                                                                  } else {
                                                                                                      vec![entry.path.clone()]
                                                                                                  };
                                                                                                  actions::begin_external_drag(ui, paths);
                                                                                              }
                                                                                              None => {}
                                                                                          }

                                                                                          let selected = self.fm.selected.contains(&entry.path);
                                                                                          let target = ContextTarget::Entries(if selected && self.fm.selected.len() > 1 {
                                                                                              entries.iter()
                                                                                              .filter(|e| self.fm.selected.contains(&e.path))
                                                                                              .map(|e| e.path.clone())
                                                                                              .collect()
                                                                                          } else {
                                                                                              vec![entry.path.clone()]
                                                                                          });
                                                                                          let dir = target.directory();
                                                                                          let options = MenuOptions { bookmarked: self.fm.bookmarks.paths.contains(&dir), ..options };
                                                                                          actions::show_menu(ui, &resp, move || target, options, &mut fm_actions);
                                                                                      }
                                                                                      self.fm.entries = entries;
                                                                                      actions::drop_on_directory(ui, &bg_resp, &self.fm.current_dir, &mut fm_actions);
                                                                                  });
                                                                                  self.fm.list_scroll_offset = list_scroll.state.offset.y;
                                                                              } else if let Some(gallery) = &mut self.gallery {
                                                                                  let options = MenuOptions {
                                                                                      bookmarked: self.fm.bookmarks.paths.contains(&self.fm.current_dir),
                                                                                      hidden: self.fm.show_hidden,
                                                                                      sort: self.fm.sort,
                                                                                      sort_descending: sort_direction_bool(self.fm.sort_direction),
                                                                                      can_paste: !self.clipboard.is_empty(),
                                                                                  };
                                                                                  let viewport = ui.available_rect_before_wrap().intersect(ui.clip_rect());
                                                                                   gallery.show(ui, &self.theme, viewport, &self.fm.current_dir, self.viewer_open, self.dialog.is_some(), options, &mut fm_actions);

                                                                                  if gallery.request_open_viewer {
                                                                                      self.viewer_open = true;
                                                                                      gallery.request_open_viewer = false;
                                                                                  }
                                                                              }

                                                                              if let Some(text) = &status_text {
                                                                                  let font = egui::FontId::proportional(13.0);
                                                                                  let galley = ui.painter().layout_no_wrap(text.clone(), font.clone(), egui::Color32::WHITE);
                                                                                  let pos = ui.min_rect().max - egui::vec2(16.0, 16.0) - galley.size();
                                                                                  let bg = egui::Rect::from_min_size(pos - egui::vec2(8.0, 5.0), galley.size() + egui::vec2(16.0, 10.0));
                                                                                  ui.painter().rect_filled(bg, 4.0, egui::Color32::from_black_alpha(200));
                                                                                  ui.painter().text(pos, egui::Align2::LEFT_TOP, text, font, egui::Color32::WHITE);
                                                                              }
                                                                          }
                                               );
                                           });
                                       }
            );
        });

        if filename_search_switched_view {
            self.set_status("Filename search results are shown in List view");
        }

        // The chrome sliders live in the theme gutter outside the content area, so persist them
        // as soon as they move.
        // Skip the shutdown frame: a viewport-size change can clamp a slider without user input.
        let closing = ctx.input(|i| i.viewport().close_requested());
        let frame_scale = self.theme.frame_scale_setting();
        let button_scale = self.theme.button_scale_setting();
        if !closing && (frame_scale != self.last_persisted_frame_scale || button_scale != self.last_persisted_button_scale) {
            self.persist_state();
            self.last_persisted_frame_scale = frame_scale;
            self.last_persisted_button_scale = button_scale;
        }

        for action in pathbar_actions {
            self.apply_action(ctx, action);
        }
        for action in fm_actions {
            self.apply_action(ctx, action);
        }

        let mut dialog_actions = Vec::new();
        self.show_dialog(ctx, &mut dialog_actions);
        for action in dialog_actions {
            self.apply_action(ctx, action);
        }

        if self.tag_manager.open {
            let review_item = self.tag_manager.item.clone().unwrap_or_default();
            let mut active_item = review_item.clone();
            let mut staged_sidecar = if review_item.is_empty() {
                None
            } else {
                self.take_staged_sidecar(&review_item)
            };
            let manager_tag_tree = self.gallery.as_ref().map(|g| g.tag_tree.clone()).unwrap_or_else(|| {
                crate::gallery::TagNode { name: "Root".into(), children: Vec::new() }
            });
            let gallery_query = self
                .gallery
                .as_ref()
                .map(|g| g.active_query.clone())
                .unwrap_or(crate::gallery::Query::AlwaysTrue);
            let outcome = tag_manager::show(
                ctx,
                &mut self.tag_manager,
                &mut self.tag_review,
                &mut self.review_ui,
                &mut staged_sidecar,
                &manager_tag_tree,
                &gallery_query,
            );
            // Apply writes for the item that was visible while the window was
            // drawn, before a target change swaps the staged sidecar.
            let outcome_sidecar = staged_sidecar.clone();
            if let Some(target) = outcome.target_changed.clone() {
                if let Some(sc) = staged_sidecar.take() {
                    self.staged_sidecars.insert(active_item.clone(), sc);
                }
                active_item = target.clone();
                staged_sidecar = self.take_staged_sidecar(&target);
                self.tag_manager.item = Some(target.clone());
            }
            if !active_item.is_empty() {
                if let Some(sc) = staged_sidecar {
                    self.staged_sidecars.insert(active_item, sc);
                }
            }
            self.apply_review_outcome(ctx, &review_item, outcome.review, outcome_sidecar);
            if outcome.find_duplicates {
                self.apply_action(ctx, Action::FindDuplicates);
            }
            if let Some(apply) = outcome.ai_apply {
                if self.pending_ai_apply.is_some() || self.query_planner.busy() {
                    self.tag_manager.ai_search.status = "Local model is still planning…".to_string();
                    self.set_status("Local model is still planning the previous query");
                } else {
                    let mut catalog = crate::sidebar::tag_catalog(&manager_tag_tree);
                    catalog.extend(crate::query_composer::builtin_tag_ids());
                    let mut seen = HashSet::new();
                    catalog.retain(|tag| seen.insert(crate::gallery::hierarchy::tag_key(tag)));
                    let token = self.query_planner.request(apply.prompt.clone(), catalog);
                    self.pending_ai_apply = Some(PendingAiApply { token, fallback: apply });
                    self.tag_manager.ai_search.status = "Planning locally with known tags…".to_string();
                    self.set_status("Planning AI query with the local model");
                }
            }
        }

        let viewer_path: Option<String> = if self.viewer_open {
            self.gallery.as_ref().and_then(|g| g.viewer_target.clone())
            .or_else(|| self.gallery.as_ref().and_then(|g| g.selected_images.first().cloned()))
        } else {
            None
        };

        if let Some(path) = viewer_path {
            let is_new = self.viewer_loaded_path.as_deref() != Some(&path);
            if is_new {
                self.remember_current_video();
                self.save_video_state();
                self.viewer_loaded_path = Some(path.clone());
                self.viewer_zoom = 1.0;
                self.viewer_pan = egui::Vec2::ZERO;
                self.viewer_anim_frames.clear();
                self.viewer_anim_durations.clear();
                self.viewer_player = None;
                self.viewer_needs_fit = true;
                self.viewer_last_fit_size = None;

                let is_video = crate::gallery::workers::is_video_path(Path::new(&path));

                if !is_video {
                    if path.to_lowercase().ends_with(".ssvg") {
                        if let Ok(file) = std::fs::File::open(&path) {
                            if let Ok((_, _, merged)) = ssvg::Package::read_baked_from(file) {
                                if let Ok(img) = image::load_from_memory(&merged) {
                                    let img = img.to_rgba8();
                                    let size = [img.width() as usize, img.height() as usize];
                                    let pixels = img.as_flat_samples();
                                    let texture = ctx.load_texture(
                                        format!("{path}_ssvg_merged"),
                                        egui::ColorImage::from_rgba_unmultiplied(size, pixels.as_slice()),
                                        egui::TextureOptions::LINEAR,
                                    );
                                    self.viewer_cache.insert(path.clone(), Some(texture));
                                }
                            }
                        }
                    }

                    if path.to_lowercase().ends_with(".webp") {
                        if let Ok(bytes) = std::fs::read(&path) {
                            if let Ok(decoder) = webp_animation::Decoder::new(&bytes) {
                                let mut t_frames = Vec::new();
                                let mut timestamps = Vec::new();
                                for frame in decoder {
                                    let (w, h) = frame.dimensions();
                                    let mut img = image::RgbaImage::from_raw(w, h, frame.data().to_vec()).unwrap();
                                    if w.max(h) > 2048 {
                                        let scale = 2048.0 / w.max(h) as f32;
                                        let nw = ((w as f32 * scale) as u32).max(1);
                                        let nh = ((h as f32 * scale) as u32).max(1);
                                        img = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Lanczos3);
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
            let mut last_fit_size = self.viewer_last_fit_size;
            let mut video_volume = self.video_volume;
            let mut video_muted = self.video_muted;
            let mut video_loop = self.video_loop;
            let restore_video = self.video_memory.get(&path).cloned();

            let mut is_edit_mode = self.is_edit_mode;
            let mut staged_sidecar = self.take_staged_sidecar(&path);
            let mut filter_query = self.filter_query.clone();
            let show_preserved_before = self
                .gallery
                .as_ref()
                .is_some_and(|gallery| gallery.show_preserved_tags);
            let mut show_preserved = show_preserved_before;

            let mut active_query = self.gallery.as_ref()
            .map(|g| g.active_query.clone())
            .unwrap_or(crate::gallery::Query::AlwaysTrue);
            let mut query_error = self.gallery.as_ref().and_then(|g| g.query_error.clone());

            let (tag_tree, mut search_query) = if let Some(g) = &self.gallery {
                (g.tag_tree.clone(), g.tag_search_query.clone())
            } else {
                (crate::gallery::TagNode { name: "Root".into(), children: vec![] }, String::new())
            };

            let mut apply_clicked = false;
            let mut revert_clicked = false;
            let mut applied_confidence: Option<ConfidenceRange> = None;
            let mut applied_scope: Option<ClaimScope> = None;
            let mut viewer_actions: Vec<Action> = Vec::new();
            let short_name = path.rsplit('/').next().unwrap_or("").to_string();
            let is_video = crate::gallery::workers::is_video_path(Path::new(&path));
            let scope_name = self
                .fm
                .current_dir
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| self.fm.current_dir.display().to_string());
            let filter_scope = self.gallery.as_ref().map_or_else(
                || format!("{} (no gallery)", scope_name),
                |gallery| format!(
                    "{} ({}/{} shown{}, claims {})",
                    scope_name,
                    gallery.filtered_cache.len(),
                    gallery.items.len(),
                    if gallery.recursive_tag_search { ", recursive" } else { "" },
                    gallery.filter_scope
                ),
            );

            ctx.show_viewport_immediate(
                egui::ViewportId::from_hash_of("media_viewer"),
                                        egui::ViewportBuilder::default()
                                        .with_title(&short_name)
                                        .with_inner_size([1400.0, 900.0])
                                        .with_decorations(false)
                                        .with_transparent(true),
                                        |ctx, _class| {
                                            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) { still_open = false; }
                                            if ctx.input(|i| i.key_pressed(egui::Key::Tab)) { show_sidebar = !show_sidebar; }
                                            let drag_suppressed = crate::wayland_drag::is_active();

                                            egui::CentralPanel::default().frame(egui::Frame::none()).show(ctx, |ui| {
                                                let rect = ui.max_rect();
                                                let actual_sidebar_width = if show_sidebar { sidebar_width } else { 0.0 };
                                                let img_rect = egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x - actual_sidebar_width, rect.max.y));
                                                let sidebar_rect = egui::Rect::from_min_max(egui::pos2(rect.max.x - actual_sidebar_width, rect.min.y), rect.max);

                                                if last_fit_size.map_or(true, |size| (size - img_rect.size()).length() > 0.5) {
                                                    needs_fit = true;
                                                    last_fit_size = Some(img_rect.size());
                                                }

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
                                                    if dr.dragged() && !drag_suppressed { sidebar_width = (sidebar_width - dr.drag_delta().x).clamp(400.0, rect.width() - 400.0); }
                                                    drag_resp_opt = Some(dr);
                                                }

                                                let close_rect = egui::Rect::from_min_size(img_rect.max - egui::vec2(56.0, 56.0), egui::vec2(40.0, 40.0));
                                                let close_resp = ui.interact(close_rect, ui.id().with("close_btn"), egui::Sense::click());
                                                let close_clicked = close_resp.clicked();
                                                let close_hovered = close_resp.hovered();
                                                let toggle_rect = egui::Rect::from_min_size(img_rect.max - egui::vec2(104.0, 56.0), egui::vec2(40.0, 40.0));
                                                let toggle_resp = ui.interact(toggle_rect, ui.id().with("sidebar_toggle"), egui::Sense::click());
                                                let toggle_clicked = toggle_resp.clicked();
                                                let toggle_hovered = toggle_resp.hovered();

                                                let controls_layout = if is_video {
                                                    video_controls_layout(img_rect)
                                                } else {
                                                    None
                                                };
                                                let controls = controls_layout.as_ref()
                                                    .map(|layout| interact_video_controls(ui, layout))
                                                    .unwrap_or_default();

                                                let over_ui = close_hovered || toggle_hovered || controls.hovered
                                                    || drag_resp_opt.as_ref().map_or(false, |r| r.hovered() || r.dragged());
                                                let bg_drag = !drag_suppressed && bg_resp.dragged_by(egui::PointerButton::Primary);
                                                let bg_drag_delta = bg_resp.drag_delta();
                                                let bg_double_clicked = bg_resp.double_clicked();

                                                if bg_drag && !over_ui { pan += bg_drag_delta; }
                                                if bg_double_clicked && !over_ui { needs_fit = true; }

                                                let scroll = ui.input(|i| i.raw_scroll_delta.y);
                                                if scroll != 0.0 && !over_ui {
                                                    let zoom_factor = (scroll * 0.005).exp();
                                                    let new_zoom = (zoom * zoom_factor).clamp(0.1, 20.0);
                                                    let f = new_zoom / zoom;
                                                    let cursor_pos = ui.input(|i| i.pointer.hover_pos()).unwrap_or(img_rect.center());
                                                    pan = (cursor_pos - img_rect.center()) * (1.0 - f) + pan * f;
                                                    zoom = new_zoom;
                                                }

                                                let mut key_toggle_play = false;
                                                let mut key_toggle_mute = false;
                                                let mut key_toggle_loop = false;
                                                let mut key_seek_delta = 0.0_f64;
                                                if is_video && !over_ui && ui.memory(|m| m.focused()).is_none() {
                                                    ui.input(|i| {
                                                        key_toggle_play = i.key_pressed(egui::Key::Space);
                                                        key_toggle_mute = i.key_pressed(egui::Key::M);
                                                        key_toggle_loop = i.key_pressed(egui::Key::L);
                                                        if i.key_pressed(egui::Key::ArrowLeft) { key_seek_delta -= 5.0; }
                                                        if i.key_pressed(egui::Key::ArrowRight) { key_seek_delta += 5.0; }
                                                    });
                                                }

                                                if close_clicked { still_open = false; }
                                                if toggle_clicked { show_sidebar = !show_sidebar; }

                                                if is_video {
                                                    if viewer_player.is_none() {
                                                        if let Ok(source) = Source::parse(&path) {
                                                            if let Ok(player) = Player::open(source, PlayerConfig::default()) {
                                                                player.set_volume(video_volume);
                                                                player.set_muted(video_muted);
                                                                let usable = restore_video
                                                                    .as_ref()
                                                                    .filter(|memory| video_memory_valid(&path, memory));
                                                                let should_play = usable.map_or(true, |memory| memory.was_playing);
                                                                if let Some(memory) = usable {
                                                                    if memory.position_secs > 0.0 {
                                                                        player.seek(Duration::from_secs_f64(memory.position_secs));
                                                                    }
                                                                }
                                                                if should_play { player.play(); }
                                                                viewer_player = Some(player);
                                                            }
                                                        }
                                                    }

                                                    if controls.toggle_mute || key_toggle_mute {
                                                        video_muted = !video_muted;
                                                    }
                                                    if let Some(fraction) = controls.volume_fraction {
                                                        video_volume = fraction.clamp(0.0, 1.0);
                                                        video_muted = video_volume <= 0.0;
                                                    }
                                                    if controls.toggle_loop || key_toggle_loop {
                                                        video_loop = !video_loop;
                                                    }

                                                    if let Some(player) = &viewer_player {
                                                        player.set_volume(video_volume);
                                                        player.set_muted(video_muted);
                                                        if controls.toggle_play || key_toggle_play {
                                                            if matches!(player.state(), PlayerState::Playing | PlayerState::Buffering) {
                                                                player.pause();
                                                            } else {
                                                                player.play();
                                                            }
                                                        }
                                                        if let Some(fraction) = controls.seek_fraction {
                                                            if let Some(duration) = player.duration() {
                                                                player.seek(Duration::from_secs_f64(
                                                                    duration.as_secs_f64() * fraction as f64,
                                                                ));
                                                            }
                                                        }
                                                        if key_seek_delta != 0.0 {
                                                            let target = player.position().as_secs_f64() + key_seek_delta;
                                                            player.seek(Duration::from_secs_f64(target.max(0.0)));
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
                                                        let mut ended = false;
                                                        for event in player.events().try_iter() {
                                                            if let PlayerEvent::Ended = event { ended = true; }
                                                        }
                                                        if ended {
                                                            if video_loop {
                                                                player.seek(Duration::ZERO);
                                                                player.play();
                                                            } else {
                                                                still_open = false;
                                                            }
                                                        }
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

                                                if let Some(layout) = &controls_layout {
                                                    let paint_playing = viewer_player.as_ref().map_or(false, |p| {
                                                        matches!(p.state(), PlayerState::Playing | PlayerState::Buffering)
                                                    });
                                                    let paint_position = viewer_player.as_ref()
                                                        .map_or(Duration::ZERO, |p| p.position());
                                                    let paint_duration = viewer_player.as_ref().and_then(|p| p.duration());
                                                    paint_video_controls(
                                                        ui,
                                                        layout,
                                                        &controls,
                                                        paint_playing,
                                                        paint_position,
                                                        paint_duration,
                                                        video_volume,
                                                        video_muted,
                                                        video_loop,
                                                    );
                                                }

                                                let close_bg = if close_hovered { egui::Color32::from_white_alpha(50) } else { egui::Color32::from_black_alpha(150) };
                                                ui.painter().rect_filled(close_rect, 8.0, close_bg);
                                                ui.painter().text(close_rect.center(), egui::Align2::CENTER_CENTER, "✕", egui::FontId::proportional(20.0), egui::Color32::WHITE);

                                                let toggle_bg = if toggle_hovered { egui::Color32::from_white_alpha(50) } else { egui::Color32::from_black_alpha(150) };
                                                ui.painter().rect_filled(toggle_rect, 8.0, toggle_bg);
                                                let toggle_icon = if show_sidebar { "▶" } else { "◀" };
                                                ui.painter().text(toggle_rect.center(), egui::Align2::CENTER_CENTER, toggle_icon, egui::FontId::proportional(18.0), egui::Color32::WHITE);

                                                if show_sidebar {
                                                    let (apply, revert) = crate::sidebar::draw_sidebar(
                                                        ui,
                                                        sidebar_rect,
                                                        &mut is_edit_mode,
                                                        &mut staged_sidecar,
                                                        &mut filter_query,
                                                        &tag_tree,
                                                        &mut search_query,
                                                        &mut show_preserved,
                                                        Some(Path::new(&path)),
                                                        query_error.as_deref(),
                                                        &filter_scope,
                                                        &mut viewer_actions,
                                                    );
                                                    if apply { apply_clicked = true; }
                                                    if revert { revert_clicked = true; }
                                                }
                                            });

                                            if ctx.input(|i| i.viewport().close_requested()) { still_open = false; }
                                        },
            );

            // Tag search is live, like the file-name search box. Applying the
            // filter only confirms the result and closes the viewer; it is not
            // required for the gallery to update.
            if !is_edit_mode && filter_query.claim_scope == ClaimScope::Curated {
                // AI-prompt filters are owned by Tag Manager. The curated
                // sidebar must not silently replace an applied machine query.
                let preview = filter_query.clone();
                match preview.compile_for_apply(&tag_tree) {
                    Ok(query) => {
                        let confidence = preview.confidence_range();
                        let scope = preview.claim_scope;
                        if query != active_query {
                            active_query = query;
                            applied_confidence = Some(confidence);
                            applied_scope = Some(scope);
                            query_error = None;
                        } else if !applied_confidence.is_some_and(|range| range == confidence)
                            || !applied_scope.is_some_and(|value| value == scope)
                        {
                            applied_confidence = Some(confidence);
                            applied_scope = Some(scope);
                        }
                    }
                    Err(error) => query_error = Some(error),
                }
            }

            let mut state_changed = apply_clicked || revert_clicked;

            if apply_clicked {
                if is_edit_mode {
                    if let Some(sc) = &staged_sidecar {
                        let mut targets = self.gallery.as_ref().unwrap().selected_images.clone();
                        if targets.is_empty() { targets.push(path.clone()); }
                        for p in targets {
                            if let Some(g) = &mut self.gallery {
                                if let Some(item) = g.items.iter_mut().find(|i| i.path == p) {
                                    item.sidecar = sc.clone();
                                    item.tags = sc.active_tags().into_iter().collect();
                                    item.rating = sc.rating;
                                    let _ = item.sidecar.save(Path::new(&item.path));

                                    let index_path = crate::gallery::workers::cache_dir().join("index.tsv");
                                    crate::gallery::workers::update_index_entry(
                                        &index_path,
                                        &item.path,
                                        &item.sidecar,
                                    );
                                }
                            }
                        }
                        if let Some(g) = &mut self.gallery {
                            g.rebuild_tag_tree();
                        }
                    }
                } else {
                    // Prompt and exact filters compile to the same AST. The
                    // rating control is folded in by FilterQuery::compile.
                    match filter_query.compile_for_apply(&tag_tree) {
                        Ok(parsed) => {
                            active_query = parsed;
                            applied_confidence = Some(filter_query.confidence_range());
                            applied_scope = Some(filter_query.claim_scope);
                            query_error = None;
                            self.set_status(format!(
                                "Filter applied: {} · {} confidence · {} claims",
                                active_query,
                                filter_query.confidence_range(),
                                filter_query.claim_scope
                            ));
                            // Show the filtered gallery instead of leaving the viewer on top.
                            still_open = false;
                        }
                        Err(error) => {
                            query_error = Some(error.clone());
                            self.set_status(format!("Invalid filter: {}", error));
                            // Keep the last valid filter instead of silently showing everything.
                        }
                    }
                }
            }

            if revert_clicked {
                if is_edit_mode {
                    if let Some(g) = &self.gallery {
                        if let Some(item) = g.items.iter().find(|i| i.path == path) {
                            staged_sidecar = Some(item.sidecar.clone());
                        }
                    }
                } else {
                    filter_query = FilterQuery::default();
                    active_query = crate::gallery::Query::AlwaysTrue;
                    applied_confidence = Some(ConfidenceRange::accepted_only());
                    applied_scope = Some(ClaimScope::Curated);
                    query_error = None;
                    self.set_status("Filters cleared");
                }
            }

            self.viewer_cache = viewer_cache;
            self.viewer_player = viewer_player;
            self.viewer_anim_frames = anim_frames;
            self.viewer_anim_durations = anim_durs;
            self.viewer_zoom = zoom;
            self.viewer_pan = pan;
            self.viewer_needs_fit = needs_fit;
            self.viewer_last_fit_size = last_fit_size;
            self.viewer_show_sidebar = show_sidebar;
            self.viewer_sidebar_width = sidebar_width;
            self.is_edit_mode = is_edit_mode;
            if let Some(sc) = staged_sidecar {
                self.staged_sidecars.insert(path.clone(), sc);
            }
            self.filter_query = filter_query;
            let video_settings_changed = self.video_volume != video_volume
                || self.video_muted != video_muted
                || self.video_loop != video_loop;
            self.video_volume = video_volume;
            self.video_muted = video_muted;
            self.video_loop = video_loop;
            if let Some(g) = &mut self.gallery {
                if g.show_preserved_tags != show_preserved {
                    g.show_preserved_tags = show_preserved;
                    g.rebuild_tag_tree();
                }
                g.tag_search_query = search_query;
                let confidence_changed =
                    applied_confidence.is_some_and(|confidence| g.filter_confidence != confidence);
                let scope_changed =
                    applied_scope.is_some_and(|scope| g.filter_scope != scope);
                if g.active_query != active_query || confidence_changed || scope_changed {
                    g.invalidate_filter_cache();
                }
                g.active_query = active_query;
                if let Some(confidence) = applied_confidence {
                    g.filter_confidence = confidence;
                }
                if let Some(scope) = applied_scope {
                    g.filter_scope = scope;
                }
                g.query_error = query_error;
            }

            // Tag-tree menus emit Actions from inside the viewer viewport. Apply
            // them only after the snapshot above has been written back, so a
            // rename/merge cannot be overwritten by the viewer's local copies.
            if !viewer_actions.is_empty() {
                state_changed = true;
                for action in viewer_actions {
                    self.apply_action(ctx, action);
                }
                if !self.viewer_open {
                    still_open = false;
                }
            }

            if state_changed || video_settings_changed || !still_open {
                self.persist_state();
            }

            if !still_open {
                self.remember_current_video();
                self.save_video_state();
                self.viewer_open = false;
                        self.viewer_loaded_path = None;
                if let Some(g) = &mut self.gallery { g.viewer_target = None; g.selected_images.clear(); }
                self.viewer_cache.clear();
                self.viewer_player = None;
                self.viewer_anim_frames.clear();
            } else if video_settings_changed {
                self.save_video_state();
            }
        }

        self.sync_tag_revision_to_gallery();
    }
}

fn video_file_fingerprint(path: &str) -> (Option<u64>, Option<u64>) {
    match std::fs::metadata(path) {
        Ok(meta) => {
            let modified = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            (modified, Some(meta.len()))
        }
        Err(_) => (None, None),
    }
}

fn video_memory_valid(path: &str, memory: &VideoPlaybackMemory) -> bool {
    let (modified, len) = video_file_fingerprint(path);
    memory.modified_secs == modified
        && memory.file_len == len
        && memory.position_secs.is_finite()
        && memory.position_secs >= 0.0
}

fn format_video_time(duration: Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[derive(Clone, Copy)]
struct VideoControlsLayout {
    outer: egui::Rect,
    compact: bool,
    center_y: f32,
    play_rect: egui::Rect,
    time_rect: egui::Rect,
    loop_rect: egui::Rect,
    volume_icon_rect: Option<egui::Rect>,
    volume_track_rect: Option<egui::Rect>,
    seek_hit: egui::Rect,
}

fn video_controls_layout(image_rect: egui::Rect) -> Option<VideoControlsLayout> {
    // Keep clear of the viewer's close/sidebar buttons at the lower-right corner.
    let outer = egui::Rect::from_min_max(
        egui::pos2(image_rect.min.x + 12.0, image_rect.max.y - 104.0),
        egui::pos2(image_rect.max.x - 12.0, image_rect.max.y - 68.0),
    );
    if outer.width() <= 16.0 || outer.height() <= 16.0 {
        return None;
    }

    let compact = outer.width() < 380.0;
    let pad = 8.0;
    let gap = 8.0;
    let button_size = 26.0;
    let center_y = outer.center().y;
    let mut left = outer.min.x + pad;

    let play_rect = egui::Rect::from_min_size(
        egui::pos2(left, center_y - button_size / 2.0),
        egui::vec2(button_size, button_size),
    );
    left += button_size + gap;

    let time_width = if compact { 0.0 } else { 104.0 };
    let time_rect = egui::Rect::from_min_size(
        egui::pos2(left, center_y - 9.0),
        egui::vec2(time_width, 18.0),
    );
    left += time_width;
    if time_width > 0.0 {
        left += gap;
    }

    let mut right = outer.max.x - pad;
    let loop_rect = egui::Rect::from_min_size(
        egui::pos2(right - button_size, center_y - button_size / 2.0),
        egui::vec2(button_size, button_size),
    );
    right -= button_size + gap;

    let (volume_icon_rect, volume_track_rect) = if compact {
        (None, None)
    } else {
        let track = egui::Rect::from_min_size(
            egui::pos2(right - 56.0, center_y - 9.0),
            egui::vec2(56.0, 18.0),
        );
        right -= track.width() + gap;
        let icon = egui::Rect::from_min_size(
            egui::pos2(right - 20.0, center_y - 10.0),
            egui::vec2(20.0, 20.0),
        );
        right -= icon.width() + gap;
        (Some(icon), Some(track))
    };

    let seek_hit = egui::Rect::from_min_size(
        egui::pos2(left, center_y - 10.0),
        egui::vec2((right - left).max(12.0), 20.0),
    );

    Some(VideoControlsLayout {
        outer,
        compact,
        center_y,
        play_rect,
        time_rect,
        loop_rect,
        volume_icon_rect,
        volume_track_rect,
        seek_hit,
    })
}

#[derive(Clone, Copy, Default)]
struct VideoControlFeedback {
    toggle_play: bool,
    toggle_mute: bool,
    toggle_loop: bool,
    seek_fraction: Option<f32>,
    volume_fraction: Option<f32>,
    hovered: bool,
    play_hovered: bool,
    loop_hovered: bool,
    mute_hovered: bool,
}

fn interact_video_controls(ui: &egui::Ui, layout: &VideoControlsLayout) -> VideoControlFeedback {
    let mut feedback = VideoControlFeedback::default();

    let play_response = ui.interact(layout.play_rect, ui.id().with("video_play"), egui::Sense::click());
    feedback.toggle_play = play_response.clicked();
    feedback.play_hovered = play_response.hovered();
    feedback.hovered |= play_response.hovered();

    let loop_response = ui.interact(layout.loop_rect, ui.id().with("video_loop"), egui::Sense::click());
    feedback.toggle_loop = loop_response.clicked();
    feedback.loop_hovered = loop_response.hovered();
    feedback.hovered |= loop_response.hovered();

    let seek_response = ui.interact(layout.seek_hit, ui.id().with("video_seek"), egui::Sense::click_and_drag());
    let seek_used = seek_response.clicked() || seek_response.dragged();
    feedback.hovered |= seek_response.hovered() || seek_response.dragged();
    if seek_used {
        feedback.seek_fraction = ui.input(|input| {
            input.pointer.hover_pos().map(|point| {
                ((point.x - layout.seek_hit.min.x) / layout.seek_hit.width()).clamp(0.0, 1.0)
            })
        });
    }

    if let (Some(icon), Some(track)) = (layout.volume_icon_rect, layout.volume_track_rect) {
        let mute_response = ui.interact(icon, ui.id().with("video_mute"), egui::Sense::click());
        feedback.toggle_mute = mute_response.clicked();
        feedback.mute_hovered = mute_response.hovered();
        feedback.hovered |= mute_response.hovered();

        let volume_response = ui.interact(track, ui.id().with("video_volume"), egui::Sense::click_and_drag());
        let volume_used = volume_response.clicked() || volume_response.dragged();
        feedback.hovered |= volume_response.hovered() || volume_response.dragged();
        if volume_used {
            feedback.volume_fraction = ui.input(|input| {
                input.pointer.hover_pos().map(|point| {
                    ((point.x - track.min.x) / track.width()).clamp(0.0, 1.0)
                })
            });
        }
    }

    feedback.hovered |= ui.input(|input| {
        input.pointer.hover_pos().map_or(false, |point| layout.outer.contains(point))
    });
    feedback
}

fn paint_video_controls(
    ui: &egui::Ui,
    layout: &VideoControlsLayout,
    feedback: &VideoControlFeedback,
    playing: bool,
    position: Duration,
    duration: Option<Duration>,
    volume: f32,
    muted: bool,
    loop_enabled: bool,
) {
    let painter = ui.painter();
    painter.rect_filled(layout.outer, 8.0, egui::Color32::from_black_alpha(190));
    painter.rect_stroke(
        layout.outer,
        8.0,
        egui::Stroke::new(1.0_f32, egui::Color32::from_white_alpha(24)),
    );

    let button_bg = if feedback.play_hovered {
        egui::Color32::from_white_alpha(45)
    } else {
        egui::Color32::from_white_alpha(16)
    };
    painter.rect_filled(layout.play_rect, 6.0, button_bg);
    let play_center = layout.play_rect.center();
    if playing {
        for offset in [-4.0, 3.0] {
            painter.rect_filled(
                egui::Rect::from_center_size(
                    egui::pos2(play_center.x + offset, play_center.y),
                    egui::vec2(3.0, 12.0),
                ),
                1.0,
                egui::Color32::WHITE,
            );
        }
    } else {
        painter.add(egui::Shape::convex_polygon(
            vec![
                egui::pos2(play_center.x - 4.0, play_center.y - 7.0),
                egui::pos2(play_center.x + 7.0, play_center.y),
                egui::pos2(play_center.x - 4.0, play_center.y + 7.0),
            ],
            egui::Color32::WHITE,
            egui::Stroke::NONE,
        ));
    }

    if !layout.compact {
        let label = match duration {
            Some(total) => format!(
                "{} / {}",
                format_video_time(position),
                format_video_time(total)
            ),
            None => format_video_time(position),
        };
        painter.text(
            layout.time_rect.min,
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(12.0),
            egui::Color32::from_white_alpha(210),
        );
    }

    let progress = duration
        .filter(|d| d.as_secs_f64() > 0.0)
        .map_or(0.0, |d| (position.as_secs_f64() / d.as_secs_f64()).clamp(0.0, 1.0) as f32);
    let track_line = egui::Rect::from_min_max(
        egui::pos2(layout.seek_hit.min.x, layout.center_y - 2.0),
        egui::pos2(layout.seek_hit.max.x, layout.center_y + 2.0),
    );
    painter.rect_filled(track_line, 2.0, egui::Color32::from_white_alpha(55));
    if progress > 0.0 {
        let filled = egui::Rect::from_min_max(
            track_line.min,
            egui::pos2(track_line.min.x + track_line.width() * progress, track_line.max.y),
        );
        painter.rect_filled(filled, 2.0, egui::Color32::from_rgb(95, 175, 255));
    }
    painter.circle_filled(
        egui::pos2(track_line.min.x + track_line.width() * progress, layout.center_y),
        5.0,
        egui::Color32::WHITE,
    );

    if let (Some(icon), Some(track)) = (layout.volume_icon_rect, layout.volume_track_rect) {
        let icon_center = icon.center();
        let icon_color = if feedback.mute_hovered {
            egui::Color32::WHITE
        } else {
            egui::Color32::from_white_alpha(225)
        };
        painter.add(egui::Shape::convex_polygon(
            vec![
                egui::pos2(icon_center.x - 7.0, icon_center.y - 3.0),
                egui::pos2(icon_center.x - 3.0, icon_center.y - 3.0),
                egui::pos2(icon_center.x + 2.0, icon_center.y - 7.0),
                egui::pos2(icon_center.x + 2.0, icon_center.y + 7.0),
                egui::pos2(icon_center.x - 3.0, icon_center.y + 3.0),
                egui::pos2(icon_center.x - 7.0, icon_center.y + 3.0),
            ],
            icon_color,
            egui::Stroke::NONE,
        ));
        if muted {
            painter.line_segment(
                [
                    egui::pos2(icon_center.x + 4.0, icon_center.y - 4.0),
                    egui::pos2(icon_center.x + 10.0, icon_center.y + 4.0),
                ],
                egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255, 120, 120)),
            );
        } else {
            for offset in [4.0, 8.0] {
                painter.line_segment(
                    [
                        egui::pos2(icon_center.x + 3.0, icon_center.y - 3.0 - offset * 0.4),
                        egui::pos2(icon_center.x + 5.0 + offset * 0.3, icon_center.y - 6.0 - offset * 0.4),
                    ],
                    egui::Stroke::new(1.5_f32, egui::Color32::from_white_alpha(200)),
                );
                painter.line_segment(
                    [
                        egui::pos2(icon_center.x + 3.0, icon_center.y + 3.0 + offset * 0.4),
                        egui::pos2(icon_center.x + 5.0 + offset * 0.3, icon_center.y + 6.0 + offset * 0.4),
                    ],
                    egui::Stroke::new(1.5_f32, egui::Color32::from_white_alpha(200)),
                );
            }
        }

        let volume_line = egui::Rect::from_min_max(
            egui::pos2(track.min.x, layout.center_y - 2.0),
            egui::pos2(track.max.x, layout.center_y + 2.0),
        );
        painter.rect_filled(volume_line, 2.0, egui::Color32::from_white_alpha(55));
        let vol_progress = if muted { 0.0 } else { volume.clamp(0.0, 1.0) };
        if vol_progress > 0.0 {
            let filled = egui::Rect::from_min_max(
                volume_line.min,
                egui::pos2(volume_line.min.x + volume_line.width() * vol_progress, volume_line.max.y),
            );
            painter.rect_filled(filled, 2.0, egui::Color32::from_rgb(95, 175, 255));
        }
        painter.circle_filled(
            egui::pos2(volume_line.min.x + volume_line.width() * vol_progress, layout.center_y),
            4.0,
            egui::Color32::WHITE,
        );
    }

    let loop_color = if loop_enabled {
        egui::Color32::from_rgb(95, 175, 255)
    } else if feedback.loop_hovered {
        egui::Color32::from_white_alpha(235)
    } else {
        egui::Color32::from_white_alpha(170)
    };
    let loop_bg = if feedback.loop_hovered {
        egui::Color32::from_white_alpha(45)
    } else {
        egui::Color32::from_white_alpha(16)
    };
    painter.rect_filled(layout.loop_rect, 6.0, loop_bg);
    painter.text(
        layout.loop_rect.center(),
        egui::Align2::CENTER_CENTER,
        "↻",
        egui::FontId::proportional(17.0),
        loop_color,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("symtropy-undo-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn create_can_be_undone_and_redone() {
        let root = scratch("create");
        let path = root.join("new folder");
        assert!(file_manager::create_item(&root, "new folder", true).is_ok());
        let op = UndoOp::Create { path: path.clone(), is_dir: true };
        assert!(undo_files(&op).is_ok());
        assert!(!path.exists());
        assert!(redo_files(&op).is_ok());
        assert!(path.is_dir());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rename_can_be_undone_and_redone() {
        let root = scratch("rename");
        let from = root.join("a.txt");
        let to = root.join("b.txt");
        std::fs::write(&from, "x").unwrap();
        let op = UndoOp::Rename { from: from.clone(), to: to.clone() };
        assert_eq!(file_manager::rename(&from, "b.txt").unwrap(), to);
        assert!(undo_files(&op).is_ok());
        assert!(from.exists() && !to.exists());
        assert!(redo_files(&op).is_ok());
        assert!(to.exists() && !from.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn copied_transfer_undo_removes_only_the_destination() {
        let root = scratch("copy");
        let src = root.join("a.txt");
        let dest_dir = root.join("dest");
        std::fs::create_dir_all(&dest_dir).unwrap();
        std::fs::write(&src, "x").unwrap();
        let pairs = file_manager::transfer(&[src.clone()], &dest_dir, true).unwrap();
        let op = UndoOp::Transfer { pairs: pairs.clone(), copy: true };
        assert!(undo_files(&op).is_ok());
        assert!(src.exists());
        assert!(!pairs[0].1.exists());
        assert!(redo_files(&op).is_ok());
        assert!(src.exists() && pairs[0].1.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn moved_transfer_undo_restores_the_source_and_redo_moves_it_back() {
        let root = scratch("move");
        let src = root.join("a.txt");
        let dest_dir = root.join("dest");
        std::fs::create_dir_all(&dest_dir).unwrap();
        std::fs::write(&src, "x").unwrap();
        let pairs = file_manager::transfer(&[src.clone()], &dest_dir, false).unwrap();
        let op = UndoOp::Transfer { pairs: pairs.clone(), copy: false };
        assert!(!src.exists() && pairs[0].1.exists());
        assert!(undo_files(&op).is_ok());
        assert!(src.exists() && !pairs[0].1.exists());
        assert_eq!(std::fs::read_to_string(&src).unwrap(), "x");
        assert!(redo_files(&op).is_ok());
        assert!(!src.exists() && pairs[0].1.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn duplicate_finder_groups_identical_files_only() {
        let root = scratch("duplicates");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("a.bin"), b"same content").unwrap();
        std::fs::write(root.join("b.bin"), b"same content").unwrap();
        std::fs::write(root.join("c.bin"), b"other content").unwrap();
        std::fs::write(root.join("nested").join("d.bin"), b"same content").unwrap();

        let groups = find_duplicate_groups(&root);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0]
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["a.bin", "b.bin"]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn staged_sidecars_are_isolated_by_full_path() {
        let mut staged = StagedSidecars::default();
        let mut first = Sidecar::default();
        first.tags.push("first-image-tag".to_string());
        let mut second = Sidecar::default();
        second.tags.push("second-image-tag".to_string());

        staged.insert("/library/first.jpg", first);
        staged.insert("/library/second.jpg", second);

        let first_out = staged.take("/library/first.jpg").unwrap();
        assert_eq!(first_out.tags, ["first-image-tag"]);
        assert_eq!(
            staged.get("/library/second.jpg").unwrap().tags,
            ["second-image-tag"]
        );
        assert!(staged.get("/library/first.jpg").is_none());
    }

    #[test]
    fn saved_state_tolerates_missing_new_fields() {
        let saved: SavedState = serde_json::from_str(r#"{"current_dir":"/tmp"}"#).unwrap();
        assert_eq!(saved.current_dir, Some(PathBuf::from("/tmp")));
        assert!(saved.sort.is_none());
        assert!(saved.sort_descending.is_none());
        assert!(saved.folder_views.is_empty());
        assert!(saved.recent_dirs.is_empty());
    }

    #[test]
    fn folder_view_memory_never_stores_active_filters() {
        let legacy = r#"{"grid_scale":1.5,"query_input":"tag","active_query":"tag","active_tags":["tag"]}"#;
        let view: FolderViewMemory = serde_json::from_str(legacy).unwrap();
        assert_eq!(view.grid_scale, Some(1.5));

        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("query_input"));
        assert!(!json.contains("active_query"));
        assert!(!json.contains("active_tags"));
    }

    #[test]
    fn subtree_remap_rewrites_sidecar_tags_and_links() {
        let mut sc = Sidecar::default();
        sc.tags = vec!["animal".into(), "fox".into(), "fennec".into()];
        sc.tag_parents.insert("fox".into(), "animal".into());
        sc.tag_parents.insert("fennec".into(), "fox".into());
        let descendants: HashSet<String> = ["fox", "fennec"].into_iter().map(str::to_string).collect();

        assert!(remap_subtree_in_sidecar(
            &mut sc,
            &descendants,
            "fox",
            Some("fox"),
            &["animal".to_string()],
            &["creature".to_string()],
        ));
        let mut tags = sc.tags.clone();
        tags.sort();
        assert_eq!(tags, vec!["creature", "fennec", "fox"]);
        assert_eq!(sc.tag_parents.get("fox").map(String::as_str), Some("creature"));
        assert_eq!(sc.tag_parents.get("fennec").map(String::as_str), Some("fox"));
        assert!(!sc.tag_parents.contains_key("animal"));
    }

    #[test]
    fn deleting_a_tag_drops_its_links_but_keeps_descendants() {
        let mut sc = Sidecar::default();
        sc.tags = vec!["animal".into(), "fox".into(), "fennec".into()];
        sc.tag_parents.insert("fox".into(), "animal".into());
        sc.tag_parents.insert("fennec".into(), "fox".into());
        let descendants: HashSet<String> = ["fox", "fennec"].into_iter().map(str::to_string).collect();

        assert!(remap_subtree_in_sidecar(
            &mut sc,
            &descendants,
            "fox",
            None,
            &["animal".to_string()],
            &[],
        ));
        assert_eq!(sc.tags, vec!["fennec"]);
        assert!(sc.tag_parents.is_empty());
    }

    #[test]
    fn saved_state_never_stores_query_history_or_presets() {
        let legacy = r#"{"query_history":["cat"],"filter_presets":[{"name":"p","query":"cat"}]}"#;
        let saved: SavedState = serde_json::from_str(legacy).unwrap();

        let json = serde_json::to_string(&saved).unwrap();
        assert!(!json.contains("query_history"));
        assert!(!json.contains("filter_presets"));
    }

    #[test]
    fn saved_state_keeps_explicit_onnx_paths() {
        let legacy = r#"{"onnx_vision":{"runtime_path":"/opt/onnx/libonnxruntime.so","model_path":"/models/clip.onnx","labels_path":"/models/labels.txt"}}"#;
        let saved: SavedState = serde_json::from_str(legacy).unwrap();
        let config = saved.onnx_vision.unwrap();

        assert_eq!(config.runtime_path, "/opt/onnx/libonnxruntime.so");
        assert_eq!(config.model_path, "/models/clip.onnx");
        assert_eq!(config.labels_path, "/models/labels.txt");
        assert_eq!(config.input_name, "input");
        assert_eq!(config.top_k, 5);
    }
}
