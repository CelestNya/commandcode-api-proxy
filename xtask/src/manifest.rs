//! Per-artifact manifests: one `<artifact>.manifest.json` and one
//! `<artifact>.SHA256SUMS` per release artifact. The JSON records the archive
//! itself (name, size, SHA-256) plus every staged file's relative path, size
//! and SHA-256. Two platforms built back-to-back produce two independent files
//! — they never overwrite a shared `SHA256SUMS`.

use std::path::Path;

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::stage::StagedFile;
use crate::Result;

/// Identity of the archive this manifest describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactInfo {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

impl ArtifactInfo {
    /// Hash an on-disk archive file.
    pub fn from_file(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let digest = hasher.finalize();
        Ok(Self {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.display().to_string()),
            size: u64::try_from(bytes.len())?,
            sha256: digest.iter().map(|b| format!("{b:02x}")).collect(),
        })
    }
}

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

/// Write `<out_json>` and `<out_sums>` for one artifact.
pub fn write(
    files: &[StagedFile],
    version: &str,
    platform: &str,
    arch: &str,
    artifact: &ArtifactInfo,
    out_json: &Path,
    out_sums: &Path,
) -> Result<()> {
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
    std::fs::write(out_sums, sums)?;

    let manifest = json!({
        "version": version,
        "platform": platform,
        "arch": arch,
        "artifact": {
            "name": artifact.name,
            "size": artifact.size,
            "sha256": artifact.sha256,
        },
        "files": entries,
    });
    std::fs::write(out_json, serde_json::to_vec_pretty(&manifest)?)?;
    Ok(())
}

/// A parsed manifest, ready for verification.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub version: String,
    pub platform: String,
    pub arch: String,
    pub artifact: ArtifactInfo,
    /// (rel path, size, sha256) as recorded.
    pub files: Vec<(String, u64, String)>,
}

impl Manifest {
    /// Read and parse a manifest JSON file.
    pub fn read(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let v: serde_json::Value = serde_json::from_slice(&bytes)?;
        let version = v
            .get("version")
            .and_then(|x| x.as_str())
            .ok_or("manifest missing version")?
            .to_string();
        let platform = v
            .get("platform")
            .and_then(|x| x.as_str())
            .ok_or("manifest missing platform")?
            .to_string();
        let arch = v
            .get("arch")
            .and_then(|x| x.as_str())
            .ok_or("manifest missing arch")?
            .to_string();
        let artifact_v = v.get("artifact").ok_or("manifest missing artifact")?;
        let artifact = ArtifactInfo {
            name: artifact_v
                .get("name")
                .and_then(|x| x.as_str())
                .ok_or("manifest artifact missing name")?
                .to_string(),
            size: artifact_v
                .get("size")
                .and_then(|x| x.as_u64())
                .ok_or("manifest artifact missing size")?,
            sha256: artifact_v
                .get("sha256")
                .and_then(|x| x.as_str())
                .ok_or("manifest artifact missing sha256")?
                .to_string(),
        };
        let mut files = Vec::new();
        for f in v
            .get("files")
            .and_then(|x| x.as_array())
            .ok_or("manifest missing files")?
        {
            let rel = f
                .get("path")
                .and_then(|x| x.as_str())
                .ok_or("manifest file entry missing path")?
                .to_string();
            let size = f
                .get("size")
                .and_then(|x| x.as_u64())
                .ok_or("manifest file entry missing size")?;
            let sha256 = f
                .get("sha256")
                .and_then(|x| x.as_str())
                .ok_or("manifest file entry missing sha256")?
                .to_string();
            files.push((rel, size, sha256));
        }
        Ok(Self {
            version,
            platform,
            arch,
            artifact,
            files,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xtask-manifest-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn hashes_a_known_file() {
        let dir = tmpdir("hash");
        let p = dir.join("a.txt");
        std::fs::write(&p, b"hello").unwrap();
        let (hash, size) = hash_file(&p).unwrap();
        assert_eq!(size, 5);
        assert_eq!(
            hash,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn write_then_read_round_trip() {
        let dir = tmpdir("roundtrip");
        let staged = dir.join("stage");
        std::fs::create_dir_all(&staged).unwrap();
        let f1 = staged.join("a.txt");
        let f2 = staged.join("sub").join("b.txt");
        stdfs_write(&f1, b"hello");
        stdfs_write(&f2, b"world!!");
        let files = vec![
            StagedFile {
                rel: "a.txt".into(),
                abs: f1,
                mode: 0o644,
            },
            StagedFile {
                rel: "sub/b.txt".into(),
                abs: f2,
                mode: 0o644,
            },
        ];
        let artifact = ArtifactInfo {
            name: "x.zip".into(),
            size: 42,
            sha256: "deadbeef".into(),
        };
        let json = dir.join("x.zip.manifest.json");
        let sums = dir.join("x.zip.SHA256SUMS");
        write(
            &files, "0.0.1", "windows", "x86_64", &artifact, &json, &sums,
        )
        .unwrap();
        let m = Manifest::read(&json).unwrap();
        assert_eq!(m.version, "0.0.1");
        assert_eq!(m.artifact.name, "x.zip");
        assert_eq!(m.files.len(), 2);
        assert_eq!(m.files[0].0, "a.txt");
        assert_eq!(m.files[0].1, 5);
        // sums file exists and lists both files.
        let sums_text = std::fs::read_to_string(&sums).unwrap();
        assert!(sums_text.contains("  a.txt\n"));
        assert!(sums_text.contains("  sub/b.txt\n"));
    }

    fn stdfs_write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
}
