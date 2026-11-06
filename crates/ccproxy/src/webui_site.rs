//! WebUI static-asset serving — the external front end.
//!
//! The front end is authored as `index.html` plus the `css/` and `js/` it
//! references, and it ships loose beside the executable (`<exe dir>/webui/`).
//! The proxy serves those files directly: no build-time inlining and no
//! embedded copy, so the page is a normal static site and an edit to a
//! stylesheet takes effect on the next reload.
//!
//! This used to *assemble* the split sources into one self-contained page at
//! build time (`build.rs`) and at run time as a fallback. That path is gone:
//! the binary no longer carries a compiled-in page. If the loose `webui/`
//! directory is absent, the proxy says so explicitly instead of half-serving a
//! page that cannot see its own styles.
//!
//! Std-only on purpose: nothing here needs a dependency, which keeps the
//! module usable from anywhere (the old `build.rs` was a separate crate and
//! could not link the library).

use std::path::{Path, PathBuf};

/// The value of `name="..."` inside a tag fragment.
fn attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)?.saturating_add(needle.len());
    let rest = tag.get(start..)?;
    let end = rest.find('"')?;
    rest.get(..end).map(str::to_owned)
}

/// Why an asset could not be served.
#[derive(Debug)]
pub enum ServeError {
    /// The requested URL path is not inside the site directory.
    Escape(String),
    /// The file could not be read (missing, permission, …).
    Io { path: PathBuf, error: String },
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Escape(url) => write!(f, "`{url}` escapes the site directory"),
            Self::Io { path, error } => write!(f, "cannot read `{}`: {error}", path.display()),
        }
    }
}

/// Resolve a `/webui/...` URL path to a file under `root`.
///
/// `index.html` is the default when the path is empty or names the directory.
/// Anything that escapes `root` — a `..` segment, an absolute path — is
/// refused. `root` itself is trusted (it is the site directory next to the
/// executable), but the URL arrives from a browser and is untrusted.
pub fn asset_path(root: &Path, url_path: &str) -> Result<PathBuf, ServeError> {
    let rel = url_path
        .strip_prefix("/webui/")
        .unwrap_or(url_path)
        .trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    if rel.contains("..") {
        return Err(ServeError::Escape(url_path.to_owned()));
    }
    Ok(root.join(rel))
}

/// Content type for a served asset, by extension.
pub fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// Every file `index.html` references — kept as the source for asset
/// validation (the radius-token test) and for packaging checks.
pub fn referenced_files(root: &Path) -> Vec<PathBuf> {
    let Ok(index) = std::fs::read_to_string(root.join("index.html")) else {
        return Vec::new();
    };
    let mut seen = std::collections::BTreeSet::new();
    for marker in ["<link", "<script"] {
        let mut rest = index.as_str();
        while let Some(at) = rest.find(marker) {
            rest = &rest[at..];
            let end = rest.find('>').map_or(rest.len(), |i| i.saturating_add(1));
            let tag = &rest[..end];
            let name = if tag.starts_with("<link") {
                "href"
            } else {
                "src"
            };
            if let Some(v) = attr(tag, name) {
                if !v.contains("..") {
                    seen.insert(root.join(v));
                }
            }
            rest = rest.get(end..).unwrap_or("");
        }
    }
    seen.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_path_maps_urls_under_the_webui_root() {
        let root = Path::new("/srv/ccproxy/webui");
        assert_eq!(
            asset_path(root, "/webui/").unwrap(),
            root.join("index.html"),
            "bare directory serves index.html"
        );
        assert_eq!(
            asset_path(root, "/webui/index.html").unwrap(),
            root.join("index.html")
        );
        assert_eq!(
            asset_path(root, "/webui/css/tokens.css").unwrap(),
            root.join("css/tokens.css")
        );
        assert_eq!(
            asset_path(root, "/webui/js/stats.js").unwrap(),
            root.join("js/stats.js")
        );
    }

    #[test]
    fn asset_path_refuses_escapes() {
        let root = Path::new("/srv/ccproxy/webui");
        for evil in [
            "/webui/../ccproxy.json",
            "/webui/css/../../secret",
            "/../etc/passwd",
        ] {
            assert!(
                matches!(asset_path(root, evil), Err(ServeError::Escape(_))),
                "must refuse {evil}"
            );
        }
    }

    #[test]
    fn content_type_is_keyed_by_extension() {
        assert_eq!(
            content_type(Path::new("index.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("tokens.css")),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("stats.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type(Path::new("icon.svg")), "image/svg+xml");
        assert_eq!(
            content_type(Path::new("unknown.xyz")),
            "application/octet-stream"
        );
    }

    #[test]
    fn referenced_files_lists_every_asset() {
        let dir = std::env::temp_dir().join(format!("ccproxy-site-refs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("css")).unwrap();
        std::fs::create_dir_all(dir.join("js")).unwrap();
        std::fs::write(
            dir.join("index.html"),
            "<link rel=\"stylesheet\" href=\"css/a.css\"><script src=\"js/b.js\"></script>",
        )
        .unwrap();
        std::fs::write(dir.join("css/a.css"), "").unwrap();
        std::fs::write(dir.join("js/b.js"), "").unwrap();

        let files = referenced_files(&dir);
        assert!(files.iter().any(|p| p.ends_with("css/a.css")));
        assert!(files.iter().any(|p| p.ends_with("js/b.js")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_deb_asset_list_carries_every_file_the_panel_references() {
        // The deb lists assets one by one (cargo-deb resolves them relative to
        // the crate dir), so a stylesheet added to index.html and forgotten
        // here ships a package whose panel silently loses it — the deb install
        // rendered with no theme at all for exactly this reason. The tarball and
        // AppImage copy the directory, so only the deb needs the guard.
        let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/webui"));
        let crate_dir = root.parent().expect("webui lives in the crate dir");
        let manifest =
            std::fs::read_to_string(crate_dir.join("Cargo.toml")).expect("crate manifest");

        let mut missing = Vec::new();
        for file in referenced_files(root) {
            // The favicon is an inline `data:` URI, not a file on disk; only
            // real files can be packaged.
            if !file.is_file() {
                continue;
            }
            let Ok(rel) = file.strip_prefix(crate_dir) else {
                continue;
            };
            // cargo-deb wants forward slashes even on Windows.
            let rel = rel.to_string_lossy().replace('\\', "/");
            if !manifest.contains(&rel) {
                missing.push(rel);
            }
        }
        assert!(
            missing.is_empty(),
            "the deb asset list omits referenced files: {missing:?}"
        );
    }

    #[test]
    fn corner_radii_come_only_from_the_token_scale() {
        // "Some buttons square, some round": the radii used to be ad-hoc
        // (16/14/8/13/2/999), so two controls on one row could disagree.
        // Every radius now comes from a named token in tokens.css; this pins
        // that the scale is declared, and that no stylesheet states a radius
        // as a bare value again.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("webui");
        let files = referenced_files(&dir);
        assert!(!files.is_empty(), "webui/index.html references no assets");
        let tokens = files
            .iter()
            .find(|p| p.ends_with("tokens.css"))
            .map(|p| std::fs::read_to_string(p).expect("tokens.css readable"))
            .expect("tokens.css is referenced by index.html");
        for token in [
            "--radius-panel",
            "--radius-card",
            "--radius-control",
            "--radius-pill",
        ] {
            assert!(tokens.contains(token), "tokens.css must declare {token}");
        }
        for path in &files {
            if path.ends_with("tokens.css") || path.extension().is_none_or(|e| e != "css") {
                continue;
            }
            let text = std::fs::read_to_string(path).expect("css readable");
            for (i, line) in text.lines().enumerate() {
                if !line.contains("border-radius") {
                    continue;
                }
                // `50%` is not a corner radius on the scale — it draws a circle
                // (the chip's status dot). Both spellings are fine; anything
                // else must name a token.
                let declared = line
                    .split("border-radius")
                    .skip(1)
                    .any(|rest| rest.contains("var(--radius-") || rest.contains("50%"));
                assert!(
                    declared,
                    "{}:{} uses a bare radius instead of a token: {}",
                    path.display(),
                    i.saturating_add(1),
                    line.trim()
                );
            }
        }
    }

    /// Every `var(--name)` occurrence on a line, paired with whether it spells
    /// a fallback (`var(--x, 0)`). A fallback means the author accepted the
    /// missing case, so only bare references have to resolve.
    fn var_references(line: &str) -> Vec<(String, bool)> {
        let mut out = Vec::new();
        let mut rest = line;
        while let Some(at) = rest.find("var(--") {
            rest = &rest[at + 4..];
            let end = rest.find(')').unwrap_or(rest.len());
            let body = &rest[..end];
            rest = &rest[end..];
            let (name, fallback) = match body.split_once(',') {
                Some((name, _)) => (name, true),
                None => (body, false),
            };
            // `var(--…)` in prose and other non-ASCII spellings are not
            // property names; a name is ASCII `[A-Za-z0-9-]`.
            let name = name.trim().trim_start_matches('-').trim();
            let valid =
                !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
            if valid {
                out.push((name.to_string(), fallback));
            }
        }
        out
    }

    /// Names declared on a line, in either spelling used here: a stylesheet's
    /// `--name: value` at the start of a line, or a script's
    /// `style="--name: 3"` written inline on an element. Anything followed by
    /// `)` is a reference, not a declaration.
    fn declared_names(line: &str) -> Vec<String> {
        let bytes = line.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i + 2 < bytes.len() {
            if bytes[i] == b'-' && bytes[i + 1] == b'-' {
                let mut j = i + 2;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'-') {
                    j += 1;
                }
                if j > i + 2 && j < bytes.len() && bytes[j] == b':' {
                    out.push(line[i + 2..j].to_string());
                }
                i = j.max(i + 2);
            } else {
                i += 1;
            }
        }
        out
    }

    #[test]
    fn every_variable_reference_resolves_to_a_definition() {
        // Merging the mainline control kit into this branch left three
        // variables referenced but never declared — `--surface-high`,
        // `--surface-variant`, `--shadow-hover`. A `var()` on a missing name
        // computes to `unset`, so every self-drawn dropdown opened with a
        // transparent menu and no shadow, on all four themes, and no test
        // noticed: the radius gate only looked at radii.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("webui");
        let files = referenced_files(&dir);
        assert!(!files.is_empty(), "webui/index.html references no assets");

        let mut defined = std::collections::HashSet::new();
        for path in &files {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default();
            if ext != "css" && ext != "js" {
                continue;
            }
            let text = std::fs::read_to_string(path).expect("asset readable");
            for line in text.lines() {
                defined.extend(declared_names(line));
            }
        }
        assert!(
            defined.contains("primary"),
            "token definitions were not picked up: {defined:?}"
        );

        let mut missing: Vec<String> = Vec::new();
        for path in &files {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default();
            if ext != "css" && ext != "js" {
                continue;
            }
            let text = std::fs::read_to_string(path).expect("asset readable");
            for (i, line) in text.lines().enumerate() {
                for (name, fallback) in var_references(line) {
                    if fallback || defined.contains(&name) {
                        continue;
                    }
                    missing.push(format!(
                        "{}:{} --{name}",
                        path.display(),
                        i.saturating_add(1)
                    ));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "these variables are used but never declared, so they compute to unset: {missing:#?}"
        );
    }
}
