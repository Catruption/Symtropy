//! Typed scene model for SuperSVG documents.
//!
//! The model describes a composite raster image: a canvas, immutable WebP
//! assets, and an ordered layer tree. Parameters and variants are ordinary
//! key/value maps so renderers can add features without changing the container
//! format.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub type Params = BTreeMap<String, Value>;

/// Deterministic JSON5-friendly rectangle in asset pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    /// Optional ICC profile name carried over from the source document.
    pub color_profile: Option<String>,
}

impl Default for Canvas {
    fn default() -> Self {
        Self {
            width: 1,
            height: 1,
            color_profile: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Asset {
    /// Container path, always `assets/<name>.webp`.
    pub path: String,
    pub width: u32,
    pub height: u32,
    /// Byte length of the WebP payload.
    pub bytes: u64,
}

impl Default for Asset {
    fn default() -> Self {
        Self {
            path: String::new(),
            width: 1,
            height: 1,
            bytes: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub generator: Option<String>,
    pub canvas: Canvas,
    pub assets: Vec<Asset>,
    pub preview: String,
    pub merged: String,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            format: "SuperSVG".to_owned(),
            version: super::FORMAT_VERSION,
            generator: None,
            canvas: Canvas::default(),
            assets: Vec::new(),
            preview: "preview.webp".to_owned(),
            merged: "merged.webp".to_owned(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlendMode {
    #[default]
    Normal,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorBurn,
    ColorDodge,
    Burn,
    Dodge,
    HardLight,
    SoftLight,
    Luminosity,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// A layer that paints an asset.
    #[default]
    Layer,
    /// A group whose children paint in order.
    Group,
    /// A non-painting reference to another node or variant.
    Reference,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Transform {
    pub offset: [i32; 2],
    pub scale: [f32; 2],
    pub rotation: f32,
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            offset: [0, 0],
            scale: [1.0, 1.0],
            rotation: 0.0,
        }
    }
}

/// Optional provenance copied from an authoring document.
///
/// Renderers must ignore this: `visible`, `opacity`, `blend`, `alphaInherit`,
/// `passThrough` and `transform` are the canonical render state. These fields
/// exist so a converter can round-trip or audit source-layer identity.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct NodeSource {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Source layer name when it differs from the emitted `name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,
    /// Source pixel-data entry, e.g. Krita's `layers/layer12`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// Position recorded by the authoring document, not a render transform.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<[i32; 2]>,
    /// Raw source channel flags; `alphaInherit` is the canonical flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_flags: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locked: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_label: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_timeline: Option<bool>,
    /// Group compositing mode; mirrored canonically by `Node::pass_through`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pass_through: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Node {
    pub id: String,
    pub name: String,
    pub kind: NodeKind,
    /// Explicit paint order within the parent, bottom-to-top. Array order is
    /// canonical; this preserves the source order for tools and round-trips.
    pub order: u32,
    /// Asset path for `Layer`, or target id for `Reference`.
    pub asset: Option<String>,
    pub visible: bool,
    /// 0.0 ..= 1.0.
    pub opacity: f32,
    pub blend: BlendMode,
    /// Krita-style alpha inheritance.
    pub alpha_inherit: bool,
    /// Groups composite children directly instead of into an offscreen layer.
    pub pass_through: bool,
    pub transform: Transform,
    /// Non-normative authoring metadata, ignored by renderers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<NodeSource>,
    /// Optional source crop/frame inside the asset.
    pub crop: Option<Rect>,
    /// Bottom-to-top paint order.
    pub children: Vec<Node>,
    /// Renderer parameters. Keyframes override these.
    pub params: Params,
}

impl Default for Node {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            kind: NodeKind::Layer,
            order: 0,
            asset: None,
            visible: true,
            opacity: 1.0,
            blend: BlendMode::Normal,
            alpha_inherit: false,
            pass_through: false,
            transform: Transform::default(),
            source: None,
            crop: None,
            children: Vec::new(),
            params: Params::new(),
        }
    }
}

impl Node {
    pub fn group(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            kind: NodeKind::Group,
            ..Self::default()
        }
    }

    pub fn layer(
        id: impl Into<String>,
        name: impl Into<String>,
        asset: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            kind: NodeKind::Layer,
            asset: Some(asset.into()),
            ..Self::default()
        }
    }

    pub fn with_order(mut self, order: u32) -> Self {
        self.order = order;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Variant {
    pub id: String,
    pub name: String,
    pub params: Params,
}

impl Default for Variant {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            params: Params::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Keyframe {
    pub time_ms: u64,
    /// Parameter overrides applied at this time.
    pub params: Params,
}

impl Default for Keyframe {
    fn default() -> Self {
        Self {
            time_ms: 0,
            params: Params::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Track {
    /// Node id, variant id, or `scene`.
    pub target: String,
    pub keyframes: Vec<Keyframe>,
}

impl Default for Track {
    fn default() -> Self {
        Self {
            target: String::new(),
            keyframes: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Timeline {
    pub duration_ms: u64,
    pub tracks: Vec<Track>,
}

impl Default for Timeline {
    fn default() -> Self {
        Self {
            duration_ms: 0,
            tracks: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Scene {
    /// Bottom-to-top layer order.
    pub layers: Vec<Node>,
    pub variants: Vec<Variant>,
    /// Optional renderable timeline. Static images leave this as `None`.
    pub timeline: Option<Timeline>,
}

impl Default for Scene {
    fn default() -> Self {
        Self {
            layers: Vec::new(),
            variants: Vec::new(),
            timeline: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_stable() {
        let manifest = Manifest::default();
        assert_eq!(manifest.format, "SuperSVG");
        assert_eq!(manifest.version, super::super::FORMAT_VERSION);
        assert_eq!(manifest.preview, "preview.webp");
        assert_eq!(manifest.merged, "merged.webp");
    }

    #[test]
    fn node_helpers_set_kind_and_asset() {
        let layer = Node::layer("a", "A", "assets/a.webp");
        assert_eq!(layer.kind, NodeKind::Layer);
        assert_eq!(layer.asset.as_deref(), Some("assets/a.webp"));

        let group = Node::group("g", "Group");
        assert_eq!(group.kind, NodeKind::Group);
        assert!(group.asset.is_none());
    }
}
