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
pub use query::Query;
pub use sidecar::Sidecar;
pub use workers::{Msg, ScanMsg};

pub const MAX_UPLOADS_PER_FRAME: usize = 24;
pub const MAX_TEXTURES: usize = 400;
pub const MARGIN: f32 = 0.0;
pub const TARGET_ROW_HEIGHT: f32 = 200.0;
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
    pub key: (i32, i32, i32, usize, String),
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
    pub(crate) anim_start: HashMap<String, f64>,

    pub grid_scale: f32,
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
    pub(crate) tags_text: String,
    pub(crate) tags_path: PathBuf,

    pub hierarchy: Hierarchy,
    pub active_query: Query,
    pub query_error: Option<String>,

    pub(crate) filter_pass: FilterPass,
    pub(crate) wanted_now: HashMap<String, [u32; 2]>,
    pub(crate) new_jobs: Vec<workers::Job>,
}

impl GalleryState {
    /// Paths currently visible under the active query, in display order. Used by the
    /// slideshow so it follows the same filters as the gallery.
    pub fn visible_paths(&self) -> Vec<String> {
        self.items
            .iter()
            .filter(|item| !item.is_folder)
            .filter(|item| self.active_query.matches(&item.tags, item.rating, &self.hierarchy))
            .map(|item| item.path.clone())
            .collect()
    }

    /// Drop every active filter without touching selection or view settings.
    pub fn clear_filters(&mut self) {
        self.active_query = Query::AlwaysTrue;
        self.query_error = None;
    }

    /// Fold sidecar-only tags and their parent links into the taxonomy. The
    /// tags.txt entries come first so their hierarchy wins on duplicates.
    pub fn rebuild_tag_tree(&mut self) {
        let known: HashSet<String> = self
            .tags_text
            .lines()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect();
        let mut extra: HashSet<String> = HashSet::new();

        for item in &self.items {
            for (child, parent) in &item.sidecar.tag_parents {
                if child.is_empty() || parent.is_empty() {
                    continue;
                }
                if !known.contains(parent) {
                    extra.insert(parent.clone());
                }
                if !known.contains(child) {
                    extra.insert(format!("  {}", child));
                }
            }
            for tag in &item.sidecar.tags {
                if !tag.is_empty() && !known.contains(tag) {
                    extra.insert(tag.clone());
                }
            }
        }

        let combined = if extra.is_empty() {
            self.tags_text.clone()
        } else {
            let mut lines: Vec<String> = extra.into_iter().collect();
            lines.sort();
            format!("{}\n{}\n", self.tags_text, lines.join("\n"))
        };
        self.hierarchy = Hierarchy::parse(&combined);
        self.tag_tree = tree_from_hierarchy(&self.hierarchy);
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
        Ok(())
    }

    pub fn scan_dir(ctx: &egui::Context, dir: &Path) -> Self {
        let tags_path = dir
        .parent()
        .map(|p| p.join("tags.txt"))
        .unwrap_or_else(|| std::path::PathBuf::from("tags.txt"));
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
            anim_start: HashMap::new(),
            grid_scale: 1.0,
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
            tags_text,
            tags_path,
            hierarchy,
            active_query: Query::AlwaysTrue,
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
