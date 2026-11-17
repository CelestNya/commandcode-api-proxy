//! Artifact manifests: a `SHA256SUMS` text file and a JSON manifest listing
//! version, platform, architecture, and every staged file's relative path,
//! size and SHA-256.

use std::path::Path;

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::stage::StagedFile;
use crate::Result;

/// Build (sha256, size) for one staged file.
pub fn hash_file(path: &Path) -> Result<(String, u64)> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    Ok((
        digest.iter().map(|b| format!("{b:02x}")).collect(),
        u64::try_from(bytes.len())?,
    ))
}

/// Write `SHA256SUMS` and the JSON manifest for a staged tree. Returns the JSON
/// manifest bytes.
pub fn write(
    files: &[StagedFile],
    version: &str,
    platform: &str,
    arch: &str,
    out_dir: &Path,
) -> Result<Vec<u8>> {
    let mut sums = String::new();
    let mut entries = Vec::new();
    for file in files {
        let (hash, size) = hash_file(&file.abs)?;
        sums.push_str(&format!("{hash}  {}\n", file.rel));
        entries.push(json!({
            "path": file.rel,
            "size": size,
            "sha256": hash,
        }));
    }
    std::fs::write(out_dir.join("SHA256SUMS"), sums)?;

    let manifest = json!({
        "version": version,
        "platform": platform,
        "arch": arch,
        "files": entries,
    });
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_a_known_file() {
        let dir = std::env::temp_dir().join(format!("xtask-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.txt");
        std::fs::write(&p, b"hello").unwrap();
        let (hash, size) = hash_file(&p).unwrap();
        assert_eq!(size, 5);
        assert_eq!(
            hash,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }
}
