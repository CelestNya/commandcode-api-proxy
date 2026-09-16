//! Static model catalog, read from the same src/models.json the Node
//! implementation uses (single source of truth, embedded at compile time).
//! Dynamic discovery is layered on in M4.

use serde::Deserialize;
use std::collections::HashMap;

const MODELS_JSON: &str = include_str!("../models.json");

#[derive(Debug, Clone, PartialEq)]
pub struct ModelMeta {
    pub id: String,
    pub display_name: String,
    pub context_window: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub models: Vec<ModelMeta>,
    pub ids: Vec<String>,
}

impl Catalog {
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

#[derive(Deserialize)]
struct RawModels {
    builtin: Vec<String>,
    #[serde(rename = "contextWindows")]
    context_windows: HashMap<String, u64>,
    #[serde(rename = "modelNames")]
    model_names: HashMap<String, String>,
}

const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

/// Build the static (built-in) catalog. Order matters: `ids[0]` is the default
/// model and the order is part of the resolution contract.
pub fn static_catalog() -> Catalog {
    let raw: RawModels = match serde_json::from_str(MODELS_JSON) {
        Ok(v) => v,
        Err(_) => return Catalog::default(),
    };
    let models: Vec<ModelMeta> = raw
        .builtin
        .iter()
        .map(|id| {
            let display_name = raw
                .model_names
                .get(id)
                .cloned()
                .unwrap_or_else(|| last_segment(id));
            let context_window = raw
                .context_windows
                .get(id)
                .copied()
                .unwrap_or(DEFAULT_CONTEXT_WINDOW);
            ModelMeta {
                id: id.clone(),
                display_name,
                context_window,
            }
        })
        .collect();
    let ids = models.iter().map(|m| m.id.clone()).collect();
    Catalog { models, ids }
}

fn last_segment(id: &str) -> String {
    id.rsplit('/').next().unwrap_or(id).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_catalog_first_entry_is_the_default_model() {
        let c = static_catalog();
        assert_eq!(
            c.models.first().map(|m| m.id.as_str()),
            Some("deepseek/deepseek-v4-pro")
        );
        assert_eq!(c.models.len(), 44);
        assert_eq!(c.ids.len(), 44);
    }

    #[test]
    fn display_names_and_windows_come_from_the_json() {
        let c = static_catalog();
        let first = &c.models[0];
        assert_eq!(first.display_name, "DeepSeek V4 Pro");
        assert_eq!(first.context_window, 1_048_576);
    }
}
