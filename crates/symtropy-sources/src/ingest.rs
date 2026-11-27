//! Shared local ingest. Adapters never call this; the caller does after a
//! user-approved download.

use crate::model::{RemoteItem, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SourceSidecar {
    #[serde(default = "default_version")]
    version: u32,
    remote_id: String,
    site: String,
    source_url: String,
    #[serde(default)] title: String,
    #[serde(default)] author: String,
    #[serde(default)] description: String,
    #[serde(default)] captured_at: String,
    #[serde(default)] tags: Vec<String>,
}

fn default_version() -> u32 {
    1
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngestResult {
    pub image_path: PathBuf,
    pub sidecar_path: PathBuf,
    pub wrote_image: bool,
    pub wrote_sidecar: bool,
}

pub struct Ingestor {
    root: PathBuf,
}

impl Ingestor {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn fingerprint(bytes: &[u8]) -> String {
        let mut hash = 0xcbf29ce484222325u64;
        for byte in bytes {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        format!("{hash:016x}")
    }

    pub fn cached_image_path(&self, item: &RemoteItem, bytes: &[u8]) -> PathBuf {
        let dir = self.root.join("sources").join(item.site.as_str());
        let name = if item.remote_id.is_empty() {
            Self::fingerprint(bytes)
        } else {
            format!("{}-{}", slug(&item.remote_id), Self::fingerprint(bytes))
        };
        dir.join(format!("{name}.{}", image_extension(bytes)))
    }

    /// Write the downloaded bytes and a metadata-only sidecar atomically.
    /// Byte-identical rewrites are skipped.
    pub fn ingest(&self, item: &RemoteItem, bytes: &[u8]) -> Result<IngestResult> {
        if bytes.is_empty() {
            return Err("refusing to ingest an empty file".to_string());
        }
        let image_path = self.cached_image_path(item, bytes);
        let sidecar_path = image_path.with_extension("json");
        let parent = image_path.parent().ok_or_else(|| "cache path has no parent".to_string())?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;

        let wrote_image = write_if_changed(&image_path, bytes)?;
        let sidecar = SourceSidecar {
            version: default_version(),
            remote_id: item.remote_id.clone(),
            site: item.site.as_str().to_string(),
            source_url: item.source_url.clone(),
            title: item.title.clone(),
            author: item.author.clone(),
            description: item.description.clone(),
            captured_at: item.captured_at.clone(),
            tags: item.tag_names(),
        };
        let sidecar_bytes = serde_json::to_string_pretty(&sidecar)
            .map_err(|error| error.to_string())?
            .into_bytes();
        let wrote_sidecar = write_if_changed(&sidecar_path, &sidecar_bytes)?;

        Ok(IngestResult {
            image_path,
            sidecar_path,
            wrote_image,
            wrote_sidecar,
        })
    }
}

fn image_extension(bytes: &[u8]) -> &'static str {
    use image::ImageFormat;
    match image::guess_format(bytes) {
        Ok(ImageFormat::Png) => "png",
        Ok(ImageFormat::Jpeg) => "jpg",
        Ok(ImageFormat::WebP) => "webp",
        Ok(ImageFormat::Gif) => "gif",
        Ok(ImageFormat::Bmp) => "bmp",
        _ => "bin",
    }
}

fn slug(value: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in value.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if mapped == '-' {
            if last_dash {
                continue;
            }
            last_dash = true;
        } else {
            last_dash = false;
        }
        out.push(mapped);
    }
    out.trim_matches('-').chars().take(48).collect()
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<bool> {
    if std::fs::read(path).ok().as_deref() == Some(bytes) {
        return Ok(false);
    }
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|ext| ext.to_str()).unwrap_or("bin")
    ));
    std::fs::write(&tmp, bytes).map_err(|error| error.to_string())?;
    std::fs::rename(&tmp, path).map_err(|error| error.to_string())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SourceSite;

    #[test]
    fn cache_extension_follows_the_actual_image_bytes() {
        use image::{ImageBuffer, Luma};
        let root = std::env::temp_dir().join(format!(
            "symtropy-extension-test-{}-{}",
            std::process::id(),
            crate::tests::unique_tag()
        ));
        let pixels = ImageBuffer::from_fn(8, 8, |x, y| Luma([x.saturating_mul(y) as u8]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageLuma8(pixels)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        let item = RemoteItem { site: SourceSite::E621, remote_id: "123".into(), ..RemoteItem::default() };
        let result = Ingestor::new(&root).ingest(&item, &bytes).unwrap();
        assert_eq!(result.image_path.extension().and_then(|ext| ext.to_str()), Some("png"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn ingest_writes_cache_once_and_skips_identical_rewrite() {
        let root = std::env::temp_dir().join(format!(
            "symtropy-sources-test-{}-{}",
            std::process::id(),
            crate::tests::unique_tag()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let item = RemoteItem {
            site: SourceSite::E621,
            remote_id: "post-123".into(),
            source_url: "https://example.invalid/post/123".into(),
            tags: vec![crate::model::RemoteTag::new("fox", "general")],
            ..RemoteItem::default()
        };

        let ingestor = Ingestor::new(&root);
        let first = ingestor.ingest(&item, b"image-bytes").unwrap();
        assert!(first.wrote_image && first.wrote_sidecar);
        assert!(first.image_path.exists() && first.sidecar_path.exists());

        let second = ingestor.ingest(&item, b"image-bytes").unwrap();
        assert!(!second.wrote_image && !second.wrote_sidecar);
        assert_eq!(first.image_path, second.image_path);

        std::fs::remove_dir_all(&root).unwrap();
    }
}
