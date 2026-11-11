//! Per-image sidecar metadata.
//!
//! Canonical: `<parent>/.<stem>.halo.json` next to the image. Survives format conversion.
//! Import fallback: DigiKam XML, in `.comments/` or next to the image. Read-only.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use super::{Margins, BorderColors};

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
    #[serde(default)] pub tag_parents: HashMap<String, String>,
    #[serde(default)] pub rating: u8,
    #[serde(default)] pub artists: Vec<String>,
    #[serde(default)] pub sources: Vec<Source>,
    #[serde(default)] pub alt_text: String,
    #[serde(default)] pub relationships: Vec<Relationship>,
    #[serde(default)] pub properties: HashMap<String, String>,

    // --- CACHE FIELDS ---
    #[serde(default)] pub width: Option<u32>,
    #[serde(default)] pub height: Option<u32>,
    #[serde(default)] pub is_animated: Option<bool>,
    #[serde(default)] pub margins: Option<Margins>,
    #[serde(default)] pub border_colors: Option<BorderColors>,
}

fn default_version() -> u32 { 1 }

impl Default for Sidecar {
    fn default() -> Self {
        Self {
            version: 1,
            tags: Vec::new(),
            tag_parents: HashMap::new(),
            rating: 0,
            artists: Vec::new(),
            sources: Vec::new(),
            alt_text: String::new(),
            relationships: Vec::new(),
            properties: HashMap::new(),
            width: None,
            height: None,
            is_animated: None,
            margins: None,
            border_colors: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum LoadSource { Json, Xml }

impl Sidecar {
    /// Canonical sidecar path: `<parent>/.<stem>.halo.json`.
    pub fn path_for(image_path: &Path) -> PathBuf {
        let stem = image_path.file_stem().unwrap_or_default();
        let parent = image_path.parent().unwrap_or(Path::new(""));
        parent.join(format!(".{}.halo.json", stem.to_string_lossy()))
    }

    /// Legacy path: `<parent>/<stem>.halo.json` (no leading dot)
    fn legacy_json_path(image_path: &Path) -> PathBuf {
        stem_file(image_path, "halo.json").unwrap_or_else(|| ext_file(image_path, "halo.json"))
    }

    fn digikam_xml_candidates(image_path: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Some(parent) = image_path.parent() else { return out };
        let Some(name) = image_path.file_name() else { return out };
        let Some(stem) = image_path.file_stem() else { return out };
        let comments = parent.join(".comments");
        let name_xml = format!("{}.xml", name.to_string_lossy());
        let stem_xml = format!("{}.xml", stem.to_string_lossy());
        out.push(comments.join(&name_xml));
        out.push(comments.join(&stem_xml));
        out.push(parent.join(&name_xml));
        out.push(parent.join(&stem_xml));
        out
    }

    /// Migrate legacy `<stem>.halo.json` → `.<stem>.halo.json`.
    /// This is a single rename syscall — no read, no write, no parse.
    pub fn migrate_if_needed(image_path: &Path) {
        let stem_json = Self::path_for(image_path);
        let legacy = Self::legacy_json_path(image_path);

        if legacy != stem_json && legacy.exists() && !stem_json.exists() {
            if std::fs::rename(&legacy, &stem_json).is_ok() {
                // Uncomment if you want per-file migration logs:
                // eprintln!("sidecar: migrated {} -> {}", legacy.display(), stem_json.display());
            }
        }
    }

    pub fn load_with_source(image_path: &Path) -> Option<(Self, LoadSource)> {
        let stem_json = Self::path_for(image_path);

        // Fast path: canonical dotfile exists (after migration or first write)
        if let Ok(text) = std::fs::read_to_string(&stem_json) {
            match serde_json::from_str::<Sidecar>(&text) {
                Ok(sc) => return Some((sc, LoadSource::Json)),
                Err(e) => eprintln!("sidecar: failed to parse {}: {}", stem_json.display(), e),
            }
        }

        // Fallback: try legacy path (in case migration hasn't run yet)
        let legacy = Self::legacy_json_path(image_path);
        if legacy != stem_json {
            if let Ok(text) = std::fs::read_to_string(&legacy) {
                match serde_json::from_str::<Sidecar>(&text) {
                    Ok(sc) => {
                        // Opportunistic migration
                        let _ = std::fs::rename(&legacy, &stem_json);
                        return Some((sc, LoadSource::Json));
                    }
                    Err(e) => eprintln!("sidecar: failed to parse {}: {}", legacy.display(), e),
                }
            }
        }

        // Fallback: DigiKam XML
        for xml_path in Self::digikam_xml_candidates(image_path) {
            if let Ok(xml) = std::fs::read_to_string(&xml_path) {
                match parse_digikam_xml(&xml) {
                    Ok(sc) => {
                        if !sc.tags.is_empty() || sc.rating > 0 {
                            eprintln!(
                                "sidecar: {} tags, rating {} from {}",
                                sc.tags.len(), sc.rating, xml_path.display()
                            );
                        }
                        return Some((sc, LoadSource::Xml));
                    }
                    Err(e) => eprintln!("sidecar: failed to parse {}: {}", xml_path.display(), e),
                }
            }
        }

        None
    }

    pub fn load(image_path: &Path) -> Option<Self> {
        Self::load_with_source(image_path).map(|(s, _)| s)
    }

    pub fn save(&self, image_path: &Path) -> std::io::Result<()> {
        let json_path = Self::path_for(image_path);
        let text = serde_json::to_string_pretty(self)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let tmp = json_path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &json_path)?;

        // Clean up legacy file if it somehow still exists
        let legacy = Self::legacy_json_path(image_path);
        if legacy != json_path && legacy.exists() {
            let _ = std::fs::remove_file(legacy);
        }

        Ok(())
    }
}

fn stem_file(image_path: &Path, suffix: &str) -> Option<PathBuf> {
    let stem = image_path.file_stem()?;
    let parent = image_path.parent()?;
    Some(parent.join(format!("{}.{}", stem.to_string_lossy(), suffix)))
}

fn ext_file(image_path: &Path, suffix: &str) -> PathBuf {
    let mut s = image_path.as_os_str().to_owned();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}

pub fn parse_digikam_xml(xml: &str) -> Result<Sidecar, String> {
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
