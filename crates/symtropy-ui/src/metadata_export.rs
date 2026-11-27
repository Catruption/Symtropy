//! Metadata-only index export.
//!
//! The export is a JSON document containing sidecar decisions: tags, ratings,
//! relationships, source provenance, and user text. It deliberately contains
//! no image bytes and no cached pixel analysis, so it can be shared without
//! redistributing third-party media.

use crate::gallery::sidecar::{Relationship, Source};
use crate::gallery::GalleryItem;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const FORMAT: &str = "symtropy-metadata-index";
pub const VERSION: u32 = 1;
const LOCAL_PROPERTY: &str = "source_cache_path";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetadataIndex {
    pub format: String,
    pub version: u32,
    pub entries: Vec<MetadataEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetadataEntry {
    pub path: String,
    pub rating: u8,
    pub tags: Vec<String>,
    pub tag_parents: BTreeMap<String, String>,
    pub artists: Vec<String>,
    pub sources: Vec<Source>,
    pub alt_text: String,
    pub relationships: Vec<Relationship>,
    pub properties: BTreeMap<String, String>,
}

/// Build a deterministic, shareable index from the current gallery metadata.
pub fn build_metadata_index(items: &[GalleryItem]) -> MetadataIndex {
    let mut entries: Vec<MetadataEntry> = items
        .iter()
        .filter(|item| !item.is_folder)
        .map(|item| {
            let sidecar = &item.sidecar;
            let mut tags: Vec<String> = sidecar.tags.iter().cloned().collect();
            tags.sort();
            tags.dedup();
            let mut artists: Vec<String> = sidecar.artists.iter().cloned().collect();
            artists.sort();
            artists.dedup();
            let mut sources = sidecar.sources.clone();
            sources.sort_by(|a, b| a.url.cmp(&b.url));
            let mut relationships = sidecar.relationships.clone();
            relationships.sort_by(|a, b| {
                (&a.subject, &a.predicate, &a.object).cmp(&(&b.subject, &b.predicate, &b.object))
            });
            let tag_parents = sidecar
                .tag_parents
                .iter()
                .map(|(child, parent)| (child.clone(), parent.clone()))
                .collect();
            let properties = sidecar
                .properties
                .iter()
                .filter(|(key, _)| !is_local_property(key))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            MetadataEntry {
                path: item.path.clone(),
                rating: sidecar.rating,
                tags,
                tag_parents,
                artists,
                sources,
                alt_text: sidecar.alt_text.clone(),
                relationships,
                properties,
            }
        })
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    MetadataIndex {
        format: FORMAT.to_string(),
        version: VERSION,
        entries,
    }
}

fn is_local_property(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower == LOCAL_PROPERTY || lower.contains("cache_path")
}

/// Write the export via a temporary file, skipping byte-identical rewrites.
pub fn write_metadata_index(items: &[GalleryItem], destination: &Path) -> Result<usize, String> {
    let index = build_metadata_index(items);
    let count = index.entries.len();
    let text = serde_json::to_string_pretty(&index)
        .map_err(|error| format!("failed to encode metadata index: {error}"))?;
    if std::fs::read_to_string(destination).ok().as_deref() == Some(text.as_str()) {
        return Ok(count);
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    let tmp = destination.with_extension("json.tmp");
    std::fs::write(&tmp, &text)
        .map_err(|error| format!("failed to write {}: {error}", tmp.display()))?;
    std::fs::rename(&tmp, destination)
        .map_err(|error| format!("failed to replace {}: {error}", destination.display()))?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gallery::{BorderColors, Margins};
    use std::collections::HashSet;

    fn item(path: &str) -> GalleryItem {
        let mut sidecar = crate::gallery::Sidecar::default();
        sidecar.tags.push("fox".to_string());
        sidecar.rating = 4;
        sidecar.artists.push("kitty".to_string());
        sidecar.sources.push(Source {
            url: "https://e621.net/posts/123".to_string(),
            site: "e621".to_string(),
            ..Source::default()
        });
        sidecar.relationships.push(Relationship {
            subject: "item:this".to_string(),
            predicate: "evidences".to_string(),
            object: "https://e621.net/posts/123".to_string(),
        });
        sidecar.properties.insert(LOCAL_PROPERTY.to_string(), "/local/cache/a.webp".to_string());
        sidecar.properties.insert("display".to_string(), "false".to_string());
        sidecar.width = Some(640);
        sidecar.height = Some(480);
        sidecar.margins = Some(Margins { top: 1, bottom: 2, left: 3, right: 4 });
        sidecar.border_colors = Some(BorderColors::default());
        GalleryItem {
            id: path.to_string(),
            title: path.to_string(),
            path: path.to_string(),
            is_folder: false,
            ext: "webp".to_string(),
            aspect_ratio: 1.0,
            width: 640.0,
            height: 480.0,
            border_colors: BorderColors::default(),
            margins: Margins::default(),
            rating: sidecar.rating,
            tags: HashSet::new(),
            sidecar,
        }
    }

    #[test]
    fn export_contains_metadata_but_not_cached_pixels_or_local_cache_paths() {
        let index = build_metadata_index(&[item("/library/b.webp"), item("/library/a.webp")]);
        assert_eq!(index.format, FORMAT);
        assert_eq!(index.version, VERSION);
        assert_eq!(index.entries.len(), 2);
        assert_eq!(index.entries[0].path, "/library/a.webp");
        let json = serde_json::to_string(&index).unwrap();
        assert!(json.contains("https://e621.net/posts/123"));
        assert!(json.contains("\"evidences\""));
        assert!(!json.contains("/local/cache/a.webp"));
        assert!(!json.contains("\"width\""));
        assert!(!json.contains("\"margins\""));
        assert!(index.entries[0].properties.contains_key("display"));
    }

    #[test]
    fn export_skips_folders_and_identical_rewrites() {
        let root = std::env::temp_dir().join(format!(
            "symtropy-export-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut folder = item("/library/folder");
        folder.is_folder = true;
        let destination = root.join("index.json");
        assert_eq!(write_metadata_index(&[item("/library/a.webp"), folder], &destination).unwrap(), 1);
        let first = std::fs::read(&destination).unwrap();
        assert_eq!(write_metadata_index(&[item("/library/a.webp")], &destination).unwrap(), 1);
        assert_eq!(std::fs::read(&destination).unwrap(), first);
        std::fs::remove_dir_all(root).unwrap();
    }
}
