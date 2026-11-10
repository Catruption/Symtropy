use egui::TextureHandle;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::Instant;

use crate::filter::FilterPass;

pub mod hierarchy;
pub mod layout;
pub mod query;
pub mod ui;
pub mod workers;

pub use hierarchy::Hierarchy;
pub use query::Query;
pub use workers::Msg;

pub const BASE_MAX: u32 = 1024; // Or 2048 if you have a 4K monitor and want it perfectly sharp zoomed in
pub const MAX_UPLOADS_PER_FRAME: usize = 12;
pub const MAX_TEXTURES: usize = 400;
pub const MARGIN: f32 = 0.0;
pub const TARGET_ROW_HEIGHT: f32 = 200.0;
pub const SMALL_MAX: f32 = 48.0;
pub const SCAN_TIMEOUT_SECS: u64 = 60;

#[derive(Clone, Debug, Default)]
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

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Margins {
    pub top: u32,
    pub bottom: u32,
    pub left: u32,
    pub right: u32,
}

impl Margins {
    pub fn crop_w(&self) -> u32 { self.left + self.right }
    pub fn crop_h(&self) -> u32 { self.top + self.bottom }
}

#[derive(Clone, Debug)]
pub struct GalleryItem {
    pub id: String,
    pub title: String,
    pub path: String,
    pub is_folder: bool,
    pub ext: String,
    pub aspect_ratio: f32,
    pub is_animated: bool,
    pub width: f32,
    pub height: f32,
    pub border_colors: BorderColors,
    pub margins: Margins,
    pub rating: u8,
    pub tags: HashSet<String>,
}

/// A renderable tag tree node for the sidebar. Built from `Hierarchy`.
#[derive(Clone, Debug)]
pub struct TagNode {
    pub name: String,
    pub children: Vec<TagNode>,
}

/// Convert the parsed `Hierarchy` into a `TagNode` tree that the sidebar can render.
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
    pub small: Arc<egui::ColorImage>,
}

pub(crate) struct LayoutCache {
    pub key: (i32, i32, i32, usize, String),
    pub cells: Vec<layout::Cell>,
    pub total_h: i32,
}

pub fn seed_of(path: &str) -> u64 {
    let mut h = DefaultHasher::new();
    path.hash(&mut h);
    h.finish()
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
    pub(crate) scan_rx: Receiver<Vec<GalleryItem>>,
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
    pub selected_images: Vec<String>,
    pub request_open_viewer: bool,
    pub viewer_target: Option<String>,
    pub tag_tree: TagNode,
    pub active_tags: HashSet<String>,
    pub tag_search_query: String,

    pub hierarchy: Hierarchy,
    pub query_input: String,
    pub active_query: Query,
    pub query_error: Option<String>,

    pub(crate) filter_pass: FilterPass,
    pub(crate) wanted_now: HashMap<String, [u32; 2]>,
    pub(crate) new_jobs: Vec<workers::Job>,
}

impl GalleryState {
    pub fn scan_dir(ctx: &egui::Context, dir: &Path) -> Self {
        let tags_path = dir
        .parent()
        .map(|p| p.join("tags.txt"))
        .unwrap_or_else(|| std::path::PathBuf::from("tags.txt"));
        let hierarchy = match std::fs::read_to_string(&tags_path) {
            Ok(text) => {
                let h = Hierarchy::parse(&text);
                eprintln!(
                    "gallery: loaded {} tags from {}",
                    h.len(),
                          tags_path.display()
                );
                h
            }
            Err(_) => {
                eprintln!(
                    "gallery: no tag file at {} — tag hierarchy disabled",
                    tags_path.display()
                );
                Hierarchy::default()
            }
        };
        // Build the renderable tree from the parsed hierarchy, not a mock.
        let tag_tree = tree_from_hierarchy(&hierarchy);

        let cache = workers::cache_dir();
        let progress = Arc::new(AtomicUsize::new(0));

        let (scan_tx, scan_rx) = channel();
        {
            let (dir, index_path, progress, ctx) =
            (dir.to_path_buf(), cache.join("index.tsv"), progress.clone(), ctx.clone());
            std::thread::spawn(move || {
                let items = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    workers::scan_worker(dir, index_path, progress)
                }))
                .unwrap_or_else(|_| {
                    eprintln!("gallery: scan worker panicked; returning empty list");
                    Vec::new()
                });
                let _ = scan_tx.send(items);
                ctx.request_repaint();
            });
        }

        let shared = Arc::new(workers::Shared {
            queue: std::sync::Mutex::new(Vec::new()),
                              cv: std::sync::Condvar::new(),
                              wanted: std::sync::Mutex::new(HashMap::new()),
                              cache_dir: cache,
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
            selected_images: Vec::new(),
            request_open_viewer: false,
            viewer_target: None,
            tag_tree,
            active_tags: HashSet::new(),
            tag_search_query: String::new(),
            hierarchy,
            query_input: String::new(),
            active_query: Query::AlwaysTrue,
            query_error: None,
            filter_pass: FilterPass::default(),
            wanted_now: HashMap::new(),
            new_jobs: Vec::new(),
        }
    }

    pub(crate) fn poll(&mut self, ctx: &egui::Context) {
        if self.scanning {
            if let Some(start) = self.scan_started_at {
                if start.elapsed().as_secs() >= SCAN_TIMEOUT_SECS {
                    eprintln!("gallery: scan timed out after {}s, forcing stop", SCAN_TIMEOUT_SECS);
                    self.scanning = false;
                }
            }
        }
        if let Ok(items) = self.scan_rx.try_recv() {
            self.items = items;
            self.scanning = false;
        }

        let mut uploads = 0;
        while uploads < MAX_UPLOADS_PER_FRAME {
            match self.result_rx.try_recv() {
                Ok(Msg::Done { path, px, content, small }) => {
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
                    self.thumbs.insert(path, Thumb { px, content: thumb_content, small });
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

    pub(crate) fn want(&mut self, path: &str, px: [u32; 2]) {
        self.wanted_now.insert(path.to_string(), px);
        let have = self.thumbs.get(path).map(|t| t.px == px).unwrap_or(false);
        if !have && self.inflight.get(path) != Some(&px) {
            self.inflight.insert(path.to_string(), px);
            self.new_jobs.push(workers::Job { path: path.to_string(), px });
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
