//! Assemble the WebUI sources into the page compiled into the binary.
//!
//! The front end lives in `crates/ccproxy/webui/` as `index.html` plus the
//! `css/` and `js/` it references. The served document must be a single offline
//! page, so this resolves those references and writes the result to
//! `$OUT_DIR/webui.html`, which `webui.rs` includes as the fallback.
//!
//! At run time the proxy prefers a loose `webui/` beside the executable — so a
//! stylesheet can be edited and reloaded without a rebuild — and falls back to
//! this compiled copy. Both paths share one implementation (`webui_site.rs`,
//! included verbatim below), so they cannot drift.
//!
//! Doing the build-time assembly here means a missing file is a compile error
//! with a clear message, instead of a release that silently serves a page
//! missing its stylesheet.

#[path = "src/webui_site.rs"]
mod webui_site;

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let root = manifest.join("webui");

    let assembled = webui_site::assemble(&root).unwrap_or_else(|e| {
        panic!(
            "webui: cannot assemble the page from {}: {e}\n\
             Every <link>/<script src> in index.html must exist; a missing one \
             would otherwise ship a page silently missing its styles or code.",
            root.display()
        )
    });

    // A style/script edit must rebuild without a clean.
    for asset in webui_site::referenced_files(&root) {
        println!("cargo:rerun-if-changed={}", asset.display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        root.join("index.html").display()
    );

    let dest = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("webui.html");
    std::fs::write(&dest, assembled)
        .unwrap_or_else(|e| panic!("webui: cannot write {}: {e}", dest.display()));
}
