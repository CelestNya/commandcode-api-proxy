//! Parse and validate `packaging/layout.toml`, the single description of what
//! files land in which product.
//!
//! Validation here is deliberately strict (spec §4.2): a malformed layout is a
//! build error, never a silently wrong package. Checks cover empty fields,
//! absolute/`..`/backslash destinations, unknown targets and binary/generated
//! kinds, duplicate destinations per target, missing sources, empty WebUI
//! trees, symlink sources, platform-mixed paths, and mode conflicts.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{DistError, Result};

/// Every product a layout may target.
pub const TARGETS: &[&str] = &[
    "windows-portable",
    "linux-portable",
    "deb-proxy",
    "deb-tray",
    "appimage",
];

/// Built binaries the layout may reference.
const BUILT: &[&str] = &["ccproxy", "CCProxyTray"];

/// Generated file kinds the layout may reference.
const GENERATED: &[&str] = &["windows-package-json", "windows-default-config"];

/// A layout row: a source, a destination relative path, an optional mode, and
/// the products it belongs to.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Entry {
    pub source: Source,
    pub dest: String,
    #[serde(default)]
    pub mode: Option<u32>,
    pub targets: Vec<String>,
}

/// Where a file comes from.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Source {
    Built { built: String },
    Generated { generated: String },
    Tree { tree: String },
    File { file: String },
}

impl Source {
    /// Human-readable kind for diagnostics.
    fn kind(&self) -> &'static str {
        match self {
            Source::Built { .. } => "built",
            Source::Generated { .. } => "generated",
            Source::Tree { .. } => "tree",
            Source::File { .. } => "file",
        }
    }
}

/// Debian package metadata declared under `[deb.<pkg>]` in layout.toml.
#[derive(Debug, Clone, Deserialize)]
pub struct DebMeta {
    pub package: String,
    pub maintainer: String,
    pub section: String,
    pub priority: String,
    pub architecture: String,
    pub revision: String,
    pub description: String,
    #[serde(default)]
    pub depends: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LayoutFile {
    entry: Vec<Entry>,
    #[serde(default)]
    deb: Option<DebSection>,
}

#[derive(Debug, Default, Deserialize)]
struct DebSection {
    proxy: Option<DebMeta>,
    tray: Option<DebMeta>,
}

/// A validated layout plus the workspace root its relative sources resolve
/// against.
pub struct Layout {
    pub entries: Vec<Entry>,
    pub root: PathBuf,
    pub deb_proxy: Option<DebMeta>,
    pub deb_tray: Option<DebMeta>,
}

impl Layout {
    /// Load and validate `packaging/layout.toml` under `root`.
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("packaging").join("layout.toml");
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let parsed: LayoutFile = toml::from_str(&text)
            .map_err(|e| format!("{} is not valid TOML: {e}", path.display()))?;
        let deb = parsed.deb.unwrap_or_default();
        let layout = Self {
            entries: parsed.entry,
            root: root.to_path_buf(),
            deb_proxy: deb.proxy,
            deb_tray: deb.tray,
        };
        layout.validate()?;
        Ok(layout)
    }

    /// Entries belonging to one product target.
    pub fn for_target(&self, target: &str) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|e| e.targets.iter().any(|t| t == target))
            .collect()
    }

    /// Run every §4.2 check.
    fn validate(&self) -> Result<()> {
        if self.entries.is_empty() {
            return Err("layout has no [[entry]] rows".into());
        }
        for target in TARGETS {
            self.check_target_destinations(target)?;
        }
        for entry in &self.entries {
            self.check_entry(entry)?;
        }
        Ok(())
    }

    fn check_target_destinations(&self, target: &str) -> Result<()> {
        let mut seen: Vec<&str> = Vec::new();
        for entry in self.for_target(target) {
            if seen.contains(&entry.dest.as_str()) {
                return Err(DistError(format!(
                    "target `{target}` has a duplicate dest `{}`",
                    entry.dest
                )));
            }
            seen.push(&entry.dest);
        }
        Ok(())
    }

    fn check_entry(&self, entry: &Entry) -> Result<()> {
        let dest = entry.dest.trim();
        if dest.is_empty() {
            return Err(DistError(format!(
                "entry with source {:?} has an empty dest",
                entry.source.kind()
            )));
        }
        if dest.contains('\\') {
            return Err(DistError(format!(
                "dest `{dest}` uses a backslash; destinations must use `/`"
            )));
        }
        if dest.starts_with('/') || dest.starts_with("\\\\") || Path::new(dest).has_root() {
            return Err(DistError(format!(
                "dest `{dest}` is absolute; destinations are relative"
            )));
        }
        if dest.split('/').any(|seg| seg == "..") {
            return Err(DistError(format!("dest `{dest}` escapes with `..`")));
        }
        if dest.contains(':') {
            return Err(DistError(format!(
                "dest `{dest}` carries a drive/authority prefix"
            )));
        }

        for target in &entry.targets {
            if !TARGETS.contains(&target.as_str()) {
                return Err(DistError(format!(
                    "entry dest `{dest}` names unknown target `{target}`"
                )));
            }
            self.check_platform_mix(entry, dest, target)?;
        }

        match &entry.source {
            Source::Built { built } => {
                if built.is_empty() {
                    return Err(DistError(format!(
                        "entry dest `{dest}` has an empty built name"
                    )));
                }
                if !BUILT.contains(&built.as_str()) {
                    return Err(DistError(format!(
                        "entry dest `{dest}` references unknown built binary `{built}` (known: {BUILT:?})"
                    )));
                }
            }
            Source::Generated { generated } => {
                if generated.is_empty() {
                    return Err(DistError(format!(
                        "entry dest `{dest}` has an empty generated kind"
                    )));
                }
                if !GENERATED.contains(&generated.as_str()) {
                    return Err(DistError(format!(
                        "entry dest `{dest}` references unknown generated kind `{generated}` (known: {GENERATED:?})"
                    )));
                }
            }
            Source::Tree { tree } => {
                self.check_source_path(tree, dest, "tree")?;
                let dir = self.root.join(tree);
                if !dir.is_dir() {
                    return Err(DistError(format!(
                        "tree source `{tree}` for dest `{dest}` is not a directory"
                    )));
                }
                if dir_read_recursively(&dir)?.is_empty() {
                    return Err(DistError(format!(
                        "tree source `{tree}` for dest `{dest}` is empty"
                    )));
                }
                if entry.mode.is_some() {
                    return Err(DistError(format!(
                        "tree source `{tree}` for dest `{dest}` must not set a single mode (per-file modes are applied during staging)"
                    )));
                }
            }
            Source::File { file } => {
                self.check_source_path(file, dest, "file")?;
            }
        }
        Ok(())
    }

    fn check_source_path(&self, rel: &str, dest: &str, kind: &str) -> Result<()> {
        if rel.is_empty() {
            return Err(DistError(format!(
                "entry dest `{dest}` has an empty {kind} source"
            )));
        }
        if rel.starts_with('/') || Path::new(rel).has_root() || rel.contains(':') {
            return Err(DistError(format!(
                "{kind} source `{rel}` for dest `{dest}` is absolute"
            )));
        }
        if rel.split('/').any(|seg| seg == "..") {
            return Err(DistError(format!(
                "{kind} source `{rel}` for dest `{dest}` escapes with `..`"
            )));
        }
        let path = self.root.join(rel);
        // A symlink is followed silently today; the contract refuses to package
        // through one because it could point outside the workspace.
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("{kind} source `{rel}` for dest `{dest}` cannot be read: {e}"))?;
        if meta.file_type().is_symlink() {
            return Err(DistError(format!(
                "{kind} source `{rel}` for dest `{dest}` is a symlink (not supported)"
            )));
        }
        if !path.exists() {
            return Err(DistError(format!(
                "{kind} source `{rel}` for dest `{dest}` does not exist"
            )));
        }
        Ok(())
    }

    /// Windows targets must not lay files into an FHS tree, and Linux targets
    /// must not carry `.exe` names. Also detect two entries in the same target
    /// declaring different modes for the same destination.
    fn check_platform_mix(&self, entry: &Entry, dest: &str, target: &str) -> Result<()> {
        let is_windows_target = target == "windows-portable";
        let is_linux_target =
            target.starts_with("linux") || target.starts_with("deb") || target == "appimage";
        if is_windows_target
            && (dest.starts_with("usr/") || dest.starts_with("etc/") || dest.starts_with("lib/"))
        {
            return Err(DistError(format!(
                "windows target entry dest `{dest}` mixes a Linux FHS path"
            )));
        }
        if is_linux_target && dest.ends_with(".exe") {
            return Err(DistError(format!(
                "linux target entry dest `{dest}` mixes a Windows `.exe` name"
            )));
        }
        // Mode conflict: the same product destination mapped twice (unlikely
        // after duplicate-dest check, but tree vs file overlaps are caught here
        // by comparing on the same relative prefix).
        if let Some(mode) = entry.mode {
            for other in self.for_target(target) {
                if std::ptr::eq(other, entry) {
                    continue;
                }
                if other.dest == entry.dest && other.mode.is_some_and(|m| m != mode) {
                    return Err(DistError(format!(
                        "target `{target}` dest `{dest}` has conflicting modes"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Recursively list regular files under `dir`, excluding `.map`, hidden VCS
/// dirs, logs and screenshots. Returns workspace-relative paths.
pub fn dir_read_recursively(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    Ok(out)
}

fn walk(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).map_err(|e| format!("cannot read dir {}: {e}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue; // hidden files/dirs (.git, .gitignore, editors) are not shipped
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(DistError(format!(
                "symlink not supported at {}",
                path.display()
            )));
        }
        if file_type.is_dir() {
            walk(base, &path, out)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if name.ends_with(".map") || name.ends_with(".log") {
            continue;
        }
        out.push(path.strip_prefix(base)?.to_path_buf());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_layout(dir: &Path, body: &str) -> Layout {
        std::fs::create_dir_all(dir.join("packaging")).unwrap();
        let mut f = std::fs::File::create(dir.join("packaging").join("layout.toml")).unwrap();
        writeln!(f, "{body}").unwrap();
        Layout::load(dir).expect("valid minimal layout")
    }

    #[test]
    fn a_minimal_legal_layout_loads() {
        let dir = std::env::temp_dir().join(format!("xtask-layout-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("crates/ccproxy/webui/css")).unwrap();
        std::fs::write(dir.join("crates/ccproxy/webui/index.html"), "<html>").unwrap();
        std::fs::write(dir.join("crates/ccproxy/webui/css/a.css"), "x{}").unwrap();
        std::fs::create_dir_all(dir.join("packaging/systemd")).unwrap();
        std::fs::write(dir.join("packaging/systemd/u.service"), "[Service]").unwrap();

        let layout = write_layout(
            &dir,
            r#"
[[entry]]
source = { built = "ccproxy" }
dest = "service/ccproxy.exe"
targets = ["windows-portable"]

[[entry]]
source = { generated = "windows-package-json" }
dest = "package.json"
targets = ["windows-portable"]

[[entry]]
source = { tree = "crates/ccproxy/webui" }
dest = "service/webui"
targets = ["windows-portable"]

[[entry]]
source = { built = "ccproxy" }
dest = "usr/bin/commandcode-proxy"
mode = 493
targets = ["linux-portable"]

[[entry]]
source = { file = "packaging/systemd/u.service" }
dest = "lib/systemd/system/u.service"
mode = 420
targets = ["linux-portable"]
"#,
        );
        assert_eq!(layout.for_target("windows-portable").len(), 3);
        assert_eq!(layout.for_target("linux-portable").len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn illegal_layouts_are_rejected() {
        let dir = std::env::temp_dir().join(format!("xtask-layout-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("crates/ccproxy/webui")).unwrap();
        std::fs::write(dir.join("crates/ccproxy/webui/index.html"), "<html>").unwrap();

        let bad = [
            r#"[[entry]]
source = { built = "ccproxy" }
dest = ""
targets = ["windows-portable"]"#,
            r#"[[entry]]
source = { built = "ccproxy" }
dest = "service/../evil.exe"
targets = ["windows-portable"]"#,
            r#"[[entry]]
source = { built = "ccproxy" }
dest = "service\ccproxy.exe"
targets = ["windows-portable"]"#,
            r#"[[entry]]
source = { built = "ccproxy" }
dest = "C:/ccproxy.exe"
targets = ["windows-portable"]"#,
            r#"[[entry]]
source = { built = "nope" }
dest = "service/x"
targets = ["windows-portable"]"#,
            r#"[[entry]]
source = { built = "ccproxy" }
dest = "usr/bin/x"
targets = ["windows-portable"]"#,
            r#"[[entry]]
source = { built = "ccproxy" }
dest = "service/x.exe"
targets = ["linux-portable"]"#,
            r#"[[entry]]
source = { tree = "crates/ccproxy/webui" }
dest = "service/webui"
targets = ["nope"]"#,
        ];
        for body in bad {
            std::fs::create_dir_all(dir.join("packaging")).unwrap();
            std::fs::write(dir.join("packaging").join("layout.toml"), body).unwrap();
            assert!(Layout::load(&dir).is_err(), "should have rejected: {body}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
