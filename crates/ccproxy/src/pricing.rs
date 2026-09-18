//! Model price table, maintained by crawling Command Code's public pricing page.
//!
//! Command Code bills the Provider API at the underlying model rates, and its
//! pricing page changes: new models appear, DEAL badges apply temporary
//! multipliers, free models come and go while capacity lasts. A static table
//! in the repo would go stale, so this module fetches the page, parses the
//! per-model rows and caches the result next to the billing database.
//!
//! Refresh policy: on load, if the cached table is missing or older than 24
//! hours, a synchronous fetch replaces it. A failed fetch (offline, page
//! restructured) keeps the previous cache — the proxy must keep working and
//! the ledger keeps its numbers; only the cost columns go missing.
//!
//! Cost model: a request bills cached prompt tokens at the cache-read rate,
//! the remaining prompt at the input rate and completion at the output rate.
//! All rates are USD per million tokens.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where the live prices live. Public, no auth, changes over time — exactly
/// why we crawl instead of vendoring the numbers.
pub const PRICING_SOURCE: &str = "https://commandcode.ai/docs/resources/pricing-limits";
/// Re-fetch when the cached copy is older than this.
const REFRESH_AFTER_SECS: u64 = 24 * 3600;
/// Network budget for one fetch; on timeout the old cache stays.
const FETCH_TIMEOUT: Duration = Duration::from_secs(12);
const USER_AGENT: &str = concat!(
    "ccproxy-pricing/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/CelestNya/commandcode-api-proxy)"
);

/// USD per million tokens for one model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    /// Most open models expose no cache-write rate; `None` means the column
    /// showed "—" on the source page.
    pub cache_write: Option<f64>,
}

/// The full table plus provenance, persisted as `pricing.json`.
#[derive(Debug, Clone)]
pub struct Pricing {
    pub models: BTreeMap<String, ModelPrice>,
    /// Epoch seconds of the last successful fetch; `None` when no cache
    /// exists yet.
    pub fetched_at: Option<u64>,
    pub source: &'static str,
}

impl Pricing {
    /// An empty table for "nothing known yet"; the UI renders no cost columns.
    pub fn empty() -> Self {
        Pricing {
            models: BTreeMap::new(),
            fetched_at: None,
            source: PRICING_SOURCE,
        }
    }

    /// Load the table for `dir`, refreshing the cache when it is missing or
    /// stale. Never panics and never blocks the caller for long: the fetch is
    /// capped at `FETCH_TIMEOUT` and a failure returns the previous cache.
    pub fn load(dir: &Path) -> Pricing {
        let mut cached = read_cache(dir).unwrap_or_else(Pricing::empty);
        if cached.stale() {
            if let Some(fresh) = fetch_latest() {
                cached = fresh;
            }
            write_cache(dir, &cached);
        }
        cached
    }

    /// Rate lookup for an exact model id as recorded in the ledger.
    pub fn price(&self, model: &str) -> Option<ModelPrice> {
        if let Some(p) = self.models.get(model) {
            return Some(*p);
        }
        // Ledger names carry a provider prefix ("deepseek/deepseek-v4.1-flash")
        // while the page names the bare model; try the id after the slash.
        // Normalize both sides ("DeepSeek V4.1 Flash" → "deepseek-v4.1-flash").
        // Exact equality only, so "gpt-5" can never match "gpt-5-pro".
        let want = normalize(model);
        self.find_normalized(&want).or_else(|| {
            // Ledger names carry a provider prefix ("deepseek/deepseek-v4.1-flash");
            // retry with the bare id.
            model
                .rsplit_once('/')
                .map(|(_, id)| normalize(id))
                .and_then(|want| self.find_normalized(&want))
        })
    }

    fn find_normalized(&self, want: &str) -> Option<ModelPrice> {
        self.models
            .iter()
            .find(|(k, _)| normalize(k) == *want)
            .map(|(_, v)| *v)
    }

    /// Cost in USD of one ledger row: cache-read for the cached portion of
    /// the prompt, input for the rest, output for completion tokens.
    pub fn cost(&self, model: &str, prompt: i64, cached: i64, completion: i64) -> Option<f64> {
        let p = self.price(model)?;
        let uncached = prompt.saturating_sub(cached).max(0) as f64;
        let cached_n = cached.max(0) as f64;
        let completion_n = completion.max(0) as f64;
        Some((p.input * uncached + p.cache_read * cached_n + p.output * completion_n) / 1e6)
    }

    fn stale(&self) -> bool {
        match self.fetched_at {
            None => true,
            Some(ts) => crate::now_epoch_secs().saturating_sub(ts) > REFRESH_AFTER_SECS,
        }
    }
}

/// Fetch and parse the current table from the official page.
pub fn fetch_latest() -> Option<Pricing> {
    let resp = ureq::get(PRICING_SOURCE)
        .set("User-Agent", USER_AGENT)
        .timeout(FETCH_TIMEOUT)
        .call()
        .ok()?;
    if resp.status() != 200 {
        return None;
    }
    let mut body = String::new();
    resp.into_reader().read_to_string(&mut body).ok()?;
    let models = parse_pricing_html(&body);
    if models.is_empty() {
        return None; // the page no longer parses — keep whatever we had
    }
    Some(Pricing {
        models,
        fetched_at: Some(crate::now_epoch_secs()),
        source: PRICING_SOURCE,
    })
}

/// Parse the pricing table out of the raw page.
///
/// The table is a Tailwind grid; every row is
/// `<div role="row">…</div>` with the model name first and then five
/// `px-2 py-3` cells: context window, input/M, output/M, cache-read/M and
/// cache-write/M (the last one is often "—"). DEAL rows strike the old price
/// and show the discounted one in a `font-semibold` span, free models put
/// "Free" in the price cells. Parsing is deliberately format-driven and
/// strict: any row that does not yield an input and output rate is skipped,
/// so a restructured page degrades to fewer rows instead of wrong numbers.
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
pub fn parse_pricing_html(html: &str) -> BTreeMap<String, ModelPrice> {
    let mut out = BTreeMap::new();
    let mut rest = html;
    while let Some(p) = rest.find("role=\"row\"") {
        let row_start = p + "role=\"row\"".len();
        let body = &rest[row_start..];
        let row_end = body.find("role=\"row\"").unwrap_or(body.len());
        if let Some((name, price)) = parse_row(&body[..row_end]) {
            out.insert(name, price);
        }
        rest = &body[row_end..];
    }
    out
}

/// One `<div role="row">` body → `(model id, price)`, or `None` when the row
/// is the header or has been restructured beyond recognition.
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
fn parse_row(row: &str) -> Option<(String, ModelPrice)> {
    let marker = "<span class=\"truncate text-[13px] font-medium";
    let mi = row.find(marker)?;
    let after = &row[mi + marker.len()..];
    let name = name_text(after)?;
    if name.is_empty() || name == "Model" {
        return None;
    }
    // cells[0] is the context-window cell (text-[11px]); the four price
    // cells follow. Table headers also carry px-2 py-3 cells but no prices.
    let cells = div_cells(row);
    if cells.len() < 5 {
        return None;
    }
    let input = cell_price(cells[1])?;
    let output = cell_price(cells[2])?;
    let cache_read = cell_price(cells[3])?;
    let cache_write = cell_price(cells[4]);
    Some((
        name,
        ModelPrice {
            input,
            output,
            cache_read,
            cache_write,
        },
    ))
}

/// Text of the model-name span, tolerating an inner `<a>` (DEAL rows wrap
/// the name in a link).
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
fn name_text(after: &str) -> Option<String> {
    let open_end = after.find('>')?;
    let mut name = &after[open_end + 1..];
    if name.starts_with("<a") {
        let link_end = name.find('>')?;
        name = &name[link_end + 1..];
    }
    let close = name.find('<')?;
    let text = name[..close].trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Split the row into its `px-2 py-3` / `px-3 py-3` cells, in document order
/// (the name cell is `px-4` and is intentionally not captured). Deal rows
/// wrap price columns in `flex flex-col items-end px-2 py-3`, so the match
/// is on the class *containing* the padding token, not on a fixed prefix.
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
fn div_cells(row: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    let mut rest = row;
    while let Some(p) = next_cell(rest) {
        let open_end = rest[p..].find('>').map(|j| p + j + 1).unwrap_or(rest.len());
        rest = &rest[open_end..];
        let end = next_cell(rest).unwrap_or(rest.len());
        out.push(&rest[..end]);
        rest = &rest[end..];
    }
    out
}

/// Byte offset (relative to `rest`) of the next `px-2/px-3 py-3` cell opening
/// div. The scan advances internally, so the returned offset is the base
/// offset plus the in-slice position — callers index `rest` with it.
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
fn next_cell(rest: &str) -> Option<usize> {
    let mut scan = rest;
    let mut base = 0;
    loop {
        let p = scan.find("<div class=\"")?;
        let val_start = scan[p..].find('"').map(|j| p + j + 1)?;
        let val_end = scan[val_start..].find('"').map(|j| val_start + j)?;
        let class_attr = &scan[val_start..val_end];
        if class_attr.contains("px-2 py-3") || class_attr.contains("px-3 py-3") {
            return Some(base + p);
        }
        let next = val_end + 1;
        base += next;
        scan = &scan[next..];
    }
}

/// Numeric value of one price cell, in USD per million tokens.
///
/// `Free` → 0; a plain `$0.15` → 0.15; a DEAL cell shows the struck old
/// price first and the discounted one second, so the last `$` amount wins;
/// "—" or any non-price text → `None`.
/// Lowercase the name and fold separators so page names and API ids meet:
/// "DeepSeek V4.1 Flash" -> "deepseek-v4.1-flash".
#[allow(clippy::arithmetic_side_effects)]
fn normalize(name: &str) -> String {
    // Page names carry editorial suffixes ("DeepSeek V4 Flash (latest)") that
    // have no API-id counterpart; drop parenthesised segments first, then
    // fold every remaining separator to '-'.
    let mut out = String::with_capacity(name.len());
    let mut depth = 0usize;
    for c in name.trim().chars() {
        match c {
            // The segment's own leading separator ("V4 Flash (latest)")
            // belongs to the suffix, not to the id: drop it with the parens.
            '(' => {
                if out.ends_with([' ', '-']) {
                    out.pop();
                }
                depth += 1;
            }
            ')' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.to_ascii_lowercase().replace([' ', '/', '\\', '_'], "-")
}
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
fn cell_price(cell: &str) -> Option<f64> {
    if cell.contains("uppercase") && cell.contains("Free") {
        return Some(0.0);
    }
    let mut last: Option<f64> = None;
    let mut rest = cell;
    while let Some(d) = rest.find('$') {
        let after = &rest[d + 1..];
        let num_end = after
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(after.len());
        if let Ok(v) = after[..num_end].parse::<f64>() {
            last = Some(v);
        }
        rest = after;
    }
    last
}

// ── cache ───────────────────────────────────────────────────────

fn cache_path(dir: &Path) -> PathBuf {
    dir.join("pricing.json")
}

fn read_cache(dir: &Path) -> Option<Pricing> {
    let text = fs::read_to_string(cache_path(dir)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let fetched_at = v.get("fetchedAt").and_then(|x| x.as_u64());
    let models = v.get("models")?.as_object()?;
    let mut map = BTreeMap::new();
    for (k, mv) in models {
        let input = mv.get("input")?.as_f64()?;
        let output = mv.get("output")?.as_f64()?;
        let cache_read = mv.get("cacheRead")?.as_f64()?;
        let cache_write = mv.get("cacheWrite").and_then(|x| x.as_f64());
        map.insert(
            k.clone(),
            ModelPrice {
                input,
                output,
                cache_read,
                cache_write,
            },
        );
    }
    Some(Pricing {
        models: map,
        fetched_at,
        source: PRICING_SOURCE,
    })
}

fn write_cache(dir: &Path, p: &Pricing) {
    let mut models = serde_json::Map::new();
    for (k, m) in &p.models {
        models.insert(
            k.clone(),
            json!({
                "input": m.input,
                "output": m.output,
                "cacheRead": m.cache_read,
                "cacheWrite": m.cache_write,
            }),
        );
    }
    let v = json!({
        "fetchedAt": p.fetched_at,
        "source": p.source,
        "models": models,
    });
    if let Ok(text) = serde_json::to_string(&v) {
        let _ = fs::write(cache_path(dir), text);
    }
}

// ── tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A faithful miniature of the Tailwind grid rows, covering a plain row,
    /// a DEAL row (struck price + discounted), a free row and the header.
    const SAMPLE: &str = r#"
<div role="row">
<div class="flex min-w-0 items-center gap-2 px-4 py-3"><span class="truncate text-[13px] font-medium normal-case tracking-normal text-foreground">DeepSeek V4.1 Flash</span></div>
<div class="px-2 py-3 text-right text-[11px] text-muted-foreground tabular-nums">1M</div>
<div class="px-2 py-3 text-right text-[12px] text-foreground tabular-nums"><span class="underline decoration-dotted"><span>$0.15</span></span></div>
<div class="px-2 py-3 text-right text-[12px] text-foreground tabular-nums"><span><span>$0.60</span></span></div>
<div class="px-2 py-3 text-right text-[12px] text-foreground tabular-nums"><span><span>$0.003</span></span></div>
<div class="px-2 py-3 text-right text-[12px] tabular-nums"><span class="text-muted-foreground">—</span></div>
<div class="px-3 py-3"><button type="button" aria-label="Capabilities: Text input, Reasoning"></button></div>
</div>
<div role="row">
<div class="flex min-w-0 items-center gap-2 px-4 py-3"><span class="truncate text-[13px] font-medium normal-case tracking-normal text-foreground underline"><a data-state="closed">MiniMax M3</a></span><a aria-label="View MiniMax M3 deal details" class="shrink-0 border border-border bg-foreground px-1.5 py-0.5 text-[10px] font-semibold">-50%</a></div>
<div class="px-2 py-3 text-right text-[11px] text-muted-foreground tabular-nums">1M</div>
<div class="flex flex-col items-end px-2 py-3 text-[12px] leading-tight tabular-nums"><s class="text-[11px] text-muted-foreground">$0.60</s><span class="font-semibold text-foreground">$0.30</span></div>
<div class="flex flex-col items-end px-2 py-3 text-[12px] leading-tight tabular-nums"><s class="text-[11px] text-muted-foreground">$2.40</s><span class="font-semibold text-foreground">$1.20</span></div>
<div class="flex flex-col items-end px-2 py-3 text-[12px] leading-tight tabular-nums"><s class="text-[11px] text-muted-foreground">$0.12</s><span class="font-semibold text-foreground">$0.06</span></div>
<div class="px-2 py-3 text-right text-[12px] tabular-nums"><span class="text-muted-foreground">—</span></div>
<div class="px-3 py-3"><button type="button"></button></div>
</div>
<div role="row">
<div class="flex min-w-0 items-center gap-2 px-4 py-3"><span class="truncate text-[13px] font-medium normal-case tracking-normal text-foreground underline"><a data-state="closed">Laguna S 2.1</a></span><a aria-label="View Laguna S 2.1 deal details" class="shrink-0 border border-border bg-foreground px-1.5 py-0.5 text-[10px] font-semibold">FREE</a></div>
<div class="px-2 py-3 text-right text-[11px] text-muted-foreground tabular-nums">256K</div>
<div class="px-2 py-3 text-right text-[12px] tabular-nums"><span class="font-semibold uppercase tracking-wide text-foreground">Free</span></div>
<div class="px-2 py-3 text-right text-[12px] tabular-nums"><span class="font-semibold uppercase tracking-wide text-foreground">Free</span></div>
<div class="px-2 py-3 text-right text-[12px] tabular-nums"><span class="font-semibold uppercase tracking-wide text-foreground">Free</span></div>
<div class="px-2 py-3 text-right text-[12px] tabular-nums"><span class="text-muted-foreground">—</span></div>
<div class="px-3 py-3"><button type="button"></button></div>
</div>
<div role="row">
<div class="flex min-w-0 items-center gap-2 px-4 py-3"><span class="truncate text-[13px] font-medium normal-case tracking-normal text-foreground">Model</span></div>
<div class="px-2 py-3 text-right text-[11px] text-muted-foreground tabular-nums">Context</div>
<div class="px-2 py-3 text-right text-[12px] text-foreground tabular-nums">Input/M</div>
<div class="px-2 py-3 text-right text-[12px] text-foreground tabular-nums">Output/M</div>
<div class="px-2 py-3 text-right text-[12px] text-foreground tabular-nums">Cache Read</div>
<div class="px-2 py-3 text-right text-[12px] tabular-nums">Cache Write</div>
<div class="px-3 py-3"><button type="button"></button></div>
</div>
"#;

    #[test]
    fn parses_plain_deal_and_free_rows_skips_header() {
        let m = parse_pricing_html(SAMPLE);
        assert_eq!(m.len(), 3);
        let dsv = m.get("DeepSeek V4.1 Flash").unwrap();
        assert!((dsv.input - 0.15).abs() < 1e-9);
        assert!((dsv.output - 0.60).abs() < 1e-9);
        assert!((dsv.cache_read - 0.003).abs() < 1e-9);
        assert!(dsv.cache_write.is_none());
        let mm = m.get("MiniMax M3").unwrap();
        // DEAL: the discounted price wins over the struck one.
        assert!((mm.input - 0.30).abs() < 1e-9);
        assert!((mm.output - 1.20).abs() < 1e-9);
        assert!((mm.cache_read - 0.06).abs() < 1e-9);
        let lag = m.get("Laguna S 2.1").unwrap();
        assert_eq!(lag.input, 0.0);
        assert_eq!(lag.output, 0.0);
        assert_eq!(lag.cache_read, 0.0);
    }

    #[test]
    fn empty_page_parses_to_empty() {
        assert!(parse_pricing_html("<html>no table here</html>").is_empty());
        assert!(parse_pricing_html("").is_empty());
    }

    #[test]
    fn price_matches_provider_prefixed_and_normalized_names() {
        let pr = Pricing {
            models: BTreeMap::from([(
                "DeepSeek V4.1 Flash".to_string(),
                ModelPrice {
                    input: 0.28,
                    output: 0.42,
                    cache_read: 0.028,
                    cache_write: None,
                },
            )]),
            fetched_at: Some(1),
            source: PRICING_SOURCE,
        };
        // exact page name
        assert_eq!(pr.price("DeepSeek V4.1 Flash").unwrap().input, 0.28);
        // provider-prefixed ledger id
        assert_eq!(
            pr.price("deepseek/deepseek-v4.1-flash").unwrap().input,
            0.28
        );
        // bare ledger id
        assert_eq!(pr.price("deepseek-v4.1-flash").unwrap().input, 0.28);
        // unrelated name must not match
        assert!(pr.price("gpt-5").is_none());
    }

    #[test]
    fn normalize_drops_editorial_suffixes() {
        assert_eq!(normalize("DeepSeek V4 Flash (latest)"), "deepseek-v4-flash");
        assert_eq!(
            normalize("DeepSeek V4 Flash Vision (exp)"),
            "deepseek-v4-flash-vision"
        );
        assert_eq!(
            normalize("deepseek/deepseek-v4.1-flash"),
            "deepseek-deepseek-v4.1-flash"
        );
        assert_eq!(normalize("MiniMax M3"), "minimax-m3");
    }

    #[test]
    fn cost_uses_cache_read_for_cached_tokens() {
        let m = parse_pricing_html(SAMPLE);
        let p = Pricing {
            models: m,
            fetched_at: None,
            source: PRICING_SOURCE,
        };
        // 1000 prompt, 800 cached, 100 completion on DeepSeek V4.1 Flash:
        // 200*0.15 + 800*0.003 + 100*0.60 = 30 + 2.4 + 60 = 92.4e-6 USD.
        let c = p.cost("DeepSeek V4.1 Flash", 1000, 800, 100).unwrap();
        assert!((c - 92.4e-6).abs() < 1e-12);
        // Unknown model → no cost.
        assert!(p.cost("nope", 1, 0, 1).is_none());
        // Cached > prompt cannot go negative: clamp keeps it sane.
        let c2 = p.cost("DeepSeek V4.1 Flash", 100, 800, 0).unwrap();
        assert!(c2 >= 0.0);
        // Free model costs nothing.
        let c3 = p.cost("Laguna S 2.1", 1000, 0, 100).unwrap();
        assert_eq!(c3, 0.0);
    }

    #[test]
    fn cache_round_trips() {
        let dir = std::env::temp_dir().join("ccproxy-pricing-test");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let m = parse_pricing_html(SAMPLE);
        let p = Pricing {
            models: m,
            fetched_at: Some(1234),
            source: PRICING_SOURCE,
        };
        write_cache(&dir, &p);
        let back = read_cache(&dir).unwrap();
        assert_eq!(back.fetched_at, Some(1234));
        assert_eq!(back.models.len(), 3);
        assert_eq!(
            back.models.get("MiniMax M3").unwrap().input,
            p.models.get("MiniMax M3").unwrap().input
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_depends_on_age() {
        let m = parse_pricing_html(SAMPLE);
        let fresh = Pricing {
            models: m.clone(),
            fetched_at: Some(crate::now_epoch_secs()),
            source: PRICING_SOURCE,
        };
        assert!(!fresh.stale());
        let old = Pricing {
            models: m,
            fetched_at: Some(crate::now_epoch_secs().saturating_sub(REFRESH_AFTER_SECS + 10)),
            source: PRICING_SOURCE,
        };
        assert!(old.stale());
        let none = Pricing {
            models: BTreeMap::new(),
            fetched_at: None,
            source: PRICING_SOURCE,
        };
        assert!(none.stale());
    }
}
