//! Verify a staged tree and (for Windows) the produced archive against the
//! contract and its manifest.
//!
//! Verification is not "files are non-empty": it enumerates every archive
//! entry, compares it to the manifest, confirms there is no extra or missing
//! file, and — for Windows — re-reads the archive with `ccproxy::archive::read`
//! so the in-app updater's own strict parser is the ground truth.

use std::collections::BTreeSet;
use std::path::Path;

use crate::stage::StagedFile;
use crate::{DistError, Result};

const WINDOWS_REQUIRED: &[&str] = &[
    "CCProxyTray.exe",
    "package.json",
    "service/ccproxy.exe",
    "service/ccproxy.json",
    "service/webui/index.html",
];
const WINDOWS_TOP: &[&str] = &["CCProxyTray.exe", "package.json", "service"];

/// Verify the staged tree: no disallowed top-level entries, paths safe, and
/// the required anchors present.
pub fn stage(target: &str, files: &[StagedFile]) -> Result<()> {
    if target == "windows-portable" {
        for f in files {
            let name = f.rel.replace('\\', "/");
            if name.starts_with('/') || name.contains(':') || name.split('/').any(|s| s == "..") {
                return Err(DistError(format!("unsafe staged path `{name}`")));
            }
            let top = name.split('/').next().unwrap_or("");
            if !WINDOWS_TOP.contains(&top) {
                return Err(DistError(format!("unexpected top-level entry `{name}`")));
            }
        }
        let present: BTreeSet<&str> = files.iter().map(|f| f.rel.as_str()).collect();
        for anchor in WINDOWS_REQUIRED {
            if !present.contains(anchor) {
                return Err(DistError(format!("missing required anchor `{anchor}`")));
            }
        }
    }
    Ok(())
}

/// Read the produced ZIP with the updater's own reader, then cross-check the
/// entry set against the staged files.
pub fn windows_zip(zip_path: &Path, staged: &[StagedFile]) -> Result<()> {
    let bytes = std::fs::read(zip_path).map_err(|e| format!("read {}: {e}", zip_path.display()))?;
    let entries = ccproxy::archive::read(&bytes)
        .map_err(|e| format!("ccproxy::archive::read rejected the zip: {e}"))?;

    let archived: BTreeSet<String> = entries.iter().map(|e| e.name.clone()).collect();
    let expected: BTreeSet<String> = staged.iter().map(|f| f.rel.clone()).collect();

    let extra: Vec<_> = archived.difference(&expected).collect();
    let missing: Vec<_> = expected.difference(&archived).collect();
    if !extra.is_empty() || !missing.is_empty() {
        return Err(DistError(format!(
            "archive/staging mismatch: extra={extra:?} missing={missing:?}"
        )));
    }
    for e in &entries {
        if e.name.contains('\\') {
            return Err(DistError(format!(
                "archive entry `{}` uses a backslash",
                e.name
            )));
        }
    }
    Ok(())
}
