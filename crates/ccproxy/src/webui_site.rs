//! Assemble the split WebUI sources into one self-contained page.
//!
//! The front end is authored as `index.html` plus the `css/` and `js/` it
//! references. The served document must be a single offline page (inline CSS/JS,
//! no sibling requests), so something has to resolve those references.
//!
//! That something runs in two places, and this file is the one implementation
//! both use:
//!
//! - **At build time** (`build.rs`) against `crates/ccproxy/webui/`, producing
//!   the copy compiled into the binary. That copy is the fallback, so a package
//!   with no loose files still serves a complete page.
//! - **At run time** against `<exe dir>/webui/` when it exists. This is what
//!   lets someone edit a stylesheet and just reload — no rebuild. The loose
//!   files win; a missing directory or a broken reference falls back to the
//!   embedded copy rather than serving a half-page.
//!
//! Std-only on purpose: `build.rs` is a separate crate and cannot use the
//! library, so anything shared has to carry no dependencies.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The value of `name="..."` inside a tag fragment.
fn attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)?.saturating_add(needle.len());
    let rest = tag.get(start..)?;
    let end = rest.find('"')?;
    rest.get(..end).map(str::to_owned)
}

/// Why assembly failed. The caller decides whether that is fatal (build) or a
/// reason to fall back (runtime).
#[derive(Debug)]
pub enum AssembleError {
    /// `index.html` could not be read.
    Index(String),
    /// A referenced file could not be read (or escapes the root).
    Asset { href: String, reason: String },
    /// A reference survived inlining, so the page would 404 in a browser.
    NotInlined(String),
}

impl std::fmt::Display for AssembleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Index(e) => write!(f, "cannot read index.html: {e}"),
            Self::Asset { href, reason } => write!(f, "cannot read `{href}`: {reason}"),
            Self::NotInlined(t) => {
                write!(
                    f,
                    "`{t}` survived inlining — the page would request a missing file"
                )
            }
        }
    }
}

/// Read a referenced asset, refusing anything that escapes `root`.
///
/// `index.html` is local, trusted content, so this is belt-and-braces rather
/// than a live threat: it keeps a typo like `../../secrets` from silently
/// reading outside the site directory.
fn read_asset(root: &Path, href: &str) -> Result<String, AssembleError> {
    if href.contains("..") {
        return Err(AssembleError::Asset {
            href: href.to_owned(),
            reason: "path escapes the site directory".into(),
        });
    }
    std::fs::read_to_string(root.join(href)).map_err(|e| AssembleError::Asset {
        href: href.to_owned(),
        reason: e.to_string(),
    })
}

/// Assemble `index.html` in `root` into a single page, inlining every
/// `<link rel=stylesheet>` and `<script src>` in source order — so the cascade
/// and load order are exactly what `index.html` spells out.
pub fn assemble(root: &Path) -> Result<String, AssembleError> {
    let index = std::fs::read_to_string(root.join("index.html"))
        .map_err(|e| AssembleError::Index(e.to_string()))?;

    let mut out = String::with_capacity(index.len().saturating_mul(2));
    let mut cursor = 0usize;

    loop {
        let next = ["<link", "<script"]
            .iter()
            .filter_map(|m| index[cursor..].find(m).map(|i| cursor.saturating_add(i)))
            .min();
        let Some(pos) = next else {
            break;
        };
        out.push_str(&index[cursor..pos]);
        let end = index[pos..]
            .find('>')
            .map_or(index.len(), |i| pos.saturating_add(i).saturating_add(1));
        let tag = &index[pos..end];

        if tag.starts_with("<link") && tag.contains("stylesheet") {
            match attr(tag, "href") {
                Some(href) => {
                    let css = read_asset(root, &href)?;
                    let _ = writeln!(out, "<style>\n{}\n</style>", css.trim_end());
                }
                None => out.push_str(tag),
            }
            cursor = end;
        } else if tag.starts_with("<script") {
            match attr(tag, "src") {
                Some(src) => {
                    // `<script src="x"></script>` has a separate close tag; the
                    // replacement supplies its own, so the original must be
                    // consumed or the page ends up with a stray `</script>`.
                    let after = index[end..].find("</script>").map_or(end, |i| {
                        end.saturating_add(i).saturating_add("</script>".len())
                    });
                    let js = read_asset(root, &src)?;
                    let _ = writeln!(out, "<script>\n{}\n</script>", js.trim_end());
                    cursor = after;
                }
                None => {
                    out.push_str(tag);
                    cursor = end;
                }
            }
        } else {
            out.push_str(tag);
            cursor = end;
        }
    }
    out.push_str(&index[cursor..]);

    // A stylesheet reference left behind would 404 in the browser. (`<link
    // rel=icon ...>` is a data: URI and legitimately stays.)
    if out.contains("<link rel=\"stylesheet") {
        return Err(AssembleError::NotInlined("<link rel=stylesheet".into()));
    }
    if out.contains("<script src") {
        return Err(AssembleError::NotInlined("<script src".into()));
    }
    Ok(out)
}

/// Every file `index.html` references, for `cargo:rerun-if-changed`.
pub fn referenced_files(root: &Path) -> Vec<PathBuf> {
    let Ok(index) = std::fs::read_to_string(root.join("index.html")) else {
        return Vec::new();
    };
    let mut seen = BTreeSet::new();
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

    /// A throwaway site directory. Each test gets its own: they run in parallel
    /// within one process, so a pid-only name would have them overwrite each
    /// other's `index.html`.
    fn site(tag: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ccproxy-site-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (name, body) in files {
            let p = dir.join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        dir
    }

    #[test]
    fn references_become_inline_blocks_in_source_order() {
        let dir = site(
            "inline-order",
            &[
                ("index.html", "<head><link rel=\"stylesheet\" href=\"a.css\"></head><body><script src=\"b.js\"></script></body>"),
                ("a.css", ".x { color: red; }"),
                ("b.js", "var y = 1;"),
            ],
        );
        let page = assemble(&dir).unwrap();
        assert!(
            page.contains("<style>\n.x { color: red; }\n</style>"),
            "{page}"
        );
        assert!(page.contains("<script>\nvar y = 1;\n</script>"), "{page}");
        // The src attribute and its close tag are gone, not merely shadowed.
        assert!(!page.contains("href=\"a.css\""));
        assert!(!page.contains("src=\"b.js\""));
        assert_eq!(page.matches("</script>").count(), 1, "no stray close tag");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_asset_is_an_error_not_a_partial_page() {
        let dir = site(
            "missing",
            &[("index.html", "<link rel=\"stylesheet\" href=\"gone.css\">")],
        );
        assert!(matches!(assemble(&dir), Err(AssembleError::Asset { .. })));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_path_escaping_the_root_is_refused() {
        let dir = site(
            "escape",
            &[(
                "index.html",
                "<link rel=\"stylesheet\" href=\"../../etc/passwd\">",
            )],
        );
        assert!(matches!(assemble(&dir), Err(AssembleError::Asset { .. })));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_icon_link_is_left_alone() {
        // A data: URI is a real link tag that must survive untouched.
        let dir = site(
            "icon",
            &[
                (
                    "index.html",
                    "<head><link rel=\"icon\" href=\"data:image/svg+xml,x\"><link rel=\"stylesheet\" href=\"a.css\"></head>",
                ),
                ("a.css", ".a{}"),
            ],
        );
        let page = assemble(&dir).unwrap();
        assert!(page.contains("rel=\"icon\""), "{page}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn referenced_files_lists_every_asset() {
        let dir = site(
            "refs",
            &[
                (
                    "index.html",
                    "<link rel=\"stylesheet\" href=\"css/a.css\"><script src=\"js/b.js\"></script>",
                ),
                ("css/a.css", ""),
                ("js/b.js", ""),
            ],
        );
        let files = referenced_files(&dir);
        assert!(files.iter().any(|p| p.ends_with("css/a.css")));
        assert!(files.iter().any(|p| p.ends_with("js/b.js")));
        std::fs::remove_dir_all(&dir).ok();
    }
}
