//! Structural validation for SuperSVG packages.

use crate::{Manifest, Node, Package, Scene};

pub fn validate_package(package: &Package) -> Result<(), String> {
    validate_manifest(&package.manifest)?;
    validate_scene(&package.scene)?;

    if package.preview.is_empty() {
        return Err("preview.webp is missing".to_owned());
    }
    if !is_webp(&package.preview) {
        return Err("preview.webp is not a WebP payload".to_owned());
    }
    if package.merged.is_empty() {
        return Err("merged.webp is missing".to_owned());
    }
    if !is_webp(&package.merged) {
        return Err("merged.webp is not a WebP payload".to_owned());
    }

    let mut declared: Vec<&str> = package
        .manifest
        .assets
        .iter()
        .map(|a| a.path.as_str())
        .collect();
    declared.sort_unstable();
    if declared.windows(2).any(|w| w[0] == w[1]) {
        return Err("duplicate asset path in manifest".to_owned());
    }

    let mut asset_sizes = std::collections::BTreeMap::new();
    for asset in &package.manifest.assets {
        let payload = package
            .assets
            .get(&asset.path)
            .ok_or_else(|| format!("asset {} is not in the package", asset.path))?;
        if payload.is_empty() || !is_webp(payload) {
            return Err(format!("asset {} is not a WebP payload", asset.path));
        }
        if asset.bytes != 0 && asset.bytes != payload.len() as u64 {
            return Err(format!(
                "asset {} declares {} bytes but contains {}",
                asset.path,
                asset.bytes,
                payload.len()
            ));
        }
        asset_sizes.insert(asset.path.as_str(), (asset.width, asset.height));
    }

    for path in package.assets.keys() {
        if !declared.binary_search(&path.as_str()).is_ok() {
            return Err(format!("undeclared asset {} in package", path));
        }
    }

    let required = required_paths(&package.manifest);
    for path in package.extras.keys() {
        if required.contains(path) {
            return Err(format!("extra entry collides with required path {}", path));
        }
    }

    validate_references(&package.scene, &asset_sizes)?;
    Ok(())
}

pub fn validate_manifest(manifest: &Manifest) -> Result<(), String> {
    if manifest.format != "SuperSVG" {
        return Err(format!("unsupported format {}", manifest.format));
    }
    if manifest.version != crate::FORMAT_VERSION {
        return Err(format!(
            "unsupported version {} (expected {})",
            manifest.version,
            crate::FORMAT_VERSION
        ));
    }
    if manifest.canvas.width == 0 || manifest.canvas.height == 0 {
        return Err("canvas dimensions must be positive".to_owned());
    }
    if manifest.preview.is_empty() || manifest.merged.is_empty() {
        return Err("preview and merged paths must be set".to_owned());
    }
    for asset in &manifest.assets {
        validate_asset_path(&asset.path)?;
    }
    Ok(())
}

pub fn validate_scene(scene: &Scene) -> Result<(), String> {
    let mut ids = std::collections::BTreeSet::new();
    for layer in &scene.layers {
        validate_node(layer, &mut ids)?;
    }
    validate_sibling_order(&scene.layers)?;

    let mut variant_ids = std::collections::BTreeSet::new();
    for variant in &scene.variants {
        if variant.id.is_empty() {
            return Err("variant id must not be empty".to_owned());
        }
        if !variant_ids.insert(variant.id.clone()) {
            return Err(format!("duplicate variant id {}", variant.id));
        }
        validate_params(&variant.params)?;
    }

    if let Some(timeline) = &scene.timeline {
        for track in &timeline.tracks {
            if track.target.is_empty() {
                return Err("timeline track target must not be empty".to_owned());
            }
            let mut last = None;
            for keyframe in &track.keyframes {
                if keyframe.time_ms > timeline.duration_ms {
                    return Err(format!(
                        "keyframe at {}ms is past timeline duration {}ms",
                        keyframe.time_ms, timeline.duration_ms
                    ));
                }
                if let Some(prev) = last {
                    if keyframe.time_ms < prev {
                        return Err("timeline keyframes must be ordered by time".to_owned());
                    }
                }
                last = Some(keyframe.time_ms);
                validate_params(&keyframe.params)?;
            }
        }
    }
    Ok(())
}

fn validate_node(node: &Node, ids: &mut std::collections::BTreeSet<String>) -> Result<(), String> {
    if node.id.is_empty() {
        return Err("node id must not be empty".to_owned());
    }
    if !ids.insert(node.id.clone()) {
        return Err(format!("duplicate node id {}", node.id));
    }
    if !node.opacity.is_finite() || !(0.0..=1.0).contains(&node.opacity) {
        return Err(format!("node {} has invalid opacity {}", node.id, node.opacity));
    }
    for s in node.transform.scale {
        if !s.is_finite() || s <= 0.0 {
            return Err(format!("node {} has invalid scale {}", node.id, s));
        }
    }
    if !node.transform.rotation.is_finite() {
        return Err(format!("node {} has invalid rotation", node.id));
    }
    if let Some(crop) = node.crop {
        if crop.w <= 0 || crop.h <= 0 {
            return Err(format!("node {} has an empty crop", node.id));
        }
    }
    validate_params(&node.params)?;
    for child in &node.children {
        validate_node(child, ids)?;
    }
    Ok(())
}

/// Sibling arrays are canonical paint order, so `order` must not go backwards.
fn validate_sibling_order(nodes: &[Node]) -> Result<(), String> {
    let mut last = None;
    for node in nodes {
        if let Some(previous) = last {
            if node.order < previous {
                return Err(format!(
                    "node {} has order {} after order {}",
                    node.id, node.order, previous
                ));
            }
        }
        last = Some(node.order);
        validate_sibling_order(&node.children)?;
    }
    Ok(())
}

fn validate_references(
    scene: &Scene,
    assets: &std::collections::BTreeMap<&str, (u32, u32)>,
) -> Result<(), String> {
    let mut ids = std::collections::BTreeSet::new();
    for layer in &scene.layers {
        collect_ids(layer, &mut ids);
    }
    let variants: std::collections::BTreeSet<&str> = scene
        .variants
        .iter()
        .map(|v| v.id.as_str())
        .collect();

    fn walk(
        node: &Node,
        ids: &std::collections::BTreeSet<String>,
        variants: &std::collections::BTreeSet<&str>,
        assets: &std::collections::BTreeMap<&str, (u32, u32)>,
    ) -> Result<(), String> {
        match node.kind {
            crate::NodeKind::Layer => {
                let path = node
                    .asset
                    .as_deref()
                    .ok_or_else(|| format!("layer {} has no asset", node.id))?;
                let size = assets
                    .get(path)
                    .ok_or_else(|| format!("layer {} references unknown asset {}", node.id, path))?;
                if let Some(crop) = node.crop {
                    if crop.x < 0
                        || crop.y < 0
                        || crop.x.saturating_add(crop.w) > size.0 as i32
                        || crop.y.saturating_add(crop.h) > size.1 as i32
                    {
                        return Err(format!("layer {} crop is outside its asset", node.id));
                    }
                }
            }
            crate::NodeKind::Reference => {
                if let Some(target) = node.asset.as_deref() {
                    if !ids.contains(target) && !variants.contains(target) {
                        return Err(format!(
                            "reference {} points to unknown target {}",
                            node.id, target
                        ));
                    }
                }
            }
            crate::NodeKind::Group => {}
        }
        for child in &node.children {
            walk(child, ids, variants, assets)?;
        }
        Ok(())
    }

    for layer in &scene.layers {
        walk(layer, &ids, &variants, assets)?;
    }

    if let Some(timeline) = &scene.timeline {
        for track in &timeline.tracks {
            if track.target != "scene"
                && !ids.contains(&track.target)
                && !variants.contains(track.target.as_str())
            {
                return Err(format!(
                    "timeline track points to unknown target {}",
                    track.target
                ));
            }
        }
    }
    Ok(())
}

fn collect_ids(node: &Node, ids: &mut std::collections::BTreeSet<String>) {
    ids.insert(node.id.clone());
    for child in &node.children {
        collect_ids(child, ids);
    }
}

fn validate_asset_path(path: &str) -> Result<(), String> {
    if !path.starts_with("assets/") || !path.ends_with(".webp") {
        return Err(format!("invalid asset path {}", path));
    }
    if path.contains("..") || path.contains('\\') {
        return Err(format!("unsafe asset path {}", path));
    }
    Ok(())
}

fn validate_params(params: &crate::Params) -> Result<(), String> {
    for (key, value) in params {
        if key.is_empty() {
            return Err("parameter key must not be empty".to_owned());
        }
        if value.is_null() {
            return Err(format!("parameter {} must not be null", key));
        }
    }
    Ok(())
}

fn required_paths(manifest: &Manifest) -> Vec<String> {
    let mut paths = vec![
        "mimetype".to_owned(),
        "manifest.json5".to_owned(),
        "scene.json5".to_owned(),
        manifest.preview.clone(),
        manifest.merged.clone(),
    ];
    paths.extend(manifest.assets.iter().map(|a| a.path.clone()));
    paths
}

/// WebP payloads begin with `RIFF` followed by `WEBP`.
pub fn is_webp(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Asset, Canvas, Node, Scene};

    fn webp() -> Vec<u8> {
        b"RIFF\x04\x00\x00\x00WEBPVP8 \x00\x00\x00\x00".to_vec()
    }

    fn package() -> Package {
        let bytes = webp();
        let mut package = Package::new(
            Manifest {
                canvas: Canvas {
                    width: 100,
                    height: 100,
                    ..Canvas::default()
                },
                assets: vec![Asset {
                    path: "assets/a.webp".to_owned(),
                    width: 100,
                    height: 100,
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
        package
            .assets
            .insert("assets/a.webp".to_owned(), bytes);
        package
    }

    #[test]
    fn valid_package_passes() {
        validate_package(&package()).unwrap();
    }

    #[test]
    fn rejects_missing_asset_payload() {
        let mut package = package();
        package.assets.remove("assets/a.webp");
        assert!(validate_package(&package).is_err());
    }

    #[test]
    fn rejects_bad_opacity() {
        let mut package = package();
        package.scene.layers[0].opacity = 1.5;
        assert!(validate_package(&package).is_err());
    }

    #[test]
    fn rejects_unknown_timeline_target() {
        let mut package = package();
        package.scene.timeline = Some(crate::Timeline {
            duration_ms: 1000,
            tracks: vec![crate::Track {
                target: "missing".to_owned(),
                ..crate::Track::default()
            }],
        });
        assert!(validate_package(&package).is_err());
    }

    #[test]
    fn rejects_out_of_order_siblings() {
        let mut package = package();
        package.scene.layers.push(
            Node::layer("b", "B", "assets/a.webp").with_order(0),
        );
        package.scene.layers[0] = package.scene.layers[0].clone().with_order(1);
        assert!(validate_package(&package).is_err());
    }

    #[test]
    fn rejects_duplicate_node_ids() {
        let mut package = package();
        package.scene.layers.push(Node::layer("a", "again", "assets/a.webp"));
        assert!(validate_package(&package).is_err());
    }

    #[test]
    fn history_entries_are_not_required() {
        let mut package = package();
        package
            .extras
            .insert("history/undo.json5".to_owned(), b"{}".to_vec());
        validate_package(&package).unwrap();
    }
}
