// Model resolution, aliasing, and discovery against the CC provider API.

import modelsData from "@/models.json" with { type: "json" };
import { getCatalog, refreshCatalog } from "@/translate/catalog.js";
import { isEffortOff } from "@/translate/validation.js";
import { logger } from "@/logger.js";

const BUILTIN_MODELS: string[] = modelsData.builtin;
const SHORT_ALIASES: Record<string, string> = modelsData.shortAliases;
/** Canonical model id → the discrete reasoning-effort levels CC accepts for it. */
const REASONING_EFFORTS: Record<string, string[]> = modelsData.reasoningEfforts ?? {};

/** Rank ordering of effort levels (low → max). Used to clip to the nearest valid. */
const EFFORT_RANK: Record<string, number> = { low: 0, medium: 1, high: 2, xhigh: 3, max: 4 };

/**
 * What an "off" marker becomes when the model's level set is unknown.
 *
 * "low" is the safest concrete level: most models accept it, and CC coerces an
 * unsupported level silently (it does not 400), so sending a level the model
 * can't use is harmless — while sending "off" is a hard 400.
 */
const LOWEST_EFFORT = "low";

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
 *   - An "off"-style marker ("off"/"none"/"disabled"/"minimal") resolves to the
 *     model's lowest supported level. It is not a request for "a little
 *     thinking"; it is a request for none, and the upstream has no such level.
 *     Clipping it by rank instead would sort it *above* nothing and pick a
 *     middle level — the bug behind "I turned thinking off and it still thinks
 *     a lot". The lowest level is also measurably less reasoning than omitting
 *     the field (which hands the choice back to the upstream's default).
 *   - Otherwise clip an unsupported request to the nearest valid level (highest
 *     supported rank ≤ requested, else the lowest supported). E.g.
 *     deepseek-v4-pro supports only {high, max}, so "low"/"medium" → "high",
 *     and "max" stays reachable.
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
  //
  // An "off" marker is the one exception: it must never reach the upstream,
  // catalog or no catalog. The upstream 400s on it, so passing it through on a
  // cold catalog (before the first refresh) would reproduce the very outage
  // this guards against. Pick the lowest *known-rankable* level instead.
  if (!supported || supported.length === 0 || !requested) {
    if (isEffortOff(requested)) return LOWEST_EFFORT;
    return requested;
  }

  const rank = (e: string): number => EFFORT_RANK[e] ?? 2;
  const lowest = (set: string[]): string =>
    set.reduce((best, e) => (rank(e) < rank(best) ? e : best));

  if (isEffortOff(requested)) return lowest(supported);

  if (!(requested in EFFORT_RANK)) {
    logger.warn(`Unknown reasoning_effort "${requested}" for ${canonicalModel}, clipping as "high"`);
  }

  if (supported.includes(requested)) return requested;

  const reqRank = rank(requested);
  const atOrBelow = supported.filter((e) => rank(e) <= reqRank);
  if (atOrBelow.length > 0) {
    return atOrBelow.reduce((best, e) => (rank(e) > rank(best) ? e : best));
  }
  // Requested rank is below every supported level → use the lowest supported.
  return lowest(supported);
}
