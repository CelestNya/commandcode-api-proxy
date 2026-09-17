//! Model resolution, aliasing and reasoning-effort clipping, ported from
//! src/translate/models.ts. The decision order is the contract: reordering any
//! step changes results (aliases are case-insensitive but full ids pass through
//! with their original casing, and bare names match on the last path segment).

use crate::models::Catalog;
use crate::validation::is_effort_off;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

const MODELS_JSON: &str = include_str!("../models.json");

/// What an "off" marker becomes when the model's level set is unknown.
/// "low" is the safest concrete level: most models accept it, and CC coerces an
/// unsupported level silently, while sending "off" is a hard 400.
const LOWEST_EFFORT: &str = "low";

#[derive(Deserialize)]
struct RawModels {
    builtin: Vec<String>,
    #[serde(rename = "shortAliases")]
    short_aliases: HashMap<String, String>,
    #[serde(rename = "reasoningEfforts", default)]
    reasoning_efforts: HashMap<String, Vec<String>>,
}

pub struct ModelTables {
    pub builtin: Vec<String>,
    pub short_aliases: HashMap<String, String>,
    pub reasoning_efforts: HashMap<String, Vec<String>>,
}

impl ModelTables {
    pub fn load() -> Self {
        match serde_json::from_str::<RawModels>(MODELS_JSON) {
            Ok(raw) => Self {
                builtin: raw.builtin,
                short_aliases: raw.short_aliases,
                reasoning_efforts: raw.reasoning_efforts,
            },
            Err(_) => Self {
                builtin: Vec::new(),
                short_aliases: HashMap::new(),
                reasoning_efforts: HashMap::new(),
            },
        }
    }
}

/// Rank ordering of effort levels (low -> max), used to clip to the nearest valid.
fn effort_rank(level: &str) -> i32 {
    match level {
        "low" => 0,
        "medium" => 1,
        "high" => 2,
        "xhigh" => 3,
        "max" => 4,
        // Unknown levels rank as "high", matching the Node `?? 2`.
        _ => 2,
    }
}

/// Step 2 of model resolution: the requested name -> a canonical model id.
///
/// Order is the contract:
///   1. empty or "default" -> the catalog's first entry
///   2. alias table, case-insensitive
///   3. contains "/" -> passed through **with its original casing**
///   4. bare name -> case-insensitive match on the catalog's last path segment
///   5. no match -> returned unchanged (CC rejects it and the caller learns)
pub fn resolve_model(model: &str, catalog: &Catalog, tables: &ModelTables) -> String {
    if model.is_empty() || model == "default" {
        return catalog
            .ids
            .first()
            .cloned()
            .or_else(|| tables.builtin.first().cloned())
            .unwrap_or_default();
    }
    // Case-insensitive so callers can pass the bare name with original casing
    // (e.g. "GLM-5.2") as well as the lowercase short alias.
    if let Some(hit) = tables.short_aliases.get(model) {
        return hit.clone();
    }
    if let Some(hit) = tables.short_aliases.get(&model.to_lowercase()) {
        return hit.clone();
    }
    // Already a full model id — pass through untouched (casing preserved).
    if model.contains('/') {
        return model.to_string();
    }
    let lower = model.to_lowercase();
    for id in &catalog.ids {
        let last = id.rsplit('/').next().unwrap_or(id);
        if last.to_lowercase() == lower {
            return id.clone();
        }
    }
    model.to_string()
}

/// Whether a request model name would reach CC unresolvable: a bare name
/// matching neither an alias nor a known model. Full ids and aliases never
/// need a discovery refresh.
pub fn needs_catalog_discovery(model: &str, catalog: &Catalog, tables: &ModelTables) -> bool {
    if model.is_empty() || model == "default" || model.contains('/') {
        return false;
    }
    if tables.short_aliases.contains_key(model)
        || tables.short_aliases.contains_key(&model.to_lowercase())
    {
        return false;
    }
    resolve_model(model, catalog, tables) == model
}

/// Clip a requested effort to a level the model actually accepts.
///
/// - Unknown model (no effort set): pass through unchanged — we do not know
///   better. An "off" marker is the one exception: it must never reach the
///   upstream, which 400s on it.
/// - "off" markers resolve to the model's lowest supported level. Clipping by
///   rank instead would sort it above nothing and pick a middle level — the bug
///   behind "I turned thinking off and it still thinks a lot".
/// - Otherwise clip to the highest supported rank <= requested, else the lowest.
pub fn resolve_effort_for_model(
    canonical_model: &str,
    requested: Option<&str>,
    tables: &ModelTables,
) -> Option<String> {
    let supported = tables
        .reasoning_efforts
        .get(canonical_model)
        .filter(|s| !s.is_empty());

    let Some(supported) = supported else {
        // Uncatalogued or empty effort set: preserve as-is, except for an
        // "off" marker which must never be forwarded.
        return match requested {
            Some(r) if is_effort_off(&Value::String(r.to_string())) => Some(LOWEST_EFFORT.into()),
            Some(r) => Some(r.to_string()),
            None => None,
        };
    };

    let lowest = |set: &Vec<String>| -> String {
        set.iter()
            .reduce(|best, e| {
                if effort_rank(e) < effort_rank(best) {
                    e
                } else {
                    best
                }
            })
            .cloned()
            .unwrap_or_else(|| LOWEST_EFFORT.into())
    };

    let requested = requested?;

    if is_effort_off(&Value::String(requested.to_string())) {
        return Some(lowest(supported));
    }
    if supported.iter().any(|s| s == requested) {
        return Some(requested.to_string());
    }

    let req_rank = effort_rank(requested);
    let at_or_below: Vec<&String> = supported
        .iter()
        .filter(|e| effort_rank(e) <= req_rank)
        .collect();
    if !at_or_below.is_empty() {
        let best = at_or_below
            .into_iter()
            .reduce(|best, e| {
                if effort_rank(e) > effort_rank(best) {
                    e
                } else {
                    best
                }
            })
            .cloned()
            .unwrap_or_else(|| LOWEST_EFFORT.into());
        return Some(best);
    }
    // Requested rank is below every supported level -> the lowest supported.
    Some(lowest(supported))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::static_catalog;

    fn setup() -> (Catalog, ModelTables) {
        (static_catalog(), ModelTables::load())
    }

    #[test]
    fn empty_and_default_resolve_to_the_first_catalog_entry() {
        let (c, t) = setup();
        assert_eq!(resolve_model("", &c, &t), "deepseek/deepseek-v4-pro");
        assert_eq!(resolve_model("default", &c, &t), "deepseek/deepseek-v4-pro");
    }

    #[test]
    fn aliases_are_case_insensitive() {
        let (c, t) = setup();
        for name in ["deepseek-v4-pro", "DEEPSEEK-V4-PRO", "DeepSeek-V4-Pro"] {
            assert_eq!(resolve_model(name, &c, &t), "deepseek/deepseek-v4-pro");
        }
    }

    #[test]
    fn full_ids_pass_through_with_original_casing() {
        let (c, t) = setup();
        assert_eq!(
            resolve_model("DeepSeek/DeepSeek-V4-Pro", &c, &t),
            "DeepSeek/DeepSeek-V4-Pro"
        );
    }

    #[test]
    fn bare_names_match_the_last_segment_case_insensitively() {
        let (c, t) = setup();
        assert_eq!(
            resolve_model("Nemotron-3-Ultra-550B-A55B", &c, &t),
            "nvidia/nemotron-3-ultra-550b-a55b"
        );
    }

    #[test]
    fn no_trim_and_unknown_names_pass_through() {
        let (c, t) = setup();
        assert_eq!(
            resolve_model("unknown-model-xyz", &c, &t),
            "unknown-model-xyz"
        );
        // The trailing space is preserved: the Node version does not trim.
        assert_eq!(
            resolve_model("deepseek-v4-pro ", &c, &t),
            "deepseek-v4-pro "
        );
    }

    #[test]
    fn effort_clipping_collapses_low_bands_for_high_max_models() {
        let (_c, t) = setup();
        let model = "deepseek/deepseek-v4-pro"; // supports [high, max]
        for requested in ["low", "medium", "high", "xhigh"] {
            assert_eq!(
                resolve_effort_for_model(model, Some(requested), &t).as_deref(),
                Some("high"),
                "{requested} should clip to high"
            );
        }
        assert_eq!(
            resolve_effort_for_model(model, Some("max"), &t).as_deref(),
            Some("max")
        );
    }

    #[test]
    fn effort_off_resolves_to_the_lowest_supported() {
        let (_c, t) = setup();
        for marker in ["off", "none", "disabled", "minimal"] {
            assert_eq!(
                resolve_effort_for_model("deepseek/deepseek-v4-pro", Some(marker), &t).as_deref(),
                Some("high")
            );
        }
    }

    #[test]
    fn uncatalogued_model_passes_effort_through_but_never_off() {
        let (_c, t) = setup();
        assert_eq!(
            resolve_effort_for_model("unknown/model", Some("medium"), &t).as_deref(),
            Some("medium")
        );
        assert_eq!(
            resolve_effort_for_model("unknown/model", Some("off"), &t).as_deref(),
            Some("low")
        );
        assert_eq!(resolve_effort_for_model("unknown/model", None, &t), None);
    }

    #[test]
    fn discovery_is_needed_only_for_unresolvable_bare_names() {
        let (c, t) = setup();
        assert!(!needs_catalog_discovery("deepseek-v4-pro", &c, &t));
        assert!(!needs_catalog_discovery("deepseek/deepseek-v4-pro", &c, &t));
        assert!(!needs_catalog_discovery("", &c, &t));
        assert!(needs_catalog_discovery("unknown-model-xyz", &c, &t));
    }

    #[test]
    fn prototype_key_names_do_not_resolve_to_functions() {
        // The Node version indexes a plain object, so "constructor" hits
        // Object.prototype and returns a function (then serialises as {}).
        // Rust's HashMap makes that impossible; this pins the sane behaviour.
        let (c, t) = setup();
        assert_eq!(resolve_model("constructor", &c, &t), "constructor");
        assert_eq!(
            resolve_effort_for_model("constructor", Some("high"), &t).as_deref(),
            Some("high")
        );
    }
}
