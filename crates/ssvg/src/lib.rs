#![doc = include_str!("../README.md")]

//! SuperSVG (`.ssvg`) composite image format.
//!
//! A package is one ZIP file. The first entry is an uncompressed `mimetype`,
//! followed by canonical JSON5 `manifest.json5` and `scene.json5`, immutable
//! WebP assets, a baked `preview.webp`, and a `merged.webp`. Optional
//! non-normative `history/` entries may be present but renderers ignore them.
//!
//! This crate only defines and transports the format. It does not store undo
//! stacks, layer edit history, UI state, or a drawing-process timelapse.

pub mod convert;
pub mod kra;
pub mod model;
pub mod validate;

pub use convert::{
    build_package, convert_kra, convert_kra_with_options, webp_canvas, webp_frame,
    ConversionReport, ConvertOptions,
};
pub use kra::{build_package_direct, build_package_direct_with_options, convert_kra_direct};
pub use model::*;

use serde::{de::DeserializeOwned, Serialize};
use std::collections::BTreeMap;
use std::io::{Cursor, Read, Seek, Write};

pub const MIME_TYPE: &str = "application/x-symtropy-ssvg";
pub const FORMAT_VERSION: u32 = 1;
pub const MIMETYPE_ENTRY: &str = "mimetype";
pub const MANIFEST_ENTRY: &str = "manifest.json5";
pub const SCENE_ENTRY: &str = "scene.json5";

pub type Result<T> = std::result::Result<T, String>;

/// A complete `.ssvg` document and its baked payloads.
#[derive(Clone, Debug, PartialEq)]
pub struct Package {
    pub manifest: Manifest,
    pub scene: Scene,
    pub assets: BTreeMap<String, Vec<u8>>,
    pub preview: Vec<u8>,
    pub merged: Vec<u8>,
    /// Opaque extra entries, typically `history/*`. Never normative.
    pub extras: BTreeMap<String, Vec<u8>>,
}

impl Package {
    pub fn new(manifest: Manifest, scene: Scene, preview: Vec<u8>, merged: Vec<u8>) -> Self {
        Self {
            manifest,
            scene,
            assets: BTreeMap::new(),
            preview,
            merged,
            extras: BTreeMap::new(),
        }
    }

    pub fn add_asset(&mut self, path: impl Into<String>, bytes: Vec<u8>) {
        self.assets.insert(path.into(), bytes);
    }

    pub fn validate(&self) -> Result<()> {
        validate::validate_package(self)
    }

    /// Strip optional history data. Renderers may call this before rewriting.
    pub fn strip_history(&mut self) {
        self.extras.retain(|path, _| !path.starts_with("history/"));
    }

    pub fn write_to<W: Write + Seek>(&self, writer: W) -> Result<()> {
        self.validate()?;
        let mut zip = zip::ZipWriter::new(writer);

        // `mimetype` must be first and uncompressed so magic-byte sniffers work.
        zip.start_file(MIMETYPE_ENTRY, stored())
            .map_err(|e| format!("cannot write mimetype: {e}"))?;
        zip.write_all(MIME_TYPE.as_bytes())
            .map_err(|e| format!("cannot write mimetype: {e}"))?;

        zip.start_file(MANIFEST_ENTRY, deflated())
            .map_err(|e| format!("cannot write manifest: {e}"))?;
        zip.write_all(encode_json5(&self.manifest)?.as_bytes())
            .map_err(|e| format!("cannot write manifest: {e}"))?;

        zip.start_file(SCENE_ENTRY, deflated())
            .map_err(|e| format!("cannot write scene: {e}"))?;
        zip.write_all(encode_json5(&self.scene)?.as_bytes())
            .map_err(|e| format!("cannot write scene: {e}"))?;

        for (path, bytes) in &self.assets {
            zip.start_file(path, deflated())
                .map_err(|e| format!("cannot write asset {path}: {e}"))?;
            zip.write_all(bytes)
                .map_err(|e| format!("cannot write asset {path}: {e}"))?;
        }

        zip.start_file(&self.manifest.preview, deflated())
            .map_err(|e| format!("cannot write preview: {e}"))?;
        zip.write_all(&self.preview)
            .map_err(|e| format!("cannot write preview: {e}"))?;

        zip.start_file(&self.manifest.merged, deflated())
            .map_err(|e| format!("cannot write merged image: {e}"))?;
        zip.write_all(&self.merged)
            .map_err(|e| format!("cannot write merged image: {e}"))?;

        for (path, bytes) in &self.extras {
            zip.start_file(path, deflated())
                .map_err(|e| format!("cannot write extra entry {path}: {e}"))?;
            zip.write_all(bytes)
                .map_err(|e| format!("cannot write extra entry {path}: {e}"))?;
        }

        zip.finish().map_err(|e| format!("cannot finish container: {e}"))?;
        Ok(())
    }

    /// Validate the container header and decode its manifest.
    fn open_container<R: Read + Seek>(reader: R) -> Result<(zip::ZipArchive<R>, Manifest)> {
        let mut zip = zip::ZipArchive::new(reader)
            .map_err(|e| format!("cannot open ssvg container: {e}"))?;
        if zip.len() == 0 {
            return Err("ssvg container is empty".to_owned());
        }

        {
            let mut first = zip
                .by_index(0)
                .map_err(|e| format!("cannot read first entry: {e}"))?;
            if first.name() != MIMETYPE_ENTRY {
                return Err(format!(
                    "first entry must be {} but was {}",
                    MIMETYPE_ENTRY,
                    first.name()
                ));
            }
            let mut mime = String::new();
            first
                .read_to_string(&mut mime)
                .map_err(|e| format!("cannot read mimetype: {e}"))?;
            if mime != MIME_TYPE {
                return Err(format!("unexpected mimetype {mime:?}"));
            }
            if first.compression() != zip::CompressionMethod::Stored {
                return Err("mimetype must be stored uncompressed".to_owned());
            }
        }

        let manifest_text = read_text(&mut zip, MANIFEST_ENTRY)?;
        let manifest: Manifest = decode_json5(&manifest_text)
            .map_err(|e| format!("invalid manifest.json5: {e}"))?;
        validate::validate_manifest(&manifest)?;
        Ok((zip, manifest))
    }

    fn read_webp<R: Read + Seek>(zip: &mut zip::ZipArchive<R>, path: &str) -> Result<Vec<u8>> {
        let bytes = read_bytes(zip, path)?;
        if !validate::is_webp(&bytes) {
            return Err(format!("{path} is not a WebP image"));
        }
        Ok(bytes)
    }

    /// Read the manifest and both baked images, skipping scene and layer assets.
    pub fn read_baked_from<R: Read + Seek>(reader: R) -> Result<(Manifest, Vec<u8>, Vec<u8>)> {
        let (mut zip, manifest) = Self::open_container(reader)?;
        let preview = Self::read_webp(&mut zip, &manifest.preview)?;
        let merged = Self::read_webp(&mut zip, &manifest.merged)?;
        Ok((manifest, preview, merged))
    }

    /// Read only the manifest and baked preview, skipping scene and layer assets.
    pub fn read_preview_from<R: Read + Seek>(reader: R) -> Result<(Manifest, Vec<u8>)> {
        let (mut zip, manifest) = Self::open_container(reader)?;
        let preview = Self::read_webp(&mut zip, &manifest.preview)?;
        Ok((manifest, preview))
    }

    pub fn read_from<R: Read + Seek>(reader: R) -> Result<Self> {
        let (mut zip, manifest) = Self::open_container(reader)?;

        let scene_text = read_text(&mut zip, SCENE_ENTRY)?;
        let scene: Scene =
            decode_json5(&scene_text).map_err(|e| format!("invalid scene.json5: {e}"))?;
        validate::validate_scene(&scene)?;

        let preview = Self::read_webp(&mut zip, &manifest.preview)?;
        let merged = Self::read_webp(&mut zip, &manifest.merged)?;
        let mut package = Self::new(manifest, scene, preview, merged);

        let reserved = vec![
            MIMETYPE_ENTRY.to_owned(),
            MANIFEST_ENTRY.to_owned(),
            SCENE_ENTRY.to_owned(),
            package.manifest.preview.clone(),
            package.manifest.merged.clone(),
        ];

        for index in 0..zip.len() {
            let mut entry = zip
                .by_index(index)
                .map_err(|e| format!("cannot read entry {index}: {e}"))?;
            let name = entry.name().to_owned();
            if reserved.contains(&name) {
                continue;
            }
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .map_err(|e| format!("cannot read entry {name}: {e}"))?;
            if name.starts_with("assets/") && name.ends_with(".webp") {
                package.assets.insert(name, bytes);
            } else {
                package.extras.insert(name, bytes);
            }
        }

        package.validate()?;
        Ok(package)
    }

    /// Canonical JSON5 text for both documents.
    pub fn to_json5_bundle(&self) -> Result<(String, String)> {
        Ok((encode_json5(&self.manifest)?, encode_json5(&self.scene)?))
    }

    /// Optional strict JSON export for tools that cannot read JSON5.
    pub fn to_json_bundle(&self) -> Result<(String, String)> {
        Ok((encode_json(&self.manifest)?, encode_json(&self.scene)?))
    }
}

pub fn read_from_bytes(bytes: &[u8]) -> Result<Package> {
    Package::read_from(Cursor::new(bytes))
}

/// Read only the manifest and baked preview from in-memory bytes.
pub fn read_preview_from_bytes(bytes: &[u8]) -> Result<(Manifest, Vec<u8>)> {
    Package::read_preview_from(Cursor::new(bytes))
}

/// Read the manifest and both baked images from in-memory bytes.
pub fn read_baked_from_bytes(bytes: &[u8]) -> Result<(Manifest, Vec<u8>, Vec<u8>)> {
    Package::read_baked_from(Cursor::new(bytes))
}

pub fn encode_json5<T: Serialize>(value: &T) -> Result<String> {
    json5::to_string(value).map_err(|e| format!("cannot encode JSON5: {e}"))
}

pub fn encode_json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string_pretty(value).map_err(|e| format!("cannot encode JSON: {e}"))
}

pub fn decode_json5<T: DeserializeOwned>(text: &str) -> Result<T> {
    json5::from_str(text).map_err(|e| format!("cannot decode JSON5: {e}"))
}

fn stored() -> zip::write::SimpleFileOptions {
    zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
}

fn deflated() -> zip::write::SimpleFileOptions {
    zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
}

fn read_text<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<String> {
    let bytes = read_bytes(zip, name)?;
    String::from_utf8(bytes).map_err(|_| format!("{name} is not UTF-8"))
}

fn read_bytes<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>> {
    let mut entry = zip
        .by_name(name)
        .map_err(|_| format!("{name} is missing from the container"))?;
    let mut bytes = Vec::new();
    entry
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read {name}: {e}"))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::is_webp;

    fn webp() -> Vec<u8> {
        b"RIFF\x04\x00\x00\x00WEBPVP8 \x00\x00\x00\x00".to_vec()
    }

    fn package() -> Package {
        let bytes = webp();
        let mut package = Package::new(
            Manifest {
                canvas: Canvas {
                    width: 64,
                    height: 64,
                    ..Canvas::default()
                },
                assets: vec![Asset {
                    path: "assets/a.webp".to_owned(),
                    width: 64,
                    height: 64,
                    bytes: bytes.len() as u64,
                }],
                ..Manifest::default()
            },
            Scene {
                layers: vec![Node::layer("a", "A", "assets/a.webp")],
                ..Scene::default()
            },
            bytes.clone(),
            bytes.clone(),
        );
        package.add_asset("assets/a.webp", bytes);
        package
    }

    #[test]
    fn round_trip_preserves_documents_and_payloads() {
        let original = package();
        let mut bytes = Vec::new();
        original.write_to(Cursor::new(&mut bytes)).unwrap();
        let read = read_from_bytes(&bytes).unwrap();
        assert_eq!(read, original);
    }

    #[test]
    fn preview_only_reading_returns_baked_payloads() {
        let original = package();
        let mut bytes = Vec::new();
        original.write_to(Cursor::new(&mut bytes)).unwrap();

        let (manifest, preview) = read_preview_from_bytes(&bytes).unwrap();
        assert_eq!(manifest.canvas.width, 64);
        assert_eq!(preview, original.preview);

        let (_, preview, merged) = read_baked_from_bytes(&bytes).unwrap();
        assert_eq!(preview, original.preview);
        assert_eq!(merged, original.merged);
    }

    #[test]
    fn explicit_layer_state_round_trips() {
        let mut original = package();
        let mut layer = Node::layer("a", "A", "assets/a.webp").with_order(7);
        layer.opacity = 0.42;
        layer.visible = false;
        layer.pass_through = true;
        layer.source = Some(crate::NodeSource {
            uuid: Some("{abc}".to_owned()),
            file_name: Some("layer12".to_owned()),
            locked: Some(true),
            ..crate::NodeSource::default()
        });
        original.scene.layers[0] = layer;
        original.scene.layers[0].name = "Disabled A".to_owned();

        let mut bytes = Vec::new();
        original.write_to(Cursor::new(&mut bytes)).unwrap();
        let read = read_from_bytes(&bytes).unwrap();
        let layer = &read.scene.layers[0];
        assert_eq!(layer.order, 7);
        assert_eq!(layer.opacity, 0.42);
        assert!(!layer.visible);
        assert!(layer.pass_through);
        assert_eq!(layer.name, "Disabled A");
        let source = layer.source.as_ref().unwrap();
        assert_eq!(source.uuid.as_deref(), Some("{abc}"));
        assert_eq!(source.file_name.as_deref(), Some("layer12"));
        assert_eq!(source.locked, Some(true));
    }

    #[test]
    fn mimetype_is_first_and_uncompressed() {
        let mut bytes = Vec::new();
        package().write_to(Cursor::new(&mut bytes)).unwrap();
        let mut zip = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
        let first = zip.by_index(0).unwrap();
        assert_eq!(first.name(), MIMETYPE_ENTRY);
        assert_eq!(first.compression(), zip::CompressionMethod::Stored);
    }

    #[test]
    fn json5_reader_accepts_comments_and_trailing_commas() {
        let text = "{
            // Hand-written document
            format: 'SuperSVG',
            version: 1,
            canvas: { width: 10, height: 20, },
            assets: [],
            preview: 'preview.webp',
            merged: 'merged.webp',
        }";
        let manifest: Manifest = decode_json5(text).unwrap();
        assert_eq!(manifest.canvas.width, 10);
        assert_eq!(manifest.canvas.height, 20);
    }

    #[test]
    fn strict_json_export_stays_parseable() {
        let package = package();
        let (manifest, scene) = package.to_json_bundle().unwrap();
        let _: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        let _: serde_json::Value = serde_json::from_str(&scene).unwrap();
    }

    #[test]
    fn round_trip_keeps_non_normative_history() {
        let mut original = package();
        original
            .extras
            .insert("history/undo.json5".to_owned(), b"{ note: 'old' }".to_vec());
        let mut bytes = Vec::new();
        original.write_to(Cursor::new(&mut bytes)).unwrap();
        let read = read_from_bytes(&bytes).unwrap();
        assert_eq!(read.extras.get("history/undo.json5"), original.extras.get("history/undo.json5"));

        let mut stripped = read;
        stripped.strip_history();
        assert!(stripped.extras.is_empty());
    }

    #[test]
    fn rejects_a_foreign_container() {
        let mut bytes = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(Cursor::new(&mut bytes));
            zip.start_file("mimetype", stored()).unwrap();
            zip.write_all(b"application/zip").unwrap();
            zip.finish().unwrap();
        }
        assert!(read_from_bytes(&bytes).is_err());
    }

    #[test]
    fn baked_payloads_are_detected_as_webp() {
        assert!(is_webp(&webp()));
        assert!(!is_webp(b"not a webp"));
    }
}
