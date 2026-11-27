//! Explicit-path local ONNX inference.
//!
//! Nothing in this module downloads a model, a runtime, or a label file. The
//! caller supplies local paths, and every result is returned as a low-confidence
//! suggestion that the UI must stage for human review.

use crate::{suggestion, LocalSuggestion};
use image::DynamicImage;
use ort::environment::Environment;
use std::path::Path;
use std::sync::OnceLock;

/// How the image is laid out in the model input tensor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputLayout {
    Nchw,
    Nhwc,
}

/// Pixel scaling expected by the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Normalization {
    None,
    ZeroOne,
    Clip,
}

/// Whether the first output tensor holds raw logits or already-normalized scores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputInterpretation {
    Logits,
    Probabilities,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OnnxConfig {
    pub runtime_path: String,
    pub model_path: String,
    pub labels_path: String,
    pub input_name: String,
    pub output_name: Option<String>,
    pub input_size: u32,
    pub layout: InputLayout,
    pub normalization: Normalization,
    pub output: OutputInterpretation,
    pub top_k: usize,
    pub min_confidence: f32,
    pub model_version: String,
}

impl OnnxConfig {
    /// Rejects URLs and missing local files before any runtime work starts.
    pub fn validate(&self) -> Result<(), String> {
        for (name, path) in [
            ("ONNX Runtime library", self.runtime_path.as_str()),
            ("model", self.model_path.as_str()),
            ("labels", self.labels_path.as_str()),
        ] {
            if path.trim().is_empty() {
                return Err(format!("{name} path is empty"));
            }
            if path.contains("://") {
                return Err(format!("{name} must be a local path, not a URL"));
            }
            if !Path::new(path).is_file() {
                return Err(format!("{name} file does not exist: {path}"));
            }
        }
        if self.input_name.trim().is_empty() {
            return Err("input name is empty".to_owned());
        }
        if self.input_size == 0 {
            return Err("input size must be greater than zero".to_owned());
        }
        if self.top_k == 0 {
            return Err("top_k must be greater than zero".to_owned());
        }
        if !(0.0..1.0).contains(&self.min_confidence) {
            return Err("minimum confidence must be between 0 and 1".to_owned());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Label {
    pub tag: String,
    pub display: String,
}

/// One label per line: `tag` or `tag | display name`. Blank lines and `#`
/// comments are ignored. Tags are canonical IDs, not free-form prose.
pub fn parse_labels(text: &str) -> Vec<Label> {
    text.lines()
        .filter_map(|line| {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                return None;
            }
            let (tag, display) = line
                .split_once('|')
                .map(|(tag, display)| (tag.trim(), display.trim()))
                .unwrap_or((line, ""));
            if tag.is_empty() {
                None
            } else {
                Some(Label {
                    tag: tag.to_owned(),
                    display: if display.is_empty() {
                        tag.to_owned()
                    } else {
                        display.to_owned()
                    },
                })
            }
        })
        .collect()
}

/// Preprocess a local image into the tensor shape requested by `config`.
pub fn preprocess(image: &DynamicImage, config: &OnnxConfig) -> (Vec<usize>, Vec<f32>) {
    let size = config.input_size.max(1) as u32;
    let resized = image.resize(size, size, image::imageops::FilterType::Triangle);
    let rgb = resized.to_rgb8();
    let pixels = rgb.pixels().count();
    let mut values = Vec::with_capacity(pixels * 3);

    let push_channel = |values: &mut Vec<f32>, channel: usize, byte: u8| {
        let value = match config.normalization {
            Normalization::None => byte as f32,
            Normalization::ZeroOne => byte as f32 / 255.0,
            Normalization::Clip => {
                const MEAN: [f32; 3] = [0.48145466, 0.4578275, 0.40821073];
                const STD: [f32; 3] = [0.26862954, 0.26130258, 0.27577711];
                (byte as f32 / 255.0 - MEAN[channel]) / STD[channel]
            }
        };
        values.push(value);
    };

    match config.layout {
        InputLayout::Nchw => {
            for channel in 0..3 {
                for pixel in rgb.pixels() {
                    push_channel(&mut values, channel, pixel.0[channel]);
                }
            }
            (vec![1, 3, size as usize, size as usize], values)
        }
        InputLayout::Nhwc => {
            for pixel in rgb.pixels() {
                for channel in 0..3 {
                    push_channel(&mut values, channel, pixel.0[channel]);
                }
            }
            (vec![1, size as usize, size as usize, 3], values)
        }
    }
}

static ENV: OnceLock<Environment> = OnceLock::new();
static ENV_PATH: OnceLock<String> = OnceLock::new();

fn environment(runtime_path: &str) -> Result<Environment, String> {
    if let Some(env) = ENV.get() {
        let active = ENV_PATH.get().map(String::as_str).unwrap_or("");
        if active != runtime_path {
            return Err(format!(
                "ONNX Runtime is already initialized from '{active}'; restart Symtropy to use '{runtime_path}'"
            ));
        }
        return Ok(env.clone());
    }
    let builder = ort::init_from(runtime_path)
        .map_err(|error| format!("cannot load ONNX Runtime from '{runtime_path}': {error}"))?;
    let env = builder
        .build()
        .map_err(|error| format!("cannot initialize ONNX Runtime: {error}"))?;
    let _ = ENV_PATH.set(runtime_path.to_owned());
    let _ = ENV.set(env.clone());
    Ok(ENV.get().cloned().unwrap_or(env))
}

fn softmax(scores: &[f32]) -> Vec<f32> {
    let max = scores.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if !max.is_finite() {
        return vec![0.0; scores.len()];
    }
    let exps = scores
        .iter()
        .map(|score| (*score - max).exp())
        .collect::<Vec<_>>();
    let total = exps.iter().sum::<f32>();
    if total <= 0.0 || !total.is_finite() {
        return vec![0.0; scores.len()];
    }
    exps.into_iter().map(|value| value / total).collect()
}

/// Run a local model and return ranked label suggestions. No network access
/// and no writes happen here.
pub fn suggest_labels(
    path: &Path,
    config: &OnnxConfig,
) -> Result<Vec<LocalSuggestion>, String> {
    config.validate()?;
    let labels_text = std::fs::read_to_string(&config.labels_path)
        .map_err(|error| format!("cannot read labels '{}': {error}", config.labels_path))?;
    let labels = parse_labels(&labels_text);
    if labels.is_empty() {
        return Err(format!("no labels found in '{}'", config.labels_path));
    }
    let bytes = std::fs::read(path)
        .map_err(|error| format!("cannot read '{}': {error}", path.display()))?;
    let image = crate::decode_image(&bytes)?;
    suggest_labels_from_image(&image, &labels, config)
}

/// Same as [`suggest_labels`] for callers that already decoded the image.
pub fn suggest_labels_from_image(
    image: &DynamicImage,
    labels: &[Label],
    config: &OnnxConfig,
) -> Result<Vec<LocalSuggestion>, String> {
    let (shape, values) = preprocess(image, config);
    let env = environment(&config.runtime_path)?;
    let mut session = ort::session::Session::builder(&env)
        .map_err(|error| format!("cannot create ONNX session: {error}"))?
        .commit_from_file(&config.model_path)
        .map_err(|error| format!("cannot load model '{}': {error}", config.model_path))?;

    let tensor = ort::value::Tensor::from_array((shape, values.into_boxed_slice()))
        .map_err(|error| format!("cannot build ONNX input tensor: {error}"))?;
    let outputs = session
        .run(ort::inputs![config.input_name.as_str() => tensor])
        .map_err(|error| format!("ONNX inference failed: {error}"))?;
    let value = match config.output_name.as_deref() {
        Some(name) if !name.is_empty() => outputs
            .get(name)
            .ok_or_else(|| format!("ONNX model has no output named '{name}'"))?,
        _ => &outputs[0],
    };
    let (_, scores) = value
        .try_extract_tensor::<f32>()
        .map_err(|error| format!("cannot read ONNX output: {error}"))?;
    if scores.len() != labels.len() {
        return Err(format!(
            "model returned {} scores but labels file has {} entries",
            scores.len(),
            labels.len()
        ));
    }

    let scores = match config.output {
        OutputInterpretation::Logits => softmax(scores),
        OutputInterpretation::Probabilities => scores.to_vec(),
    };
    let mut ranked = labels
        .iter()
        .zip(scores)
        .filter(|(_, score)| *score >= config.min_confidence)
        .collect::<Vec<_>>();
    ranked.sort_by(|(_, a), (_, b)| b.total_cmp(a));
    ranked.truncate(config.top_k);

    Ok(ranked
        .into_iter()
        .map(|(label, score)| {
            suggestion(
                &label.tag,
                score,
                format!("ONNX label '{}' scored {:.3}", label.display, score),
            )
            .with_model_version(&config.model_version)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    #[test]
    fn labels_accept_canonical_ids_and_optional_display_names() {
        let labels = parse_labels(
            "# model labels\nact:possession | Claimed possession\n\nstate:possessed\n",
        );
        assert_eq!(
            labels,
            vec![
                Label {
                    tag: "act:possession".to_owned(),
                    display: "Claimed possession".to_owned(),
                },
                Label {
                    tag: "state:possessed".to_owned(),
                    display: "state:possessed".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn config_validation_rejects_urls_and_missing_files() {
        let mut config = OnnxConfig {
            runtime_path: "https://example.test/lib.so".to_owned(),
            model_path: "/tmp/model.onnx".to_owned(),
            labels_path: "/tmp/labels.txt".to_owned(),
            input_name: "input".to_owned(),
            output_name: None,
            input_size: 224,
            layout: InputLayout::Nchw,
            normalization: Normalization::Clip,
            output: OutputInterpretation::Logits,
            top_k: 5,
            min_confidence: 0.05,
            model_version: "test".to_owned(),
        };
        assert!(config.validate().is_err());
        config.runtime_path = "/tmp/does-not-exist.so".to_owned();
        assert!(config.validate().is_err());
    }

    #[test]
    fn preprocessing_respects_layout() {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(8, 8, Rgba([10, 20, 30, 255])));
        let base = OnnxConfig {
            runtime_path: String::new(),
            model_path: String::new(),
            labels_path: String::new(),
            input_name: "input".to_owned(),
            output_name: None,
            input_size: 4,
            layout: InputLayout::Nchw,
            normalization: Normalization::ZeroOne,
            output: OutputInterpretation::Logits,
            top_k: 1,
            min_confidence: 0.0,
            model_version: "test".to_owned(),
        };
        let mut config = base.clone();
        config.layout = InputLayout::Nchw;
        let (shape, values) = preprocess(&image, &config);
        assert_eq!(shape, vec![1, 3, 4, 4]);
        assert_eq!(values.len(), 48);
        assert!((values[0] - 10.0 / 255.0).abs() < f32::EPSILON);
        assert!((values[16] - 20.0 / 255.0).abs() < f32::EPSILON);

        config.layout = InputLayout::Nhwc;
        let (shape, values) = preprocess(&image, &config);
        assert_eq!(shape, vec![1, 4, 4, 3]);
        assert!((values[1] - 20.0 / 255.0).abs() < f32::EPSILON);
    }
}
