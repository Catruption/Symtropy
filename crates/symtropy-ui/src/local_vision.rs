//! UI bridge for local-only image suggestions.
//!
//! The worker reads local pixels only. Results are staged into the review
//! store as `Suggested` assertions and can never overwrite accepted work.

use serde::{Deserialize, Serialize};
use std::sync::mpsc::{channel, Receiver};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnnxInputLayout {
    #[default]
    Nchw,
    Nhwc,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnnxNormalization {
    None,
    ZeroOne,
    #[default]
    Clip,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnnxOutputInterpretation {
    #[default]
    Logits,
    Probabilities,
}

/// All model paths are supplied by the user. Nothing here fetches a runtime,
/// a model, or labels from the network.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OnnxUiConfig {
    pub runtime_path: String,
    pub model_path: String,
    pub labels_path: String,
    pub input_name: String,
    pub output_name: String,
    pub input_size: u32,
    pub layout: OnnxInputLayout,
    pub normalization: OnnxNormalization,
    pub output: OnnxOutputInterpretation,
    pub top_k: usize,
    pub min_confidence: f32,
    pub model_version: String,
}

impl Default for OnnxUiConfig {
    fn default() -> Self {
        Self {
            runtime_path: String::new(),
            model_path: String::new(),
            labels_path: String::new(),
            input_name: "input".to_owned(),
            output_name: String::new(),
            input_size: 224,
            layout: OnnxInputLayout::Nchw,
            normalization: OnnxNormalization::Clip,
            output: OnnxOutputInterpretation::Logits,
            top_k: 5,
            min_confidence: 0.05,
            model_version: "local-onnx".to_owned(),
        }
    }
}

impl OnnxUiConfig {
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
            if !std::path::Path::new(path).is_file() {
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

    #[cfg(feature = "onnx-vision")]
    fn to_vision(&self) -> symtropy_vision::onnx::OnnxConfig {
        use symtropy_vision::onnx::{InputLayout, Normalization, OnnxConfig, OutputInterpretation};
        let output_name = self.output_name.trim();
        OnnxConfig {
            runtime_path: self.runtime_path.clone(),
            model_path: self.model_path.clone(),
            labels_path: self.labels_path.clone(),
            input_name: self.input_name.clone(),
            output_name: if output_name.is_empty() {
                None
            } else {
                Some(output_name.to_owned())
            },
            input_size: self.input_size,
            layout: match self.layout {
                OnnxInputLayout::Nchw => InputLayout::Nchw,
                OnnxInputLayout::Nhwc => InputLayout::Nhwc,
            },
            normalization: match self.normalization {
                OnnxNormalization::None => Normalization::None,
                OnnxNormalization::ZeroOne => Normalization::ZeroOne,
                OnnxNormalization::Clip => Normalization::Clip,
            },
            output: match self.output {
                OnnxOutputInterpretation::Logits => OutputInterpretation::Logits,
                OnnxOutputInterpretation::Probabilities => OutputInterpretation::Probabilities,
            },
            top_k: self.top_k,
            min_confidence: self.min_confidence,
            model_version: self.model_version.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum LocalVisionBackend {
    Features,
    Onnx(OnnxUiConfig),
}

#[derive(Clone, Debug)]
pub struct LocalVisionJob {
    pub item_id: String,
    pub path: String,
    pub backend: LocalVisionBackend,
}

impl LocalVisionJob {
    pub fn features(item_id: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            item_id: item_id.into(),
            path: path.into(),
            backend: LocalVisionBackend::Features,
        }
    }

    pub fn onnx(
        item_id: impl Into<String>,
        path: impl Into<String>,
        config: OnnxUiConfig,
    ) -> Self {
        Self {
            item_id: item_id.into(),
            path: path.into(),
            backend: LocalVisionBackend::Onnx(config),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LocalSuggestion {
    pub tag: String,
    pub confidence: f32,
    pub reason: String,
    pub model_version: String,
}

impl LocalSuggestion {
    #[cfg(feature = "local-vision")]
    fn from_vision(suggestion: symtropy_vision::LocalSuggestion) -> Self {
        Self {
            tag: suggestion.tag,
            confidence: suggestion.confidence,
            reason: suggestion.reason,
            model_version: suggestion.model_version,
        }
    }
}

#[derive(Debug)]
pub enum LocalVisionWorkerMsg {
    Suggestions {
        item_id: String,
        result: Result<Vec<LocalSuggestion>, String>,
    },
}

pub fn enabled() -> bool {
    cfg!(feature = "local-vision")
}

pub fn onnx_enabled() -> bool {
    cfg!(feature = "onnx-vision")
}

#[cfg(feature = "local-vision")]
pub fn spawn_local_vision(
    ctx: &egui::Context,
    job: LocalVisionJob,
) -> Receiver<LocalVisionWorkerMsg> {
    let (tx, rx) = channel();
    let worker_ctx = ctx.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(|| match job.backend {
            LocalVisionBackend::Features => {
                symtropy_vision::suggest_features(std::path::Path::new(&job.path))
            }
            LocalVisionBackend::Onnx(config) => {
                #[cfg(feature = "onnx-vision")]
                {
                    config.validate()?;
                    symtropy_vision::onnx::suggest_labels(
                        std::path::Path::new(&job.path),
                        &config.to_vision(),
                    )
                }
                #[cfg(not(feature = "onnx-vision"))]
                {
                    Err(format!(
                        "ONNX local model disabled for '{}' (build with --features onnx-vision)",
                        config.model_path
                    ))
                }
            }
        })
        .unwrap_or_else(|_| Err("local vision worker panicked".to_string()))
        .map(|suggestions| {
            suggestions
                .into_iter()
                .map(LocalSuggestion::from_vision)
                .collect()
        });
        let _ = tx.send(LocalVisionWorkerMsg::Suggestions {
            item_id: job.item_id,
            result,
        });
        worker_ctx.request_repaint();
    });
    rx
}

#[cfg(not(feature = "local-vision"))]
pub fn spawn_local_vision(
    _ctx: &egui::Context,
    job: LocalVisionJob,
) -> Receiver<LocalVisionWorkerMsg> {
    let (tx, rx) = channel();
    let detail = match &job.backend {
        LocalVisionBackend::Features => format!("local features for '{}'", job.path),
        LocalVisionBackend::Onnx(config) => {
            format!("local ONNX model '{}'", config.model_path)
        }
    };
    let _ = tx.send(LocalVisionWorkerMsg::Suggestions {
        item_id: job.item_id,
        result: Err(format!("local-vision feature is not enabled for {detail}")),
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onnx_config_validation_rejects_urls() {
        let mut config = OnnxUiConfig::default();
        config.runtime_path = "https://example.test/onnxruntime.so".to_owned();
        assert!(config.validate().is_err());
    }
}
