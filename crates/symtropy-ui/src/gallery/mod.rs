use egui::TextureHandle;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::{Instant, UNIX_EPOCH};

use crate::file_manager::{compare_sortable, SortDirection, SortKey, SortableEntry};
use crate::filter::FilterPass;

pub mod hierarchy;
pub mod layout;
pub mod query;
pub mod sidecar;
pub mod ui;
pub mod workers;

pub use hierarchy::Hierarchy;
pub use query::{ClaimScope, ClaimSource, ConfidenceRange, Query, TagEvidence};
pub use sidecar::Sidecar;
pub use workers::{Msg, ScanMsg};

pub const MAX_UPLOADS_PER_FRAME: usize = 24;
pub const MAX_TEXTURES: usize = 400;
pub const MARGIN: f32 = 0.0;
pub const TARGET_ROW_HEIGHT: f32 = 200.0;

/// Height is quantized for zoom stability. The mosaic layout is O(n), so width
/// uses exact pixels and always fills the viewport.
pub(crate) const LAYOUT_WIDTH_BUCKET: i32 = 1;
pub(crate) const LAYOUT_HEIGHT_BUCKET: i32 = 4;

pub(crate) fn layout_bucket(px: i32, bucket: i32) -> i32 {
    let bucket = bucket.max(1);
    ((px.max(1) / bucket) * bucket).max(1)
}

/// How gallery thumbnails are arranged. Rows are uniform and predictable;
/// masonry keeps the variable-height aspect mosaic for dense libraries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GalleryLayout {
    #[default]
    Rows,
    Masonry,
}

pub const SCAN_TIMEOUT_SECS: u64 = 1800;

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct BorderColors {
    pub top: Option<[u8; 4]>,
    pub bottom: Option<[u8; 4]>,
    pub left: Option<[u8; 4]>,
    pub right: Option<[u8; 4]>,
}

impl BorderColors {
    pub fn is_flexible(&self) -> bool {
        self.top.is_some() || self.bottom.is_some() || self.left.is_some() || self.right.is_some()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Margins {
    pub top: u32,
    pub bottom: u32,
    pub left: u32,
    pub right: u32,
}

#[derive(Clone, Debug)]
pub struct GalleryItem {
    pub id: String,
    pub title: String,
    pub path: String,
    pub is_folder: bool,
    pub ext: String,
    pub aspect_ratio: f32,
    pub width: f32,
    pub height: f32,
    pub border_colors: BorderColors,
    pub margins: Margins,
    pub rating: u8,
    pub tags: HashSet<String>,
    pub sidecar: Sidecar,
}

#[derive(Clone, Debug)]
pub struct TagNode {
    pub name: String,
    pub children: Vec<TagNode>,
}

pub fn tree_from_hierarchy(h: &Hierarchy) -> TagNode {
    fn build(h: &Hierarchy, name: &str) -> TagNode {
        let children = h
        .node(name)
        .map(|n| n.children.iter().map(|c| build(h, c)).collect())
        .unwrap_or_default();
        TagNode { name: name.to_string(), children }
    }
    TagNode {
        name: "Root".to_string(),
        children: h.roots().iter().map(|r| build(h, r)).collect(),
    }
}

pub enum ThumbContent {
    Static(TextureHandle),
    Animated { frames: Vec<TextureHandle>, durations: Vec<f32> },
}

pub(crate) struct Thumb {
    pub px: [u32; 2],
    pub content: ThumbContent,
}

pub(crate) struct LayoutCache {
    pub key: (u8, i32, i32, i32, usize, u64),
    pub cells: Vec<layout::Cell>,
    pub total_h: i32,
}

pub fn frame_from_time(durations: &[f32], elapsed: f64) -> usize {
    if durations.is_empty() {
        return 0;
    }
    let total: f64 = durations.iter().map(|d| (*d as f64).max(0.0)).sum();
    if !(total > 0.0) || !elapsed.is_finite() {
        return 0;
    }
    let wrapped = elapsed.abs() % total;
    let mut acc = 0.0;
    for (i, dur) in durations.iter().enumerate() {
        acc += (*dur as f64).max(0.0);
        if wrapped < acc {
            return i;
        }
    }
    durations.len() - 1
}

pub struct GalleryState {
    pub items: Vec<GalleryItem>,

    pub(crate) scanning: bool,
    pub(crate) scan_rx: Receiver<ScanMsg>,
    pub(crate) scan_progress: Arc<AtomicUsize>,
    pub(crate) scan_started_at: Option<Instant>,

    pub(crate) shared: Arc<workers::Shared>,
    pub(crate) result_rx: Receiver<Msg>,

    pub(crate) thumbs: HashMap<String, Thumb>,
    pub(crate) failed: HashSet<String>,
    pub(crate) inflight: HashMap<String, [u32; 2]>,
    pub(crate) icon_cache: HashMap<String, Option<TextureHandle>>,

    pub(crate) layout_cache: Option<LayoutCache>,
    pub(crate) filtered_cache: Vec<usize>,
    pub(crate) filter_cache_key: Option<(String, String, usize, u64, ConfidenceRange, ClaimScope, u64)>,
    pub(crate) filter_generation: u64,
    /// Effective per-image tag confidences used by the active filter.
    pub(crate) filter_evidence: HashMap<String, TagEvidence>,
    pub(crate) filter_confidence: ConfidenceRange,
    /// Which authorities the active filter may match. Curated is the default;
    /// machine scope is only installed by an explicit Tag Manager search.
    pub(crate) filter_scope: ClaimScope,
    pub(crate) filter_revision: u64,
    pub(crate) anim_start: HashMap<String, f64>,

    pub grid_scale: f32,
    pub layout: GalleryLayout,
    pub scroll_offset: f32,
    pub sort_key: SortKey,
    pub sort_direction: SortDirection,
    pub selected_images: Vec<String>,
    pub selection_anchor: Option<String>,
    /// Item addressed by keyboard navigation (independent of the range anchor).
    pub keyboard_focus: Option<String>,
    pub request_open_viewer: bool,
    pub viewer_target: Option<String>,
    pub tag_tree: TagNode,
    pub tag_search_query: String,
    /// Inspection toggle: include preserved-only tags in the taxonomy tree.
    pub show_preserved_tags: bool,
    pub(crate) tags_text: String,
    pub(crate) tags_path: PathBuf,

    pub hierarchy: Hierarchy,
    pub active_query: Query,
    /// Recursive filename search from the toolbar. Kept separate from the tag
    /// query so `Find files` works for non-image projects too.
    pub filename_query: String,
    /// Visible toolbar toggle: extend the tag filter to descendant folders.
    /// Never saved; the default is current-folder-only.
    pub recursive_tag_search: bool,
    pub(crate) recursive_root: Option<PathBuf>,
    pub(crate) recursive_extra_paths: HashSet<String>,
    pub query_error: Option<String>,

    pub(crate) filter_pass: FilterPass,
    pub(crate) wanted_now: HashMap<String, [u32; 2]>,
    pub(crate) new_jobs: Vec<workers::Job>,
}

impl GalleryState {
    /// Drop every active filter without touching selection or view settings.
    pub fn clear_filters(&mut self) {
        self.active_query = Query::AlwaysTrue;
        self.filename_query.clear();
        self.filter_confidence = ConfidenceRange::accepted_only();
        self.filter_scope = ClaimScope::Curated;
        self.query_error = None;
        self.filter_generation = self.filter_generation.wrapping_add(1);
        self.filter_cache_key = None;
        self.layout_cache = None;
    }

    /// Remove any descendant items added by a previous recursive search.
    /// Called whenever the toggle is switched off or the tag query is cleared,
    /// so an unfiltered gallery never shows files from other folders.
    pub(crate) fn clear_recursive_items(&mut self) {
        if self.recursive_extra_paths.is_empty() {
            return;
        }
        self.items.retain(|item| !self.recursive_extra_paths.contains(&item.path));
        self.recursive_extra_paths.clear();
        self.recursive_root = None;
        self.selected_images.retain(|path| self.items.iter().any(|item| &item.path == path));
        self.invalidate_filter_cache();
    }

    /// Add descendant items from the global index. `root` records which folder
    /// the extras belong to so navigating elsewhere cannot reuse stale rows.
    pub(crate) fn add_recursive_items(&mut self, root: PathBuf, items: Vec<GalleryItem>) {
        self.recursive_extra_paths = items.iter().map(|item| item.path.clone()).collect();
        self.recursive_root = Some(root);
        // Descendant rows are appended in index order. Running the full
        // metadata sort here would stat tens of thousands of files and stall
        // the frame; explicit sort changes still reorder everything later.
        self.items.extend(items);
        self.invalidate_filter_cache();
    }

    pub fn set_layout(&mut self, layout: GalleryLayout) {
        if self.layout == layout {
            return;
        }
        self.layout = layout;
        self.layout_cache = None;
    }

    /// Update the filename portion of the visible set. Lowercasing once here
    /// keeps the per-item check cheap even in a large gallery.
    pub fn set_filename_query(&mut self, query: &str) {
        if self.filename_query == query {
            return;
        }
        self.filename_query = query.to_string();
        self.invalidate_filter_cache();
    }

    pub(crate) fn invalidate_filter_cache(&mut self) {
        self.filter_generation = self.filter_generation.wrapping_add(1);
        self.filter_cache_key = None;
        self.layout_cache = None;
    }

    pub(crate) fn filtered_indices(&mut self) -> &[usize] {
        let filename = self.filename_query.to_lowercase();
        let key = (
            self.active_query.to_string(),
            filename.clone(),
            self.items.len(),
            self.filter_generation,
            self.filter_confidence,
            self.filter_scope,
            self.filter_revision,
        );
        if self.filter_cache_key.as_ref() != Some(&key) {
            let indices = self
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| {
                    if !filename.is_empty() {
                        let haystack = format!("{} {}", item.title, item.path).to_lowercase();
                        if !haystack.contains(&filename) {
                            return false;
                        }
                    }
                    if matches!(self.active_query, Query::AlwaysTrue) {
                        return true;
                    }
                    let fallback = TagEvidence::from_tags(item.tags.iter().map(String::as_str));
                    let evidence = self.filter_evidence.get(&item.path).unwrap_or(&fallback);
                    self.active_query.matches_evidence(
                        evidence,
                        item.rating,
                        &self.hierarchy,
                        self.filter_confidence,
                        self.filter_scope,
                    )
                })
                .map(|(i, _)| i)
                .collect();
            self.filtered_cache = indices;
            self.filter_cache_key = Some(key);
        }
        &self.filtered_cache
    }

    /// Fold sidecar-only tags and their parent links into the taxonomy. The
    /// tags.txt entries come first so their hierarchy wins on duplicates.
    ///
    /// Sidecar-only tags are inserted as real tree nodes instead of loose
    /// indented lines. Flattening them made every tag a root, which broke both
    /// the tree display and ancestor-aware filter matching.
    pub fn rebuild_tag_tree(&mut self) {
        #[derive(Clone)]
        struct PendingTag {
            name: String,
            parent: Option<String>,
        }

        fn note_tag(
            known: &HashSet<String>,
            pending: &mut HashMap<String, PendingTag>,
            name: &str,
            parent: Option<&str>,
        ) {
            let name = name.trim();
            if name.is_empty() {
                return;
            }
            let key = hierarchy::tag_key(name);
            if known.contains(&key) {
                return;
            }
            let entry = pending
                .entry(key)
                .or_insert_with(|| PendingTag { name: name.to_string(), parent: None });
            if entry.parent.is_none() {
                entry.parent = parent.map(str::trim).filter(|p| !p.is_empty()).map(str::to_string);
            }
        }

        let mut hierarchy = Hierarchy::parse(&self.tags_text);
        let mut known: HashSet<String> = hierarchy.keys().into_iter().collect();
        let mut names_by_key: HashMap<String, String> = known
            .iter()
            .filter_map(|key| hierarchy.name_for_key(key).map(|name| (key.clone(), name.to_string())))
            .collect();
        let mut pending: HashMap<String, PendingTag> = HashMap::new();

        for item in &self.items {
            let parent_links: Vec<(String, String)> = if self.show_preserved_tags {
                item.sidecar
                    .tag_parents
                    .iter()
                    .map(|(child, parent)| (child.clone(), parent.clone()))
                    .collect()
            } else {
                item.sidecar.active_tag_parents().into_iter().collect()
            };
            for (child, parent) in parent_links {
                // A tag already in tags.txt keeps its canonical parent. Only
                // sidecar-only descendants need to be grafted onto the tree.
                if !hierarchy.contains_key(&child) {
                    note_tag(&known, &mut pending, &child, Some(&parent));
                }
                note_tag(&known, &mut pending, &parent, None);
            }
            let tags = if self.show_preserved_tags {
                item.sidecar.tags.clone()
            } else {
                item.sidecar.active_tags()
            };
            for tag in tags {
                note_tag(&known, &mut pending, &tag, None);
            }
        }

        // Resolve parents before children, but queue the inserts instead of
        // applying them one at a time. Each isolated insert rebuilds the full
        // ancestor index, which stalled navigation in large folders.
        let mut queued: Vec<(String, Option<String>)> = Vec::new();
        loop {
            let keys: Vec<String> = pending.keys().cloned().collect();
            let mut progress = false;
            for key in keys {
                let Some(node) = pending.get(&key).cloned() else { continue };
                let parent_name = node
                    .parent
                    .as_deref()
                    .map(hierarchy::tag_key)
                    .and_then(|parent_key| names_by_key.get(&parent_key).cloned());
                if node.parent.is_some() && parent_name.is_none() {
                    continue;
                }
                queued.push((node.name.clone(), parent_name));
                names_by_key.insert(key.clone(), node.name.clone());
                pending.remove(&key);
                known.insert(key);
                progress = true;
            }
            if !progress || pending.is_empty() {
                break;
            }
        }
        for node in pending.into_values() {
            queued.push((node.name, None));
        }
        hierarchy.insert_batch(queued);

        self.hierarchy = hierarchy;
        self.tag_tree = tree_from_hierarchy(&self.hierarchy);
        self.filter_generation = self.filter_generation.wrapping_add(1);
        self.filter_cache_key = None;
        self.layout_cache = None;
    }

    /// Persist the current taxonomy and refresh the sidebar tree from it.
    pub fn commit_tag_hierarchy(&mut self) -> Result<(), String> {
        let text = self.hierarchy.to_text();
        if let Some(parent) = self.tags_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let tmp = self.tags_path.with_extension("txt.tmp");
        if std::fs::read_to_string(&self.tags_path).ok().as_deref() != Some(text.as_str()) {
            std::fs::write(&tmp, &text).map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, &self.tags_path).map_err(|e| e.to_string())?;
        }
        self.tags_text = text;
        self.tag_tree = tree_from_hierarchy(&self.hierarchy);
        self.filter_generation = self.filter_generation.wrapping_add(1);
        self.filter_cache_key = None;
        Ok(())
    }

    /// Find the taxonomy closest to a gallery folder. Nested folders such as
    /// `database/References/X` must still use `database/tags.txt`; looking only
    /// at the immediate parent silently started from an empty taxonomy.
    fn find_tags_path(dir: &Path) -> PathBuf {
        let mut current = Some(dir);
        while let Some(path) = current {
            let candidate = path.join("tags.txt");
            if candidate.is_file() {
                return candidate;
            }
            current = path.parent();
        }
        dir.parent()
            .map(|parent| parent.join("tags.txt"))
            .unwrap_or_else(|| PathBuf::from("tags.txt"))
    }

    pub fn scan_dir(ctx: &egui::Context, dir: &Path) -> Self {
        let tags_path = Self::find_tags_path(dir);
        let tags_text = std::fs::read_to_string(&tags_path).unwrap_or_default();
        if tags_text.is_empty() {
            eprintln!("gallery: no tag file at {} — starting from sidecar tags", tags_path.display());
        }
        let hierarchy = Hierarchy::parse(&tags_text);
        eprintln!("gallery: loaded {} tags from {}", hierarchy.len(), tags_path.display());
        let tag_tree = tree_from_hierarchy(&hierarchy);

        let cache = workers::cache_dir();
        let progress = Arc::new(AtomicUsize::new(0));

        let (scan_tx, scan_rx) = channel();
        {
            let (dir, index_path, progress, ctx) =
            (dir.to_path_buf(), cache.join("index.tsv"), progress.clone(), ctx.clone());
            std::thread::spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    workers::scan_worker(dir, index_path, progress, scan_tx, ctx);
                }));
            });
        }

        let shared = Arc::new(workers::Shared {
            queue: std::sync::Mutex::new(Vec::new()),
                              cv: std::sync::Condvar::new(),
                              wanted: std::sync::Mutex::new(HashMap::new()),
        });
        let (tx, result_rx) = channel();
        let n = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .saturating_sub(2)
        .clamp(2, 6);
        for _ in 0..n {
            let (shared, tx, ctx) = (shared.clone(), tx.clone(), ctx.clone());
            std::thread::spawn(move || workers::worker(shared, tx, ctx));
        }

        Self {
            items: Vec::new(),
            scanning: true,
            scan_rx,
            scan_progress: progress,
            scan_started_at: Some(Instant::now()),
            shared,
            result_rx,
            thumbs: HashMap::new(),
            failed: HashSet::new(),
            inflight: HashMap::new(),
            icon_cache: HashMap::new(),
            layout_cache: None,
            filtered_cache: Vec::new(),
            filter_cache_key: None,
            filter_generation: 0,
            filter_evidence: HashMap::new(),
            filter_confidence: ConfidenceRange::accepted_only(),
            filter_scope: ClaimScope::Curated,
            filter_revision: 0,
            anim_start: HashMap::new(),
            grid_scale: 1.0,
            layout: GalleryLayout::default(),
            scroll_offset: 0.0,
            sort_key: SortKey::default(),
            sort_direction: SortKey::default().default_direction(),
            selected_images: Vec::new(),
            selection_anchor: None,
            keyboard_focus: None,
            request_open_viewer: false,
            viewer_target: None,
            tag_tree,
            tag_search_query: String::new(),
            show_preserved_tags: false,
            tags_text,
            tags_path,
            hierarchy,
            active_query: Query::AlwaysTrue,
            filename_query: String::new(),
            recursive_tag_search: false,
            recursive_root: None,
            recursive_extra_paths: HashSet::new(),
            query_error: None,
            filter_pass: FilterPass::default(),
            wanted_now: HashMap::new(),
            new_jobs: Vec::new(),
        }
    }

    /// Store and apply the shared list/gallery sort order.
    pub fn set_sort(&mut self, sort: SortKey, direction: SortDirection) {
        self.sort_key = sort;
        self.sort_direction = direction;
        self.sort_items();
    }

    /// Reorder the gallery with the same smart grouping used by the file list.
    pub fn sort_items(&mut self) {
        let sort = self.sort_key;
        let direction = self.sort_direction;
        let meta: HashMap<String, (u64, i64)> = self
            .items
            .iter()
            .map(|item| {
                let path = PathBuf::from(&item.path);
                let key = std::fs::metadata(&path)
                    .map(|m| {
                        let date = m
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        (m.len(), date)
                    })
                    .unwrap_or((0, 0));
                (item.path.clone(), key)
            })
            .collect();

        self.items.sort_by(|a, b| {
            let (a_len, a_date) = meta.get(&a.path).copied().unwrap_or((0, 0));
            let (b_len, b_date) = meta.get(&b.path).copied().unwrap_or((0, 0));
            compare_sortable(
                &SortableEntry {
                    path: PathBuf::from(&a.path),
                    name: a.title.clone(),
                    is_dir: a.is_folder,
                    rating: a.rating,
                    len: a_len,
                    date: a_date,
                },
                &SortableEntry {
                    path: PathBuf::from(&b.path),
                    name: b.title.clone(),
                    is_dir: b.is_folder,
                    rating: b.rating,
                    len: b_len,
                    date: b_date,
                },
                sort,
                direction,
            )
        });
        self.layout_cache = None;
        self.filter_cache_key = None;
        self.filter_generation = self.filter_generation.wrapping_add(1);
    }

    pub(crate) fn poll(&mut self, ctx: &egui::Context) {
        if self.scanning {
            if let Some(start) = self.scan_started_at {
                if start.elapsed().as_secs() >= SCAN_TIMEOUT_SECS {
                    eprintln!(
                        "gallery: scan still running after {}s — background will finish; items keep arriving",
                        SCAN_TIMEOUT_SECS
                    );
                    self.scanning = false;
                }
            }
        }

        loop {
            match self.scan_rx.try_recv() {
                Ok(ScanMsg::Batch(items)) => {
                    self.items.extend(items);
                    self.filter_generation = self.filter_generation.wrapping_add(1);
                    self.filter_cache_key = None;
                    self.layout_cache = None;
                }
                Ok(ScanMsg::MarginUpdate { path, margins, colors }) => {
                    if let Some(it) = self.items.iter_mut().find(|i| i.path == path) {
                        it.margins = margins;
                        it.border_colors = colors;
                    }
                }
                Ok(ScanMsg::Done) => {
                    self.scanning = false;
                    self.rebuild_tag_tree();
                    self.sort_items();
                    self.filter_generation = self.filter_generation.wrapping_add(1);
                    self.filter_cache_key = None;
                    ctx.request_repaint();
                    eprintln!(
                        "startup: item list ready @ {}ms ({} items)",
                        crate::startup_ms(), self.items.len()
                    );
                }
                Err(_) => break,
            }
        }

        let mut uploads = 0;
        while uploads < MAX_UPLOADS_PER_FRAME {
            match self.result_rx.try_recv() {
                Ok(Msg::Done { path, px, content }) => {
                    if self.inflight.get(&path) == Some(&px) {
                        self.inflight.remove(&path);
                    }
                    let thumb_content = match content {
                        workers::WorkerThumbContent::Static(img) => {
                            let tex = ctx.load_texture(&path, img, egui::TextureOptions::LINEAR);
                            ThumbContent::Static(tex)
                        }
                        workers::WorkerThumbContent::Animated { frames, durations } => {
                            let mut handles = Vec::with_capacity(frames.len());
                            for (i, img) in frames.into_iter().enumerate() {
                                let tex = ctx.load_texture(format!("{}_{}", path, i), img, egui::TextureOptions::LINEAR);
                                handles.push(tex);
                            }
                            ThumbContent::Animated { frames: handles, durations }
                        }
                    };
                    let first_preview = self.thumbs.is_empty();
                    self.thumbs.insert(path, Thumb { px, content: thumb_content });
                    if first_preview {
                        eprintln!("startup: first preview ready @ {}ms", crate::startup_ms());
                    }
                    uploads += 1;
                }
                Ok(Msg::Skipped { path, px }) => {
                    if self.inflight.get(&path) == Some(&px) {
                        self.inflight.remove(&path);
                    }
                }
                Ok(Msg::Failed { path }) => {
                    self.inflight.remove(&path);
                    self.failed.insert(path);
                }
                Err(_) => break,
            }
        }
        if uploads == MAX_UPLOADS_PER_FRAME {
            ctx.request_repaint();
        }
    }

    pub(crate) fn want(&mut self, path: &str, px: [u32; 2], video_duration: Option<f64>) {
        self.wanted_now.insert(path.to_string(), px);
        let have = self.thumbs.get(path).map(|t| t.px == px).unwrap_or(false);
        if !have && self.inflight.get(path) != Some(&px) {
            self.inflight.insert(path.to_string(), px);
            self.new_jobs.push(workers::Job { path: path.to_string(), px, duration: video_duration });
        }
    }

    pub(crate) fn flush_requests(&mut self) {
        if self.thumbs.len() > MAX_TEXTURES {
            let w = &self.wanted_now;
            self.thumbs.retain(|k, _| w.contains_key(k));
            self.anim_start.retain(|k, _| w.contains_key(k));
        }

        *self.shared.wanted.lock().unwrap() = std::mem::take(&mut self.wanted_now);

        if !self.new_jobs.is_empty() {
            let mut q = self.shared.queue.lock().unwrap();
            for j in self.new_jobs.drain(..).rev() {
                q.push(j);
            }
            drop(q);
            self.shared.cv.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_bucket_rounds_down_and_keeps_tiny_widths() {
        assert_eq!(layout_bucket(321, 16), 320);
        assert_eq!(layout_bucket(319, 16), 304);
        assert_eq!(layout_bucket(1, 16), 1);
        assert_eq!(layout_bucket(321, 1), 321);
    }

    #[test]
    fn tags_path_is_found_in_an_ancestor_directory() {
        let base = std::env::temp_dir().join(format!(
            "symtropy-tags-path-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let nested = base.join("References").join("X");
        std::fs::create_dir_all(&nested).unwrap();
        let tags = base.join("tags.txt");
        std::fs::write(&tags, "animal\n  feline\n").unwrap();

        assert_eq!(GalleryState::find_tags_path(&nested), tags);
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn sidecar_tags_keep_their_parent_links() {
        let mut sc = Sidecar::default();
        sc.tags = vec!["thunderclan".into(), "firestar".into()];
        sc.tag_parents.insert("firestar".into(), "thunderclan".into());

        let (scan_tx, scan_rx) = channel::<ScanMsg>();
        drop(scan_tx);
        let (result_tx, result_rx) = channel::<Msg>();
        drop(result_tx);
        let shared = Arc::new(workers::Shared {
            queue: std::sync::Mutex::new(Vec::new()),
            cv: std::sync::Condvar::new(),
            wanted: std::sync::Mutex::new(HashMap::new()),
        });
        let item = GalleryItem {
            id: "a.png".into(),
            title: "a".into(),
            path: "/library/a.png".into(),
            is_folder: false,
            ext: "png".into(),
            aspect_ratio: 1.0,
            width: 1.0,
            height: 1.0,
            border_colors: BorderColors::default(),
            margins: Margins::default(),
            rating: 0,
            tags: sc.active_tags().into_iter().collect(),
            sidecar: sc,
        };
        let mut gallery = GalleryState {
            items: vec![item],
            scanning: false,
            scan_rx,
            scan_progress: Arc::new(AtomicUsize::new(0)),
            scan_started_at: None,
            shared,
            result_rx,
            thumbs: HashMap::new(),
            failed: HashSet::new(),
            inflight: HashMap::new(),
            icon_cache: HashMap::new(),
            layout_cache: None,
            filtered_cache: Vec::new(),
            filter_cache_key: None,
            filter_generation: 0,
            filter_evidence: HashMap::new(),
            filter_confidence: ConfidenceRange::accepted_only(),
            filter_scope: ClaimScope::Curated,
            filter_revision: 0,
            anim_start: HashMap::new(),
            grid_scale: 1.0,
            layout: GalleryLayout::default(),
            scroll_offset: 0.0,
            sort_key: SortKey::default(),
            sort_direction: SortKey::default().default_direction(),
            selected_images: Vec::new(),
            selection_anchor: None,
            keyboard_focus: None,
            request_open_viewer: false,
            viewer_target: None,
            tag_tree: TagNode { name: "Root".into(), children: Vec::new() },
            tag_search_query: String::new(),
            show_preserved_tags: false,
            tags_text: "allegiance (clans)\n  thunderclan\n".into(),
            tags_path: PathBuf::from("/library/tags.txt"),
            hierarchy: Hierarchy::parse("allegiance (clans)\n  thunderclan\n"),
            active_query: Query::AlwaysTrue,
            filename_query: String::new(),
            recursive_tag_search: false,
            recursive_root: None,
            recursive_extra_paths: HashSet::new(),
            query_error: None,
            filter_pass: crate::filter::FilterPass::default(),
            wanted_now: HashMap::new(),
            new_jobs: Vec::new(),
        };

        gallery.rebuild_tag_tree();

        assert!(gallery.hierarchy.contains_key("firestar"));
        assert!(gallery.hierarchy.is_descendant_of("firestar", "thunderclan"));
        assert!(!gallery.hierarchy.roots().iter().any(|root| root == "firestar"));
        let allegiance = gallery
            .tag_tree
            .children
            .iter()
            .find(|node| node.name == "allegiance (clans)")
            .unwrap();
        let thunderclan = allegiance.children.iter().find(|node| node.name == "thunderclan").unwrap();
        assert!(thunderclan.children.iter().any(|node| node.name == "firestar"));

        gallery.set_filename_query("missing-name");
        assert!(gallery.filtered_indices().is_empty());
        gallery.set_filename_query("a.png");
        assert_eq!(gallery.filtered_indices().len(), 1);
        gallery.clear_filters();
        assert!(gallery.filename_query.is_empty());
        assert_eq!(gallery.filtered_indices().len(), 1);
    }

    #[test]
    fn recursive_items_are_removed_when_scope_is_cleared() {
        let (scan_tx, scan_rx) = channel::<ScanMsg>();
        drop(scan_tx);
        let (result_tx, result_rx) = channel::<Msg>();
        drop(result_tx);
        let shared = Arc::new(workers::Shared {
            queue: std::sync::Mutex::new(Vec::new()),
            cv: std::sync::Condvar::new(),
            wanted: std::sync::Mutex::new(HashMap::new()),
        });
        let here = GalleryItem {
            id: "/root/here.webp".into(), title: "here.webp".into(),
            path: "/root/here.webp".into(), is_folder: false, ext: "webp".into(),
            aspect_ratio: 1.0, width: 10.0, height: 10.0,
            border_colors: BorderColors::default(), margins: Margins::default(),
            rating: 0, tags: HashSet::new(), sidecar: Sidecar::default(),
        };
        let deep = GalleryItem {
            id: "/root/deep/deep.webp".into(), title: "deep.webp".into(),
            path: "/root/deep/deep.webp".into(), is_folder: false, ext: "webp".into(),
            aspect_ratio: 1.0, width: 10.0, height: 10.0,
            border_colors: BorderColors::default(), margins: Margins::default(),
            rating: 0, tags: HashSet::from(["fox".into()]), sidecar: Sidecar::default(),
        };
        let mut gallery = GalleryState {
            items: vec![here],
            scanning: false,
            scan_rx,
            scan_progress: Arc::new(AtomicUsize::new(0)),
            scan_started_at: None,
            shared,
            result_rx,
            thumbs: HashMap::new(),
            failed: HashSet::new(),
            inflight: HashMap::new(),
            icon_cache: HashMap::new(),
            layout_cache: None,
            filtered_cache: Vec::new(),
            filter_cache_key: None,
            filter_generation: 0,
            filter_evidence: HashMap::new(),
            filter_confidence: ConfidenceRange::accepted_only(),
            filter_scope: ClaimScope::Curated,
            filter_revision: 0,
            anim_start: HashMap::new(),
            grid_scale: 1.0,
            layout: GalleryLayout::default(),
            scroll_offset: 0.0,
            sort_key: SortKey::default(),
            sort_direction: SortKey::default().default_direction(),
            selected_images: vec!["/root/deep/deep.webp".into()],
            selection_anchor: None,
            keyboard_focus: None,
            request_open_viewer: false,
            viewer_target: None,
            tag_tree: TagNode { name: "Root".into(), children: Vec::new() },
            tag_search_query: String::new(),
            show_preserved_tags: false,
            tags_text: String::new(),
            tags_path: PathBuf::from("/root/tags.txt"),
            hierarchy: Hierarchy::default(),
            active_query: Query::Tag("fox".into()),
            filename_query: String::new(),
            recursive_tag_search: true,
            recursive_root: None,
            recursive_extra_paths: HashSet::new(),
            query_error: None,
            filter_pass: FilterPass::default(),
            wanted_now: HashMap::new(),
            new_jobs: Vec::new(),
        };

        gallery.add_recursive_items(PathBuf::from("/root"), vec![deep]);
        assert_eq!(gallery.items.len(), 2);
        assert_eq!(gallery.filtered_indices().len(), 1);
        assert_eq!(gallery.recursive_root.as_deref(), Some(Path::new("/root")));

        gallery.clear_recursive_items();
        assert_eq!(gallery.items.len(), 1);
        assert!(gallery.filtered_indices().is_empty());
        assert!(gallery.selected_images.is_empty());
    }
}
