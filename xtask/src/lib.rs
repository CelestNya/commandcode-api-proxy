//! Declarative staging, archiving, manifest and verification for cc-proxy.
//!
//! The single packaging entry point shared by local builds and CI. Nothing
//! here is shipped in a release artifact. The layout of every product is
//! described once in `packaging/layout.toml`; this crate only executes that
//! description safely and deterministically.

#![forbid(unsafe_code)]

pub mod archive;
pub mod deb;
pub mod layout;
pub mod manifest;
pub mod stage;
pub mod verify;

use std::path::{Path, PathBuf};

/// Any failure of a packaging step. A dev tool may box errors at the boundary;
/// what matters is that none of them is a panic on bad input.
pub type Result<T> = std::result::Result<T, DistError>;

/// A packaging error, carried as a human-readable string. Hand-rolled rather
/// than `anyhow`/`thiserror` to match the workspace's no-extra-error-stack rule.
#[derive(Debug, Clone)]
pub struct DistError(pub String);

impl std::fmt::Display for DistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DistError {}

impl From<String> for DistError {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for DistError {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<std::io::Error> for DistError {
    fn from(value: std::io::Error) -> Self {
        Self(value.to_string())
    }
}

impl From<toml::de::Error> for DistError {
    fn from(value: toml::de::Error) -> Self {
        Self(value.to_string())
    }
}

impl From<serde_json::Error> for DistError {
    fn from(value: serde_json::Error) -> Self {
        Self(value.to_string())
    }
}

impl From<std::num::TryFromIntError> for DistError {
    fn from(value: std::num::TryFromIntError) -> Self {
        Self(value.to_string())
    }
}

impl From<std::path::StripPrefixError> for DistError {
    fn from(value: std::path::StripPrefixError) -> Self {
        Self(value.to_string())
    }
}

/// The workspace root, resolved at compile time from this crate's manifest dir
/// (`<root>/xtask/`), so commands work no matter where cargo runs them.
pub fn workspace_root() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    // CARGO_MANIFEST_DIR is `<workspace>/xtask`; parent() is always Some there.
    match dir.parent() {
        Some(p) => p.to_path_buf(),
        None => dir.to_path_buf(),
    }
}

/// The cargo target directory, honouring `CARGO_TARGET_DIR` exactly as the
/// build does, so the release binaries we stage are the ones just built.
pub fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target"))
}

/// The workspace version, the single source of truth for artifact names.
///
/// Read from `[workspace.package] version` in the root manifest and checked for
/// a `x.y.z` shape; it must never be inferred from a local `package.json`.
pub fn workspace_version() -> Result<String> {
    let manifest = std::fs::read_to_string(workspace_root().join("Cargo.toml"))?;
    let value: toml::Value = toml::from_str(&manifest)?;
    let version = value
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .ok_or("workspace Cargo.toml has no [workspace.package] version")?;
    check_semver(version)?;
    Ok(version.to_string())
}

/// Require a `major.minor.patch` shape. Pre-release/build metadata is rejected
/// for now — this project tags plain releases — so a junk value cannot leak
/// into an artifact name.
fn check_semver(version: &str) -> Result<()> {
    let mut parts = version.split('.');
    for _ in 0..3 {
        let part = parts
            .next()
            .ok_or_else(|| format!("version `{version}` is not major.minor.patch"))?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(DistError(format!(
                "version `{version}` has a non-numeric component"
            )));
        }
    }
    if parts.next().is_some() {
        return Err(DistError(format!(
            "version `{version}` has extra components"
        )));
    }
    Ok(())
}

/// Path of a built release binary, refusing to silently fall back to a debug
/// build. On Windows the on-disk name carries `.exe`; on Unix it does not.
pub fn release_binary(name: &str) -> Result<PathBuf> {
    let file_name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let path = target_dir().join("release").join(file_name);
    if !path.is_file() {
        return Err(DistError(format!(
            "release binary not found: {} (build with `cargo build --release` first)",
            path.display()
        )));
    }
    Ok(path)
}

/// Canonicalize `dir` and confirm it (or a required parent) sits inside
/// `expected_root`. Used before deleting or writing an output directory so a
/// passed-in path can never escape the workspace.
pub fn ensure_inside(root: &Path, dir: &Path) -> Result<PathBuf> {
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("cannot canonicalize workspace root {}: {e}", root.display()))?;
    let canon_dir = dir
        .canonicalize()
        .map_err(|e| format!("cannot canonicalize output {}: {e}", dir.display()))?;
    if !canon_dir.starts_with(&canon_root) {
        return Err(DistError(format!(
            "output {} is outside the workspace {} — refusing to touch it",
            dir.display(),
            canon_root.display()
        )));
    }
    Ok(canon_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_accepts_dotted_numbers_only() {
        assert!(check_semver("0.7.11").is_ok());
        assert!(check_semver("1.2.3").is_ok());
        for bad in [
            "",
            "0.7",
            "0.7.x",
            "v0.7.11",
            "0.7.11-rc1",
            "0.7.11.1",
            "a.b.c",
        ] {
            assert!(check_semver(bad).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn workspace_version_reads_the_root_manifest() {
        let v = workspace_version().expect("version present");
        assert_eq!(v, "0.7.11");
    }
}
