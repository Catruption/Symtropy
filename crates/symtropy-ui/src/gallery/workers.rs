use super::sidecar::{LoadSource, Sidecar};
use super::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Instant, UNIX_EPOCH};

const ANALYSIS_VERSION: &str = "margins-v5";
const EDGE_TOL: u8 = 8;
const MIN_KEEP: u32 = 2;
const ANALYSIS_PX: u32 = 64;
const ANIM_FRAME_PX: u32 = 512;
const VIDEO_EXTS: [&str; 3] = ["mp4", "webm", "mkv"];
const BATCH_SIZE: usize = 256;

/// Sentinel value in index.tsv meaning "this image has never been margin-analyzed".
/// A real top/bottom/left/right margin will never be 4 billion, so this is safe.
const NO_MARGIN_SENTINEL: u32 = u32::MAX;

pub struct Job {
    pub path: String,
    pub px: [u32; 2],
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
    Done { path: String, px: [u32; 2], content: WorkerThumbContent, small: Arc<egui::ColorImage> },
    Skipped { path: String, px: [u32; 2] },
    Failed { path: String },
}

pub enum ScanMsg {
    Batch(Vec<GalleryItem>),
    MarginUpdate { path: String, margins: Margins, colors: BorderColors },
    Done,
}

pub fn cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
    .map(PathBuf::from)
    .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    .unwrap_or_else(std::env::temp_dir);
    base.join("symtropy")
}

pub fn mtime_secs(m: &std::fs::Metadata) -> u64 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn make_thumb(path: &str, px: [u32; 2]) -> Option<(WorkerThumbContent, egui::ColorImage)> {
    let bytes = std::fs::read(path).ok()?;

    if path.to_lowercase().ends_with(".webp") {
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
                let small_ci = create_small_preview(&tex_frames[0]);
                return Some((WorkerThumbContent::Animated { frames: tex_frames, durations }, small_ci));
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
    let small_ci = create_small_preview(&ci);
    Some((WorkerThumbContent::Static(ci), small_ci))
}

fn create_small_preview(img: &egui::ColorImage) -> egui::ColorImage {
    let k = SMALL_MAX / img.width().max(img.height()) as f32;
    let sw = ((img.width() as f32 * k).round() as u32).max(1);
    let sh = ((img.height() as f32 * k).round() as u32).max(1);
    let raw = image::RgbaImage::from_raw(img.width() as u32, img.height() as u32, img.as_raw().to_vec()).unwrap();
    let small = image::imageops::thumbnail(&raw, sw, sh);
    egui::ColorImage::from_rgba_unmultiplied(
        [small.width() as usize, small.height() as usize],
                                             small.as_raw(),
    )
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
            match make_thumb(&job.path, job.px) {
                Some((content, small)) => Msg::Done { path: job.path, px: job.px, content, small: Arc::new(small) },
                None => Msg::Failed { path: job.path },
            }
        };
        if tx.send(msg).is_err() { return; }
        ctx.request_repaint();
    }
}

// ---------------------------------------------------------------------------
// Index cache (index.tsv) - 16 columns
// ---------------------------------------------------------------------------

struct IndexEntry {
    mtime: u64,
    len: u64,
    is_animated: bool,
    w: u32,
    h: u32,
    analysis: Option<(Margins, BorderColors)>,
    rating: u8,
    tags: Vec<String>,
}

type Index = HashMap<String, IndexEntry>;
type MarginJob = (String, u32, u32);

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

fn load_index(path: &Path) -> Index {
    let mut idx = Index::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        eprintln!("gallery: no index.tsv found — cold start");
        return idx;
    };

    let mut version_ok = false;
    if let Some(first_line) = text.lines().next() {
        if let Some(v) = first_line.strip_prefix('#') {
            version_ok = v.trim() == ANALYSIS_VERSION;
            if !version_ok {
                eprintln!("gallery: index version mismatch ('{}' vs '{}') — cache invalidated",
                          v.trim(), ANALYSIS_VERSION);
            }
        }
    }
    if !version_ok {
        return idx;
    }

    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 16 { continue; }
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
                mtime: m, len: l, is_animated, w, h, analysis, rating: r, tags
            });
        }
    }
    idx
}

pub fn update_index_entry(index_path: &Path, image_path: &str, tags: &[String], rating: u8) {
    let Ok(text) = std::fs::read_to_string(index_path) else { return };
    let mut out = String::with_capacity(text.len() + 100);
    let mut found = false;
    for line in text.lines() {
        if line.starts_with('#') { out.push_str(line); out.push('\n'); continue; }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 16 || f[0] != image_path {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let tags_str = if tags.is_empty() { "-".to_string() } else { tags.join("|") };
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            f[0], f[1], f[2], f[3], f[4], f[5], f[6], f[7], f[8], f[9], f[10], f[11], f[12], f[13], rating, tags_str
        ));
        found = true;
    }
    if !found { return; }
    let tmp = index_path.with_extension("tsv.tmp");
    if std::fs::write(&tmp, out).is_ok() {
        let _ = std::fs::rename(&tmp, index_path);
    }
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

    let mut all_index_lines = String::from(format!("#{ANALYSIS_VERSION}\n"));
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
                                           is_animated: false, width: 1.0, height: 1.0,
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
                        if n == 1 || n % 500 == 0 {
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

                            if VIDEO_EXTS.contains(&ext.as_str()) {
                                batch.push(GalleryItem {
                                    id: path_str.clone(), title, path: path_str,
                                           is_folder: false, ext, aspect_ratio: 16.0 / 9.0,
                                           is_animated: false, width: 1920.0, height: 1080.0,
                                           border_colors: BorderColors::default(),
                                           margins: Margins::default(),
                                           rating: 0, tags: HashSet::new(), sidecar: Sidecar::default(),
                                });
                                if batch.len() >= BATCH_SIZE { flush(&mut batch, &tx, &ctx); }
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

                        let mut sidecar_dirty = false;

                        let dims: Option<(u32, u32)> = if let (Some(w), Some(h)) = (sidecar.width, sidecar.height) {
                            Some((w, h))
                        } else {
                            match old.get(&path_str) {
                                Some(e) if e.mtime == mtime && e.len == len => Some((e.w, e.h)),
                                                                                  _ => image::image_dimensions(&path).ok(),
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
                            if has_cached_analysis {
                                local_index.push_str(&format!(
                                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                                    path_str, mtime, len,
                                    if is_animated { 1 } else { 0 },
                                        w, h,
                                        margins.top, margins.bottom, margins.left, margins.right,
                                        hex(border_colors.top), hex(border_colors.bottom), hex(border_colors.left), hex(border_colors.right),
                                                              rating, tags_str
                                ));
                            } else {
                                // Sentinel: "never analyzed" — will be filled in by Phase 3
                                local_index.push_str(&format!(
                                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t-\t-\t-\t-\t{}\t{}\n",
                                    path_str, mtime, len,
                                    if is_animated { 1 } else { 0 },
                                        w, h,
                                        NO_MARGIN_SENTINEL, NO_MARGIN_SENTINEL, NO_MARGIN_SENTINEL, NO_MARGIN_SENTINEL,
                                        rating, tags_str
                                ));
                            }
                        }

                        let aspect = if h > 0 { w as f32 / h as f32 } else { 1.0 };
                        batch.push(GalleryItem {
                            id: path_str.clone(), title, path: path_str,
                                   is_folder: false, ext, aspect_ratio: aspect, is_animated,
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

    let tmp = index_path.with_extension("tsv.tmp");
    if std::fs::write(&tmp, &all_index_lines).is_ok() {
        let _ = std::fs::rename(&tmp, &index_path);
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
            if n % 2000 == 0 || n == total {
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

        let Ok(text) = std::fs::read_to_string(&index_path) else {
            eprintln!("gallery: [phase 3] ERROR — could not re-read {}", index_path.display());
            return;
        };

        let p3_start = Instant::now();
        eprintln!("gallery: [phase 3] merging margin data back into index.tsv...");

        let results = results.lock().unwrap();
        let mut out = String::with_capacity(text.len() + 1024 * 1024);
        let mut merged = 0usize;
        for line in text.lines() {
            if line.starts_with('#') { out.push_str(line); out.push('\n'); continue; }
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 16 {
                out.push_str(line);
                out.push('\n');
                continue;
            }
            let path = f[0];
            if let Some((m, c)) = results.get(path) {
                out.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                    path, f[1], f[2], f[3], f[4], f[5],
                    m.top, m.bottom, m.left, m.right,
                    hex(c.top), hex(c.bottom), hex(c.left), hex(c.right),
                                      f[14], f[15]
                ));
                merged += 1;
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }

        let tmp = index_path.with_extension("tsv.tmp");
        match std::fs::write(&tmp, out) {
            Ok(()) => {
                if std::fs::rename(&tmp, &index_path).is_ok() {
                    eprintln!("gallery: [phase 3] ✅ complete in {:.2}s ({} entries updated)",
                              p3_start.elapsed().as_secs_f64(), merged);
                } else {
                    eprintln!("gallery: [phase 3] ERROR — rename failed");
                }
            }
            Err(e) => eprintln!("gallery: [phase 3] ERROR — write failed: {}", e),
        }

        eprintln!("gallery: ═══════════════════════════════════════════════════");
        eprintln!("gallery: SCAN COMPLETE in {:.2}s total", scan_start.elapsed().as_secs_f64());
        eprintln!("gallery:   phase 1 (scan):    {:.2}s", p1_elapsed);
        eprintln!("gallery:   phase 2 (margins): {:.2}s", p2_elapsed);
        eprintln!("gallery:   phase 3 (merge):   {:.2}s", p3_start.elapsed().as_secs_f64());
        eprintln!("gallery: ═══════════════════════════════════════════════════");
        ctx3.request_repaint();
    });
}
