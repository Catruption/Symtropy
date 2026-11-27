//! Generate a data-driven icon index at build time.
//!
//! Icons are discovered by filename, not hardcoded in Rust. For every file in
//! `assets/icons/` with an image format the app can decode, the generated table
//! maps its stem to its bytes. Theme overrides under
//! `assets/themes/<theme>/icons/` are indexed by theme name too.
//!
//! Source SVGs and KRA files are intentionally skipped: they are authoring
//! sources, not payloads the runtime image loader can decode. `assets/icons/`
//! includes a `regenerate.sh` script that re-bakes SVGs into 512px lossless
//! WebP payloads with the same stem.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const DECODABLE: &[&str] = &["webp", "png", "jpg", "jpeg", "gif"];

fn extension_priority(ext: &str) -> usize {
    DECODABLE
        .iter()
        .position(|candidate| *candidate == ext)
        .unwrap_or(DECODABLE.len())
}

fn collect(dir: &Path, theme: &str, out: &mut BTreeSet<(String, String, usize, String)>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if extension_priority(&ext) >= DECODABLE.len() {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        let relative = path
            .strip_prefix(env::var("CARGO_MANIFEST_DIR").unwrap_or_default())
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        out.insert((theme.to_owned(), name, extension_priority(&ext), relative));
    }
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let assets = manifest_dir.join("assets");
    println!("cargo:rerun-if-changed={}", assets.display());

    let mut icons = BTreeSet::new();
    collect(&assets.join("icons"), "", &mut icons);

    if let Ok(themes) = fs::read_dir(assets.join("themes")) {
        for theme in themes.flatten() {
            if !theme.path().is_dir() {
                continue;
            }
            let theme_name = theme
                .file_name()
                .to_string_lossy()
                .to_ascii_lowercase();
            collect(&theme.path().join("icons"), &theme_name, &mut icons);
        }
    }

    let mut out = String::from("pub static ICON_ASSETS: &[(&str, &str, &[u8])] = &[\n");
    for (theme, name, _, relative) in icons {
        out.push_str(&format!(
            "    ({theme:?}, {name:?}, include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/{relative}\"))),\n"
        ));
    }
    out.push_str("];\n");

    let out_path = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("generated_icons.rs");
    fs::write(out_path, out).expect("cannot write generated icon index");
}
