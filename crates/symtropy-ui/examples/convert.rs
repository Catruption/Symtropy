//! One-shot WebP converter for the image library.
//!
//! Usage:
//!     cargo run --release -p symtropy-ui --example convert <dir>
//!
//! For each file in <dir>:
//!   - Sniff format via magic bytes (never trust the extension).
//!   - PNG  → Lossless WebP (mathematically identical pixels).
//!   - JPEG → Lossy WebP (tries q80, falls back to q65, keeps original if both grow).
//!   - WebP/AVIF/HEIC/GIF → skip.
//!
//! Writes `<stem>.webp` next to the source, then removes the source on success.
//! Sidecars (`<stem>.halo.json`, `.comments/<name>.xml`) are renamed or left untouched.

use image::{ImageFormat, ImageReader};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::io::Write;
use std::process::{Command, Stdio};
use rayon::prelude::*;

fn spawn_gui() {
    let mut exe_path = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return,
    };
    // current_exe is target/{profile}/examples/convert
    exe_path.pop(); // remove the binary name (convert)
    exe_path.pop(); // remove "examples" directory
    exe_path.push("symtropy-ui");

    if exe_path.exists() {
        match Command::new(&exe_path)
        .stdout(Stdio::null()) // Ignore GUI logs so they don't interleave with conversion logs
        .stderr(Stdio::null())
        .spawn()
        {
            Ok(_) => println!("Launched GUI: {}\n", exe_path.display()),
            Err(e) => eprintln!("Failed to launch GUI: {}\n", e),
        }
    } else {
        eprintln!("GUI binary not found at {}. Run 'cargo build --release -p symtropy-ui' first.\n", exe_path.display());
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = if args.len() > 1 {
        PathBuf::from(&args[1])
    } else {
        PathBuf::from("database/References")
    };

    println!("Converting images in: {}", dir.display());
    println!("WARNING: this replaces originals with WebP. Run on a backup first.\n");

    // Open the GUI in the background
    spawn_gui();

    let entries: Vec<PathBuf> = match fs::read_dir(&dir) {
        Ok(rd) => rd.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect(),
        Err(e) => {
            eprintln!("could not read {}: {}", dir.display(), e);
            return;
        }
    };

    let count = entries.len();
    println!("Scanning {} files...\n", count);

    let converted = std::sync::atomic::AtomicUsize::new(0);
    let kept = std::sync::atomic::AtomicUsize::new(0);
    let failed = std::sync::atomic::AtomicUsize::new(0);
    let bytes_before = std::sync::atomic::AtomicU64::new(0);
    let bytes_after = std::sync::atomic::AtomicU64::new(0);

    // Lock stdout so parallel threads don't jumble the text
    let print_lock = Mutex::new(());

    entries.par_iter().for_each(|path| {
        let outcome = convert_file(path);
        let mut stdout = std::io::stdout();
        let _guard = print_lock.lock().unwrap();

        match outcome {
            Outcome::Converted { before, after, format, lossless, filename } => {
                bytes_before.fetch_add(before, std::sync::atomic::Ordering::Relaxed);
                bytes_after.fetch_add(after, std::sync::atomic::Ordering::Relaxed);
                converted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                let saved = (before - after) as f64 / before as f64 * 100.0;
                let _ = writeln!(stdout,
                                 "[{:<4}] {:>50}  {:.2} MB -> {:.2} MB  ({} saved {:.1}%)",
                                 format, filename,
                                     before as f64 / 1_048_576.0, after as f64 / 1_048_576.0,
                                 if lossless { "lossless" } else { "lossy" }, saved
                );
            }
            Outcome::KeptOriginal { size, format, filename } => {
                kept.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let _ = writeln!(stdout,
                                 "[{:<4}] {:>50}  {:.2} MB -> KEPT (WebP was bigger)",
                                 format, filename, size as f64 / 1_048_576.0
                );
            }
            Outcome::Failed(e) => {
                failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let _ = writeln!(stdout, "[FAIL] {}", e);
            }
            Outcome::Skipped => {}
        }
        let _ = stdout.flush();
    });

    println!("\nDone. {} files seen.", count);
    println!("  converted: {}", converted.load(std::sync::atomic::Ordering::Relaxed));
    println!("  kept:      {}", kept.load(std::sync::atomic::Ordering::Relaxed));
    println!("  failed:    {}", failed.load(std::sync::atomic::Ordering::Relaxed));

    let b_before = bytes_before.load(std::sync::atomic::Ordering::Relaxed);
    let b_after = bytes_after.load(std::sync::atomic::Ordering::Relaxed);
    if b_before > 0 {
        println!(
            "  total:     {:.2} MB -> {:.2} MB ({:.1}% smaller)",
                 b_before as f64 / 1_048_576.0,
                 b_after as f64 / 1_048_576.0,
                 (b_before.saturating_sub(b_after)) as f64 / b_before as f64 * 100.0,
        );
    }
}

enum Outcome {
    Converted { before: u64, after: u64, format: &'static str, lossless: bool, filename: String },
    KeptOriginal { size: u64, format: &'static str, filename: String },
    Skipped,
    Failed(String),
}

fn convert_file(path: &Path) -> Outcome {
    let filename = path.file_name().unwrap_or_default().to_string_lossy().into_owned();

    let reader = match ImageReader::open(path) {
        Ok(r) => r.with_guessed_format(),
        Err(e) => return Outcome::Failed(format!("{}: open: {}", filename, e)),
    };
    let reader = match reader {
        Ok(r) => r,
        Err(e) => return Outcome::Failed(format!("{}: sniff: {}", filename, e)),
    };
    let format = match reader.format() {
        Some(f) => f,
        None => return Outcome::Skipped,
    };

    let (lossless, format_name) = match format {
        ImageFormat::Png => (true, "PNG"),
        ImageFormat::Jpeg => (false, "JPEG"),
        _ => return Outcome::Skipped,
    };

    let original_size = match fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) => return Outcome::Failed(format!("{}: stat: {}", filename, e)),
    };

    let img = match reader.decode() {
        Ok(i) => i,
        Err(e) => return Outcome::Failed(format!("{}: decode: {}", filename, e)),
    };
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());

    let encoder = webp::Encoder::from_rgba(rgba.as_raw(), w, h);

    let webp_data = if lossless {
        encoder.encode_lossless().to_vec()
    } else {
        let mut data = encoder.encode(80.0).to_vec();
        if data.len() >= original_size as usize {
            data = encoder.encode(65.0).to_vec();
        }
        data
    };

    if webp_data.len() >= original_size as usize {
        return Outcome::KeptOriginal { size: original_size, format: format_name, filename };
    }

    let before = original_size;
    let after = webp_data.len() as u64;

    let new_path = path.with_extension("webp");
    let tmp = path.with_extension("webp.tmp");
    if let Err(e) = fs::write(&tmp, &webp_data) {
        let _ = fs::remove_file(&tmp);
        return Outcome::Failed(format!("{}: write tmp: {}", filename, e));
    }
    if let Err(e) = fs::rename(&tmp, &new_path) {
        let _ = fs::remove_file(&tmp);
        return Outcome::Failed(format!("{}: rename: {}", filename, e));
    }

    if path != new_path {
        if let Err(e) = fs::remove_file(path) {
            return Outcome::Failed(format!("{}: remove original: {}", filename, e));
        }
    }

    if let Some(parent) = path.parent() {
        let old_name = path.file_name().unwrap_or_default().to_string_lossy();
        let new_name = new_path.file_name().unwrap_or_default().to_string_lossy();
        let comments_dir = parent.join(".comments");

        let old_xml = comments_dir.join(format!("{}.xml", old_name));
        let new_xml = comments_dir.join(format!("{}.xml", new_name));
        if old_xml.exists() && !new_xml.exists() {
            let _ = fs::rename(&old_xml, &new_xml);
        }

        let old_xml_root = parent.join(format!("{}.xml", old_name));
        let new_xml_root = parent.join(format!("{}.xml", new_name));
        if old_xml_root.exists() && !new_xml_root.exists() {
            let _ = fs::rename(&old_xml_root, &new_xml_root);
        }
    }

    Outcome::Converted { before, after, format: format_name, lossless, filename }
}
