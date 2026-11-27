//! Converter from Krita `.kra` projects plus exported per-layer WebPs.
//!
//! The KRA document supplies canvas metadata and authoritative layer metadata.
//! The exported WebP tree supplies the layer stack, group nesting, and source
//! pixels. Krita's `Name#index(flags).webp` filenames are treated as temporary
//! input metadata: flags may contain `disabled`, `alpha`, blend modes, or an
//! opacity percentage. The emitted package uses clean asset paths and stores
//! order, opacity and visibility explicitly in `scene.json5`.

use crate::{Asset, BlendMode, Canvas, Manifest, Node, NodeSource, Rect, Scene};
use quick_xml::events::Event;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug)]
pub struct ConvertOptions {
    /// Turn an exported WebP's ANMF frame rectangle into `crop` + offset.
    pub crop_frames: bool,
    /// Copy opacity/blend/visibility from matching Krita layers.
    pub use_krita_metadata: bool,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            crop_frames: true,
            use_krita_metadata: true,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ConversionReport {
    pub groups: usize,
    pub layers: usize,
    pub matched_krita_layers: usize,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Default)]
struct Flags {
    disabled: bool,
    alpha: bool,
    blend: Option<BlendMode>,
    opacity: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PathPart {
    name: String,
    index: u32,
}

#[derive(Clone, Debug)]
struct ExportEntry {
    dirs: Vec<PathPart>,
    file: PathPart,
    flags: Flags,
    asset_path: String,
    bytes: Vec<u8>,
    frame: Option<Rect>,
    canvas: Option<(u32, u32)>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct KritaLayer {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) is_group: bool,
    pub(crate) opacity: i32,
    pub(crate) blend: String,
    pub(crate) visible: bool,
    pub(crate) alpha_inherit: bool,
    pub(crate) file_name: Option<String>,
    pub(crate) uuid: Option<String>,
    pub(crate) x: Option<i32>,
    pub(crate) y: Option<i32>,
    pub(crate) channel_flags: Option<String>,
    pub(crate) locked: Option<bool>,
    pub(crate) color_label: Option<u32>,
    pub(crate) in_timeline: Option<bool>,
    pub(crate) pass_through: Option<bool>,
}

impl KritaLayer {
    pub(crate) fn source(&self, emitted_name: &str) -> NodeSource {
        NodeSource {
            uuid: self.uuid.clone(),
            source_name: (self.name != emitted_name).then(|| self.name.clone()),
            file_name: self.file_name.clone(),
            offset: match (self.x, self.y) {
                (Some(x), Some(y)) => Some([x, y]),
                _ => None,
            },
            channel_flags: self.channel_flags.clone(),
            locked: self.locked,
            color_label: self.color_label,
            in_timeline: self.in_timeline,
            pass_through: self.pass_through,
        }
    }
}

/// Convert a KRA project and its exported WebP layers into a `.ssvg` package.
pub fn convert_kra(kra: &Path, exports: &Path, output: &Path) -> Result<ConversionReport, String> {
    convert_kra_with_options(kra, exports, output, ConvertOptions::default())
}

/// Convert with explicit options. Returns the finished package and a report.
pub fn convert_kra_with_options(
    kra: &Path,
    exports: &Path,
    output: &Path,
    options: ConvertOptions,
) -> Result<ConversionReport, String> {
    let (package, report) = build_package(kra, exports, options)?;
    let mut file = fs::File::create(output)
        .map_err(|e| format!("cannot create {}: {e}", output.display()))?;
    package
        .write_to(&mut file)
        .map_err(|e| format!("cannot write {}: {e}", output.display()))?;
    Ok(report)
}

/// Build the package without writing it to disk.
pub fn build_package(
    kra: &Path,
    exports: &Path,
    options: ConvertOptions,
) -> Result<(crate::Package, ConversionReport), String> {
    let archive_bytes = fs::read(kra).map_err(|e| format!("cannot read {}: {e}", kra.display()))?;
    let mut archive = zip::ZipArchive::new(Cursor::new(archive_bytes))
        .map_err(|e| format!("cannot open {}: {e}", kra.display()))?;

    let xml = read_zip_text(&mut archive, "maindoc.xml")?;
    let (canvas, krita_layers) = parse_maindoc(&xml)?;
    let preview_png = read_zip_bytes(&mut archive, "preview.png")?;
    let merged_png = read_zip_bytes(&mut archive, "mergedimage.png")?;
    drop(archive);

    let mut entries = Vec::new();
    collect_webps(exports, Path::new(""), &mut entries)?;
    if entries.is_empty() {
        return Err(format!("no .webp files found under {}", exports.display()));
    }
    dedupe_asset_paths(&mut entries);
    entries.sort_by(|a, b| {
        a.dirs
            .len()
            .cmp(&b.dirs.len())
            .then_with(|| a.asset_path.cmp(&b.asset_path))
    });

    let mut report = ConversionReport::default();
    let mut used = BTreeSet::new();
    let layers = build_nodes(
        &entries,
        0,
        &[],
        &krita_layers,
        &mut used,
        &options,
        &mut report,
    )?;
    report.layers = entries.len();

    let mut manifest = Manifest::default();
    manifest.canvas = canvas.clone();
    manifest.generator = Some(format!("ssvg converter for {}", file_name(kra)));
    for entry in &entries {
        let (w, h) = entry.canvas.unwrap_or((canvas.width, canvas.height));
        manifest.assets.push(Asset {
            path: entry.asset_path.clone(),
            width: w,
            height: h,
            bytes: entry.bytes.len() as u64,
        });
    }

    let package = crate::Package::new(
        manifest,
        Scene {
            layers,
            ..Scene::default()
        },
        png_to_webp(&preview_png)?,
        png_to_webp(&merged_png)?,
    );
    let mut assets = BTreeMap::new();
    for entry in &entries {
        assets.insert(entry.asset_path.clone(), entry.bytes.clone());
    }
    // `Package::new` starts with no assets; fill them through the public field.
    let mut package = package;
    package.assets = assets;

    package.validate()?;
    Ok((package, report))
}

fn build_nodes(
    entries: &[ExportEntry],
    depth: usize,
    prefix: &[PathPart],
    krita: &[KritaLayer],
    used: &mut BTreeSet<String>,
    options: &ConvertOptions,
    report: &mut ConversionReport,
) -> Result<Vec<Node>, String> {
    enum Item<'a> {
        Group(&'a PathPart),
        Layer(&'a ExportEntry),
    }

    let mut items: Vec<(u32, String, Item)> = Vec::new();
    let mut seen_dirs = BTreeMap::new();

    for entry in entries {
        if !entry.dirs.starts_with(prefix) {
            continue;
        }
        if entry.dirs.len() == depth {
            items.push((
                entry.file.index,
                entry.file.name.clone(),
                Item::Layer(entry),
            ));
        } else if entry.dirs.len() > depth {
            let part = &entry.dirs[depth];
            seen_dirs.entry(part.name.clone()).or_insert(part);
        }
    }
    for (name, part) in seen_dirs {
        items.push((part.index, name, Item::Group(part)));
    }

    // Directories without an explicit `#index` sort after all numbered
    // siblings instead of emitting a sentinel order into scene.json5.
    let mut next_order = items
        .iter()
        .map(|(order, _, _)| *order)
        .filter(|order| *order != u32::MAX)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    for (order, _, _) in &mut items {
        if *order == u32::MAX {
            *order = next_order;
            next_order = next_order.saturating_add(1);
        }
    }
    items.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    let mut nodes = Vec::with_capacity(items.len());
    for (order, _, item) in items {
        match item {
            Item::Group(part) => {
                let mut child_prefix = prefix.to_vec();
                child_prefix.push((*part).clone());
                let children = build_nodes(
                    entries,
                    depth + 1,
                    &child_prefix,
                    krita,
                    used,
                    options,
                    report,
                )?;
                report.groups += 1;
                let mut node = Node::group(group_id(prefix, part), part.name.clone());
                node.order = order;
                node.children = children;
                apply_group_metadata(&mut node, part, krita);
                nodes.push(node);
            }
            Item::Layer(entry) => {
                nodes.push(build_layer(entry, krita, used, options, report));
            }
        }
    }
    Ok(nodes)
}

fn apply_group_metadata(node: &mut Node, part: &PathPart, krita: &[KritaLayer]) {
    let key = normalize(&part.name);
    if let Some(layer) = krita
        .iter()
        .find(|k| k.is_group && normalize(&k.name) == key)
    {
        node.visible = layer.visible;
        node.opacity = layer.opacity as f32 / 255.0;
        node.blend = parse_blend(&layer.blend).unwrap_or(BlendMode::Normal);
        node.alpha_inherit = layer.alpha_inherit;
        node.pass_through = layer.pass_through.unwrap_or(false);
        node.source = Some(layer.source(&node.name));
    }
}

fn build_layer(
    entry: &ExportEntry,
    krita: &[KritaLayer],
    used: &mut BTreeSet<String>,
    options: &ConvertOptions,
    report: &mut ConversionReport,
) -> Node {
    let id = layer_id(&entry.asset_path);
    let mut node = Node::layer(id, entry.file.name.clone(), entry.asset_path.clone());
    node.order = entry.file.index;

    if options.crop_frames {
        if let Some(frame) = entry.frame {
            node.crop = Some(frame);
            node.transform.offset = [frame.x, frame.y];
        }
    }

    if options.use_krita_metadata {
        let group_key = entry
            .dirs
            .iter()
            .map(|d| normalize(&d.name))
            .collect::<Vec<_>>()
            .join("/");
        let mut best: Option<(i32, &KritaLayer)> = None;
        for layer in krita {
            if layer.is_group || used.contains(&layer.name) {
                continue;
            }
            let score = match_score(entry, &group_key, layer);
            if best.map_or(true, |(best_score, _)| score > best_score) {
                best = Some((score, layer));
            }
        }
        if let Some((score, layer)) = best {
            if score >= 35 {
                used.insert(layer.name.clone());
                report.matched_krita_layers += 1;
                node.visible = layer.visible;
                node.opacity = layer.opacity as f32 / 255.0;
                node.blend = parse_blend(&layer.blend).unwrap_or(BlendMode::Normal);
                node.alpha_inherit = layer.alpha_inherit;
                node.pass_through = layer.pass_through.unwrap_or(false);
                node.source = Some(layer.source(&node.name));
            }
        }
    }

    // Filename flags are the fallback, and always win for properties they
    // encode explicitly when no Krita layer matched.
    {
        let flags = &entry.flags;
        if flags.disabled {
            node.visible = false;
        }
        if flags.alpha {
            node.alpha_inherit = true;
        }
        if let Some(blend) = flags.blend {
            node.blend = blend;
        }
        if let Some(opacity) = flags.opacity {
            node.opacity = opacity;
        }
    }
    node
}

fn match_score(entry: &ExportEntry, group_key: &str, layer: &KritaLayer) -> i32 {
    let name = normalize(&entry.file.name);
    let krita_name = normalize(&layer.name);
    let mut score = 0;
    if name == krita_name {
        score += 100;
    } else if !name.is_empty() && (krita_name.contains(&name) || name.contains(&krita_name)) {
        score += 60;
    }
    if normalize(&layer.path) == group_key {
        score += 30;
    }
    if entry.flags.blend == parse_blend(&layer.blend) && entry.flags.blend.is_some() {
        score += 25;
    }
    if let Some(opacity) = entry.flags.opacity {
        if (opacity * 255.0).round() as i32 == layer.opacity {
            score += 25;
        }
    }
    if entry.flags.disabled == !layer.visible {
        score += 15;
    }
    if entry.flags.alpha == layer.alpha_inherit {
        score += 15;
    }
    score
}

fn collect_webps(dir: &Path, prefix: &Path, out: &mut Vec<ExportEntry>) -> Result<(), String> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    paths.sort();

    for path in paths {
        if path.is_dir() {
            let label = parse_label(&path_name(&path))?;
            let mut next = prefix.to_path_buf();
            if label.has_index {
                next.push(format!("{}#{}", label.name, label.index));
            } else {
                next.push(label.name);
            }
            collect_webps(&path, &next, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("webp") {
            let label = parse_label(&path_name(&path))?;
            let dirs = parse_prefix(prefix)?;
            let file = PathPart {
                name: label.name.clone(),
                index: label.index,
            };
            let asset_path = clean_asset_path(&dirs, &file);
            let bytes = fs::read(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let frame = webp_frame(&bytes);
            let canvas = webp_canvas(&bytes);
            out.push(ExportEntry {
                dirs,
                file,
                flags: label.flags,
                asset_path,
                bytes,
                frame,
                canvas,
            });
        }
    }
    Ok(())
}

fn parse_prefix(prefix: &Path) -> Result<Vec<PathPart>, String> {
    let mut parts = Vec::new();
    for component in prefix.components() {
        let text = component.as_os_str().to_string_lossy();
        let label = parse_label(&text)?;
        parts.push(PathPart {
            name: label.name,
            index: label.index,
        });
    }
    Ok(parts)
}

#[derive(Debug)]
struct Label {
    name: String,
    index: u32,
    has_index: bool,
    flags: Flags,
}

fn parse_label(raw: &str) -> Result<Label, String> {
    let stem = raw
        .strip_suffix(".webp")
        .or_else(|| raw.strip_suffix(".WEBP"))
        .unwrap_or(raw);
    let has_index = stem.contains('#');
    let (name, rest) = match stem.rfind('#') {
        Some(i) => (&stem[..i], &stem[i + 1..]),
        None => (stem, ""),
    };
    let (index_text, flags_text) = match rest.find('(') {
        Some(i) => (&rest[..i], rest[i + 1..].trim_end_matches(')')),
        None => (rest, ""),
    };
    let index = index_text.parse::<u32>().unwrap_or(u32::MAX);
    if name.is_empty() {
        return Err(format!("empty WebP layer name in {raw:?}"));
    }

    let mut flags = Flags::default();
    for token in flags_text.split('+').filter(|s| !s.is_empty()) {
        match token.to_ascii_lowercase().as_str() {
            "disabled" => flags.disabled = true,
            "alpha" => flags.alpha = true,
            "normal" => flags.blend = Some(BlendMode::Normal),
            "multiply" => flags.blend = Some(BlendMode::Multiply),
            "screen" => flags.blend = Some(BlendMode::Screen),
            "overlay" => flags.blend = Some(BlendMode::Overlay),
            "darken" => flags.blend = Some(BlendMode::Darken),
            "lighten" => flags.blend = Some(BlendMode::Lighten),
            "burn" => flags.blend = Some(BlendMode::Burn),
            "dodge" => flags.blend = Some(BlendMode::Dodge),
            "hard_light" | "hardlight" => flags.blend = Some(BlendMode::HardLight),
            "soft_light" | "softlight" => flags.blend = Some(BlendMode::SoftLight),
            "luminosity" | "luminize" => flags.blend = Some(BlendMode::Luminosity),
            other if other.ends_with('%') => {
                if let Ok(value) = other.trim_end_matches('%').parse::<f32>() {
                    flags.opacity = Some((value / 100.0).clamp(0.0, 1.0));
                }
            }
            _ => {}
        }
    }

    Ok(Label {
        name: name.to_owned(),
        index,
        has_index,
        flags,
    })
}

pub(crate) fn parse_blend(text: &str) -> Option<BlendMode> {
    match text.to_ascii_lowercase().as_str() {
        "normal" => Some(BlendMode::Normal),
        "multiply" => Some(BlendMode::Multiply),
        "screen" => Some(BlendMode::Screen),
        "overlay" => Some(BlendMode::Overlay),
        "darken" => Some(BlendMode::Darken),
        "lighten" => Some(BlendMode::Lighten),
        "color_burn" | "colorburn" => Some(BlendMode::ColorBurn),
        "color_dodge" | "colordodge" => Some(BlendMode::ColorDodge),
        "burn" => Some(BlendMode::Burn),
        "dodge" => Some(BlendMode::Dodge),
        "hard_light" | "hardlight" => Some(BlendMode::HardLight),
        "soft_light" | "softlight" => Some(BlendMode::SoftLight),
        "luminize" | "luminosity" => Some(BlendMode::Luminosity),
        _ => None,
    }
}

pub(crate) fn parse_maindoc(xml: &str) -> Result<(Canvas, Vec<KritaLayer>), String> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut canvas = Canvas::default();
    let mut layers = Vec::new();
    let mut groups: Vec<String> = Vec::new();
    let mut group_flags: Vec<bool> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"IMAGE" => {
                let attrs = attributes(&e)?;
                canvas.width = parse_u32(attrs.get("width"))?;
                canvas.height = parse_u32(attrs.get("height"))?;
                canvas.color_profile = attrs.get("profile").cloned();
            }
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"layer" => {
                let attrs = attributes(&e)?;
                if attrs.get("nodetype").map(String::as_str) == Some("referenceimages") {
                    continue;
                }
                let is_group = attrs.get("nodetype").map(String::as_str) == Some("grouplayer");
                let name = attrs
                    .get("name")
                    .cloned()
                    .unwrap_or_else(|| format!("layer{}", layers.len()));
                push_layer(&mut layers, &mut groups, &attrs, name.clone(), is_group);
                if is_group {
                    groups.push(name);
                    group_flags.push(true);
                } else {
                    group_flags.push(false);
                }
            }
            Ok(Event::Empty(e)) if e.local_name().as_ref() == b"layer" => {
                let attrs = attributes(&e)?;
                if attrs.get("nodetype").map(String::as_str) == Some("referenceimages") {
                    continue;
                }
                let is_group = attrs.get("nodetype").map(String::as_str) == Some("grouplayer");
                let name = attrs
                    .get("name")
                    .cloned()
                    .unwrap_or_else(|| format!("layer{}", layers.len()));
                push_layer(&mut layers, &mut groups, &attrs, name, is_group);
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == b"layer" => {
                if group_flags.pop().unwrap_or(false) {
                    groups.pop();
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("invalid maindoc.xml: {e}")),
            _ => {}
        }
    }
    if canvas.width == 0 || canvas.height == 0 {
        return Err("maindoc.xml has no usable IMAGE dimensions".to_owned());
    }
    Ok((canvas, layers))
}

fn push_layer(
    out: &mut Vec<KritaLayer>,
    groups: &[String],
    attrs: &BTreeMap<String, String>,
    name: String,
    is_group: bool,
) {
    let channel_flags = attrs.get("channelflags").cloned();
    out.push(KritaLayer {
        name,
        path: groups.join("/"),
        is_group,
        opacity: attrs
            .get("opacity")
            .and_then(|s| s.parse().ok())
            .unwrap_or(255),
        blend: attrs.get("compositeop").cloned().unwrap_or_else(|| "normal".to_owned()),
        visible: parse_bool(attrs.get("visible")).unwrap_or(true),
        alpha_inherit: channel_flags
            .as_deref()
            .map(|s| s.contains("1110"))
            .unwrap_or(false),
        file_name: attrs.get("filename").cloned(),
        uuid: attrs.get("uuid").cloned(),
        x: attrs.get("x").and_then(|s| s.parse().ok()),
        y: attrs.get("y").and_then(|s| s.parse().ok()),
        channel_flags,
        locked: parse_bool(attrs.get("locked")),
        color_label: attrs.get("colorlabel").and_then(|s| s.parse().ok()),
        in_timeline: parse_bool(attrs.get("intimeline")),
        pass_through: parse_bool(attrs.get("passthrough")),
    });
}

fn parse_bool(value: Option<&String>) -> Option<bool> {
    value.and_then(|s| match s.as_str() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    })
}

fn attributes(e: &quick_xml::events::BytesStart<'_>) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for attr in e.attributes() {
        let attr = attr.map_err(|e| format!("invalid XML attribute: {e}"))?;
        let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
        let value = attr
            .unescape_value()
            .map_err(|e| format!("invalid XML attribute value: {e}"))?
            .into_owned();
        out.insert(key, value);
    }
    Ok(out)
}

fn parse_u32(value: Option<&String>) -> Result<u32, String> {
    value
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| "missing or invalid numeric attribute".to_owned())
}

pub(crate) fn read_zip_text<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<String, String> {
    let bytes = read_zip_bytes(archive, name)?;
    String::from_utf8(bytes).map_err(|_| format!("{name} is not UTF-8"))
}

pub(crate) fn read_zip_bytes<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>, String> {
    let mut entry = archive
        .by_name(name)
        .map_err(|_| format!("{name} is missing from the KRA project"))?;
    let mut bytes = Vec::new();
    entry
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read {name}: {e}"))?;
    Ok(bytes)
}

pub(crate) fn png_to_webp(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .map_err(|e| format!("cannot decode PNG: {e}"))?
        .to_rgba8();
    let (w, h) = (image.width(), image.height());
    Ok(webp::Encoder::from_rgba(image.as_raw(), w, h)
        .encode_lossless()
        .to_vec())
}

/// Return the first WebP animation frame rectangle, if present.
pub fn webp_frame(bytes: &[u8]) -> Option<Rect> {
    each_webp_chunk(bytes, |fourcc, payload| {
        if fourcc == *b"ANMF" && payload.len() >= 16 {
            Some(Rect {
                x: read_u24(payload, 0) as i32,
                y: read_u24(payload, 3) as i32,
                w: (read_u24(payload, 6) + 1) as i32,
                h: (read_u24(payload, 9) + 1) as i32,
            })
        } else {
            None
        }
    })
}

/// Return the canvas size declared by VP8X, if present.
pub fn webp_canvas(bytes: &[u8]) -> Option<(u32, u32)> {
    each_webp_chunk(bytes, |fourcc, payload| {
        if fourcc == *b"VP8X" && payload.len() >= 10 {
            Some((
                read_u24(payload, 4) + 1,
                read_u24(payload, 7) + 1,
            ))
        } else {
            None
        }
    })
}

fn each_webp_chunk<T>(
    bytes: &[u8],
    mut visit: impl FnMut([u8; 4], &[u8]) -> Option<T>,
) -> Option<T> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    let mut pos = 12usize;
    while pos + 8 <= bytes.len() {
        let mut fourcc = [0u8; 4];
        fourcc.copy_from_slice(&bytes[pos..pos + 4]);
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let start = pos + 8;
        let end = start.saturating_add(size).min(bytes.len());
        if let Some(value) = visit(fourcc, &bytes[start..end]) {
            return Some(value);
        }
        pos = end + (size & 1);
    }
    None
}

fn read_u24(bytes: &[u8], offset: usize) -> u32 {
    if offset + 3 > bytes.len() {
        return 0;
    }
    u32::from(bytes[offset])
        | (u32::from(bytes[offset + 1]) << 8)
        | (u32::from(bytes[offset + 2]) << 16)
}

fn normalize(text: &str) -> String {
    text.to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// Build a stable, readable asset path. The source `#index(flags)` namespace
/// is not carried into the package; the index is recorded as `Node::order`.
fn clean_asset_path(dirs: &[PathPart], file: &PathPart) -> String {
    let mut parts = vec!["assets".to_owned()];
    parts.extend(dirs.iter().map(asset_component));
    parts.push(asset_component(file));
    format!("{}.webp", parts.join("/"))
}

fn asset_component(part: &PathPart) -> String {
    let slug = slug(&part.name);
    if part.index == u32::MAX {
        slug
    } else {
        format!("{:02}-{}", part.index, slug)
    }
}

pub(crate) fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
            pending_dash = false;
        } else {
            pending_dash = true;
        }
    }
    if out.is_empty() {
        "layer".to_owned()
    } else {
        out
    }
}

fn dedupe_asset_paths(entries: &mut [ExportEntry]) {
    let mut used = BTreeSet::new();
    for entry in entries {
        let base = entry.asset_path.clone();
        let stem = base.strip_suffix(".webp").unwrap_or(&base);
        let mut candidate = base.clone();
        let mut suffix = 2;
        while !used.insert(candidate.clone()) {
            candidate = format!("{stem}-{suffix}.webp");
            suffix += 1;
        }
        entry.asset_path = candidate;
    }
}

fn layer_id(asset_path: &str) -> String {
    let stem = asset_path
        .strip_prefix("assets/")
        .unwrap_or(asset_path)
        .trim_end_matches(".webp");
    format!("layer_{}", stem.replace('/', "_").replace('-', "_"))
}

fn group_id(prefix: &[PathPart], part: &PathPart) -> String {
    let mut parts: Vec<String> = prefix.iter().map(|p| asset_component(p)).collect();
    parts.push(asset_component(part));
    format!("group_{}", parts.join("_"))
}

fn path_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn file_name(path: &Path) -> String {
    path_name(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_export_filenames() {
        let label = parse_label("Dress-shading#3(61%).webp").unwrap();
        assert_eq!(label.name, "Dress-shading");
        assert_eq!(label.index, 3);
        assert_eq!(label.flags.opacity, Some(0.61));

        let label = parse_label("CoverShading#3(alpha+overlay).webp").unwrap();
        assert!(label.flags.alpha);
        assert_eq!(label.flags.blend, Some(BlendMode::Overlay));

        let label = parse_label("Sweater-lineart#4(disabled).webp").unwrap();
        assert!(label.flags.disabled);

        let label = parse_label("Table").unwrap();
        assert_eq!(label.name, "Table");
        assert!(!label.has_index);
    }

    #[test]
    fn emits_clean_asset_paths_without_the_temporary_namespace() {
        let dirs = vec![PathPart {
            name: "Dress-coloring".to_owned(),
            index: 5,
        }];
        let label = parse_label("Dress-shading#10(burn).webp").unwrap();
        let file = PathPart {
            name: label.name,
            index: label.index,
        };
        assert_eq!(
            clean_asset_path(&dirs, &file),
            "assets/05-dress-coloring/10-dress-shading.webp"
        );

        let table = vec![PathPart {
            name: "Table".to_owned(),
            index: u32::MAX,
        }];
        let label = parse_label("Table#1().webp").unwrap();
        let file = PathPart {
            name: label.name,
            index: label.index,
        };
        assert_eq!(
            clean_asset_path(&table, &file),
            "assets/table/01-table.webp"
        );
    }

    #[test]
    fn parses_krita_layer_metadata() {
        let xml = r#"
            <DOC><IMAGE width="64" height="32" profile="sRGB">
            <layers>
              <layer nodetype="grouplayer" name="Group" opacity="255" compositeop="normal" visible="1">
                <layers>
                  <layer nodetype="paintlayer" name="Shading" opacity="156" compositeop="burn" visible="0" channelflags="1110"/>
                </layers>
              </layer>
            </layers></IMAGE></DOC>
        "#;
        let (canvas, layers) = parse_maindoc(xml).unwrap();
        assert_eq!((canvas.width, canvas.height), (64, 32));
        assert_eq!(layers.len(), 2);
        assert!(layers[0].is_group);
        assert_eq!(layers[1].name, "Shading");
        assert_eq!(layers[1].opacity, 156);
        assert_eq!(layers[1].blend, "burn");
        assert!(!layers[1].visible);
        assert!(layers[1].alpha_inherit);
    }

    #[test]
    fn reads_animation_frame_rectangle() {
        let mut webp = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
        let mut anmf = vec![0u8; 24];
        anmf[0..4].copy_from_slice(b"ANMF");
        anmf[4..8].copy_from_slice(&16u32.to_le_bytes());
        anmf[8] = 10;
        anmf[11] = 20;
        anmf[14] = 30;
        anmf[17] = 40;
        webp.extend_from_slice(&anmf);
        let rect = webp_frame(&webp).unwrap();
        assert_eq!(rect, Rect { x: 10, y: 20, w: 31, h: 41 });
    }

    #[test]
    fn sibling_groups_do_not_duplicate_their_children() {
        fn entry(dirs: &[(&str, u32)], name: &str, index: u32) -> ExportEntry {
            let asset_path = format!("assets/{name}#{index}.webp");
            ExportEntry {
                dirs: dirs
                    .iter()
                    .map(|(name, index)| PathPart {
                        name: (*name).to_owned(),
                        index: *index,
                    })
                    .collect(),
                file: PathPart {
                    name: name.to_owned(),
                    index,
                },
                flags: Flags::default(),
                asset_path,
                bytes: Vec::new(),
                frame: None,
                canvas: None,
            }
        }

        let entries = vec![
            entry(&[("A", 0), ("InnerA", 0)], "One", 1),
            entry(&[("B", 1), ("InnerB", 0)], "Two", 1),
        ];
        let mut report = ConversionReport::default();
        let mut used = BTreeSet::new();
        let nodes = build_nodes(
            &entries,
            0,
            &[],
            &[],
            &mut used,
            &ConvertOptions::default(),
            &mut report,
        )
        .unwrap();

        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].name, "A");
        assert_eq!(nodes[0].order, 0);
        assert_eq!(nodes[1].name, "B");
        assert_eq!(nodes[1].order, 1);
        assert_eq!(nodes[0].children.len(), 1);
        assert_eq!(nodes[1].children.len(), 1);
        assert_eq!(nodes[0].children[0].name, "InnerA");
        assert_eq!(nodes[1].children[0].name, "InnerB");
        assert_eq!(nodes[0].children[0].children.len(), 1);
        assert_eq!(nodes[1].children[0].children.len(), 1);
        assert_eq!(nodes[0].children[0].children[0].name, "One");
        assert_eq!(nodes[1].children[0].children[0].name, "Two");
    }

    #[test]
    fn unindexed_groups_sort_after_numbered_siblings() {
        fn entry(dirs: &[(&str, u32)], name: &str, index: u32) -> ExportEntry {
            ExportEntry {
                dirs: dirs
                    .iter()
                    .map(|(name, index)| PathPart {
                        name: (*name).to_owned(),
                        index: *index,
                    })
                    .collect(),
                file: PathPart {
                    name: name.to_owned(),
                    index,
                },
                flags: Flags::default(),
                asset_path: format!("assets/{name}#{index}.webp"),
                bytes: Vec::new(),
                frame: None,
                canvas: None,
            }
        }

        let entries = vec![
            entry(&[], "Base", 1),
            entry(&[("Extra", u32::MAX)], "Thing", 0),
        ];
        let mut report = ConversionReport::default();
        let mut used = BTreeSet::new();
        let nodes = build_nodes(
            &entries,
            0,
            &[],
            &[],
            &mut used,
            &ConvertOptions::default(),
            &mut report,
        )
        .unwrap();

        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].name, "Base");
        assert_eq!(nodes[0].order, 1);
        assert_eq!(nodes[1].name, "Extra");
        assert_eq!(nodes[1].order, 2);
    }

    #[test]
    fn krita_matching_prefers_the_best_candidate() {
        let entry = ExportEntry {
            dirs: Vec::new(),
            file: PathPart {
                name: "Shading".to_owned(),
                index: 0,
            },
            flags: Flags::default(),
            asset_path: "assets/Shading#0().webp".to_owned(),
            bytes: Vec::new(),
            frame: None,
            canvas: None,
        };
        let krita = vec![
            KritaLayer {
                name: "Paint Layer 1".to_owned(),
                opacity: 255,
                blend: "normal".to_owned(),
                visible: true,
                ..KritaLayer::default()
            },
            KritaLayer {
                name: "Shading".to_owned(),
                opacity: 156,
                blend: "burn".to_owned(),
                visible: true,
                ..KritaLayer::default()
            },
        ];
        let mut used = BTreeSet::new();
        let mut report = ConversionReport::default();
        let node = build_layer(
            &entry,
            &krita,
            &mut used,
            &ConvertOptions::default(),
            &mut report,
        );

        assert_eq!(node.opacity, 156.0 / 255.0);
        assert_eq!(node.blend, BlendMode::Burn);
    }

    #[test]
    fn blend_tokens_cover_fixture_flags() {
        assert_eq!(parse_blend("luminize"), Some(BlendMode::Luminosity));
        assert_eq!(parse_blend("burn"), Some(BlendMode::Burn));
        assert_eq!(parse_blend("overlay"), Some(BlendMode::Overlay));
    }
}
