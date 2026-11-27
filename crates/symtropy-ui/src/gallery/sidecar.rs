//! Per-image sidecar metadata.
//!
//! Canonical: `<parent>/.<stem>.halo.json` next to the image. Survives format conversion.
//! Import fallback: DigiKam XML, in `.comments/` or next to the image. Read-only.
//! `display=false` in `properties` suppresses gallery visibility without deleting metadata;
//! the file-manager menu can restore it with "Show in Gallery". `preserved_tags` keeps
//! historical tags in the file while excluding them from the active index, and
//! `history` stores the last three metadata generations for bounded self-healing.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use super::{Margins, BorderColors};

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Source {
    #[serde(default)] pub url: String,
    #[serde(default)] pub site: String,
    #[serde(default)] pub captured_at: String,
    #[serde(default)] pub title: String,
    #[serde(default)] pub description: String,
}

/// One rolling metadata generation. Cache fields are deliberately absent:
/// history is for recovering tags/provenance, never for storing pixels.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct SidecarSnapshot {
    pub version: u32,
    #[serde(default)] pub tags: Vec<String>,
    #[serde(default)] pub preserved_tags: Vec<String>,
    #[serde(default)] pub tag_parents: HashMap<String, String>,
    #[serde(default)] pub rating: u8,
    #[serde(default)] pub artists: Vec<String>,
    #[serde(default)] pub sources: Vec<Source>,
    #[serde(default)] pub alt_text: String,
    #[serde(default)] pub relationships: Vec<Relationship>,
    #[serde(default)] pub properties: HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Relationship {
    pub subject: String,
    pub predicate: String,
    pub object: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Sidecar {
    #[serde(default = "default_version")] pub version: u32,
    #[serde(default)] pub tags: Vec<String>,
    /// Historical tags kept on disk but withheld from the active index/tree.
    /// They remain available for preservation review and can be restored.
    #[serde(default)] pub preserved_tags: Vec<String>,
    #[serde(default)] pub tag_parents: HashMap<String, String>,
    #[serde(default)] pub rating: u8,
    #[serde(default)] pub artists: Vec<String>,
    #[serde(default)] pub sources: Vec<Source>,
    #[serde(default)] pub alt_text: String,
    #[serde(default)] pub relationships: Vec<Relationship>,
    #[serde(default)] pub properties: HashMap<String, String>,
    /// Last three metadata generations, newest last. Cache fields are excluded.
    #[serde(default)] pub history: Vec<SidecarSnapshot>,

    // --- CACHE FIELDS ---
    #[serde(default)] pub width: Option<u32>,
    #[serde(default)] pub height: Option<u32>,
    #[serde(default)] pub is_animated: Option<bool>,
    #[serde(default)] pub margins: Option<Margins>,
    #[serde(default)] pub border_colors: Option<BorderColors>,
}

fn default_version() -> u32 { 1 }

/// Keep the preservation history bounded. Three full metadata copies are still
/// tiny for this library; unbounded sidecars eventually hurt scan/backup cost.
pub const SIDECAR_HISTORY_LIMIT: usize = 3;
pub const SIDECAR_WARN_BYTES: usize = 16 * 1024;
pub const SIDECAR_MAX_BYTES: usize = 64 * 1024;
pub const SIDECAR_PRESERVED_TAG_LIMIT: usize = 256;

impl Default for Sidecar {
    fn default() -> Self {
        Self {
            version: 1,
            tags: Vec::new(),
            preserved_tags: Vec::new(),
            tag_parents: HashMap::new(),
            rating: 0,
            artists: Vec::new(),
            sources: Vec::new(),
            alt_text: String::new(),
            relationships: Vec::new(),
            properties: HashMap::new(),
            history: Vec::new(),
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

    /// Cheap rating lookup for sorting large folders. Only the canonical JSON
    /// sidecar is consulted; legacy/XML fallback is handled by the full scanner.
    pub fn quick_rating(image_path: &Path) -> u8 {
        #[derive(serde::Deserialize)]
        struct RatingOnly {
            #[serde(default)]
            rating: u8,
        }

        std::fs::read_to_string(Self::path_for(image_path))
            .ok()
            .and_then(|text| serde_json::from_str::<RatingOnly>(&text).ok())
            .map(|parsed| parsed.rating)
            .unwrap_or(0)
    }

    /// A preserved tag remains in `tags` on disk but is hidden from the active
    /// gallery index and taxonomy until it is explicitly restored.
    pub fn is_preserved_tag(&self, tag: &str) -> bool {
        let trimmed = tag.trim();
        self.preserved_tags
            .iter()
            .any(|value| value.trim() == trimmed)
    }

    pub fn active_tags(&self) -> Vec<String> {
        let mut seen: HashSet<String> = HashSet::new();
        self.tags
            .iter()
            .filter(|tag| !self.is_preserved_tag(tag))
            .filter(|tag| seen.insert((*tag).clone()))
            .cloned()
            .collect()
    }

    pub fn active_tag_parents(&self) -> HashMap<String, String> {
        self.tag_parents
            .iter()
            .filter(|(child, parent)| {
                !self.is_preserved_tag(child) && !self.is_preserved_tag(parent)
            })
            .map(|(child, parent)| (child.clone(), parent.clone()))
            .collect()
    }

    /// Explicit preservation toggle. This never drops the tag from `tags`, so
    /// the archive copy survives index edits and sidecar syncs.
    pub fn set_tag_preserved(&mut self, tag: &str, preserved: bool) {
        let tag = tag.trim();
        if tag.is_empty() {
            return;
        }
        if preserved {
            if !self.tags.iter().any(|value| value.trim() == tag) {
                self.tags.push(tag.to_string());
            }
            if !self.is_preserved_tag(tag) {
                if self.preserved_tags.len() >= SIDECAR_PRESERVED_TAG_LIMIT {
                    eprintln!(
                        "sidecar: preservation limit reached ({} tags); keeping the file small",
                        SIDECAR_PRESERVED_TAG_LIMIT
                    );
                    return;
                }
                self.preserved_tags.push(tag.to_string());
            }
        } else {
            self.preserved_tags.retain(|value| value.trim() != tag);
        }
    }

    fn metadata_snapshot(&self) -> SidecarSnapshot {
        SidecarSnapshot {
            version: 0,
            tags: self.tags.clone(),
            preserved_tags: self.preserved_tags.clone(),
            tag_parents: self.tag_parents.clone(),
            rating: self.rating,
            artists: self.artists.clone(),
            sources: self.sources.clone(),
            alt_text: self.alt_text.clone(),
            relationships: self.relationships.clone(),
            properties: self.properties.clone(),
        }
    }

    /// Restore a historical metadata generation without touching cached pixel
    /// data. The restored generation becomes the new current version on write.
    pub fn restore_snapshot(&mut self, index: usize) -> bool {
        let Some(snapshot) = self.history.get(index).cloned() else {
            return false;
        };
        self.version = snapshot.version;
        self.tags = snapshot.tags;
        self.preserved_tags = snapshot.preserved_tags;
        self.tag_parents = snapshot.tag_parents;
        self.rating = snapshot.rating;
        self.artists = snapshot.artists;
        self.sources = snapshot.sources;
        self.alt_text = snapshot.alt_text;
        self.relationships = snapshot.relationships;
        self.properties = snapshot.properties;
        true
    }

    /// A sidecar-level visibility decision. Suppressed files stay on disk and
    /// keep their metadata; they are simply omitted from gallery listings.
    pub fn is_display_suppressed(&self) -> bool {
        self.properties
            .get("display")
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("false"))
    }

    pub fn save(&mut self, image_path: &Path) -> std::io::Result<()> {
        let json_path = Self::path_for(image_path);
        self.save_to(&json_path)?;

        // Clean up legacy file if it somehow still exists
        let legacy = Self::legacy_json_path(image_path);
        if legacy != json_path && legacy.exists() {
            let _ = std::fs::remove_file(legacy);
        }

        Ok(())
    }

    /// Write to an explicit sidecar path, skipping byte-identical rewrites.
    /// Meaningful metadata changes append one bounded generation to `history`.
    pub fn save_to(&mut self, json_path: &Path) -> std::io::Result<()> {
        let prior_text = std::fs::read_to_string(json_path).ok();
        let prior = prior_text
            .as_deref()
            .and_then(|text| serde_json::from_str::<Sidecar>(text).ok());
        let current = self.metadata_snapshot();

        let mut out = self.clone();
        let metadata_changed = prior
            .as_ref()
            .map(|old| old.metadata_snapshot() != current)
            .unwrap_or(true);
        let history_matches = out.history.last().is_some_and(|snapshot| {
            let mut snapshot = snapshot.clone();
            snapshot.version = 0;
            snapshot == current
        });

        if metadata_changed && !history_matches {
            let base = prior
                .as_ref()
                .map(|old| old.version.max(self.version))
                .unwrap_or(self.version.saturating_sub(1));
            out.version = base.saturating_add(1);
            let mut snapshot = out.metadata_snapshot();
            snapshot.version = out.version;
            out.history.push(snapshot);
            if out.history.len() > SIDECAR_HISTORY_LIMIT {
                let drop = out.history.len() - SIDECAR_HISTORY_LIMIT;
                out.history.drain(..drop);
            }
        }

        let text = serde_json::to_string_pretty(&out)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        if text.len() > SIDECAR_MAX_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "sidecar is {} bytes, above the {} byte preservation limit",
                    text.len(),
                    SIDECAR_MAX_BYTES
                ),
            ));
        }
        if text.len() > SIDECAR_WARN_BYTES {
            eprintln!(
                "sidecar: {} is {} bytes; review preservation history",
                json_path.display(),
                text.len()
            );
        }
        if prior_text.as_deref() != Some(text.as_str()) {
            let tmp = json_path.with_extension("json.tmp");
            std::fs::write(&tmp, text)?;
            std::fs::rename(&tmp, json_path)?;
        }
        *self = out;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_rating_reads_canonical_sidecar() {
        let dir = std::env::temp_dir().join(format!("symtropy-rating-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("photo.webp");
        std::fs::write(Sidecar::path_for(&image), r#"{"rating":5}"#).unwrap();

        assert_eq!(Sidecar::quick_rating(&image), 5);
        assert_eq!(Sidecar::quick_rating(&dir.join("missing.webp")), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn preserved_tags_stay_in_the_file_but_leave_the_active_index() {
        let mut sidecar = Sidecar::default();
        sidecar.tags = vec!["fox".to_string(), "old-artist".to_string()];
        sidecar.tag_parents.insert("old-artist".to_string(), "artist".to_string());
        sidecar.tag_parents.insert("fox".to_string(), "animal".to_string());

        sidecar.set_tag_preserved("old-artist", true);
        assert_eq!(sidecar.version, 1);
        assert!(sidecar.tags.contains(&"old-artist".to_string()));
        assert_eq!(sidecar.active_tags(), vec!["fox".to_string()]);
        assert_eq!(sidecar.active_tag_parents().len(), 1);
        assert!(sidecar.active_tag_parents().contains_key("fox"));

        sidecar.set_tag_preserved("old-artist", false);
        assert_eq!(sidecar.active_tags().len(), 2);
        assert_eq!(sidecar.active_tag_parents().len(), 2);
    }

    #[test]
    fn save_keeps_three_rolling_metadata_generations() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("symtropy-history-{}", unique));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".a.halo.json");

        let mut sidecar = Sidecar::default();
        sidecar.tags.push("fox".to_string());
        sidecar.save_to(&path).unwrap();
        sidecar.tags.push("cat".to_string());
        sidecar.save_to(&path).unwrap();
        sidecar.tags.push("bird".to_string());
        sidecar.save_to(&path).unwrap();
        sidecar.tags.push("fish".to_string());
        sidecar.save_to(&path).unwrap();

        assert_eq!(sidecar.version, 4);
        assert_eq!(sidecar.history.len(), 3);
        assert_eq!(
            sidecar.history[0].tags,
            vec!["fox".to_string(), "cat".to_string()]
        );
        assert_eq!(sidecar.history[2].tags.last().unwrap(), "fish");

        assert!(sidecar.restore_snapshot(0));
        assert_eq!(sidecar.tags, vec!["fox".to_string(), "cat".to_string()]);
        sidecar.save_to(&path).unwrap();
        let disk: Sidecar =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(disk.tags, vec!["fox".to_string(), "cat".to_string()]);
        assert_eq!(disk.history.len(), 3);
        assert_eq!(disk.history.last().unwrap().tags, disk.tags);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn display_false_suppresses_the_file_without_discarding_metadata() {
        let mut sidecar = Sidecar::default();
        assert!(!sidecar.is_display_suppressed());
        sidecar.properties.insert("display".to_string(), "FALSE".to_string());
        assert!(sidecar.is_display_suppressed());
        sidecar.tags.push("keep-me".to_string());
        assert_eq!(sidecar.tags, vec!["keep-me".to_string()]);
    }
}
