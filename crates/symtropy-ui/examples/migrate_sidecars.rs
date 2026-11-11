//! One-shot DigiKam XML → Halo JSON sidecar migrator.
//!
//! Usage:
//!     cargo run --release -p symtropy-ui --example migrate_sidecars <dir>
//!
//! Scans the directory for `.xml` sidecars in `.comments/` or next to the image.
//! Parses them, builds the tag hierarchy, and writes `<stem>.halo.json` next to the image.
//! DOES NOT delete the original XMLs. OVERWRITES existing .halo.json files.

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Source {
    #[serde(default)] pub url: String,
    #[serde(default)] pub site: String,
    #[serde(default)] pub captured_at: String,
    #[serde(default)] pub title: String,
    #[serde(default)] pub description: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Relationship {
    pub subject: String,
    pub predicate: String,
    pub object: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Sidecar {
    #[serde(default = "default_version")] pub version: u32,
    #[serde(default)] pub tags: Vec<String>,
    #[serde(default)] pub tag_parents: HashMap<String, String>, // <-- ADDED
    #[serde(default)] pub rating: u8,
    #[serde(default)] pub artists: Vec<String>,
    #[serde(default)] pub sources: Vec<Source>,
    #[serde(default)] pub alt_text: String,
    #[serde(default)] pub relationships: Vec<Relationship>,
    #[serde(default)] pub properties: HashMap<String, String>,
}

fn default_version() -> u32 { 1 }

impl Default for Sidecar {
    fn default() -> Self {
        Self {
            version: 1,
            tags: Vec::new(),
            tag_parents: HashMap::new(), // <-- ADDED
            rating: 0,
            artists: Vec::new(),
            sources: Vec::new(),
            alt_text: String::new(),
            relationships: Vec::new(),
            properties: HashMap::new(),
        }
    }
}

fn parse_digikam_xml(xml: &str) -> Result<Sidecar, String> {
    use quick_xml::events::{BytesStart, Event};
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    let mut sc = Sidecar::default();
    sc.properties.insert("imported_from".into(), "digikam-xml".into());

    let mut current_tag: Option<String> = None;
    let mut buf = Vec::new();

    fn attr(e: &BytesStart, key: &[u8]) -> Option<String> {
        for a in e.attributes().flatten() {
            if a.key.as_ref() == key {
                if let Ok(v) = std::str::from_utf8(&a.value) {
                    return Some(v.to_string());
                }
            }
        }
        None
    }

    let push_category = |v: String, sc: &mut Sidecar| {
        // DigiKam sometimes uses '>' and sometimes '/' as a delimiter.
        let delimiter = if v.contains('>') { '>' } else { '/' };
        let parts: Vec<String> = v.split(delimiter)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

        let mut prev = String::new();
        for part in parts {
            if !sc.tags.contains(&part) {
                sc.tags.push(part.clone());
            }
            if !prev.is_empty() {
                sc.tag_parents.insert(part.clone(), prev.clone());
            }
            prev = part;
        }
    };

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let name = std::str::from_utf8(qname.as_ref()).unwrap_or("").to_string();
                match name.as_str() {
                    "category" => {
                        if let Some(v) = attr(&e, b"value") {
                            push_category(v, &mut sc);
                        }
                    }
                    "rating" => {
                        if let Some(v) = attr(&e, b"value") {
                            if let Ok(n) = v.parse::<u8>() { sc.rating = n.min(5); }
                        }
                    }
                    "time" => {
                        if let Some(v) = attr(&e, b"value") {
                            let normalized = v.replacen(":", "-", 2).replacen(" ", "T", 1);
                            sc.properties.insert("creation_date".into(), normalized);
                        }
                    }
                    "caption" | "note" | "place" => {
                        current_tag = Some(name);
                    }
                    _ => {}
                }
            }
            Ok(Event::End(_)) => { current_tag = None; }
            Ok(Event::Empty(e)) => {
                let qname = e.name();
                let name = std::str::from_utf8(qname.as_ref()).unwrap_or("").to_string();
                match name.as_str() {
                    "category" => {
                        if let Some(v) = attr(&e, b"value") {
                            push_category(v, &mut sc);
                        }
                    }
                    "rating" => {
                        if let Some(v) = attr(&e, b"value") {
                            if let Ok(n) = v.parse::<u8>() { sc.rating = n.min(5); }
                        }
                    }
                    "time" => {
                        if let Some(v) = attr(&e, b"value") {
                            let normalized = v.replacen(":", "-", 2).replacen(" ", "T", 1);
                            sc.properties.insert("creation_date".into(), normalized);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(e)) => {
                if let Some(tag) = &current_tag {
                    if let Ok(text) = e.unescape() {
                        let t = text.trim();
                        if !t.is_empty() {
                            match tag.as_str() {
                                "caption" => sc.alt_text = t.to_string(),
                                "note" => { sc.properties.insert("notes".into(), t.to_string()); }
                                "place" => { sc.properties.insert("place".into(), t.to_string()); }
                                _ => {}
                            }
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML parse error: {}", e)),
            _ => {}
        }
        buf.clear();
    }

    sc.tags.sort();
    sc.tags.dedup();
    Ok(sc)
}

fn stem_file(image_path: &Path, suffix: &str) -> Option<PathBuf> {
    let stem = image_path.file_stem()?;
    let parent = image_path.parent()?;
    Some(parent.join(format!("{}.{}", stem.to_string_lossy(), suffix)))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = if args.len() > 1 {
        PathBuf::from(&args[1])
    } else {
        PathBuf::from("database/References")
    };

    println!("Migrating sidecars in: {}", dir.display());
    println!("WARNING: This will overwrite existing .halo.json files to add tag hierarchy!");

    let entries: Vec<PathBuf> = match fs::read_dir(&dir) {
        Ok(rd) => rd.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect(),
        Err(e) => {
            eprintln!("could not read {}: {}", dir.display(), e);
            return;
        }
    };

    let migrated = AtomicUsize::new(0);
    let skipped = AtomicUsize::new(0);

    entries.par_iter().for_each(|path| {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else { return };
        let ext = ext.to_lowercase();
        if !["jpg", "jpeg", "png", "webp", "gif"].contains(&ext.as_str()) { return; }

        let Some(json_path) = stem_file(path, "halo.json") else { return };

        // Removed the "if json_path.exists() { return; }" check so it overwrites.

        let parent = path.parent().unwrap_or(Path::new("."));
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();

        let candidates = [
            parent.join(".comments").join(format!("{}.xml", name)),
                                parent.join(".comments").join(format!("{}.xml", stem)),
                                parent.join(format!("{}.xml", name)),
                                parent.join(format!("{}.xml", stem)),
        ];

        for xml_path in candidates {
            if let Ok(xml) = fs::read_to_string(&xml_path) {
                match parse_digikam_xml(&xml) {
                    Ok(sc) => {
                        if !sc.tags.is_empty() || sc.rating > 0 || !sc.alt_text.is_empty() {
                            let text = serde_json::to_string_pretty(&sc).unwrap();
                            let tmp = json_path.with_extension("halo.json.tmp");
                            if fs::write(&tmp, text).is_ok() {
                                let _ = fs::rename(&tmp, &json_path);
                                println!("[MIGRATED] {} -> {}", xml_path.display(), json_path.display());
                                migrated.fetch_add(1, Ordering::Relaxed);
                            }
                        } else {
                            skipped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(e) => eprintln!("[ERROR] {}: {}", xml_path.display(), e),
                }
                break; // Stop after first successful match
            }
        }
    });

    println!(
        "\nDone. Migrated: {}, Empty/Skipped: {}",
        migrated.load(Ordering::Relaxed),
             skipped.load(Ordering::Relaxed)
    );
}
