// Extract the official reasoning-effort table from the Command Code CLI bundle.
//
// Why this exists
// ---------------
// `/provider/v1/models` does NOT report reasoning efforts — verified 2026-09-15
// against the live API: all 69 entries carry only id/object/created/owned_by/
// name/context_length. A client that meets a new model therefore cannot ask the
// API what levels it supports; the official CLI answers from a table embedded
// in its own bundle. That table is the only authoritative enumeration we have,
// and it is what `src/models.json` must agree with.
//
// How it is read
// --------------
// The CLI ships as a tsup bundle (`dist/cli.mjs`): ~2.5 MB on 16 lines, minified
// identifiers, no sourcemap, no public repo. String literals survive minification
// though, so we locate the table by its shape and resolve the level-set constants
// it references. Nothing here is inferred from names — every value comes from a
// literal in the file.
//
// Usage
// -----
// Refresh the snapshot in one command (downloads the package itself):
//
//   node conformance/extract-official-efforts.mjs
//
// Or read a bundle you already have on disk:
//
//   node conformance/extract-official-efforts.mjs --bundle <path/to/dist/cli.mjs>
//
// Writes conformance/official-efforts.json, which tests/effort-table.test.ts
// diffs src/models.json against. Re-run this when the CLI is bumped.

import { readFileSync, writeFileSync, mkdtempSync, rmSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const OUT = path.join(HERE, "official-efforts.json");

const args = process.argv.slice(2);
const flag = (name, fallback) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 ? args[i + 1] : fallback;
};

/**
 * Download `command-code` from the registry and return the path to the bundle
 * inside it. Uses `npm pack` rather than a hand-rolled tarball fetch so the
 * registry URL, integrity check and extraction all stay npm's business.
 * The temp dir is removed by the caller.
 */
function fetchBundle() {
  const dir = mkdtempSync(path.join(tmpdir(), "cc-efforts-"));
  // `shell: true` so the platform's npm shim resolves — on Windows npm is
  // npm.cmd and a bare execFileSync("npm") fails with ENOENT.
  // npm writes the tarball name to stdout; keep it out of our own output.
  const tgz = execFileSync("npm pack command-code --silent", {
    cwd: dir,
    encoding: "utf8",
    shell: true,
  })
    .trim()
    .split("\n")
    .pop();
  if (!tgz || !tgz.endsWith(".tgz")) {
    throw new Error(`npm pack produced no tarball (got ${JSON.stringify(tgz)})`);
  }
  execFileSync("tar xzf " + JSON.stringify(tgz), { cwd: dir, encoding: "utf8", shell: true });
  return { dir, bundle: path.join(dir, "package", "dist", "cli.mjs") };
}

let BUNDLE = flag("bundle", null);
let cleanupDir = null;
if (!BUNDLE) {
  console.error("no --bundle given; fetching command-code from the registry...");
  const fetched = fetchBundle();
  BUNDLE = fetched.bundle;
  cleanupDir = fetched.dir;
}

let src;
try {
  src = readFileSync(BUNDLE, "utf8");
} catch (err) {
  if (cleanupDir) rmSync(cleanupDir, { recursive: true, force: true });
  console.error(`cannot read bundle at ${BUNDLE}: ${err.message}`);
  process.exit(2);
}

/** The five level sets the table references, resolved from their literals. */
function levelSets() {
  const valid = new Set(["low", "medium", "high", "xhigh", "max"]);
  const sets = {};
  // `X=["low","high"]` or `X=new Set(["low","high"])`
  for (const m of src.matchAll(/(?<![A-Za-z0-9_$])([A-Za-z_$][\w$]*)=(new Set\(\[[^\[\]]*\]\)|\[[^\[\]]*\])/g)) {
    const body = m[2].replace(/^new Set\(|\)$/g, "");
    let arr;
    try {
      arr = JSON.parse(body);
    } catch {
      continue;
    }
    // Only keep exact level sets: a same-named unrelated array would be a bug.
    if (arr.length > 0 && arr.every((x) => typeof x === "string" && valid.has(x))) {
      sets[m[1]] = arr;
    }
  }
  return sets;
}

/** Slice a balanced [...] starting at the '[' located at `open`. */
function balanced(src, open) {
  let depth = 0;
  for (let j = open; j < src.length; j++) {
    if (src[j] === "[") depth++;
    else if (src[j] === "]") {
      depth--;
      if (depth === 0) return src.slice(open, j + 1);
    }
  }
  throw new Error("unbalanced brackets — bundle layout changed");
}

const sets = levelSets();
if (Object.keys(sets).length === 0) throw new Error("no level-set constants found — bundle layout changed");

// The table is the only `new Map([[ "<model-id>", <setVar|literal> ], ...])`
// whose values all resolve to level sets. Anchor on the model-id shape.
const mapStart = src.search(/new Map\(\[\["/);
if (mapStart < 0) throw new Error("effort table not found — bundle layout changed");
const openBracket = src.indexOf("[", src.indexOf("(", mapStart));
const table = balanced(src, openBracket);

// Model ids appear either as literals or as single-letter aliases bound once
// near the top (e.g. `dr="MiniMaxAI/MiniMax-M3-Free"`). Resolve those too.
const aliases = {};
for (const m of src.matchAll(/(?<![A-Za-z0-9_$])([A-Za-z_$][\w$]*)="([A-Za-z0-9][\w./:-]*)"/g)) {
  aliases[m[1]] = m[2];
}

const efforts = {};
let unresolved = 0;
for (const m of table.matchAll(/\[\s*(?:"([^"]+)"|([A-Za-z_$][\w$]*))\s*,\s*(?:([A-Za-z_$][\w$]*)|(\[[^\[\]]*\]))\s*\]/g)) {
  const id = m[1] ?? aliases[m[2]];
  const levels = m[3] ? sets[m[3]] : JSON.parse(m[4]);
  if (!id || !levels) {
    unresolved++;
    continue;
  }
  efforts[id] = levels;
}

// Refuse to emit a partial table: a silent gap would make the diff test pass
// on wrong data, which is worse than failing loudly here.
const residue = table
  .replace(/\[\s*(?:"[^"]+"|[A-Za-z_$][\w$]*)\s*,\s*(?:[A-Za-z_$][\w$]*|\[[^\[\]]*\])\s*\]/g, "")
  .replace(/[,\s[\]]+/g, "");
if (unresolved > 0 || residue.length > 0) {
  throw new Error(
    `incomplete extraction (unresolved=${unresolved}, residue=${JSON.stringify(residue)}) — refusing to write a partial table`,
  );
}

// Drop closed models: this proxy does not route claude-*/gpt-* and filters them
// out anyway, so carrying them here would only create diff noise.
const CLOSED_ORGS = new Set(["anthropic", "openai", "google", "gemini"]);
const isClosed = (id) => {
  const l = id.toLowerCase();
  if (l.startsWith("claude-") || l.startsWith("gpt-")) return true;
  return CLOSED_ORGS.has(l.split("/")[0]);
};

const open = Object.fromEntries(Object.entries(efforts).filter(([id]) => !isClosed(id)));
const sorted = Object.fromEntries(Object.keys(open).sort().map((k) => [k, open[k]]));

// Version comes from the tarball's package.json when available.
let version = "unknown";
try {
  const pkg = JSON.parse(readFileSync(path.join(path.dirname(BUNDLE), "..", "package.json"), "utf8"));
  version = pkg.version ?? "unknown";
} catch {
  /* bundle passed directly; version stays unknown */
}

writeFileSync(
  OUT,
  `${JSON.stringify(
    {
      _comment: [
        "Reasoning-effort levels per model, extracted verbatim from the official",
        "Command Code CLI bundle (npm package `command-code`, dist/cli.mjs).",
        "",
        "The upstream /provider/v1/models endpoint does NOT report reasoning",
        "efforts (verified 2026-09-15: all 69 entries carry only id/object/",
        "created/owned_by/name/context_length), so that is not an option. The",
        "CLI's embedded table is the only authoritative enumeration available;",
        "tests/effort-table.test.ts diffs src/models.json against this file.",
        "",
        "GENERATED — do not hand-edit. Refresh with:",
        "  node conformance/extract-official-efforts.mjs",
        "  (fetches command-code from npm; add --bundle <path> to read a local copy)",
        "",
        "Closed models (claude-*/gpt-*) are omitted: this proxy does not route",
        "them, and isClosedModel() filters them out regardless.",
      ],
      source: { package: "command-code", version, bundle: path.basename(BUNDLE) },
      efforts: sorted,
    },
    null,
    2,
  )}\n`,
);

console.log(`wrote ${path.relative(process.cwd(), OUT)}`);
console.log(`  ${Object.keys(sorted).length} open models, from command-code@${version}`);
for (const [k, v] of Object.entries(sorted)) console.log(`    ${k.padEnd(46)} ${v.join(",")}`);

if (cleanupDir) rmSync(cleanupDir, { recursive: true, force: true });
