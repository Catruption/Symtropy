use super::sidecar::{LoadSource, Relationship, Sidecar, Source};
use super::*;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Instant, UNIX_EPOCH};

/// Bump whenever the index columns or the cached sidecar fields change.
const INDEX_VERSION: &str = "sidecar-v6";
const EDGE_TOL: u8 = 8;
const MIN_KEEP: u32 = 2;
const ANALYSIS_PX: u32 = 64;
const ANIM_FRAME_PX: u32 = 512;
pub const VIDEO_FRAME_PX: u32 = 512;
pub const VIDEO_EXTS: [&str; 10] = [
    "mp4", "m4v", "webm", "mkv", "avi", "mov", "wmv", "flv", "mpeg", "mpg",
];
pub const VIDEO_DURATION_PROPERTY: &str = "symtropy.video_duration";
const MAX_VIDEO_EXTRACT: usize = 2;
const BATCH_SIZE: usize = 256;

/// Sentinel value in index.tsv meaning "this image has never been margin-analyzed".
/// A real top/bottom/left/right margin will never be 4 billion, so this is safe.
const NO_MARGIN_SENTINEL: u32 = u32::MAX;

pub struct Job {
    pub path: String,
    pub px: [u32; 2],
    pub duration: Option<f64>,
}

pub struct Shared {
    pub queue: Mutex<Vec<Job>>,
    pub cv: Condvar,
    pub wanted: Mutex<HashMap<String, [u32; 2]>>,
}

pub enum WorkerThumbContent {
    Static(egui::ColorImage),
    Animated { frames: Vec<egui::ColorImage>, durations: Vec<f32> },
}

pub enum Msg {
    Done { path: String, px: [u32; 2], content: WorkerThumbContent },
    Skipped { path: String, px: [u32; 2] },
    Failed { path: String },
}

pub enum ScanMsg {
    Batch(Vec<GalleryItem>),
    MarginUpdate { path: String, margins: Margins, colors: BorderColors },
    Done,
}

pub use crate::paths::cache_dir;

pub fn mtime_secs(m: &std::fs::Metadata) -> u64 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0)
}

/// Read image dimensions from the file contents, not just the extension.
///
/// `image::image_dimensions` only looks at the path extension, so files such as
/// `photo.jpg%3Alarge` (actually a WebP) are silently skipped on every scan.
/// Sniffing the header lets those files enter the cache once instead of being
/// read from disk forever.
fn image_dimensions(path: &Path) -> Option<(u32, u32)> {
    image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

pub fn is_video_ext(ext: &str) -> bool {
    let ext = ext.to_ascii_lowercase();
    VIDEO_EXTS.iter().any(|candidate| *candidate == ext)
}

pub fn is_video_path(path: &Path) -> bool {
    path.extension()
        .map(|e| is_video_ext(&e.to_string_lossy().to_lowercase()))
        .unwrap_or(false)
}

pub fn video_duration(sidecar: &Sidecar) -> Option<f64> {
    sidecar
        .properties
        .get(VIDEO_DURATION_PROPERTY)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0)
}

pub fn format_duration(seconds: Option<f64>) -> String {
    let Some(seconds) = seconds.filter(|s| s.is_finite() && *s >= 0.0) else {
        return "VIDEO".to_string();
    };
    let total = seconds.round() as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

fn video_sample_times(duration: Option<f64>) -> [f64; 3] {
    let d = duration.filter(|d| d.is_finite() && *d > 0.0).unwrap_or(10.0);
    [d * 0.10, d * 0.50, d * 0.90]
}

#[derive(Clone, Copy, Debug)]
struct VideoMeta {
    width: u32,
    height: u32,
    duration: f64,
}

/// Read dimensions/duration once via ffprobe. This is only called when the
/// index has no usable cached metadata for the video.
fn probe_video(path: &Path) -> Option<VideoMeta> {
    let output = Command::new("ffprobe")
        .arg("-v").arg("error")
        .arg("-show_entries")
        .arg("stream=codec_type,width,height,duration,side_data_list:format=duration")
        .arg("-of").arg("json")
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let stream = json
        .get("streams")?
        .as_array()?
        .iter()
        .find(|s| s.get("codec_type").and_then(|v| v.as_str()) == Some("video"))?;

    let mut width = stream.get("width")?.as_u64()? as u32;
    let mut height = stream.get("height")?.as_u64()? as u32;
    if stream
        .get("side_data_list")
        .and_then(|v| v.as_array())
        .and_then(|items| {
            items.iter().find_map(|item| {
                item.get("rotation")
                    .and_then(|r| r.as_i64().or_else(|| r.as_str()?.parse().ok()))
            })
        })
        .map(|rotation| rotation.abs() % 180 == 90)
        .unwrap_or(false)
    {
        std::mem::swap(&mut width, &mut height);
    }

    let duration = json
        .pointer("/format/duration")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| {
            stream
                .get("duration")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
        })
        .unwrap_or(0.0);

    Some(VideoMeta { width, height, duration })
}

fn video_cache_dir(path: &Path) -> Option<PathBuf> {
    let meta = std::fs::metadata(path).ok()?;
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    mtime_secs(&meta).hash(&mut hasher);
    meta.len().hash(&mut hasher);
    VIDEO_FRAME_PX.hash(&mut hasher);
    Some(cache_dir().join("video_frames").join(format!("{:016x}", hasher.finish())))
}

fn load_video_frames(dir: &Path) -> Option<Vec<egui::ColorImage>> {
    let mut frames = Vec::with_capacity(3);
    for i in 0..3 {
        let img = image::open(dir.join(format!("{}.png", i))).ok()?.to_rgba8();
        frames.push(egui::ColorImage::from_rgba_unmultiplied(
            [img.width() as usize, img.height() as usize],
            img.as_raw(),
        ));
    }
    Some(frames)
}

static VIDEO_EXTRACT_COUNT: Mutex<usize> = Mutex::new(0);
static VIDEO_EXTRACT_CV: Condvar = Condvar::new();

struct VideoExtractSlot;

impl Drop for VideoExtractSlot {
    fn drop(&mut self) {
        let mut count = VIDEO_EXTRACT_COUNT.lock().unwrap_or_else(|e| e.into_inner());
        *count = count.saturating_sub(1);
        VIDEO_EXTRACT_CV.notify_all();
    }
}

fn acquire_video_extract_slot() -> VideoExtractSlot {
    let mut count = VIDEO_EXTRACT_COUNT.lock().unwrap_or_else(|e| e.into_inner());
    while *count >= MAX_VIDEO_EXTRACT {
        count = VIDEO_EXTRACT_CV.wait(count).unwrap_or_else(|e| e.into_inner());
    }
    *count += 1;
    VideoExtractSlot
}

fn extract_video_frames(path: &Path, dir: &Path, duration: Option<f64>) -> Option<Vec<egui::ColorImage>> {
    let _slot = acquire_video_extract_slot();
    std::fs::create_dir_all(dir).ok()?;
    let scale = format!(
        "scale={0}:{0}:force_original_aspect_ratio=decrease",
        VIDEO_FRAME_PX
    );

    for (i, seconds) in video_sample_times(duration).iter().enumerate() {
        let final_path = dir.join(format!("{}.png", i));
        if final_path.exists() && image::open(&final_path).is_ok() {
            continue;
        }

        let tmp = dir.join(format!("{}.tmp.png", i));
        let status = Command::new("ffmpeg")
            .arg("-hide_banner").arg("-loglevel").arg("error")
            .arg("-ss").arg(format!("{:.3}", seconds))
            .arg("-i").arg(path)
            .arg("-frames:v").arg("1")
            .arg("-an").arg("-sn").arg("-dn")
            .arg("-vf").arg(&scale)
            .arg("-y")
            .arg("-update").arg("1")
            .arg(&tmp)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        if !status.success() {
            let _ = std::fs::remove_file(&tmp);
            return None;
        }
        std::fs::rename(&tmp, &final_path).ok()?;
    }

    load_video_frames(dir)
}

fn make_video_thumb(path: &str, duration: Option<f64>) -> Option<WorkerThumbContent> {
    let path = Path::new(path);
    let dir = video_cache_dir(path)?;
    let frames = load_video_frames(&dir).or_else(|| extract_video_frames(path, &dir, duration))?;
    if frames.len() != 3 {
        return None;
    }
    Some(WorkerThumbContent::Animated { frames, durations: vec![0.5; 3] })
}

/// Replace `path` with `text` only when the contents actually differ.
///
/// The index and sidecars are rewritten via a temp file + rename. Skipping an
/// identical rewrite avoids needless mtime churn and stops the app from
/// treating its own freshly replaced cache file as a change.
fn write_index_if_changed(path: &Path, text: &str) -> std::io::Result<bool> {
    if std::fs::read_to_string(path).map(|old| old == text).unwrap_or(false) {
        return Ok(false);
    }
    let tmp = path.with_extension("tsv.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(true)
}

pub fn make_thumb(
    path: &str,
    px: [u32; 2],
    duration: Option<f64>,
) -> Option<WorkerThumbContent> {
    let ext = Path::new(path)
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    if is_video_ext(&ext) {
        return make_video_thumb(path, duration);
    }

    let bytes = std::fs::read(path).ok()?;

    if ext == "webp" {
        if let Ok(decoder) = webp_animation::Decoder::new(&bytes) {
            let mut tex_frames = Vec::new();
            let mut timestamps_ms = Vec::new();
            for frame in decoder {
                let (w, h) = frame.dimensions();
                let mut img = image::RgbaImage::from_raw(w, h, frame.data().to_vec())?;
                if w.max(h) > ANIM_FRAME_PX {
                    let scale = ANIM_FRAME_PX as f32 / w.max(h) as f32;
                    let nw = ((w as f32 * scale) as u32).max(1);
                    let nh = ((h as f32 * scale) as u32).max(1);
                    img = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Lanczos3);
                }
                tex_frames.push(egui::ColorImage::from_rgba_unmultiplied(
                    [img.width() as usize, img.height() as usize],
                                                                         img.as_raw(),
                ));
                timestamps_ms.push(frame.timestamp() as f32);
            }
            if !tex_frames.is_empty() {
                let mut durations = Vec::with_capacity(tex_frames.len());
                for i in 0..timestamps_ms.len() {
                    let current = timestamps_ms[i];
                    let next = if i + 1 < timestamps_ms.len() { timestamps_ms[i + 1] } else { current + 100.0 };
                    let mut dur_ms = next - current;
                    if i == timestamps_ms.len() - 1 && dur_ms <= 0.0 { dur_ms = 100.0; }
                    durations.push(if dur_ms <= 0.0 { 0.05 } else { dur_ms / 1000.0 });
                }
                return Some(WorkerThumbContent::Animated { frames: tex_frames, durations });
            }
        }
    }

    let img = image::ImageReader::open(path).ok()?
    .with_guessed_format().ok()?
    .decode().ok()?;
    let rgba = img.to_rgba8();
    let resized = if rgba.width() == px[0] && rgba.height() == px[1] {
        rgba
    } else {
        image::imageops::resize(&rgba, px[0], px[1], image::imageops::FilterType::Triangle)
    };
    let size = [resized.width() as usize, resized.height() as usize];
    let ci = egui::ColorImage::from_rgba_unmultiplied(size, resized.as_raw());
    Some(WorkerThumbContent::Static(ci))
}

fn same_colour(a: [u8; 4], b: [u8; 4]) -> bool {
    if a[3] < EDGE_TOL && b[3] < EDGE_TOL { return true; }
    a.iter().zip(b.iter()).all(|(x, y)| x.abs_diff(*y) <= EDGE_TOL)
}

fn scan_side(img: &image::RgbaImage, side: usize) -> (u32, Option<[u8; 4]>) {
    let (w, h) = img.dimensions();
    let (lines, len) = if side < 2 { (h, w) } else { (w, h) };
    let px = |k: u32, i: u32| -> [u8; 4] {
        let (x, y) = match side {
            0 => (i, k),
            1 => (i, h - 1 - k),
            2 => (k, i),
            _ => (w - 1 - k, i),
        };
        img.get_pixel(x, y).0
    };
    let reference = px(0, 0);
    let mut count = 0u32;
    'outer: for k in 0..lines {
        for i in 0..len {
            if !same_colour(px(k, i), reference) { break 'outer; }
        }
        count += 1;
    }
    (count, if count > 0 { Some(reference) } else { None })
}

fn clamp_pair(a: u32, b: u32, total: u32) -> (u32, u32) {
    let max_sum = total.saturating_sub(MIN_KEEP.min(total));
    if a + b <= max_sum { return (a, b); }
    let f = max_sum as f32 / (a + b) as f32;
    ((a as f32 * f) as u32, (b as f32 * f) as u32)
}

fn analyse_margins(base: &image::RgbaImage, orig_w: u32, orig_h: u32) -> (Margins, BorderColors) {
    let (bw, bh) = base.dimensions();
    if bw == 0 || bh == 0 || orig_w == 0 || orig_h == 0 {
        return (Margins::default(), BorderColors::default());
    }
    let sides = [scan_side(base, 0), scan_side(base, 1), scan_side(base, 2), scan_side(base, 3)];
    let m = [
        sides[0].0.saturating_sub(1),
        sides[1].0.saturating_sub(1),
        sides[2].0.saturating_sub(1),
        sides[3].0.saturating_sub(1),
    ];
    let (t, b) = clamp_pair(m[0], m[1], bh);
    let (l, r) = clamp_pair(m[2], m[3], bw);
    let sx = orig_w as f32 / bw as f32;
    let sy = orig_h as f32 / bh as f32;
    (
        Margins {
            top: (t as f32 * sy) as u32,
     bottom: (b as f32 * sy) as u32,
     left: (l as f32 * sx) as u32,
     right: (r as f32 * sx) as u32,
        },
     BorderColors { top: sides[0].1, bottom: sides[1].1, left: sides[2].1, right: sides[3].1 },
    )
}

fn analyse_file(path: &str, w: u32, h: u32) -> (Margins, BorderColors) {
    match image::ImageReader::open(path)
    .ok()
    .and_then(|r| r.with_guessed_format().ok())
    .and_then(|r| r.decode().ok())
    {
        Some(img) => {
            let small = img.thumbnail(ANALYSIS_PX, ANALYSIS_PX);
            analyse_margins(&small.to_rgba8(), w, h)
        }
        None => (Margins::default(), BorderColors::default()),
    }
}

fn is_truly_animated(path: &Path, ext: &str) -> bool {
    if ext == "gif" { return true; }
    if ext != "webp" { return false; }
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let mut head = [0u8; 32];
    if f.read_exact(&mut head).is_err() { return false; }
    &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" && &head[12..16] == b"VP8X" && head[20] & 0x02 != 0
}

pub fn worker(shared: Arc<Shared>, tx: Sender<Msg>, ctx: egui::Context) {
    loop {
        let job = {
            let mut q = shared.queue.lock().unwrap();
            loop {
                if let Some(j) = q.pop() { break j; }
                q = shared.cv.wait(q).unwrap();
            }
        };
        let still_wanted = shared.wanted.lock().unwrap().get(&job.path) == Some(&job.px);
        let msg = if !still_wanted {
            Msg::Skipped { path: job.path, px: job.px }
        } else {
            match make_thumb(&job.path, job.px, job.duration) {
                Some(content) => Msg::Done { path: job.path, px: job.px, content },
                None => Msg::Failed { path: job.path },
            }
        };
        if tx.send(msg).is_err() { return; }
        ctx.request_repaint();
    }
}

// ---------------------------------------------------------------------------
// Index cache (index.tsv) - 17 columns
// ---------------------------------------------------------------------------

/// Everything from a Sidecar that isn't derivable from the image file or the
/// dedicated index columns. Stored as one JSON field so TSV stays simple.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SidecarExtra {
    #[serde(default)] tag_parents: HashMap<String, String>,
    #[serde(default)] artists: Vec<String>,
    #[serde(default)] sources: Vec<Source>,
    #[serde(default)] alt_text: String,
    #[serde(default)] relationships: Vec<Relationship>,
    #[serde(default)] properties: HashMap<String, String>,
}

impl SidecarExtra {
    fn from_sidecar(sc: &Sidecar) -> Self {
        Self {
            tag_parents: sc.tag_parents.clone(),
            artists: sc.artists.clone(),
            sources: sc.sources.clone(),
            alt_text: sc.alt_text.clone(),
            relationships: sc.relationships.clone(),
            properties: sc.properties.clone(),
        }
    }

    fn apply_to(&self, sc: &mut Sidecar) {
        sc.tag_parents = self.tag_parents.clone();
        sc.artists = self.artists.clone();
        sc.sources = self.sources.clone();
        sc.alt_text = self.alt_text.clone();
        sc.relationships = self.relationships.clone();
        sc.properties = self.properties.clone();
    }

    fn encode(sc: &Sidecar) -> String {
        serde_json::to_string(&Self::from_sidecar(sc)).unwrap_or_else(|_| "{}".into())
    }

    fn decode(s: &str) -> Self {
        serde_json::from_str(s).unwrap_or_default()
    }
}

struct IndexEntry {
    mtime: u64,
    len: u64,
    is_animated: bool,
    w: u32,
    h: u32,
    analysis: Option<(Margins, BorderColors)>,
    rating: u8,
    tags: Vec<String>,
    extra: SidecarExtra,
}

type Index = HashMap<String, IndexEntry>;
type MarginJob = (String, u32, u32);

/// The index is global (one file for the whole library). Folder scans, margin
/// merges and UI edits can all be in flight at once, so every read-modify-write
/// cycle must be serialized. Otherwise a later writer can restore a stale
/// snapshot and silently wipe another folder's cached rows.
static INDEX_LOCK: Mutex<()> = Mutex::new(());

/// Count indexed images directly inside each directory.
///
/// Used by the path bar to annotate sibling folders without rescanning disk.
pub fn directory_counts(index_path: &Path) -> HashMap<PathBuf, usize> {
    let mut counts = HashMap::new();
    for path in load_index(index_path).keys() {
        if let Some(parent) = Path::new(path).parent() {
            *counts.entry(parent.to_path_buf()).or_insert(0) += 1;
        }
    }
    counts
}

fn hex(c: Option<[u8; 4]>) -> String {
    match c {
        Some(c) => format!("{:02x}{:02x}{:02x}{:02x}", c[0], c[1], c[2], c[3]),
        None => "-".to_string(),
    }
}

fn unhex(s: &str) -> Option<Option<[u8; 4]>> {
    if s == "-" { return Some(None); }
    if s.len() != 8 { return None; }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some(Some(v.to_be_bytes()))
}

fn version_from_text(text: &str) -> Option<&str> {
    text.lines().next()?.strip_prefix('#').map(str::trim)
}

fn parse_index_text(text: &str) -> Index {
    let mut idx = Index::new();
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 17 { continue; }
        if let (Ok(m), Ok(l), Ok(w), Ok(h), Ok(r)) = (
            f[1].parse(), f[2].parse(), f[4].parse(), f[5].parse(), f[14].parse()
        ) {
            let is_animated = f[3] == "1";
            let c: Vec<u32> = f[6..10].iter().map(|s| s.parse().ok()).collect::<Option<Vec<u32>>>().unwrap_or_default();
            let col: Vec<Option<[u8; 4]>> = f[10..14].iter().map(|s| unhex(s)).collect::<Option<Vec<_>>>().unwrap_or_default();

            // Sentinel check: NO_MARGIN_SENTINEL means "never analyzed", everything else
            // (including all-zero margins) means "analyzed, trust the cached value".
            let analysis = if c.len() == 4 && col.len() == 4 && c[0] != NO_MARGIN_SENTINEL {
                Some((
                    Margins { top: c[0], bottom: c[1], left: c[2], right: c[3] },
                    BorderColors { top: col[0], bottom: col[1], left: col[2], right: col[3] },
                ))
            } else { None };

            let tags = if f[15] == "-" { Vec::new() } else { f[15].split('|').map(|s| s.to_string()).collect() };

            idx.insert(f[0].to_string(), IndexEntry {
                mtime: m, len: l, is_animated, w, h, analysis, rating: r, tags,
                extra: SidecarExtra::decode(f[16]),
            });
        }
    }
    idx
}

/// Load the cache and keep the raw text. The raw rows are needed when a scan
/// finishes so rows for folders that were not scanned can be preserved.
fn load_index_with_text(path: &Path) -> (Index, String) {
    let Ok(text) = std::fs::read_to_string(path) else {
        eprintln!("gallery: no index.tsv found — cold start");
        return (Index::new(), String::new());
    };

    let version = version_from_text(&text);
    if version != Some(INDEX_VERSION) {
        if let Some(v) = version {
            eprintln!("gallery: index version mismatch ('{}' vs '{}') — cache invalidated",
                      v, INDEX_VERSION);
        }
        return (Index::new(), String::new());
    }

    (parse_index_text(&text), text)
}

fn load_index(path: &Path) -> Index {
    load_index_with_text(path).0
}

/// Apply a change to the global index under a single lock.
///
/// `None` means "nothing to write"; `Some` is written only when byte-different.
fn mutate_index<F>(path: &Path, mutate: F) -> std::io::Result<bool>
where
    F: FnOnce(&str) -> Option<String>,
{
    let _guard = INDEX_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let Some(next) = mutate(&text) else { return Ok(false) };
    write_index_if_changed(path, &next)
}

/// Merge freshly scanned rows for `dir` into the current global index.
///
/// Rows for other directories are kept verbatim, so opening a small or empty
/// folder can never wipe the cached metadata for the rest of the library. Rows
/// previously stored for `dir` but not produced by this scan are dropped, so
/// deleted files still leave the cache.
fn merge_index_rows(old_text: &str, dir: &Path, scanned_rows: &str) -> String {
    let mut out = String::with_capacity(old_text.len() + scanned_rows.len());
    out.push('#');
    out.push_str(INDEX_VERSION);
    out.push('\n');

    let mut seen: HashSet<String> = HashSet::new();
    for line in scanned_rows.lines().skip(1) {
        let Some(path) = line.split('\t').next() else { continue };
        if path.is_empty() || !seen.insert(path.to_string()) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }

    for line in old_text.lines().skip(1) {
        let Some(path) = line.split('\t').next() else { continue };
        if path.is_empty() || seen.contains(path) {
            continue;
        }
        // Stale rows for the folder just scanned are intentionally discarded.
        // A sibling with a matching string prefix (e.g. `library2`) is not.
        if Path::new(path).starts_with(dir) {
            continue;
        }
        seen.insert(path.to_string());
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn write_scanned_index_rows(
    path: &Path,
    dir: &Path,
    scanned_rows: &str,
) -> std::io::Result<bool> {
    mutate_index(path, |old_text| {
        Some(merge_index_rows(old_text, dir, scanned_rows))
    })
}

fn index_line_with_sidecar(line: &str, sc: &Sidecar) -> Option<String> {
    let f: Vec<&str> = line.split('\t').collect();
    if f.len() < 16 {
        return None;
    }
    let tags_str = if sc.tags.is_empty() { "-".to_string() } else { sc.tags.join("|") };
    Some(format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        f[0], f[1], f[2], f[3], f[4], f[5], f[6], f[7], f[8], f[9], f[10], f[11], f[12], f[13],
        sc.rating, tags_str, SidecarExtra::encode(sc)
    ))
}

pub fn update_index_entry(index_path: &Path, image_path: &str, sc: &Sidecar) {
    let _ = mutate_index(index_path, |text| {
        let mut out = String::with_capacity(text.len() + 100);
        let mut found = false;
        for line in text.lines() {
            if line.starts_with('#') { out.push_str(line); out.push('\n'); continue; }
            let is_target = line.split('\t').next() == Some(image_path);
            if !is_target {
                out.push_str(line);
                out.push('\n');
                continue;
            }
            if let Some(rewritten) = index_line_with_sidecar(line, sc) {
                out.push_str(&rewritten);
                found = true;
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        found.then_some(out)
    });
}

fn collect_sidecar_files(root: &Path, out: &mut Vec<PathBuf>) {
    let skip_dirs = ["replace", ".git", "target", "node_modules", ".cache"];
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !skip_dirs.iter().any(|skip| name == *skip) {
                    stack.push(path);
                }
            } else if kind.is_file()
                && path.file_name().map_or(false, |n| n.to_string_lossy().ends_with(".halo.json"))
            {
                out.push(path);
            }
        }
    }
}

fn image_path_for_sidecar(sidecar: &Path) -> Option<String> {
    let name = sidecar.file_name()?.to_string_lossy();
    let stem = name.strip_prefix('.')?.strip_suffix(".halo.json")?;
    let parent = sidecar.parent()?;
    let image = std::fs::read_dir(parent).ok()?.flatten().find_map(|entry| {
        let path = entry.path();
        let matches = path.is_file()
            && path.file_stem().map_or(false, |s| s.to_string_lossy() == stem)
            && !path.to_string_lossy().ends_with(".halo.json");
        matches.then(|| path.to_string_lossy().into_owned())
    })?;
    Some(image)
}

/// Apply a tag mutation to every halo sidecar under `root` and update the
/// matching index rows once. Used by taxonomy operations, which must survive a
/// warm launch even for folders that are not currently open.
pub fn rewrite_tag_metadata<F>(root: &Path, index_path: &Path, transform: F) -> usize
where
    F: Fn(&mut Sidecar) -> bool,
{
    let mut sidecars = Vec::new();
    collect_sidecar_files(root, &mut sidecars);
    let mut changed = 0;
    let mut rewrites: HashMap<String, Sidecar> = HashMap::new();
    for path in sidecars {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(mut sc) = serde_json::from_str::<Sidecar>(&text) else { continue };
        if !transform(&mut sc) {
            continue;
        }
        if sc.save_to(&path).is_err() {
            continue;
        }
        changed += 1;
        if let Some(image) = image_path_for_sidecar(&path) {
            rewrites.insert(image, sc);
        }
    }

    if changed > 0 && !rewrites.is_empty() {
        let _ = mutate_index(index_path, |text| {
            let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
            if lines.is_empty() {
                return None;
            }
            for line in &mut lines {
                if line.starts_with('#') {
                    continue;
                }
                let Some(path) = line.split('\t').next() else { continue };
                let Some(sc) = rewrites.get(path) else { continue };
                if let Some(rewritten) = index_line_with_sidecar(line, sc) {
                    *line = rewritten.trim_end_matches('\n').to_string();
                }
            }
            let mut out = lines.join("\n");
            out.push('\n');
            Some(out)
        });
    }
    changed
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn scan_worker(
    dir: PathBuf,
    index_path: PathBuf,
    progress: Arc<AtomicUsize>,
    tx: Sender<ScanMsg>,
    ctx: egui::Context,
) {
    let scan_start = Instant::now();
    eprintln!("gallery: ═══════════════════════════════════════════════════");
    eprintln!("gallery: SCAN STARTING");
    eprintln!("gallery:   directory: {}", dir.display());
    eprintln!("gallery:   index:     {}", index_path.display());

    if !dir.exists() {
        eprintln!("gallery: ERROR directory does not exist: {}", dir.display());
        let _ = tx.send(ScanMsg::Done);
        return;
    }

    eprintln!("gallery: [phase 0] migrating legacy sidecars...");
    let migrate_start = Instant::now();
    let mut sidecar_stems = HashSet::new();
    let mut migrated = 0usize;
    let mut dotfile_count = 0usize;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().to_string();

            if let Some(stem) = name.strip_suffix(".halo.json") {
                let trimmed = stem.trim_start_matches('.');
                sidecar_stems.insert(trimmed.to_string());

                if !stem.starts_with('.') {
                    let dot_name = format!(".{}.halo.json", stem);
                    if std::fs::rename(&path, dir.join(&dot_name)).is_ok() {
                        migrated += 1;
                    }
                } else {
                    dotfile_count += 1;
                }
            } else if let Some(stem) = name.strip_suffix(".xml") {
                sidecar_stems.insert(stem.to_string());
                if let Some(inner) = Path::new(stem).file_stem() {
                    sidecar_stems.insert(inner.to_string_lossy().to_string());
                }
            }
        }
    }
    eprintln!("gallery: [phase 0] {} renamed, {} already dotfiles, {:.2}s",
              migrated, dotfile_count, migrate_start.elapsed().as_secs_f64());
    let sidecar_stems = Arc::new(sidecar_stems);

    let entries: Vec<_> = std::fs::read_dir(&dir)
    .map(|it| it.flatten().collect())
    .unwrap_or_default();
    let total_est = entries.len();

    let old = Arc::new(load_index(&index_path));
    if old.is_empty() {
        eprintln!("gallery: [phase 1] mode: COLD (all {} files will be read from disk)", total_est);
    } else {
        eprintln!("gallery: [phase 1] mode: WARM ({} cached entries from index.tsv)", old.len());
    }
    eprintln!("gallery: [phase 1] scanning {} entries...", total_est);

    let phase1_start = Instant::now();

    let n_threads = std::thread::available_parallelism()
    .map(|v| v.get())
    .unwrap_or(4)
    .clamp(4, 16);
    let chunk_size = (total_est + n_threads - 1) / n_threads;

    let mut all_index_lines = format!("#{INDEX_VERSION}\n");
    let mut all_margin_jobs = Vec::new();
    let mut json_loaded = 0usize;
    let mut cache_hits = 0usize;
    let mut xml_loaded = 0usize;

    let log_mutex = Arc::new(Mutex::new(()));
    let processed_total = Arc::new(AtomicUsize::new(0));
    let hits_counter = Arc::new(AtomicUsize::new(0));

    std::thread::scope(|s| {
        let mut handles = Vec::new();
        for chunk in entries.chunks(chunk_size.max(1)) {
            let (tx, ctx, progress) = (tx.clone(), ctx.clone(), progress.clone());
            let (old, sidecar_stems) = (old.clone(), sidecar_stems.clone());
            let (log_mutex, processed_total, hits_counter) =
            (log_mutex.clone(), processed_total.clone(), hits_counter.clone());
            let phase_start = phase1_start;

            handles.push(s.spawn(move || {
                let mut local_jobs: Vec<MarginJob> = Vec::new();
                let mut local_index = String::new();
                let mut local_json = 0usize;
                let mut local_xml = 0usize;
                let mut local_hits = 0usize;
                let mut batch = Vec::with_capacity(BATCH_SIZE);

                let flush = |batch: &mut Vec<GalleryItem>, tx: &Sender<ScanMsg>, ctx: &egui::Context| {
                    if batch.is_empty() { return; }
                    let _ = tx.send(ScanMsg::Batch(std::mem::take(batch)));
                    ctx.request_repaint();
                };

                for entry in chunk {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let path = entry.path();
                        let title = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                        if title.starts_with('.') { return; }

                        let Ok(meta) = std::fs::metadata(&path) else { return };
                        if !meta.is_file() {
                            if meta.is_dir() {
                                let path_str = path.to_string_lossy().to_string();
                                batch.push(GalleryItem {
                                    id: path_str.clone(), title, path: path_str,
                                           is_folder: true, ext: "".into(), aspect_ratio: 1.0,
                                           width: 1.0, height: 1.0,
                                           border_colors: BorderColors::default(),
                                           margins: Margins::default(),
                                           rating: 0, tags: HashSet::new(), sidecar: Sidecar::default(),
                                });
                                if batch.len() >= BATCH_SIZE { flush(&mut batch, &tx, &ctx); }
                            }
                            return;
                        }

                        let mtime = mtime_secs(&meta);
                        let len = meta.len();
                        let path_str = path.to_string_lossy().to_string();

                        progress.fetch_add(1, Ordering::Relaxed);
                        let n = processed_total.fetch_add(1, Ordering::Relaxed) + 1;
                        if n == 1 || n % 10000 == 0 {
                            let elapsed = phase_start.elapsed().as_secs_f64();
                            let rate = n as f64 / elapsed.max(0.001);
                            let eta = if rate > 0.0 { (total_est as f64 - n as f64) / rate } else { 0.0 };
                            let hits = hits_counter.load(Ordering::Relaxed);
                            let _g = log_mutex.lock().unwrap();
                            eprintln!("gallery: [phase 1] {}/{} ({:.0}/s, ~{:.0}s left, {} cache hits)",
                                      n, total_est, rate, eta, hits);
                        }

                        let ext = path.extension().unwrap_or_default().to_string_lossy().to_lowercase();

                        let (mut sidecar, from_cache) = if let Some(e) = old.get(&path_str) {
                            if e.mtime == mtime && e.len == len {
                                let mut sc = Sidecar::default();
                                sc.tags = e.tags.clone();
                                sc.rating = e.rating;
                                sc.width = Some(e.w);
                                sc.height = Some(e.h);
                                sc.is_animated = Some(e.is_animated);
                                if let Some((m, c)) = &e.analysis {
                                    sc.margins = Some(*m);
                                    sc.border_colors = Some(c.clone());
                                }
                                e.extra.apply_to(&mut sc);
                                local_hits += 1;
                                hits_counter.fetch_add(1, Ordering::Relaxed);
                                (sc, true)
                            } else {
                                (Sidecar::default(), false)
                            }
                        } else {
                            (Sidecar::default(), false)
                        };

                        if !from_cache {
                            if ["json", "xml", "tmp", "db", "tsv", "html", "css", "js"].contains(&ext.as_str()) {
                                return;
                            }

                            let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                            if sidecar_stems.contains(&stem) {
                                if let Some((sc, src)) = Sidecar::load_with_source(Path::new(&path_str)) {
                                    sidecar = sc;
                                    match src {
                                        LoadSource::Json => local_json += 1,
                                        LoadSource::Xml => local_xml += 1,
                                    }
                                }
                            }
                        }

                        // Videos share the normal sidecar/index path so ratings,
                        // tags and metadata survive warm launches. Probe only when
                        // the cache cannot supply dimensions/duration.
                        if is_video_ext(&ext) {
                            sidecar.is_animated = Some(true);
                            if !from_cache
                                && (sidecar.width.is_none()
                                    || sidecar.height.is_none()
                                    || video_duration(&sidecar).is_none())
                            {
                                if let Some(meta) = probe_video(&path) {
                                    if sidecar.width.is_none() { sidecar.width = Some(meta.width); }
                                    if sidecar.height.is_none() { sidecar.height = Some(meta.height); }
                                    if video_duration(&sidecar).is_none() && meta.duration > 0.0 {
                                        sidecar.properties.insert(
                                            VIDEO_DURATION_PROPERTY.to_string(),
                                            meta.duration.to_string(),
                                        );
                                    }
                                }
                            }
                            if sidecar.width.is_none() { sidecar.width = Some(1920); }
                            if sidecar.height.is_none() { sidecar.height = Some(1080); }
                        }

                        let mut sidecar_dirty = false;

                        let dims: Option<(u32, u32)> = if let (Some(w), Some(h)) = (sidecar.width, sidecar.height) {
                            Some((w, h))
                        } else {
                            match old.get(&path_str) {
                                Some(e) if e.mtime == mtime && e.len == len => Some((e.w, e.h)),
                                                                                  _ => image_dimensions(&path),
                            }
                        };
                        let Some((w, h)) = dims else { return };

                        if sidecar.width.is_none() || sidecar.height.is_none() {
                            sidecar.width = Some(w);
                            sidecar.height = Some(h);
                            sidecar_dirty = true;
                        }

                        let is_animated = if let Some(anim) = sidecar.is_animated {
                            anim
                        } else {
                            let anim = is_truly_animated(&path, &ext);
                            sidecar.is_animated = Some(anim);
                            sidecar_dirty = true;
                            anim
                        };

                        let cached_analysis = if let (Some(m), Some(c)) = (sidecar.margins, sidecar.border_colors.clone()) {
                            Some((m, c))
                        } else {
                            old.get(&path_str)
                            .filter(|e| e.mtime == mtime && e.len == len)
                            .and_then(|e| e.analysis.clone())
                        };

                        let has_cached_analysis = cached_analysis.is_some();

                        let (margins, border_colors) = match cached_analysis {
                            Some(a) => a,
                                                                                  None => {
                                                                                      if !is_animated {
                                                                                          local_jobs.push((path_str.clone(), w, h));
                                                                                      }
                                                                                      (Margins::default(), BorderColors::default())
                                                                                  }
                        };

                        if sidecar_dirty {
                            let _ = sidecar.save(Path::new(&path_str));
                        }

                        let tags: HashSet<String> = sidecar.tags.iter().cloned().collect();
                        let rating = sidecar.rating;

                        if !path_str.contains('\t') && !path_str.contains('\n') {
                            let tags_str = if sidecar.tags.is_empty() { "-".to_string() } else { sidecar.tags.join("|") };
                            let extra_str = SidecarExtra::encode(&sidecar);
                            if has_cached_analysis {
                                local_index.push_str(&format!(
                                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                                    path_str, mtime, len,
                                    if is_animated { 1 } else { 0 },
                                        w, h,
                                        margins.top, margins.bottom, margins.left, margins.right,
                                        hex(border_colors.top), hex(border_colors.bottom), hex(border_colors.left), hex(border_colors.right),
                                                              rating, tags_str, extra_str
                                ));
                            } else {
                                // Sentinel: "never analyzed" — will be filled in by Phase 3
                                local_index.push_str(&format!(
                                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t-\t-\t-\t-\t{}\t{}\t{}\n",
                                    path_str, mtime, len,
                                    if is_animated { 1 } else { 0 },
                                        w, h,
                                        NO_MARGIN_SENTINEL, NO_MARGIN_SENTINEL, NO_MARGIN_SENTINEL, NO_MARGIN_SENTINEL,
                                        rating, tags_str, extra_str
                                ));
                            }
                        }

                        let aspect = if h > 0 { w as f32 / h as f32 } else { 1.0 };
                        batch.push(GalleryItem {
                            id: path_str.clone(), title, path: path_str,
                                   is_folder: false, ext, aspect_ratio: aspect,
                                   width: w as f32, height: h as f32, border_colors, margins,
                                   rating, tags, sidecar,
                        });

                        if batch.len() >= BATCH_SIZE { flush(&mut batch, &tx, &ctx); }
                    }));
                }

                flush(&mut batch, &tx, &ctx);
                (local_jobs, local_index, local_json, local_xml, local_hits)
            }));
        }

        for h in handles {
            if let Ok((jobs, idx, j_count, x_count, h_count)) = h.join() {
                all_margin_jobs.extend(jobs);
                all_index_lines.push_str(&idx);
                json_loaded += j_count;
                xml_loaded += x_count;
                cache_hits += h_count;
            }
        }
    });

    if let Some(parent) = index_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = write_scanned_index_rows(&index_path, &dir, &all_index_lines) {
        eprintln!("gallery: [phase 1] ERROR writing index: {}", e);
    }

    let p1_elapsed = phase1_start.elapsed().as_secs_f64();
    eprintln!("gallery: [phase 1] ✅ complete in {:.2}s", p1_elapsed);
    eprintln!("gallery:   {} cache hits, {} disk reads, {} xml imports",
              cache_hits, json_loaded, xml_loaded);
    eprintln!("gallery:   {} items queued for margin analysis", all_margin_jobs.len());

    let _ = tx.send(ScanMsg::Done);
    ctx.request_repaint();
    eprintln!("gallery: [phase 1] → items sent to UI, gallery is now interactive");

    if all_margin_jobs.is_empty() {
        eprintln!("gallery: [phase 2] ⊘ SKIPPED (all margins already cached)");
        eprintln!("gallery: ═══════════════════════════════════════════════════");
        eprintln!("gallery: SCAN COMPLETE in {:.2}s (phases 0+1 only)", scan_start.elapsed().as_secs_f64());
        eprintln!("gallery: ═══════════════════════════════════════════════════");
        eprintln!("startup: gallery scan complete @ {}ms", crate::startup_ms());
        return;
    }

    spawn_margin_workers(all_margin_jobs, tx, ctx, index_path, scan_start, p1_elapsed);
}

fn spawn_margin_workers(
    jobs: Vec<MarginJob>,
    tx: Sender<ScanMsg>,
    ctx: egui::Context,
    index_path: PathBuf,
    scan_start: Instant,
    p1_elapsed: f64,
) {
    let total = jobs.len();
    let queue = Arc::new(Mutex::new(jobs));
    let done = Arc::new(AtomicUsize::new(0));
    let results: Arc<Mutex<HashMap<String, (Margins, BorderColors)>>> =
    Arc::new(Mutex::new(HashMap::new()));

    // I/O-bound workload: fewer workers = less seek thrashing on USB/spinning disks.
    let n = std::thread::available_parallelism()
    .map(|v| v.get())
    .unwrap_or(4)
    .clamp(3, 6);
    eprintln!("gallery: [phase 2] using {} workers (I/O-bound workload)", n);

    let p2_start = Instant::now();
    eprintln!("gallery: [phase 2] starting: {} margin jobs", total);
    eprintln!("gallery: [phase 2] gallery is interactive; you can browse while this runs");

    let mut handles = Vec::new();
    for _ in 0..n {
        let (queue, tx, ctx, done, results) =
        (queue.clone(), tx.clone(), ctx.clone(), done.clone(), results.clone());
        handles.push(std::thread::spawn(move || loop {
            let job = { queue.lock().unwrap().pop() };
            let Some((path, w, h)) = job else { break };

            let (margins, colors) = analyse_file(&path, w, h);

            let _ = tx.send(ScanMsg::MarginUpdate {
                path: path.clone(),
                            margins,
                            colors: colors.clone()
            });

            results.lock().unwrap().insert(path, (margins, colors));

            ctx.request_repaint();

            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 10000 == 0 || n == total {
                let elapsed = p2_start.elapsed().as_secs_f64();
                let rate = n as f64 / elapsed.max(0.001);
                let eta = if rate > 0.0 { (total as f64 - n as f64) / rate } else { 0.0 };
                eprintln!("gallery: [phase 2] {}/{} ({:.0}/s, ~{:.0}s left)", n, total, rate, eta);
            }
        }));
    }

    let ctx3 = ctx.clone();
    std::thread::spawn(move || {
        for h in handles { let _ = h.join(); }

        let p2_elapsed = p2_start.elapsed().as_secs_f64();
        eprintln!("gallery: [phase 2] ✅ complete in {:.2}s ({} jobs)", p2_elapsed, total);

        let p3_start = Instant::now();
        eprintln!("gallery: [phase 3] merging margin data back into index.tsv...");

        let results = results.lock().unwrap();
        let mut merged = 0usize;
        match mutate_index(&index_path, |text| {
            if text.is_empty() {
                eprintln!("gallery: [phase 3] ERROR — could not re-read {}", index_path.display());
                return None;
            }
            let mut out = String::with_capacity(text.len() + 1024 * 1024);
            for line in text.lines() {
                if line.starts_with('#') { out.push_str(line); out.push('\n'); continue; }
                let f: Vec<&str> = line.split('\t').collect();
                if f.len() < 17 {
                    out.push_str(line);
                    out.push('\n');
                    continue;
                }
                let path = f[0];
                if let Some((m, c)) = results.get(path) {
                    out.push_str(&format!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                        path, f[1], f[2], f[3], f[4], f[5],
                        m.top, m.bottom, m.left, m.right,
                        hex(c.top), hex(c.bottom), hex(c.left), hex(c.right),
                                          f[14], f[15], f[16]
                    ));
                    merged += 1;
                } else {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            Some(out)
        }) {
            Ok(true) => eprintln!("gallery: [phase 3] ✅ complete in {:.2}s ({} entries updated)",
                                  p3_start.elapsed().as_secs_f64(), merged),
            Ok(false) => eprintln!("gallery: [phase 3] ✅ no changes in {:.2}s ({} entries already current)",
                                   p3_start.elapsed().as_secs_f64(), merged),
            Err(e) => eprintln!("gallery: [phase 3] ERROR — write failed: {}", e),
        }

        eprintln!("gallery: ═══════════════════════════════════════════════════");
        eprintln!("gallery: SCAN COMPLETE in {:.2}s total", scan_start.elapsed().as_secs_f64());
        eprintln!("startup: gallery scan complete @ {}ms", crate::startup_ms());
        eprintln!("gallery:   phase 1 (scan):    {:.2}s", p1_elapsed);
        eprintln!("gallery:   phase 2 (margins): {:.2}s", p2_elapsed);
        eprintln!("gallery:   phase 3 (merge):   {:.2}s", p3_start.elapsed().as_secs_f64());
        eprintln!("gallery: ═══════════════════════════════════════════════════");
        ctx3.request_repaint();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_common_video_extensions() {
        assert!(is_video_ext("mp4"));
        assert!(is_video_ext("WEBM"));
        assert!(is_video_path(Path::new("/tmp/clip.MKV")));
        assert!(!is_video_ext("webp"));
    }

    #[test]
    fn video_samples_are_spread_across_the_clip() {
        let times = video_sample_times(Some(20.0));
        assert_eq!(times, [2.0, 10.0, 18.0]);
        assert_eq!(video_sample_times(None), [1.0, 5.0, 9.0]);
    }

    #[test]
    fn formats_video_duration() {
        assert_eq!(format_duration(Some(75.0)), "1:15");
        assert_eq!(format_duration(None), "VIDEO");
    }

    #[test]
    fn index_round_trips_full_sidecar() {
        let mut sc = Sidecar::default();
        sc.tags = vec!["fox".into(), "animal".into()];
        sc.rating = 3;
        sc.artists = vec!["somebody".into()];
        sc.alt_text = "a fox with a tab\tand newline\n".into();
        sc.tag_parents.insert("fox".into(), "animal".into());
        sc.properties.insert("key".into(), "value".into());
        sc.sources.push(Source { url: "https://example.com".into(), ..Source::default() });
        sc.relationships.push(Relationship {
            subject: "fox".into(), predicate: "holding".into(), object: "plush".into(),
        });

        let line = format!(
            "#{}\n/tmp/a.webp\t1\t2\t0\t10\t20\t0\t0\t0\t0\t-\t-\t-\t-\t{}\t{}\t{}\n",
            INDEX_VERSION,
            sc.rating,
            sc.tags.join("|"),
            SidecarExtra::encode(&sc),
        );
        let dir = std::env::temp_dir().join("symtropy-index-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.tsv");
        std::fs::write(&path, line).unwrap();

        let idx = load_index(&path);
        let e = idx.get("/tmp/a.webp").expect("entry parsed");
        assert_eq!(e.rating, 3);
        assert_eq!(e.tags, sc.tags);

        let mut restored = Sidecar::default();
        e.extra.apply_to(&mut restored);
        assert_eq!(restored.artists, sc.artists);
        assert_eq!(restored.alt_text, sc.alt_text);
        assert_eq!(restored.tag_parents, sc.tag_parents);
        assert_eq!(restored.properties, sc.properties);
        assert_eq!(restored.sources.len(), 1);
        assert_eq!(restored.relationships.len(), 1);
    }

    #[test]
    fn rejects_stale_index_version() {
        let dir = std::env::temp_dir().join("symtropy-index-test-old");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.tsv");
        std::fs::write(&path, "#margins-v5\n/tmp/a.webp\t1\t2\t0\t10\t20\t0\t0\t0\t0\t-\t-\t-\t-\t0\t-\t{}\n").unwrap();
        assert!(load_index(&path).is_empty());
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("symtropy-cache-{}-{}", name, nanos));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn index_row(path: &str, mtime: u64) -> String {
        format!(
            "{}\t{}\t2\t0\t10\t20\t0\t0\t0\t0\t-\t-\t-\t-\t0\t-\t{{}}\n",
            path, mtime
        )
    }

    #[test]
    fn scanning_an_empty_folder_keeps_other_library_cache_rows() {
        let keep = "/library/keep.webp";
        let sibling = "/library2/sibling.webp";
        let stale = "/empty/gone.webp";
        let old = format!(
            "#{}\n{}{}{}",
            INDEX_VERSION,
            index_row(keep, 1),
            index_row(sibling, 2),
            index_row(stale, 3),
        );
        let scanned = format!("#{}\n", INDEX_VERSION);

        let merged = merge_index_rows(&old, Path::new("/empty"), &scanned);
        let idx = parse_index_text(&merged);

        assert_eq!(idx.len(), 2);
        assert!(idx.contains_key(keep));
        assert!(idx.contains_key(sibling));
        assert!(!idx.contains_key(stale));
    }

    #[test]
    fn scanning_a_folder_refreshes_only_its_own_rows() {
        let old = format!(
            "#{}\n{}{}{}",
            INDEX_VERSION,
            index_row("/dir/a.webp", 1),
            index_row("/dir/b.webp", 2),
            index_row("/other/c.webp", 3),
        );
        let scanned = format!(
            "#{}\n{}",
            INDEX_VERSION,
            index_row("/dir/a.webp", 9),
        );

        let merged = merge_index_rows(&old, Path::new("/dir"), &scanned);
        let idx = parse_index_text(&merged);

        assert_eq!(idx.len(), 2);
        assert_eq!(idx["/dir/a.webp"].mtime, 9);
        assert!(!idx.contains_key("/dir/b.webp"));
        assert!(idx.contains_key("/other/c.webp"));

        // The refreshed row exists exactly once.
        assert_eq!(
            merged
                .lines()
                .filter(|line| line.starts_with("/dir/a.webp\t"))
                .count(),
            1
        );
    }

    #[test]
    fn identical_index_rewrite_is_skipped() {
        let dir = scratch("rewrite");
        let path = dir.join("index.tsv");
        let text = format!("#{}\n/tmp/a.webp\t1\t2\t0\t10\t20\t0\t0\t0\t0\t-\t-\t-\t-\t0\t-\t{{}}\n", INDEX_VERSION);

        assert!(write_index_if_changed(&path, &text).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert!(!write_index_if_changed(&path, &text).unwrap());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn metadata_update_preserves_cached_margins() {
        let dir = scratch("metadata");
        let path = dir.join("index.tsv");
        let original = format!(
            "#{}\n/tmp/a.webp\t1\t2\t0\t10\t20\t3\t4\t5\t6\t-\t-\t-\t-\t0\told\t{{}}\n",
            INDEX_VERSION
        );
        std::fs::write(&path, original).unwrap();

        let mut sc = Sidecar::default();
        sc.tags = vec!["new".into()];
        sc.rating = 4;
        sc.alt_text = "updated".into();
        update_index_entry(&path, "/tmp/a.webp", &sc);

        let idx = load_index(&path);
        let e = idx.get("/tmp/a.webp").expect("entry preserved");
        let (m, _) = e.analysis.as_ref().expect("analysis preserved");
        assert_eq!((m.top, m.bottom, m.left, m.right), (3, 4, 5, 6));
        assert_eq!(e.tags, sc.tags);
        assert_eq!(e.rating, 4);
        assert_eq!(e.extra.alt_text, "updated");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn global_tag_rewrite_updates_sidecars_and_index() {
        let root = scratch("rewrite-tags");
        let image = root.join("a.webp");
        std::fs::write(&image, b"x").unwrap();
        let sidecar_path = root.join(".a.halo.json");
        let mut sc = Sidecar::default();
        sc.tags = vec!["old".into()];
        sc.tag_parents.insert("old".into(), "parent".into());
        sc.save_to(&sidecar_path).unwrap();

        let index = root.join("index.tsv");
        std::fs::write(
            &index,
            format!(
                "#{}\n{}\t1\t2\t0\t10\t20\t0\t0\t0\t0\t-\t-\t-\t-\t0\told\t{{}}\n",
                INDEX_VERSION,
                image.to_string_lossy()
            ),
        )
        .unwrap();

        let changed = rewrite_tag_metadata(&root, &index, |sc| {
            if !sc.tags.iter().any(|tag| tag == "old") {
                return false;
            }
            sc.tags = vec!["new".into()];
            sc.tag_parents.clear();
            true
        });
        assert_eq!(changed, 1);
        assert_eq!(Sidecar::load(&image).unwrap().tags, vec!["new"]);
        assert_eq!(load_index(&index)[image.to_string_lossy().as_ref()].tags, vec!["new"]);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dimensions_are_sniffed_from_file_contents() {
        let dir = scratch("dims");
        let png = dir.join("photo.png");
        let path = dir.join("photo.jpg%3Alarge");
        image::RgbaImage::from_pixel(7, 5, image::Rgba([1, 2, 3, 255]))
            .save(&png)
            .unwrap();
        std::fs::rename(&png, &path).unwrap();

        assert_eq!(image_dimensions(&path), Some((7, 5)));

        std::fs::remove_dir_all(dir).unwrap();
    }
}
