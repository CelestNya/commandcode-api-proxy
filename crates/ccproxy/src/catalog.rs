//! Dynamic model catalog, ported from src/translate/catalog.ts.
//!
//! The CC provider API is the source of truth for which models exist, their
//! display names and their context windows; `models.json` supplies what the API
//! never returns (aliases, reasoning efforts, max output tokens) and doubles as
//! the offline fallback.
//!
//! Two intervals keep the API from being hammered. A successful refresh is good
//! for an hour; after a failed one the API is not retried for 30 seconds, so a
//! proxy that started without a key — or hit a transient outage — still
//! converges on the live list without stalling every request behind a 5s fetch.

use crate::models::{Catalog, ModelMeta};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MODELS_JSON: &str = include_str!("../models.json");
const CATALOG_TTL: Duration = Duration::from_secs(60 * 60);
const CATALOG_FAILURE_RETRY: Duration = Duration::from_secs(30);
const MODELS_FETCH_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

#[derive(Deserialize)]
struct RawMeta {
    #[serde(default)]
    builtin: Vec<String>,
    #[serde(rename = "contextWindows", default)]
    context_windows: HashMap<String, u64>,
    #[serde(rename = "modelNames", default)]
    model_names: HashMap<String, String>,
    #[serde(rename = "closedModelOrgs", default)]
    closed_model_orgs: Vec<String>,
}

fn meta() -> RawMeta {
    serde_json::from_str(MODELS_JSON).unwrap_or(RawMeta {
        builtin: Vec::new(),
        context_windows: HashMap::new(),
        model_names: HashMap::new(),
        closed_model_orgs: Vec::new(),
    })
}

/// Best display name: API name, then static name, then the bare last segment.
fn display_name_for(id: &str, api_name: Option<&str>, names: &HashMap<String, String>) -> String {
    api_name
        .map(str::to_owned)
        .or_else(|| names.get(id).cloned())
        .unwrap_or_else(|| id.rsplit('/').next().unwrap_or(id).to_string())
}

/// CC serves closed models (Anthropic/OpenAI/Google) that this proxy
/// deliberately does not target. They appear bare ("claude-opus-5") and
/// org-prefixed ("google/gemini-3.7-flash").
fn is_closed_model(id: &str, orgs: &HashSet<String>) -> bool {
    let lower = id.to_lowercase();
    if lower.starts_with("claude-") || lower.starts_with("gpt-") {
        return true;
    }
    lower
        .split('/')
        .next()
        .is_some_and(|org| orgs.contains(org))
}

#[derive(Deserialize)]
struct ApiModel {
    id: Option<String>,
    name: Option<String>,
    context_length: Option<u64>,
}

#[derive(Deserialize)]
struct ApiModelsResponse {
    #[serde(default)]
    data: Vec<ApiModel>,
}

impl ApiModelsResponse {
    fn empty() -> Self {
        Self { data: Vec::new() }
    }
}

/// The catalog plus the state that decides when to refresh it.
pub struct CatalogStore {
    inner: Mutex<Inner>,
    /// Where the learned catalog is persisted, if anywhere. Every successful
    /// refresh rewrites it, so a restarted proxy serves the first request from
    /// disk instead of paying a 403 round-trip to learn the names again.
    cache_path: Option<PathBuf>,
}

struct Inner {
    current: Arc<Catalog>,
    last_fetch_at: Option<Instant>,
    last_attempt_at: Option<Instant>,
}

impl Default for CatalogStore {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Serialize, Deserialize)]
struct CachedCatalog {
    ids: Vec<String>,
    names: HashMap<String, String>,
}

impl CatalogStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                current: Arc::new(crate::models::static_catalog()),
                last_fetch_at: None,
                last_attempt_at: None,
            }),
            cache_path: None,
        }
    }

    /// Use `path` as the persistence location, seeding the in-memory catalog
    /// from it when the file parses. A missing, stale, or corrupt file is not
    /// an error — the static catalog is the fallback and the next refresh
    /// rewrites the cache.
    pub fn with_cache(path: PathBuf) -> Self {
        let seeded = read_cache(&path);
        Self {
            inner: Mutex::new(Inner {
                current: Arc::new(seeded.unwrap_or_else(crate::models::static_catalog)),
                last_fetch_at: None,
                last_attempt_at: None,
            }),
            cache_path: Some(path),
        }
    }

    /// The catalog in force right now. Cheap: a clone of an `Arc`.
    pub fn current(&self) -> Arc<Catalog> {
        match self.inner.lock() {
            Ok(g) => Arc::clone(&g.current),
            Err(p) => Arc::clone(&p.into_inner().current),
        }
    }

    /// Refresh from the provider API, subject to the TTL guard.
    ///
    /// Never fails: an unreachable API (or an empty filtered result) leaves the
    /// current catalog in place. The lock is held across the fetch, which gives
    /// the same effect as the Node version's in-flight de-duplication —
    /// concurrent callers wait for the one fetch instead of starting their own.
    pub fn refresh(&self, api_base: &str, api_key: &str) -> Arc<Catalog> {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let now = Instant::now();
        if let Some(last) = guard.last_fetch_at {
            if now.duration_since(last) < CATALOG_TTL {
                return Arc::clone(&guard.current);
            }
        } else if let Some(attempt) = guard.last_attempt_at {
            if now.duration_since(attempt) < CATALOG_FAILURE_RETRY {
                return Arc::clone(&guard.current);
            }
        }
        guard.last_attempt_at = Some(now);

        let merged = fetch_and_merge(api_base, api_key);
        if let Some(models) = merged {
            let catalog = build(models);
            write_cache(self.cache_path.as_deref(), &catalog);
            guard.current = Arc::new(catalog);
            guard.last_fetch_at = Some(Instant::now());
        }
        Arc::clone(&guard.current)
    }
}

fn build(models: Vec<ModelMeta>) -> Catalog {
    Catalog {
        ids: models.iter().map(|m| m.id.clone()).collect(),
        models,
    }
}

/// Where the learned catalog is persisted. Same directory and namespace rules
/// as the billing ledger, so a test instance cannot pollute production.
pub fn cache_file() -> PathBuf {
    crate::billing::billing_dir().join("model-catalog.json")
}

/// Persist `catalog` as bare ids + display names. Atomic: write a temp file
/// then rename, so a reader never sees a half-written cache. Best effort —
/// an unwritable directory costs the next start its warm catalog, nothing more.
fn write_cache(path: Option<&Path>, catalog: &Catalog) {
    let Some(path) = path else { return };
    let cached = CachedCatalog {
        ids: catalog.ids.clone(),
        names: catalog
            .models
            .iter()
            .map(|m| (m.id.clone(), m.display_name.clone()))
            .collect(),
    };
    let Ok(text) = serde_json::to_string(&cached) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Load a persisted catalog. `None` on any failure — the caller falls back to
/// the static catalog and the next refresh rewrites the file.
fn read_cache(path: &Path) -> Option<Catalog> {
    let text = std::fs::read_to_string(path).ok()?;
    let cached: CachedCatalog = serde_json::from_str(&text).ok()?;
    if cached.ids.is_empty() {
        return None;
    }
    let models: Vec<ModelMeta> = cached
        .ids
        .iter()
        .map(|id| ModelMeta {
            id: id.clone(),
            display_name: cached
                .names
                .get(id)
                .cloned()
                .unwrap_or_else(|| id.rsplit('/').next().unwrap_or(id).to_string()),
            context_window: DEFAULT_CONTEXT_WINDOW,
        })
        .collect();
    Some(Catalog {
        ids: cached.ids,
        models,
    })
}

/// Fetch the API list and merge it with the static builtins.
///
/// `None` means "nothing usable came back" — the caller keeps what it has.
fn fetch_and_merge(api_base: &str, api_key: &str) -> Option<Vec<ModelMeta>> {
    let url = format!("{}/provider/v1/models", api_base.trim_end_matches('/'));
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(MODELS_FETCH_TIMEOUT_MS))
        .build();
    let response = agent
        .get(&url)
        .set("Authorization", &format!("Bearer {api_key}"))
        .call();
    // A non-2xx is treated as "nothing usable" rather than an error to surface:
    // the caller keeps the catalog it already has.
    let parsed: ApiModelsResponse = match response {
        Ok(r) => match r.into_string() {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|_| ApiModelsResponse::empty()),
            Err(_) => {
                crate::log::warn("model catalog fetch: response body unreadable");
                return None;
            }
        },
        Err(err) => {
            // A silent failure here reads as "the model does not exist" — log
            // it so an unreachable API is diagnosable from the proxy log.
            crate::log::warn(&format!("model catalog fetch failed: {err}"));
            return None;
        }
    };
    merge(parsed.data)
}

/// Merge an API model list with the static builtins.
///
/// Static entries keep their order and come first, which is what keeps the
/// default model (`ids[0]`) stable across a refresh. `None` means the list
/// contributed nothing usable.
fn merge(api_models: Vec<ApiModel>) -> Option<Vec<ModelMeta>> {
    let raw: RawMeta = meta();
    let orgs: HashSet<String> = raw
        .closed_model_orgs
        .iter()
        .map(|o| o.to_lowercase())
        .collect();

    // Tolerate junk entries rather than letting one malformed record discard
    // the whole refresh.
    let open: Vec<ApiModel> = api_models
        .into_iter()
        .filter(|m| match m.id.as_deref() {
            Some(id) => !is_closed_model(id, &orgs),
            None => false,
        })
        .collect();
    if open.is_empty() {
        return None;
    }

    let mut by_id: HashMap<&str, &ApiModel> = HashMap::new();
    for m in &open {
        if let Some(id) = m.id.as_deref() {
            by_id.insert(id, m);
        }
    }

    let mut merged: Vec<ModelMeta> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for id in &raw.builtin {
        let api = by_id.get(id.as_str());
        merged.push(ModelMeta {
            id: id.clone(),
            display_name: display_name_for(
                id,
                api.and_then(|m| m.name.as_deref()),
                &raw.model_names,
            ),
            context_window: api
                .and_then(|m| m.context_length)
                .or_else(|| raw.context_windows.get(id).copied())
                .unwrap_or(DEFAULT_CONTEXT_WINDOW),
        });
        seen.insert(id.clone());
    }
    for m in &open {
        let Some(id) = m.id.as_deref() else { continue };
        // `seen` also absorbs duplicate ids within the API list itself.
        if !seen.insert(id.to_string()) {
            continue;
        }
        merged.push(ModelMeta {
            id: id.to_string(),
            display_name: display_name_for(id, m.name.as_deref(), &raw.model_names),
            context_window: m.context_length.unwrap_or(DEFAULT_CONTEXT_WINDOW),
        });
    }
    Some(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_static_catalog_is_the_starting_point() {
        let store = CatalogStore::new();
        assert_eq!(store.current().ids.len(), 44);
        assert_eq!(store.current().ids[0], "deepseek/deepseek-v4-pro");
    }

    #[test]
    fn closed_models_are_filtered_out() {
        let orgs: HashSet<String> = ["anthropic", "openai", "google", "gemini"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(is_closed_model("claude-opus-5", &orgs));
        assert!(is_closed_model("gpt-5.5", &orgs));
        assert!(is_closed_model("google/gemini-3.7-flash", &orgs));
        assert!(is_closed_model("OpenAI/gpt-5.5", &orgs));
        assert!(!is_closed_model("deepseek/deepseek-v4-pro", &orgs));
        assert!(!is_closed_model("zai-org/GLM-5.3", &orgs));
    }

    #[test]
    fn the_merge_keeps_static_order_first_so_the_default_model_is_stable() {
        // The API returns the builtins in a different order, plus a new model.
        // Static order must win, or `ids[0]` — the default model — would move
        // under a client that never asked for a specific one.
        let merged = merge(vec![
            ApiModel {
                id: Some("deepseek/deepseek-v4-flash".into()),
                name: None,
                context_length: None,
            },
            ApiModel {
                id: Some("brand/new-model".into()),
                name: Some("New Model".into()),
                context_length: Some(999),
            },
        ])
        .expect("merge succeeds");
        assert_eq!(merged[0].id, "deepseek/deepseek-v4-pro");
        assert_eq!(merged[1].id, "deepseek/deepseek-v4-flash");
        // New models are appended after every static entry.
        let new = merged
            .iter()
            .find(|m| m.id == "brand/new-model")
            .expect("api-only model present");
        assert_eq!(new.display_name, "New Model");
        assert_eq!(new.context_window, 999);
        assert_eq!(merged.len(), 45);
    }

    #[test]
    fn a_refresh_that_only_returns_closed_models_is_discarded() {
        // CC serves Anthropic/OpenAI/Google models this proxy does not target.
        // A list containing only those must not replace the catalog — an empty
        // result is "nothing usable", not "no models exist".
        assert!(merge(vec![
            ApiModel {
                id: Some("claude-opus-5".into()),
                name: None,
                context_length: None,
            },
            ApiModel {
                id: Some("gpt-5.5".into()),
                name: None,
                context_length: None,
            },
            ApiModel {
                id: Some("google/gemini-3.7-flash".into()),
                name: None,
                context_length: None,
            },
        ])
        .is_none());
        // Junk records with no id at all are skipped, not fatal.
        assert!(merge(vec![ApiModel {
            id: None,
            name: Some("anonymous".into()),
            context_length: None,
        },])
        .is_none());
    }

    #[test]
    fn an_api_entry_overrides_the_static_display_name_and_window() {
        // The API is the source of truth for what it reports; models.json fills
        // the gaps (aliases, efforts, and any field the API omits).
        let merged = merge(vec![ApiModel {
            id: Some("deepseek/deepseek-v4-pro".into()),
            name: Some("Renamed By API".into()),
            context_length: Some(555),
        }])
        .expect("merge succeeds");
        let pro = merged
            .iter()
            .find(|m| m.id == "deepseek/deepseek-v4-pro")
            .expect("static entry present");
        assert_eq!(pro.display_name, "Renamed By API");
        assert_eq!(pro.context_window, 555);
    }

    #[test]
    fn duplicate_api_ids_appear_once() {
        let merged = merge(vec![
            ApiModel {
                id: Some("brand/dupe".into()),
                name: None,
                context_length: None,
            },
            ApiModel {
                id: Some("brand/dupe".into()),
                name: None,
                context_length: None,
            },
        ])
        .expect("merge succeeds");
        let count = merged.iter().filter(|m| m.id == "brand/dupe").count();
        assert_eq!(count, 1);
    }

    #[test]
    fn an_unreachable_api_leaves_the_catalog_alone() {
        // Port 1 is not listening; the refresh must fail quietly.
        let store = CatalogStore::new();
        let after = store.refresh("http://127.0.0.1:1", "key");
        assert_eq!(after.ids.len(), 44);
        assert_eq!(after.ids[0], "deepseek/deepseek-v4-pro");
    }

    #[test]
    fn a_failed_refresh_is_not_retried_on_the_next_calls() {
        // Without the failure window, an unreachable API would be re-fetched —
        // and re-timed-out for 5s — on every single request. The guard is what
        // keeps a down provider from stalling the proxy.
        let store = CatalogStore::new();
        let first = store.refresh("http://127.0.0.1:1", "key");
        // The second call is short-circuited by the failure window, so it does
        // not touch the network at all. That is asserted structurally — a
        // timing assertion would measure how fast this machine refuses a
        // connection, which varies (Windows retries the SYN for ~2s).
        let started = std::time::Instant::now();
        let second = store.refresh("http://127.0.0.1:1", "key");
        let throttled = started.elapsed();
        assert!(
            throttled < Duration::from_millis(100),
            "a throttled refresh must not reach the network, took {throttled:?}"
        );
        assert_eq!(first.ids, second.ids);
        // The same catalog instance, so no reader observes a replacement.
        assert!(std::sync::Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn the_catalog_is_shared_not_copied_per_reader() {
        // Requests read the catalog on every call; cloning the Arc keeps that
        // free, and a refresh is visible to readers that already held one.
        let store = CatalogStore::new();
        let held = store.current();
        assert!(std::sync::Arc::ptr_eq(&held, &store.current()));
        // A failed refresh must not replace what readers are holding.
        let _ = store.refresh("http://127.0.0.1:1", "key");
        assert_eq!(held.ids, store.current().ids);
    }

    #[test]
    fn a_persisted_catalog_is_restored_on_start() {
        // The whole point of the cache: a restarted proxy serves the first
        // request from disk instead of re-learning the names via a 403.
        let dir = std::env::temp_dir().join(format!("cc-catalog-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("model-catalog.json");
        std::fs::write(
            &path,
            r#"{"ids":["deepseek/deepseek-v4.1-flash"],"names":{"deepseek/deepseek-v4.1-flash":"DeepSeek v4.1 Flash"}}"#,
        )
        .unwrap();
        let store = CatalogStore::with_cache(path.clone());
        // The bare name resolves through the persisted entry's last segment.
        let tables = crate::translate::ModelTables::load();
        assert_eq!(
            crate::translate::resolve_model("deepseek-v4.1-flash", &store.current(), &tables),
            "deepseek/deepseek-v4.1-flash"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_or_corrupt_cache_falls_back_to_static() {
        let dir = std::env::temp_dir().join(format!("cc-catalog-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("model-catalog.json");
        std::fs::write(&path, "not json at all").unwrap();
        let store = CatalogStore::with_cache(path);
        // The static catalog is the fallback; the next refresh rewrites the file.
        assert_eq!(store.current().ids, crate::models::static_catalog().ids);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_cache_is_not_a_catalog() {
        let dir = std::env::temp_dir().join(format!("cc-catalog-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("model-catalog.json");
        std::fs::write(&path, r#"{"ids":[],"names":{}}"#).unwrap();
        let store = CatalogStore::with_cache(path);
        assert_eq!(store.current().ids, crate::models::static_catalog().ids);
        std::fs::remove_dir_all(&dir).ok();
    }
}
