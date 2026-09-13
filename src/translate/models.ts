// Model resolution, aliasing, and discovery against the CC provider API.

import modelsData from "@/models.json" with { type: "json" };
import { getCatalog, refreshCatalog } from "@/translate/catalog.js";
import { logger } from "@/logger.js";

const BUILTIN_MODELS: string[] = modelsData.builtin;
const SHORT_ALIASES: Record<string, string> = modelsData.shortAliases;
/** Canonical model id → the discrete reasoning-effort levels CC accepts for it. */
const REASONING_EFFORTS: Record<string, string[]> = modelsData.reasoningEfforts ?? {};

/** Rank ordering of effort levels (low → max). Used to clip to the nearest valid. */
const EFFORT_RANK: Record<string, number> = { low: 0, medium: 1, high: 2, xhigh: 3, max: 4 };

export function getDefaultModels(): string[] {
  return getCatalog().ids;
}

export function resolveModel(model: string): string {
  if (!model || model === "default") {
    return getCatalog().ids[0] ?? BUILTIN_MODELS[0];
  }
  // Alias lookup is case-insensitive so callers can pass the bare model name
  // with original casing (e.g. "GLM-5.2") as well as the lowercase short alias.
  const aliased = SHORT_ALIASES[model] ?? SHORT_ALIASES[model.toLowerCase()];
  if (aliased) return aliased;
  // Already a full model ID (contains "/") — pass through untouched.
  if (model.includes("/")) return model;
  // Bare name without an org prefix (e.g. "GLM-5.2", "Kimi-K3", or
  // "nemotron-3-ultra-550b-a55b" — which has no short alias). Match it against
  // the (dynamic) catalog by last path segment so it still resolves to a full ID.
  const lower = model.toLowerCase();
  for (const id of getCatalog().ids) {
    const last = id.split("/").pop() ?? id;
    if (last.toLowerCase() === lower) return id;
  }
  return model;
}

/**
 * Whether a request model name would reach CC unresolvable.
 *
 * A bare name (no "/") that matches neither an alias nor any known model is
 * the dangerous case: CC treats an unprefixed name as `anthropic:<name>` and
 * rejects it with 403 FORBIDDEN, even when the model exists upstream. Full ids
 * and aliases pass through untouched, so they never need a discovery refresh.
 */
export function needsCatalogDiscovery(model: string): boolean {
  if (!model || model === "default" || model.includes("/")) return false;
  if (SHORT_ALIASES[model] ?? SHORT_ALIASES[model.toLowerCase()]) return false;
  return resolveModel(model) === model;
}

/**
 * Teach the catalog a bare model name it doesn't know yet.
 *
 * Returns true only when the refresh actually made the name resolvable. A name
 * that stays unknown does not exist upstream, so callers should surface the
 * original failure instead of retrying blindly. Refreshes are TTL-guarded and
 * their failures are throttled, so repeated misses cannot stampede the API.
 */
export async function discoverModel(
  model: string,
  apiBase: string,
  apiKey: string,
): Promise<boolean> {
  if (!needsCatalogDiscovery(model)) return false;
  // Refresh failures are non-fatal: the name simply stays unresolved.
  await refreshCatalog(apiBase, apiKey).catch(() => undefined);
  return !needsCatalogDiscovery(model);
}

/**
 * Resolve a requested reasoning effort for a specific (canonical) model.
 *
 * CC accepts `params.reasoning_effort` but each model supports a different set
 * of levels, and CC silently coerces unsupported values (it does not 400). To
 * honor caller intent without sending a value the model can't use:
 *   - If the model isn't catalogued (no known effort set), pass the request
 *     through unchanged — we don't know better, so don't regress.
 *   - If the model has a known effort set, clip an unsupported request to the
 *     nearest valid level (highest supported rank ≤ requested, else the lowest
 *     supported). E.g. deepseek-v4-pro supports only {high, max}, so a request
 *     for "low"/"medium" becomes "high", and "max" stays reachable.
 *   - If no effort was requested, return undefined and let CC pick its default.
 */
export function resolveEffortForModel(
  canonicalModel: string,
  requested?: string,
): string | undefined {
  const supported = REASONING_EFFORTS[canonicalModel];
  // Uncatalogued/empty effort set, or nothing requested → preserve as-is
  // (undefined or the value). The empty-array guard matters: `[]` is truthy,
  // and reduce() on it below would otherwise throw.
  if (!supported || supported.length === 0 || !requested) return requested;

  if (!(requested in EFFORT_RANK)) {
    logger.warn(`Unknown reasoning_effort "${requested}" for ${canonicalModel}, clipping as "high"`);
  }

  if (supported.includes(requested)) return requested;

  const rank = (e: string): number => EFFORT_RANK[e] ?? 2;
  const reqRank = rank(requested);
  const atOrBelow = supported.filter((e) => rank(e) <= reqRank);
  if (atOrBelow.length > 0) {
    return atOrBelow.reduce((best, e) => (rank(e) > rank(best) ? e : best));
  }
  // Requested rank is below every supported level → use the lowest supported.
  return supported.reduce((best, e) => (rank(e) < rank(best) ? e : best));
}
