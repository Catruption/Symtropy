use super::*;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::UNIX_EPOCH;

const ANALYSIS_VERSION: &str = "margins-v2";
const EDGE_TOL: u8 = 8;
const MIN_KEEP: u32 = 8;
const VIDEO_EXTS: [&str; 3] = ["mp4", "webm", "mkv"];

pub struct Job {
    pub path: String,
    pub px: [u32; 2],
}

pub struct Shared {
    pub queue: Mutex<Vec<Job>>,
    pub cv: Condvar,
    pub wanted: Mutex<HashMap<String, [u32; 2]>>,
    pub cache_dir: PathBuf,
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

pub fn cache_file(dir: &Path, path: &str) -> Option<PathBuf> {
    let meta = std::fs::metadata(path).ok()?;
    let mut h = DefaultHasher::new();
    (path, mtime_secs(&meta), meta.len(), BASE_MAX).hash(&mut h);
    Some(dir.join("thumbs").join(format!("{:016x}.png", h.finish())))
}

pub fn load_base(dir: &Path, path: &str) -> Option<image::RgbaImage> {
    let cf = cache_file(dir, path)?;
    if let Ok(img) = image::open(&cf) {
        return Some(img.to_rgba8());
    }

    let img = match image::ImageReader::open(path).ok()?.with_guessed_format().ok()?.decode() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("gallery: failed to decode {path}: {e}");
            return None;
        }
    };

    let img = if img.width().max(img.height()) > BASE_MAX {
        img.resize(BASE_MAX, BASE_MAX, image::imageops::FilterType::Lanczos3)
    } else { img };
    let rgba = img.to_rgba8();

    if let Some(parent) = cf.parent() { let _ = std::fs::create_dir_all(parent); }
    let tmp = cf.with_extension("tmp.png");
    if rgba.save(&tmp).is_ok() { let _ = std::fs::rename(&tmp, &cf); }
    Some(rgba)
}

pub fn make_thumb(dir: &Path, path: &str, px: [u32; 2]) -> Option<(WorkerThumbContent, egui::ColorImage)> {
    let bytes = std::fs::read(path).ok()?;

    if path.to_lowercase().ends_with(".webp") {
        if let Ok(decoder) = webp_animation::Decoder::new(&bytes) {
            let mut tex_frames = Vec::new();
            let mut timestamps_ms = Vec::new();
            for frame in decoder {
                let (w, h) = frame.dimensions();
                let data = frame.data();
                let mut img = image::RgbaImage::from_raw(w, h, data.to_vec())?;
                if w.max(h) > BASE_MAX {
                    img = image::imageops::resize(&img, BASE_MAX, BASE_MAX, image::imageops::FilterType::Lanczos3);
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

    let base = load_base(dir, path)?;
    let img = if base.width() == px[0] && base.height() == px[1] {
        base
    } else {
        image::imageops::resize(&base, px[0], px[1], image::imageops::FilterType::Lanczos3)
    };
    let size = [img.width() as usize, img.height() as usize];
    let ci = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
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
    if a[3] < EDGE_TOL && b[3] < EDGE_TOL {
        return true;
    }
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
            if !same_colour(px(k, i), reference) {
                break 'outer;
            }
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
    let margins = Margins {
        top: (t as f32 * sy) as u32,
        bottom: (b as f32 * sy) as u32,
        left: (l as f32 * sx) as u32,
        right: (r as f32 * sx) as u32,
    };
    let colors = BorderColors { top: sides[0].1, bottom: sides[1].1, left: sides[2].1, right: sides[3].1 };
    (margins, colors)
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

fn analyse_file(cache: &Path, path: &str, w: u32, h: u32) -> (Margins, BorderColors) {
    match load_base(cache, path) {
        Some(base) => analyse_margins(&base, w, h),
        None => (Margins::default(), BorderColors::default()),
    }
}

fn report_margins(items: &[GalleryItem]) {
    let imgs: Vec<&GalleryItem> =
        items.iter().filter(|i| !i.is_folder && !VIDEO_EXTS.contains(&i.ext.as_str())).collect();
    let n = imgs.len();
    if n == 0 { return; }
    let bucket = |f: f32| -> usize {
        if f <= 0.0 { 0 } else if f < 0.05 { 1 } else if f < 0.20 { 2 } else if f < 0.50 { 3 } else { 4 }
    };
    let mut wb = [0usize; 5];
    let mut hb = [0usize; 5];
    let mut rigid = 0usize;
    let mut padable = 0usize;
    let mut both_axes = 0usize;
    let mut removable: Vec<(f32, &GalleryItem)> = Vec::with_capacity(n);
    for it in &imgs {
        let m = it.margins;
        let fw = m.crop_w() as f32 / it.width.max(1.0);
        let fh = m.crop_h() as f32 / it.height.max(1.0);
        wb[bucket(fw)] += 1;
        hb[bucket(fh)] += 1;
        let can_pad = it.border_colors.is_flexible();
        if can_pad { padable += 1; }
        if m.crop_w() == 0 && m.crop_h() == 0 && !can_pad { rigid += 1; }
        if m.crop_w() > 0 && m.crop_h() > 0 { both_axes += 1; }
        let kept = (1.0 - fw).max(0.0) * (1.0 - fh).max(0.0);
        removable.push((1.0 - kept, *it));
    }
    removable.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mean = removable.iter().map(|r| r.0).sum::<f32>() / n as f32;
    let median = removable[n / 2].0;
    let pct = |c: usize| 100.0 * c as f32 / n as f32;
    eprintln!("gallery: ---- margin report ({n} images) ----");
    eprintln!("gallery:   rigid (no crop, no plain edge to pad): {rigid} ({:.1}%)", pct(rigid));
    eprintln!("gallery:   at least one plain edge (can pad):     {padable} ({:.1}%)", pct(padable));
    eprintln!("gallery:   can crop on both axes:                 {both_axes} ({:.1}%)", pct(both_axes));
    eprintln!("gallery:   width crop   0 / <5% / 5-20% / 20-50% / >=50%:  {} / {} / {} / {} / {}", wb[0], wb[1], wb[2], wb[3], wb[4]);
    eprintln!("gallery:   height crop  0 / <5% / 5-20% / 20-50% / >=50%:  {} / {} / {} / {} / {}", hb[0], hb[1], hb[2], hb[3], hb[4]);
    eprintln!("gallery:   removable area per image: mean {:.1}%, median {:.1}%", mean * 100.0, median * 100.0);
    eprintln!("gallery:   most cropped (open these and check the crop only removes plain colour):");
    for (frac, it) in removable.iter().rev().take(5) {
        let m = it.margins;
        eprintln!(
            "gallery:     {:.0}%  {}x{}  crop t{} b{} l{} r{}  {}",
            frac * 100.0, it.width as u32, it.height as u32, m.top, m.bottom, m.left, m.right, it.path
        );
    }
    eprintln!("gallery: -----------------------------------");
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
            match make_thumb(&shared.cache_dir, &job.path, job.px) {
                Some((content, small)) => Msg::Done { path: job.path, px: job.px, content, small: Arc::new(small) },
                None => Msg::Failed { path: job.path },
            }
        };
        if tx.send(msg).is_err() { return; }
        ctx.request_repaint();
    }
}

struct IndexEntry {
    mtime: u64,
    len: u64,
    w: u32,
    h: u32,
    analysis: Option<(Margins, BorderColors)>,
}

type Index = HashMap<String, IndexEntry>;

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

fn parse_analysis(f: &[&str]) -> Option<(Margins, BorderColors)> {
    let c: Vec<u32> = f[5..9].iter().map(|s| s.parse().ok()).collect::<Option<Vec<u32>>>()?;
    let col: Vec<Option<[u8; 4]>> = f[9..13].iter().map(|s| unhex(s)).collect::<Option<Vec<_>>>()?;
    Some((
        Margins { top: c[0], bottom: c[1], left: c[2], right: c[3] },
        BorderColors { top: col[0], bottom: col[1], left: col[2], right: col[3] },
    ))
}

fn load_index(path: &Path) -> Index {
    let mut idx = Index::new();
    let Ok(text) = std::fs::read_to_string(path) else { return idx };
    let mut version_ok = false;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix('#') {
            version_ok = v.trim() == ANALYSIS_VERSION;
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 5 && f.len() != 13 { continue; }
        if let (Ok(m), Ok(l), Ok(w), Ok(h)) = (f[1].parse(), f[2].parse(), f[3].parse(), f[4].parse()) {
            let analysis = if f.len() == 13 && version_ok { parse_analysis(&f) } else { None };
            idx.insert(f[0].to_string(), IndexEntry { mtime: m, len: l, w, h, analysis });
        }
    }
    idx
}

pub fn scan_worker(dir: PathBuf, index_path: PathBuf, progress: Arc<AtomicUsize>) -> Vec<GalleryItem> {
    eprintln!("gallery: scan_worker starting on {}", dir.display());
    if !dir.exists() {
        eprintln!("gallery: ERROR directory does not exist: {}", dir.display());
        return Vec::new();
    }

    let cache = index_path.parent().map(Path::to_path_buf).unwrap_or_default();
    let old = load_index(&index_path);
    let mut new_index = format!("#{ANALYSIS_VERSION}\n");
    let mut items = Vec::new();

    match std::fs::read_dir(&dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let path = entry.path();
                    let title = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                    if title.starts_with('.') { return; }
                    let path_str = path.to_string_lossy().to_string();
                    progress.fetch_add(1, Ordering::Relaxed);

                    if path.is_dir() {
                        items.push(GalleryItem {
                            id: path_str.clone(), title, path: path_str,
                                   is_folder: true, ext: "".into(), aspect_ratio: 1.0,
                                   is_animated: false, width: 1.0, height: 1.0,
                                   border_colors: BorderColors::default(),
                                   margins: Margins::default(),
                                   rating: 0,
                                   tags: HashSet::new(),
                        });
                        return;
                    }

                    if !path.is_file() { return; }

                    let ext = path.extension().unwrap_or_default().to_string_lossy().to_lowercase();

                    if VIDEO_EXTS.contains(&ext.as_str()) {
                        items.push(GalleryItem {
                            id: path_str.clone(), title, path: path_str,
                                   is_folder: false, ext, aspect_ratio: 16.0 / 9.0,
                                   is_animated: false, width: 1920.0, height: 1080.0,
                                   border_colors: BorderColors::default(),
                                   margins: Margins::default(),
                                   rating: 0,
                                   tags: HashSet::new(),
                        });
                        return;
                    }

                    let (mtime, len) = std::fs::metadata(&path).map(|m| (mtime_secs(&m), m.len())).unwrap_or((0, 0));
                    let dims: Option<(u32, u32)> = match old.get(&path_str) {
                        Some(e) if e.mtime == mtime && e.len == len => Some((e.w, e.h)),
                        _ => image::image_dimensions(&path).ok(),
                    };
                    let Some((w, h)) = dims else { return };

                    let is_animated = is_truly_animated(&path, &ext);
                    let (margins, border_colors) = match old.get(&path_str).and_then(|e| e.analysis.clone()) {
                        Some(a) => a,
                        None if is_animated => (Margins::default(), BorderColors::default()),
                        None => analyse_file(&cache, &path_str, w, h),
                    };

                    if !path_str.contains('\t') && !path_str.contains('\n') {
                        new_index.push_str(&format!(
                            "{path_str}\t{mtime}\t{len}\t{w}\t{h}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                            margins.top, margins.bottom, margins.left, margins.right,
                            hex(border_colors.top), hex(border_colors.bottom),
                            hex(border_colors.left), hex(border_colors.right),
                        ));
                    }

                    let aspect = if h > 0 { w as f32 / h as f32 } else { 1.0 };
                    items.push(GalleryItem {
                        id: path_str.clone(), title, path: path_str,
                               is_folder: false, ext, aspect_ratio: aspect, is_animated,
                               width: w as f32, height: h as f32, border_colors, margins,
                               rating: 0,
                               tags: HashSet::new(),
                    });
                }));
            }
        }
        Err(e) => {
            eprintln!("gallery: ERROR read_dir({}): {}", dir.display(), e);
        }
    }

    eprintln!("gallery: scan_worker finished with {} items", items.len());

    items.sort_by(|a, b| {
        b.is_folder.cmp(&a.is_folder)
            .then_with(|| (b.width * b.height).partial_cmp(&(a.width * a.height)).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
    });

    report_margins(&items);

    if let Some(parent) = index_path.parent() { let _ = std::fs::create_dir_all(parent); }
    let _ = std::fs::write(&index_path, new_index);
    items
}
