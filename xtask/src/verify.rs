//! Verify a staged tree and (for Windows) the produced archive against the
//! contract and its manifest.
//!
//! Verification is not "files are non-empty": it enumerates every archive
//! entry, compares it to the manifest, confirms there is no extra or missing
//! file, and — for Windows — re-reads the archive with `ccproxy::archive::read`
//! so the in-app updater's own strict parser is the ground truth.

use std::collections::BTreeSet;
use std::path::Path;

use crate::manifest::{ArtifactInfo, Manifest};
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

/// Cross-check an on-disk staging tree against a parsed manifest:
/// the file set must match exactly (no extra, no missing) and every file's
/// actual SHA-256 and size must equal the manifest. Optionally also verify the
/// archive file itself matches `manifest.artifact`.
pub fn against_manifest(
    files: &[StagedFile],
    manifest: &Manifest,
    artifact_path: Option<&Path>,
) -> Result<()> {
    let expected: BTreeSet<&str> = manifest.files.iter().map(|(p, _, _)| p.as_str()).collect();
    let actual: BTreeSet<&str> = files.iter().map(|f| f.rel.as_str()).collect();
    let extra: Vec<&str> = actual.difference(&expected).copied().collect();
    let missing: Vec<&str> = expected.difference(&actual).copied().collect();
    if !extra.is_empty() || !missing.is_empty() {
        return Err(DistError(format!(
            "staging/manifest mismatch: extra={extra:?} missing={missing:?}"
        )));
    }
    for (rel, size, sha256) in &manifest.files {
        let staged = files
            .iter()
            .find(|f| f.rel == *rel)
            .ok_or_else(|| DistError(format!("manifest lists `{rel}` but it is not staged")))?;
        let (actual_hash, actual_size) = crate::manifest::hash_file(&staged.abs)?;
        if &actual_hash != sha256 || actual_size != *size {
            return Err(DistError(format!(
                "content drift in `{rel}`: expected {sha256}/{size}, got {actual_hash}/{actual_size}"
            )));
        }
    }
    if let Some(path) = artifact_path {
        let on_disk = ArtifactInfo::from_file(path)?;
        if on_disk.sha256 != manifest.artifact.sha256 || on_disk.size != manifest.artifact.size {
            return Err(DistError(format!(
                "artifact {} drift: expected {}/{}, got {}/{}",
                path.display(),
                manifest.artifact.sha256,
                manifest.artifact.size,
                on_disk.sha256,
                on_disk.size
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("xtask-verify-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn drift_in_one_file_is_rejected() {
        let dir = tmp("drift");
        let a = dir.join("a.txt");
        std::fs::write(&a, b"original").unwrap();
        let files = vec![StagedFile {
            rel: "a.txt".into(),
            abs: a.clone(),
            mode: 0o644,
        }];
        // Manifest built against the original bytes.
        let (hash, size) = crate::manifest::hash_file(&a).unwrap();
        let manifest = Manifest {
            version: "1".into(),
            platform: "windows".into(),
            arch: "x86_64".into(),
            artifact: ArtifactInfo {
                name: "x.zip".into(),
                size: 0,
                sha256: String::new(),
            },
            files: vec![("a.txt".into(), size, hash)],
        };
        // Sanity: passes before drift.
        assert!(against_manifest(&files, &manifest, None).is_ok());
        // Now change the file on disk.
        std::fs::write(&a, b"TAMPERED").unwrap();
        let err = against_manifest(&files, &manifest, None).unwrap_err();
        assert!(err.to_string().contains("content drift"), "got: {err}");
    }

    #[test]
    fn extra_or_missing_file_is_rejected() {
        let dir = tmp("set");
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        std::fs::write(&a, b"aaa").unwrap();
        std::fs::write(&b, b"bbb").unwrap();
        let (ha, sa) = crate::manifest::hash_file(&a).unwrap();
        let (hb, sb) = crate::manifest::hash_file(&b).unwrap();
        let manifest = Manifest {
            version: "1".into(),
            platform: "windows".into(),
            arch: "x86_64".into(),
            artifact: ArtifactInfo {
                name: "x.zip".into(),
                size: 0,
                sha256: String::new(),
            },
            files: vec![("a.txt".into(), sa, ha), ("b.txt".into(), sb, hb)],
        };
        // Staging only has a.txt → b.txt missing.
        let files = vec![StagedFile {
            rel: "a.txt".into(),
            abs: a,
            mode: 0o644,
        }];
        let err = against_manifest(&files, &manifest, None).unwrap_err();
        assert!(err.to_string().contains("mismatch"), "got: {err}");
        assert!(err.to_string().contains("b.txt"), "got: {err}");
    }
}
