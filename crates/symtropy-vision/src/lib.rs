//! Local-only image feature suggestions.
//!
//! This crate never downloads models or contacts a network. The first backend
//! is deliberately deterministic: it derives low-confidence feature tags from
//! the pixels already on disk. A future ONNX/CLIP/SigLIP backend can produce
//! the same `LocalSuggestion` shape, but every result must still be staged for
//! human review before it becomes part of the canonical tag set.

use image::{DynamicImage, GenericImageView};
use std::path::Path;

#[cfg(feature = "onnx")]
pub mod onnx;

pub const MODEL_VERSION: &str = "local-features-v1";

/// A reviewable machine guess. Confidence is always below `100%`; the review
/// store is responsible for keeping it out of accepted tags.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalSuggestion {
    pub tag: String,
    pub confidence: f32,
    pub reason: String,
    pub model_version: String,
}

impl LocalSuggestion {
    /// Override the model version for a non-default backend.
    pub fn with_model_version(mut self, model_version: &str) -> Self {
        self.model_version = model_version.to_owned();
        self
    }
}

fn suggestion(tag: &str, confidence: f32, reason: String) -> LocalSuggestion {
    LocalSuggestion {
        tag: tag.to_string(),
        confidence: confidence.clamp(0.0, 0.99),
        reason,
        model_version: MODEL_VERSION.to_string(),
    }
}

/// Decode ordinary images and `.ssvg` baked previews without any network I/O.
pub(crate) fn decode_image(bytes: &[u8]) -> Result<DynamicImage, String> {
    if bytes.starts_with(b"PK") {
        if let Ok((_, preview)) = ssvg::read_preview_from_bytes(bytes) {
            return image::load_from_memory(&preview)
                .map_err(|error| format!("cannot decode ssvg preview: {error}"));
        }
    }
    image::load_from_memory(bytes).map_err(|error| format!("cannot decode image: {error}"))
}

/// Derive feature suggestions from a local file.
pub fn suggest_features(path: &Path) -> Result<Vec<LocalSuggestion>, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("cannot read '{}': {error}", path.display()))?;
    suggest_features_from_bytes(&bytes)
}

/// Same as [`suggest_features`], for callers that already hold local bytes.
pub fn suggest_features_from_bytes(bytes: &[u8]) -> Result<Vec<LocalSuggestion>, String> {
    let image = decode_image(bytes)?;
    Ok(feature_suggestions(&image))
}

fn feature_suggestions(image: &DynamicImage) -> Vec<LocalSuggestion> {
    let (width, height) = image.dimensions();
    let sample = image.resize(64, 64, image::imageops::FilterType::Triangle);
    let rgba = sample.to_rgba8();
    let pixels = rgba.pixels().collect::<Vec<_>>();
    let count = pixels.len().max(1) as f32;

    let mut hue_weights = [0.0f32; 12];
    let mut brightness_sum = 0.0f32;
    let mut neutral_count = 0usize;

    for pixel in pixels {
        let [r, g, b, _] = pixel.0;
        let (hue, saturation, value) = rgb_to_hsv(r, g, b);
        brightness_sum += value;
        if saturation < 0.15 {
            neutral_count += 1;
        } else {
            let bucket = ((hue / 30.0).floor() as usize) % 12;
            hue_weights[bucket] += saturation * value;
        }
    }

    let mut out = Vec::new();
    let total_weight: f32 = hue_weights.iter().sum();
    if total_weight <= 0.01 {
        let neutral_share = neutral_count as f32 / count;
        out.push(suggestion(
            "feature:color:dominant_neutral",
            0.4 + neutral_share * 0.2,
            format!("{:.0}% of sampled pixels are near-neutral", neutral_share * 100.0),
        ));
    } else {
        let (bucket, weight) = hue_weights
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.total_cmp(b))
            .expect("twelve hue buckets are always present");
        let share = *weight / total_weight;
        let name = HUE_NAMES[bucket];
        out.push(suggestion(
            &format!("feature:color:dominant_{name}"),
            0.35 + share * 0.3,
            format!("dominant sampled hue is {name} ({:.0}%)", share * 100.0),
        ));
    }

    let brightness = brightness_sum / count;
    let tone = if brightness < 0.25 {
        "dark"
    } else if brightness > 0.75 {
        "light"
    } else {
        "medium"
    };
    out.push(suggestion(
        &format!("feature:tone:{tone}"),
        0.45,
        format!("mean sampled brightness {:.0}%", brightness * 100.0),
    ));

    let ratio = width as f32 / height.max(1) as f32;
    let aspect = if ratio > 1.25 {
        "wide"
    } else if ratio < 0.8 {
        "tall"
    } else {
        "square"
    };
    out.push(suggestion(
        &format!("feature:aspect:{aspect}"),
        0.55,
        format!("source aspect ratio {ratio:.2}"),
    ));
    out
}

const HUE_NAMES: [&str; 12] = [
    "red", "orange", "yellow", "lime", "green", "teal", "cyan", "azure", "blue", "violet",
    "magenta", "rose",
];

fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let hue = if delta <= 0.0 {
        0.0
    } else if max == r {
        60.0 * (((g - b) / delta) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let hue = if hue < 0.0 { hue + 360.0 } else { hue };
    let saturation = if max <= 0.0 { 0.0 } else { delta / max };
    (hue, saturation, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba, RgbaImage};
    use std::io::Cursor;

    fn png_bytes(image: &DynamicImage) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        image
            .write_to(&mut cursor, ImageFormat::Png)
            .expect("write png");
        cursor.into_inner()
    }

    fn webp_bytes(image: &DynamicImage) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        image
            .write_to(&mut cursor, ImageFormat::WebP)
            .expect("write webp");
        cursor.into_inner()
    }

    #[test]
    fn solid_orange_produces_reviewable_feature_tags() {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            16,
            16,
            Rgba([255, 128, 0, 255]),
        ));
        let suggestions = suggest_features_from_bytes(&png_bytes(&image)).unwrap();

        assert!(suggestions.iter().any(|s| s.tag == "feature:color:dominant_orange"));
        assert!(suggestions.iter().any(|s| s.tag == "feature:tone:light"));
        assert!(suggestions.iter().any(|s| s.tag == "feature:aspect:square"));
        assert!(suggestions.iter().all(|s| s.confidence <= 0.99));
        assert!(suggestions.iter().all(|s| s.model_version == MODEL_VERSION));
    }

    #[test]
    fn neutral_images_do_not_get_a_hue_tag() {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            32,
            32,
            Rgba([128, 128, 128, 255]),
        ));
        let suggestions = suggest_features_from_bytes(&png_bytes(&image)).unwrap();

        assert!(suggestions.iter().any(|s| s.tag == "feature:color:dominant_neutral"));
        assert!(suggestions.iter().all(|s| !s.tag.contains("dominant_orange")));
    }

    #[test]
    fn aspect_is_measured_from_the_source_image() {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            64,
            16,
            Rgba([0, 0, 255, 255]),
        ));
        let suggestions = suggest_features_from_bytes(&png_bytes(&image)).unwrap();

        assert!(suggestions.iter().any(|s| s.tag == "feature:aspect:wide"));
    }

    #[test]
    fn ssvg_packages_use_the_baked_preview() {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            8,
            8,
            Rgba([240, 128, 16, 255]),
        ));
        let webp = webp_bytes(&image);
        let package = ssvg::Package::new(
            ssvg::Manifest::default(),
            ssvg::Scene::default(),
            webp.clone(),
            webp,
        );
        let mut cursor = Cursor::new(Vec::new());
        package.write_to(&mut cursor).unwrap();

        let suggestions = suggest_features_from_bytes(&cursor.into_inner()).unwrap();
        assert!(suggestions.iter().any(|s| s.tag == "feature:color:dominant_orange"));
    }
}
