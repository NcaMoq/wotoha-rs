//! Minimal embedded rten backend for `beat-this`.
//!
//! `beat-this` 1.0.0 keeps its bundled rten model type private and its public
//! runtime accepts paths. This adapter uses the public rten static-slice loader
//! so the release binary contains the two ONNX assets and never depends on the
//! build machine's source directory.

use std::{collections::HashMap, path::Path};

use anyhow::{Result, anyhow};
use beat_this::{Model, Runtime, Tensor};
use rten::{Model as RtenGraph, NodeId, Value};
use rten_tensor::{AsView, Layout};

pub(crate) struct EmbeddedRtenRuntime {
    pub(crate) mel_model: &'static [u8],
    pub(crate) beat_model: &'static [u8],
}

impl EmbeddedRtenRuntime {
    pub(crate) fn new(mel_model: &'static [u8], beat_model: &'static [u8]) -> Self {
        Self {
            mel_model,
            beat_model,
        }
    }
}

impl Runtime for EmbeddedRtenRuntime {
    type Model = EmbeddedRtenModel;

    fn load_model(&self, path: &Path) -> Result<Self::Model> {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("embedded beat model path has no UTF-8 file name"))?;
        let bytes = match file_name {
            "mel_spectrogram.onnx" => self.mel_model,
            "beat_this_small.onnx" => self.beat_model,
            other => return Err(anyhow!("unknown embedded beat model asset: {other}")),
        };
        let model = RtenGraph::load_static_slice(bytes)?;
        let input_map = model
            .input_ids()
            .iter()
            .filter_map(|&id| {
                let info = model.node_info(id)?;
                Some((info.name()?.to_string(), id))
            })
            .collect();
        let output_names = model
            .output_ids()
            .iter()
            .filter_map(|&id| {
                let info = model.node_info(id)?;
                Some((id, info.name()?.to_string()))
            })
            .collect();
        Ok(EmbeddedRtenModel {
            output_ids: model.output_ids().to_vec(),
            model,
            input_map,
            output_names,
        })
    }
}

pub(crate) struct EmbeddedRtenModel {
    model: RtenGraph,
    input_map: HashMap<String, NodeId>,
    output_names: Vec<(NodeId, String)>,
    output_ids: Vec<NodeId>,
}

impl Model for EmbeddedRtenModel {
    fn run(&mut self, inputs: &[(&str, &Tensor)]) -> Result<HashMap<String, Tensor>> {
        let rten_inputs = inputs
            .iter()
            .map(|(name, tensor)| {
                let node_id = self
                    .input_map
                    .get(*name)
                    .copied()
                    .ok_or_else(|| anyhow!("rten: unknown input name '{name}'"))?;
                let value =
                    Value::from_shape(&tensor.shape, tensor.data.clone()).map_err(|error| {
                        anyhow!("rten: failed to create input tensor '{name}': {error}")
                    })?;
                Ok((node_id, value))
            })
            .collect::<Result<Vec<_>>>()?;
        let inputs_with_views = rten_inputs
            .iter()
            .map(|(id, value)| (*id, value.into()))
            .collect::<Vec<_>>();
        let outputs = self.model.run(inputs_with_views, &self.output_ids, None)?;
        let mut result = HashMap::new();
        for (&id, value) in self.output_ids.iter().zip(outputs) {
            let name = self
                .output_names
                .iter()
                .find(|(node_id, _)| *node_id == id)
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| format!("output_{id:?}"));
            let tensor = value
                .into_tensor::<f32>()
                .ok_or_else(|| anyhow!("rten: output '{name}' is not f32"))?;
            result.insert(
                name,
                Tensor {
                    shape: tensor.shape().to_vec(),
                    data: tensor.to_vec(),
                },
            );
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_model_paths_fail_closed() {
        let runtime = EmbeddedRtenRuntime::new(b"mel", b"beat");
        assert!(
            runtime
                .load_model(Path::new("downloaded-model.onnx"))
                .is_err()
        );
    }
}
