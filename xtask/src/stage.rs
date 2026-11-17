//! Safe staging: materialise one product target under `dist/stage/<target>`.
//!
//! Safety rules (spec §5.2): the output directory is canonicalised and must
//! sit inside the workspace `dist/` subtree — a passed-in repo root or drive
//! root is refused. Copying and deletion use Rust fs APIs, never a shell. The
//! staged tree contains only what the layout names: no logs, no billing
//! database, no `selfcheck.log`, and default configs written fresh (never a
//! copy of the developer's local `ccproxy.json`).

use std::path::{Path, PathBuf};

use crate::layout::{dir_read_recursively, Entry, Layout, Source};
use crate::{ensure_inside, workspace_root, DistError, Result};

/// Stage one target into `out_dir` and return the list of staged relative
/// files (forward-slash, sorted).
pub fn stage(layout: &Layout, target: &str, out_dir: &Path) -> Result<Vec<StagedFile>> {
    if !crate::layout::TARGETS.contains(&target) {
        return Err(DistError(format!("unknown target `{target}`")));
    }
    let root = workspace_root();
    // The output must live under <workspace>/dist. Resolve (creating if needed)
    // and canonicalise the parent before we ever delete it.
    std::fs::create_dir_all(out_dir)?;
    let dist_root = root.join("dist");
    std::fs::create_dir_all(&dist_root)?;
    let canon_out = ensure_inside(&dist_root, out_dir)?;
    if canon_out == dist_root {
        return Err("refusing to stage directly into dist/; choose dist/stage/<target>".into());
    }

    // Wipe a previous staging run of this target, via fs APIs only.
    if canon_out.exists() {
        std::fs::remove_dir_all(&canon_out)?;
    }
    std::fs::create_dir_all(&canon_out)?;

    let mut files = Vec::new();
    for entry in layout.for_target(target) {
        stage_entry(layout, entry, &canon_out, &mut files)?;
    }
    files.sort();
    Ok(files)
}

/// One staged file: relative path (forward slashes), on-disk absolute path,
/// and the mode recorded for the archive (Unix). Windows ignores mode.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct StagedFile {
    pub rel: String,
    pub abs: PathBuf,
    pub mode: u32,
}

fn stage_entry(
    layout: &Layout,
    entry: &Entry,
    out: &Path,
    files: &mut Vec<StagedFile>,
) -> Result<()> {
    match &entry.source {
        Source::Built { built } => {
            let src = crate::release_binary(built)?;
            let dest = out.join(entry.dest());
            copy_one(&src, &dest, out, entry.mode_or(0o755), files)?;
        }
        Source::Generated { generated } => {
            let content = generated_content(generated, &crate::workspace_version()?)?;
            let dest = out.join(entry.dest());
            write_utf8_no_bom(&dest, &content)?;
            files.push(StagedFile {
                rel: entry.dest().to_string(),
                abs: dest,
                mode: entry.mode_or(0o644),
            });
        }
        Source::Tree { tree } => {
            let src_dir = layout.root.join(tree);
            let rels = dir_read_recursively(&src_dir)?;
            for rel in rels {
                let src = src_dir.join(&rel);
                let dest = out.join(entry.dest()).join(&rel);
                copy_one(&src, &dest, out, 0o644, files)?;
            }
            webui_integrity(&src_dir)?;
        }
        Source::File { file } => {
            let src = layout.root.join(file);
            let dest = out.join(entry.dest());
            copy_one(&src, &dest, out, entry.mode_or(0o644), files)?;
        }
    }
    Ok(())
}

trait EntryExt {
    fn dest(&self) -> &str;
    fn mode_or(&self, default: u32) -> u32;
}
impl EntryExt for Entry {
    fn dest(&self) -> &str {
        &self.dest
    }
    fn mode_or(&self, default: u32) -> u32 {
        self.mode.unwrap_or(default)
    }
}

fn copy_one(
    src: &Path,
    dest: &Path,
    base: &Path,
    mode: u32,
    files: &mut Vec<StagedFile>,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dest)
        .map_err(|e| format!("copy {} -> {}: {e}", src.display(), dest.display()))?;
    let rel = dest
        .strip_prefix(base)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .map_err(|_| format!("staged file {} escaped staging root", dest.display()))?;
    files.push(StagedFile {
        rel,
        abs: dest.to_path_buf(),
        mode,
    });
    Ok(())
}

fn write_utf8_no_bom(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

/// Content of the generated Windows metadata files, matching the shipped
/// defaults byte-for-byte (no BOM, exactly what release.yml / build-rust.cmd
/// write today).
fn generated_content(kind: &str, version: &str) -> Result<Vec<u8>> {
    match kind {
        "windows-package-json" => Ok(format!("{{\"version\":\"{version}\"}}").into_bytes()),
        "windows-default-config" => Ok(
            "{\n  \"proxy\": \"default\",\n  \"noProxy\": \"localhost,127.0.0.1\"\n}"
                .as_bytes()
                .to_vec(),
        ),
        other => Err(format!("unknown generated kind `{other}`").into()),
    }
}

/// Parse `index.html` in the staged WebUI tree and confirm every local
/// reference resolves. Reuses `ccproxy::webui_site::referenced_files` so there
/// is no second, looser parser.
fn webui_integrity(webui_dir: &Path) -> Result<()> {
    let index = webui_dir.join("index.html");
    if !index.is_file() {
        return Err(DistError(format!(
            "webui tree at {} has no index.html",
            webui_dir.display()
        )));
    }
    let mut missing = Vec::new();
    for path in ccproxy::webui_site::referenced_files(webui_dir) {
        if path.is_file() {
            continue;
        }
        let rel = path.strip_prefix(webui_dir).unwrap_or(&path);
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.starts_with("data:") || rel.starts_with("http://") || rel.starts_with("https://") {
            continue;
        }
        missing.push(rel);
    }
    if !missing.is_empty() {
        return Err(DistError(format!(
            "webui/index.html references local files that are missing: {missing:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::zip::DEFLATE_METHOD;

    #[test]
    fn generated_files_are_exact_defaults() {
        assert_eq!(
            generated_content("windows-package-json", "0.7.11").unwrap(),
            b"{\"version\":\"0.7.11\"}"
        );
        assert_eq!(
            generated_content("windows-default-config", "0.7.11").unwrap(),
            "{\n  \"proxy\": \"default\",\n  \"noProxy\": \"localhost,127.0.0.1\"\n}".as_bytes()
        );
        // No BOM: the first byte must be `{`.
        assert_eq!(
            generated_content("windows-package-json", "1").unwrap()[0],
            b'{'
        );
    }

    #[test]
    fn zip_method_constant_is_deflate() {
        assert_eq!(DEFLATE_METHOD, 8);
    }
}
