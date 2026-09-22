//! Inline the WebUI into a single self-contained page at compile time.
//!
//! The front end is authored as separate files — `webui/index.html` plus the
//! `webui/css/` and `webui/js/` it references — so a person editing the panel
//! touches one small file and reads the whole composition from `index.html`.
//! The shipped page must stay a single offline document (no sibling requests,
//! works with the proxy up and nothing else), so this build script resolves
//! each `<link rel="stylesheet">` and `<script src>` in place and writes the
//! result to `$OUT_DIR/webui.html`, which `webui.rs` includes.
//!
//! Doing it here rather than at runtime makes a missing file a compile error
//! with a clear message, instead of a page that quietly loses a stylesheet.
//! `cargo:rerun-if-changed` on every input means a style edit rebuilds with no
//! manual step.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The value of `name="..."` inside a tag fragment.
fn attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let rest = tag.get(start..)?;
    let end = rest.find('"')?;
    rest.get(..end).map(str::to_owned)
}

/// Read a referenced asset, failing the build with a readable message.
fn read_asset(root: &Path, href: &str, kind: &str) -> String {
    let path = root.join(href);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "webui: cannot read {kind} `{href}` (looked at {}): {e}\n\
             Every <link>/<script src> in index.html must exist; a missing one \
             would otherwise ship a page silently missing its styles or code.",
            path.display()
        )
    })
}

/// Replace every `<link rel=stylesheet>` and `<script src>` with the file's
/// contents, in source order — so the cascade and load order are exactly what
/// `index.html` spells out.
fn inline(root: &Path, html: &str) -> (String, Vec<String>) {
    let mut out = String::with_capacity(html.len() * 2);
    let mut inputs = Vec::new();
    let mut cursor = 0usize;

    loop {
        let next = ["<link", "<script"]
            .iter()
            .filter_map(|m| html[cursor..].find(m).map(|i| cursor + i))
            .min();
        let Some(pos) = next else {
            break;
        };
        out.push_str(&html[cursor..pos]);
        let end = html[pos..].find('>').map_or(html.len(), |i| pos + i + 1);
        let tag = &html[pos..end];

        if tag.starts_with("<link") && tag.contains("stylesheet") {
            // <link> is void: nothing to consume after it.
            if let Some(href) = attr(tag, "href") {
                inputs.push(href.clone());
                let css = read_asset(root, &href, "stylesheet");
                let _ = writeln!(out, "<style>\n{}\n</style>", css.trim_end());
            } else {
                out.push_str(tag);
            }
            cursor = end;
        } else if tag.starts_with("<script") {
            if let Some(src) = attr(tag, "src") {
                // `<script src="x"></script>` has a separate close tag; the
                // replacement supplies its own, so the original must be
                // consumed or the page ends up with a stray `</script>`.
                let after = html[end..]
                    .find("</script>")
                    .map_or(end, |i| end + i + "</script>".len());
                inputs.push(src.clone());
                let js = read_asset(root, &src, "script");
                let _ = writeln!(out, "<script>\n{}\n</script>", js.trim_end());
                cursor = after;
            } else {
                out.push_str(tag);
                cursor = end;
            }
        } else {
            out.push_str(tag);
            cursor = end;
        }
    }
    out.push_str(&html[cursor..]);
    (out, inputs)
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let root = manifest.join("webui");
    let index_path = root.join("index.html");
    let html = std::fs::read_to_string(&index_path)
        .unwrap_or_else(|e| panic!("webui: cannot read {}: {e}", index_path.display()));

    let (assembled, inputs) = inline(&root, &html);

    // A page that still references an external file did not get inlined, and
    // would 404 in the browser. Fail loudly rather than ship it.
    for leftover in ["<link", "<script src"] {
        if assembled.contains(leftover) {
            // `<link rel="icon" ...>` is legitimately left as-is (a data: URI).
            if leftover == "<link" && !assembled.contains("<link rel=\"stylesheet") {
                continue;
            }
            panic!("webui: `{leftover}` survived inlining — the page would request a missing file");
        }
    }

    for href in &inputs {
        println!("cargo:rerun-if-changed={}", root.join(href).display());
    }
    println!("cargo:rerun-if-changed={}", index_path.display());

    let dest = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("webui.html");
    std::fs::write(&dest, assembled)
        .unwrap_or_else(|e| panic!("webui: cannot write {}: {e}", dest.display()));
}
