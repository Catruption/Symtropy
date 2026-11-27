//! Direct KRA reader for `.ssvg`.
//!
//! Krita stores each paint layer as a set of 64x64 tiles in a binary file under
//! `<document>/layers/<filename>`. The header is text; each tile has a text
//! record followed by a flag byte and either raw or LZF-compressed pixel data.
//! RGBA/8-bit tiles are stored in BGRA byte planes.
//!
//! This module decodes those tiles straight from the `.kra`, so layers that
//! were never separately exported still become real `.ssvg` assets. The XML
//! layer tree and metadata remain the source of truth.

use crate::convert::{
    png_to_webp, read_zip_bytes, read_zip_text, slug, ConversionReport, ConvertOptions, KritaLayer,
};
use crate::{Asset, BlendMode, Manifest, Node, Package, Rect, Scene};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Cursor;
use std::path::Path;

/// Convert a `.kra` straight into a `.ssvg` package, no WebP export step.
pub fn convert_kra_direct(kra: &Path, output: &Path) -> Result<ConversionReport, String> {
    let (package, report) = build_package_direct_with_options(kra, ConvertOptions::default())?;
    let mut file = fs::File::create(output)
        .map_err(|e| format!("cannot create {}: {e}", output.display()))?;
    package
        .write_to(&mut file)
        .map_err(|e| format!("cannot write {}: {e}", output.display()))?;
    Ok(report)
}

/// Build a package directly from a `.kra` with default options.
pub fn build_package_direct(kra: &Path) -> Result<(Package, ConversionReport), String> {
    build_package_direct_with_options(kra, ConvertOptions::default())
}

/// Build a package directly from a `.kra`.
///
/// `ConvertOptions::use_krita_metadata` is always honoured; the direct reader
/// has no export filenames to fall back on. `crop_frames` is unused because
/// the decoded tile bounds are already the frame.
pub fn build_package_direct_with_options(
    kra: &Path,
    _options: ConvertOptions,
) -> Result<(Package, ConversionReport), String> {
    let archive_bytes = fs::read(kra).map_err(|e| format!("cannot read {}: {e}", kra.display()))?;
    let mut archive = zip::ZipArchive::new(Cursor::new(archive_bytes))
        .map_err(|e| format!("cannot open {}: {e}", kra.display()))?;

    let xml = read_zip_text(&mut archive, "maindoc.xml")?;
    let (canvas, krita_layers) = crate::convert::parse_maindoc(&xml)?;
    let preview_png = read_zip_bytes(&mut archive, "preview.png")?;
    let merged_png = read_zip_bytes(&mut archive, "mergedimage.png")?;

    let names: Vec<String> = archive.file_names().map(str::to_owned).collect();
    let name_set: BTreeSet<String> = names.iter().cloned().collect();
    let layer_entries: BTreeMap<String, String> = names
        .iter()
        .filter_map(|name| {
            let file = name.rsplit("/layers/").next()?;
            if file.is_empty() || name.ends_with(".defaultpixel") || name.ends_with(".icc") {
                return None;
            }
            Some((file.to_owned(), name.clone()))
        })
        .collect();

    let mut decoded: BTreeMap<usize, DecodedLayer> = BTreeMap::new();
    let mut assets: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut manifest_assets = Vec::new();
    let mut groups = 0;

    for (index, layer) in krita_layers.iter().enumerate() {
        if layer.is_group {
            groups += 1;
            continue;
        }
        let Some(file_name) = layer.file_name.as_deref() else {
            continue;
        };
        let entry = layer_entries.get(file_name).ok_or_else(|| {
            format!("maindoc.xml references missing pixel data {file_name}")
        })?;
        let bytes = read_zip_bytes(&mut archive, entry)?;
        let default_pixel = read_default_pixel(&mut archive, &name_set, entry)?;
        let order = sibling_order(&krita_layers, index);
        let path = asset_path(&parse_path(&layer.path), order, &layer.name);
        let pixels = decode_layer(&bytes, canvas.width, canvas.height, default_pixel)?;
        manifest_assets.push(Asset {
            path: path.clone(),
            width: pixels.width,
            height: pixels.height,
            bytes: pixels.bytes.len() as u64,
        });
        assets.insert(path.clone(), pixels.bytes);
        decoded.insert(
            index,
            DecodedLayer {
                path,
                crop: pixels.crop,
                offset: pixels.offset,
            },
        );
    }
    drop(archive);

    let layers = build_nodes(&krita_layers, &decoded, &[]);
    manifest_assets.sort_by(|a, b| a.path.cmp(&b.path));

    let mut manifest = Manifest::default();
    manifest.canvas = canvas;
    manifest.generator = Some(format!(
        "ssvg direct KRA converter for {}",
        Path::new(kra)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    manifest.assets = manifest_assets;

    let mut package = Package::new(
        manifest,
        Scene {
            layers,
            ..Scene::default()
        },
        png_to_webp(&preview_png)?,
        png_to_webp(&merged_png)?,
    );
    package.assets = assets;
    package.validate()?;

    Ok((
        package,
        ConversionReport {
            groups,
            layers: decoded.len(),
            matched_krita_layers: decoded.len(),
            warnings: Vec::new(),
        },
    ))
}

struct DecodedLayer {
    path: String,
    crop: Rect,
    offset: [i32; 2],
}

struct Pixels {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    crop: Rect,
    offset: [i32; 2],
}

struct TileHeader {
    x: i32,
    y: i32,
    size: usize,
    offset: usize,
}

fn parse_path(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

fn parse_key_value(line: &str) -> Result<(&str, u32), String> {
    let (key, rest) = line.split_once(' ').ok_or("bad tile header line")?;
    let value = rest.trim();
    Ok((key, value.parse().map_err(|_| format!("bad {key} value {value}"))?))
}

fn next_line<'a>(data: &'a [u8], pos: &mut usize) -> Result<&'a [u8], String> {
    if *pos > data.len() {
        return Err("tile file ends unexpectedly".to_owned());
    }
    let end = data[*pos..]
        .iter()
        .position(|b| *b == b'\n')
        .map(|i| *pos + i)
        .ok_or("unterminated tile header line")?;
    let line = &data[*pos..end];
    *pos = end + 1;
    Ok(line)
}

fn parse_tile_headers(data: &[u8]) -> Result<(u32, u32, usize, Vec<TileHeader>), String> {
    let mut pos = 0;
    let version = parse_key_value(std::str::from_utf8(next_line(data, &mut pos)?).map_err(|e| e.to_string())?)?;
    if version.0 != "VERSION" || version.1 != 2 {
        return Err(format!("unsupported KRA tile version {}", version.1));
    }

    let mut tile_width = 0;
    let mut tile_height = 0;
    let mut pixel_size = 0;
    let count;
    loop {
        let line = std::str::from_utf8(next_line(data, &mut pos)?).map_err(|e| e.to_string())?;
        let (key, value) = parse_key_value(line)?;
        match key {
            "TILEWIDTH" => tile_width = value,
            "TILEHEIGHT" => tile_height = value,
            "PIXELSIZE" => pixel_size = value as usize,
            "DATA" => {
                count = value;
                break;
            }
            other => return Err(format!("unexpected tile header {other}")),
        }
    }
    if tile_width == 0 || tile_height == 0 {
        return Err("tile dimensions must be positive".to_owned());
    }
    if pixel_size != 4 {
        return Err(format!(
            "only RGBA/8-bit KRA layers are supported (PIXELSIZE {pixel_size})"
        ));
    }

    let mut tiles = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let line = std::str::from_utf8(next_line(data, &mut pos)?).map_err(|e| e.to_string())?;
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() != 4 {
            return Err(format!("bad tile record {line}"));
        }
        let x: i32 = fields[0].parse().map_err(|_| format!("bad tile x {}", fields[0]))?;
        let y: i32 = fields[1].parse().map_err(|_| format!("bad tile y {}", fields[1]))?;
        let size: usize = fields[3]
            .parse()
            .map_err(|_| format!("bad tile size {}", fields[3]))?;
        if pos.checked_add(size).map_or(true, |end| end > data.len()) {
            return Err("tile payload exceeds layer data".to_owned());
        }
        tiles.push(TileHeader {
            x,
            y,
            size,
            offset: pos,
        });
        pos += size;
    }
    Ok((tile_width, tile_height, pixel_size, tiles))
}

fn read_default_pixel<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    names: &BTreeSet<String>,
    entry: &str,
) -> Result<Option<[u8; 4]>, String> {
    let path = format!("{entry}.defaultpixel");
    if !names.contains(&path) {
        return Ok(None);
    }
    let bytes = read_zip_bytes(archive, &path)?;
    if bytes.len() < 4 {
        return Ok(None);
    }
    // Krita stores the default pixel in BGRA order, like tile data.
    Ok(Some([bytes[2], bytes[1], bytes[0], bytes[3]]))
}

fn decode_layer(
    data: &[u8],
    canvas_width: u32,
    canvas_height: u32,
    default_pixel: Option<[u8; 4]>,
) -> Result<Pixels, String> {
    let (tile_width, tile_height, pixel_size, tiles) = parse_tile_headers(data)?;
    let tile_w = tile_width as i32;
    let tile_h = tile_height as i32;
    let canvas_w = canvas_width as i32;
    let canvas_h = canvas_height as i32;

    let opaque_default = default_pixel.map_or(false, |pixel| pixel[3] != 0);
    let (min_x, min_y, max_x, max_y) = if opaque_default {
        (0, 0, canvas_w, canvas_h)
    } else {
        let mut min_x = canvas_w;
        let mut min_y = canvas_h;
        let mut max_x = 0;
        let mut max_y = 0;
        for tile in &tiles {
            min_x = min_x.min(tile.x.max(0));
            min_y = min_y.min(tile.y.max(0));
            max_x = max_x.max((tile.x + tile_w).min(canvas_w));
            max_y = max_y.max((tile.y + tile_h).min(canvas_h));
        }
        (min_x, min_y, max_x, max_y)
    };
    if min_x >= max_x || min_y >= max_y {
        return Ok(transparent_pixel());
    }

    let width = (max_x - min_x) as u32;
    let height = (max_y - min_y) as u32;
    let mut rgba = vec![0u8; width as usize * height as usize * pixel_size];
    if let Some(pixel) = default_pixel {
        for chunk in rgba.chunks_exact_mut(4) {
            chunk.copy_from_slice(&pixel);
        }
    }

    for tile in &tiles {
        let payload = &data[tile.offset..tile.offset + tile.size];
        let bgra = tile_pixels(payload, tile_width, tile_height, pixel_size)?;
        let sx0 = (min_x - tile.x).max(0);
        let sy0 = (min_y - tile.y).max(0);
        let sx1 = (max_x - tile.x).min(tile_w);
        let sy1 = (max_y - tile.y).min(tile_h);
        if sx0 >= sx1 || sy0 >= sy1 {
            continue;
        }
        for y in sy0..sy1 {
            for x in sx0..sx1 {
                let src = ((y * tile_w + x) as usize) * 4;
                let dst = (((tile.y + y - min_y) * width as i32 + tile.x + x - min_x) as usize) * 4;
                rgba[dst] = bgra[src + 2];
                rgba[dst + 1] = bgra[src + 1];
                rgba[dst + 2] = bgra[src];
                rgba[dst + 3] = bgra[src + 3];
            }
        }
    }

    Ok(trim_and_encode(&rgba, width, height, min_x, min_y))
}

fn tile_pixels(payload: &[u8], width: u32, height: u32, pixel_size: usize) -> Result<Vec<u8>, String> {
    let expected = width as usize * height as usize * pixel_size;
    if payload.is_empty() {
        return Err("empty tile payload".to_owned());
    }
    let raw = match payload[0] {
        0 => &payload[1..],
        1 => return lzf_decompress(&payload[1..], expected),
        flag => return Err(format!("unsupported tile compression flag {flag}")),
    };
    if raw.len() != expected {
        return Err(format!(
            "tile payload has {} bytes, expected {expected}",
            raw.len()
        ));
    }
    Ok(raw.to_vec())
}

/// liblzf decoder: literal runs below 32, otherwise overlapping back references.
fn lzf_decompress(input: &[u8], expected: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(expected);
    let mut ip = 0;
    while ip < input.len() {
        let control = input[ip] as usize;
        ip += 1;
        if control < 32 {
            let len = control + 1;
            let end = ip
                .checked_add(len)
                .filter(|end| *end <= input.len() && out.len() + len <= expected)
                .ok_or("truncated LZF literal run")?;
            out.extend_from_slice(&input[ip..end]);
            ip = end;
        } else {
            let mut len = control >> 5;
            let mut reference = out
                .len()
                .checked_sub(((control & 0x1f) << 8) + 1)
                .ok_or("invalid LZF back reference")?;
            if len == 7 {
                let extra = *input.get(ip).ok_or("truncated LZF length")? as usize;
                ip += 1;
                len += extra;
            }
            let low = *input.get(ip).ok_or("truncated LZF offset")? as usize;
            ip += 1;
            reference = reference.checked_sub(low).ok_or("invalid LZF offset")?;
            len += 2;
            if reference >= out.len() || out.len() + len > expected {
                return Err("LZF back reference exceeds output".to_owned());
            }
            for _ in 0..len {
                let byte = out[reference];
                out.push(byte);
                reference += 1;
            }
        }
    }
    if out.len() != expected {
        return Err(format!(
            "LZF decoded {} bytes, expected {expected}",
            out.len()
        ));
    }
    Ok(out)
}

fn trim_and_encode(
    rgba: &[u8],
    width: u32,
    height: u32,
    origin_x: i32,
    origin_y: i32,
) -> Pixels {
    let mut x0 = width;
    let mut y0 = height;
    let mut x1 = 0;
    let mut y1 = 0;
    for y in 0..height {
        for x in 0..width {
            if rgba[((y * width + x) as usize) * 4 + 3] != 0 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x + 1);
                y1 = y1.max(y + 1);
            }
        }
    }
    if x0 >= x1 || y0 >= y1 {
        return transparent_pixel();
    }

    let crop_width = x1 - x0;
    let crop_height = y1 - y0;
    let mut cropped = Vec::with_capacity(crop_width as usize * crop_height as usize * 4);
    for y in y0..y1 {
        let start = ((y * width + x0) as usize) * 4;
        let end = ((y * width + x1) as usize) * 4;
        cropped.extend_from_slice(&rgba[start..end]);
    }
    let bytes = webp::Encoder::from_rgba(&cropped, crop_width, crop_height)
        .encode_lossless()
        .to_vec();
    Pixels {
        bytes,
        width: crop_width,
        height: crop_height,
        crop: Rect {
            x: 0,
            y: 0,
            w: crop_width as i32,
            h: crop_height as i32,
        },
        offset: [origin_x + x0 as i32, origin_y + y0 as i32],
    }
}

fn transparent_pixel() -> Pixels {
    let bytes = webp::Encoder::from_rgba(&[0, 0, 0, 0], 1, 1)
        .encode_lossless()
        .to_vec();
    Pixels {
        bytes,
        width: 1,
        height: 1,
        crop: Rect {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
        },
        offset: [0, 0],
    }
}

fn asset_path(prefix: &[String], order: u32, name: &str) -> String {
    let mut parts = vec!["assets".to_owned()];
    parts.extend(prefix.iter().map(|part| slug(part)));
    parts.push(format!("{:02}-{}", order, slug(name)));
    format!("{}.webp", parts.join("/"))
}

fn sibling_order(layers: &[KritaLayer], index: usize) -> u32 {
    let parent = parse_path(&layers[index].path);
    layers[..index]
        .iter()
        .filter(|layer| parse_path(&layer.path) == parent)
        .count() as u32
}

fn build_nodes(
    layers: &[KritaLayer],
    decoded: &BTreeMap<usize, DecodedLayer>,
    prefix: &[String],
) -> Vec<Node> {
    let members: Vec<usize> = layers
        .iter()
        .enumerate()
        .filter(|(_, layer)| parse_path(&layer.path) == prefix)
        .map(|(index, _)| index)
        .collect();

    // maindoc.xml lists layers top-first; sibling arrays are bottom-first.
    let mut nodes = Vec::with_capacity(members.len());
    for (order, index) in members.into_iter().rev().enumerate() {
        let layer = &layers[index];
        let mut node = if layer.is_group {
            let mut group = Node::group(format!("group_{}", index), layer.name.clone());
            let mut child_prefix = prefix.to_vec();
            child_prefix.push(layer.name.clone());
            group.children = build_nodes(layers, decoded, &child_prefix);
            group
        } else if let Some(pixels) = decoded.get(&index) {
            let mut node = Node::layer(format!("layer_{}", index), layer.name.clone(), pixels.path.clone());
            node.crop = Some(pixels.crop);
            node.transform.offset = pixels.offset;
            node
        } else {
            continue;
        };
        node.order = order as u32;
        node.visible = layer.visible;
        node.opacity = (layer.opacity as f32 / 255.0).clamp(0.0, 1.0);
        node.blend = crate::convert::parse_blend(&layer.blend).unwrap_or(BlendMode::Normal);
        node.alpha_inherit = layer.alpha_inherit;
        node.pass_through = layer.pass_through.unwrap_or(false);
        node.source = Some(layer.source(&node.name));
        nodes.push(node);
    }
    nodes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lzf_decodes_literal_runs_and_back_references() {
        // 3 literal bytes, then a 4-byte back reference to offset -3.
        let input = [2, b'a', b'b', b'c', (2 << 5), 2];
        let decoded = lzf_decompress(&input, 7).unwrap();
        assert_eq!(decoded, b"abcabca");
    }

    #[test]
    fn lzf_rejects_short_output() {
        assert!(lzf_decompress(&[0, b'a'], 2).is_err());
    }

    #[test]
    fn tile_payload_honours_the_raw_flag() {
        let raw = vec![7u8; 16];
        let mut payload = vec![0];
        payload.extend_from_slice(&raw);
        assert_eq!(tile_pixels(&payload, 2, 2, 4).unwrap(), raw);
    }

    #[test]
    fn tile_header_parses_the_krita_header() {
        let data = b"VERSION 2\nTILEWIDTH 64\nTILEHEIGHT 64\nPIXELSIZE 4\nDATA 1\n3,-4,LZF,9\n012345678";
        let (w, h, pixel, tiles) = parse_tile_headers(data).unwrap();
        assert_eq!((w, h, pixel), (64, 64, 4));
        assert_eq!(tiles.len(), 1);
        assert_eq!((tiles[0].x, tiles[0].y, tiles[0].size), (3, -4, 9));
    }

    #[test]
    fn siblings_are_reversed_from_xml_order() {
        let base = KritaLayer {
            name: "Top".to_owned(),
            opacity: 255,
            blend: "normal".to_owned(),
            visible: true,
            file_name: Some("layer1".to_owned()),
            ..KritaLayer::default()
        };
        let layers = vec![
            base.clone(),
            KritaLayer {
                name: "Bottom".to_owned(),
                ..base
            },
        ];
        assert_eq!(sibling_order(&layers, 0), 0);
        assert_eq!(sibling_order(&layers, 1), 1);
    }
}
