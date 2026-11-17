//! Outward-facing artifact and self-update contracts, pinned BEFORE the
//! packaging refactor (see `docs/MULTIPLATFORM-BUILD-CONVERGENCE-SPEC.md`).
//!
//! These are guardrails. They read the sources that *currently* produce the
//! package (the workspace manifests, `release.yml`, the on-disk WebUI tree and
//! the real `ccproxy::archive` reader), so a layout drift, an asset rename, a
//! split-package regression or a missing WebUI file turns them red. They do
//! not call any packager — that does not exist yet — and they never touch the
//! production instance, a real port or a real user config.

use std::path::{Path, PathBuf};

/// `crates/ccproxy` at compile time — stable regardless of the CWD cargo runs
/// the tests from.
const CRATE_DIR: &str = env!("CARGO_MANIFEST_DIR");

fn webui_dir() -> PathBuf {
    Path::new(CRATE_DIR).join("webui")
}

fn workspace_root() -> PathBuf {
    Path::new(CRATE_DIR).join("../..")
}

fn read(repo_rel: &str) -> String {
    let path = workspace_root().join(repo_rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("contract test could not read {}: {e}", path.display()))
}

/// The five things a Windows release archive must carry at the documented
/// paths (spec §2.3 Windows). `service/webui/**` is the recursive tree.
const WINDOWS_REQUIRED_ANCHORS: &[&str] = &[
    "CCProxyTray.exe",
    "package.json",
    "service/ccproxy.exe",
    "service/ccproxy.json",
    "service/webui/index.html",
];

/// Every top-level segment in a portable Windows package. Anything else
/// (`logs/`, `data/`, a stray debug folder) is an extra top-level directory
/// the contract forbids.
const WINDOWS_ALLOWED_TOP_SEGMENTS: &[&str] = &["CCProxyTray.exe", "package.json", "service"];

/// Validate a parsed archive against the Windows ZIP shape contract.
///
/// This is the shape the updater and the release handover rely on: forward-
/// slash entries only, no absolute/`..`/drive names, a closed set of top-level
/// entries, and the five anchors present.
fn validate_windows_zip_shape(entries: &[ccproxy::archive::Entry]) -> Result<(), String> {
    for entry in entries {
        let name = &entry.name;
        if name.contains('\\') {
            return Err(format!(
                "entry `{name}` uses a backslash; ZIP entries must use `/`"
            ));
        }
        if name.starts_with('/') {
            return Err(format!("entry `{name}` is absolute"));
        }
        if name.contains(':') {
            return Err(format!("entry `{name}` carries a drive/authority prefix"));
        }
        if name.split('/').any(|seg| seg == "..") {
            return Err(format!("entry `{name}` escapes with `..`"));
        }
        let first = name.split('/').next().unwrap_or("");
        if !WINDOWS_ALLOWED_TOP_SEGMENTS.contains(&first) {
            return Err(format!("unexpected top-level entry `{name}`"));
        }
    }
    for anchor in WINDOWS_REQUIRED_ANCHORS {
        if !entries.iter().any(|e| e.name == *anchor) {
            return Err(format!("missing required anchor `{anchor}`"));
        }
    }
    Ok(())
}

/// A deliberately tiny stored-only ZIP writer, enough to feed
/// `ccproxy::archive::read` a representative package. It exists only so the
/// shape validator can be exercised on a real parse; it is not a packager.
fn build_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut locals = Vec::new();
    let mut central = Vec::new();
    let mut offset = 0u32;
    for (name, data) in files {
        let nb = name.as_bytes();
        let len = u32::try_from(data.len()).expect("test file too long");
        let mut local = Vec::new();
        local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        local.extend_from_slice(&20u16.to_le_bytes()); // version needed
        local.extend_from_slice(&0u16.to_le_bytes()); // flags
        local.extend_from_slice(&0u16.to_le_bytes()); // method = stored
        local.extend_from_slice(&0u16.to_le_bytes()); // time
        local.extend_from_slice(&0u16.to_le_bytes()); // date
        local.extend_from_slice(&0u32.to_le_bytes()); // crc
        local.extend_from_slice(&len.to_le_bytes());
        local.extend_from_slice(&len.to_le_bytes());
        local.extend_from_slice(&u16::try_from(nb.len()).unwrap().to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes()); // extra
        local.extend_from_slice(nb);
        local.extend_from_slice(data);
        locals.extend_from_slice(&local);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes()); // made by
        central.extend_from_slice(&20u16.to_le_bytes()); // needed
        central.extend_from_slice(&0u16.to_le_bytes()); // flags
        central.extend_from_slice(&0u16.to_le_bytes()); // method
        central.extend_from_slice(&0u16.to_le_bytes()); // time
        central.extend_from_slice(&0u16.to_le_bytes()); // date
        central.extend_from_slice(&0u32.to_le_bytes()); // crc
        central.extend_from_slice(&len.to_le_bytes());
        central.extend_from_slice(&len.to_le_bytes());
        central.extend_from_slice(&u16::try_from(nb.len()).unwrap().to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra
        central.extend_from_slice(&0u16.to_le_bytes()); // comment
        central.extend_from_slice(&0u16.to_le_bytes()); // disk
        central.extend_from_slice(&0u16.to_le_bytes()); // attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // ext attrs
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(nb);
        offset = offset.saturating_add(u32::try_from(local.len()).unwrap());
    }
    let mut out = locals;
    let cd_offset = u32::try_from(out.len()).unwrap();
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&u16::try_from(files.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&u16::try_from(files.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&u32::try_from(central.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// The current Windows package, summarised as a representative entry list.
fn representative_windows_files() -> Vec<(&'static str, &'static [u8])> {
    vec![
        ("CCProxyTray.exe", b"tray"),
        ("package.json", b"{\"version\":\"0.7.11\"}"),
        ("service/ccproxy.exe", b"proxy"),
        (
            "service/ccproxy.json",
            b"{\"proxy\":\"default\",\"noProxy\":\"localhost,127.0.0.1\"}",
        ),
        ("service/webui/index.html", b"<html></html>"),
        ("service/webui/css/tokens.css", b":root{}"),
        ("service/webui/js/core.js", b"()=>{}"),
    ]
}

#[test]
fn the_current_windows_layout_passes_its_own_shape() {
    let bytes = build_zip(&representative_windows_files());
    let entries = ccproxy::archive::read(&bytes).expect("representative zip must be readable");
    validate_windows_zip_shape(&entries).expect("current layout must satisfy its own contract");
}

/// One named (label, entry list) case for the shape-rejection table.
type BadCase<'a> = (&'a str, &'a [(&'a str, &'a [u8])]);

#[test]
fn a_windows_zip_with_a_forbidden_shape_is_rejected() {
    // Each variant breaks exactly one rule; the validator must catch it.
    let cases: &[BadCase] = &[
        (
            "backslash entry separator",
            &[
                ("CCProxyTray.exe", b"t"),
                ("package.json", b"{}"),
                ("service\\ccproxy.exe", b"p"),
                ("service\\ccproxy.json", b"{}"),
                ("service\\webui\\index.html", b"<html>"),
            ],
        ),
        (
            "traversal entry",
            &[
                ("CCProxyTray.exe", b"t"),
                ("package.json", b"{}"),
                ("service/ccproxy.exe", b"p"),
                ("service/ccproxy.json", b"{}"),
                ("service/webui/index.html", b"<html>"),
                ("service/../evil.exe", b"x"),
            ],
        ),
        (
            "absolute entry",
            &[
                ("CCProxyTray.exe", b"t"),
                ("package.json", b"{}"),
                ("service/ccproxy.exe", b"p"),
                ("service/ccproxy.json", b"{}"),
                ("service/webui/index.html", b"<html>"),
                ("/etc/passwd", b"x"),
            ],
        ),
        (
            "extra top-level directory",
            &[
                ("CCProxyTray.exe", b"t"),
                ("package.json", b"{}"),
                ("service/ccproxy.exe", b"p"),
                ("service/ccproxy.json", b"{}"),
                ("service/webui/index.html", b"<html>"),
                ("logs/proxy.log", b"log"),
            ],
        ),
        (
            "missing the default config anchor",
            &[
                ("CCProxyTray.exe", b"t"),
                ("package.json", b"{}"),
                ("service/ccproxy.exe", b"p"),
                ("service/webui/index.html", b"<html>"),
            ],
        ),
    ];
    for (label, files) in cases {
        let bytes = build_zip(files);
        let entries = ccproxy::archive::read(&bytes)
            .unwrap_or_else(|e| panic!("{label}: representative zip must parse: {e}"));
        assert!(
            validate_windows_zip_shape(&entries).is_err(),
            "{label}: the shape validator should have rejected this package"
        );
    }
}

#[test]
fn the_windows_asset_name_and_picker_contract_hold() {
    // The updater's picker is compiled for this OS; on Windows it must look
    // for the `.zip` suffix. The published file is
    // `commandcode-proxy-v<version>-windows-x86_64.zip`.
    assert_eq!(
        ccproxy::update::asset_suffix(),
        "windows-x86_64.zip",
        "this build is Windows; asset_suffix() must name the .zip"
    );
    let version = ccproxy::proxy_version();
    let expected = format!("commandcode-proxy-v{version}-windows-x86_64.zip");

    let assets = vec![
        (
            expected.clone(),
            "https://example.invalid/win.zip".to_string(),
            1,
        ),
        (
            format!("commandcode-proxy-v{version}-linux-x86_64.tar.gz"),
            "https://example.invalid/linux.tar.gz".to_string(),
            2,
        ),
        (
            "source.tar.gz".to_string(),
            "https://example.invalid/src.tar.gz".to_string(),
            3,
        ),
    ];
    let picked = ccproxy::update::pick_asset(&assets).expect("the windows asset must be picked");
    assert_eq!(
        picked.0, expected,
        "pick_asset must select the windows asset"
    );
}

#[test]
fn the_release_workflow_publishes_both_platform_asset_names() {
    // Cross-platform anchor: whatever the build OS, the release must publish
    // the exact names the updater matches on and that existing GitHub Releases
    // (v0.7.9–v0.7.11) already ship. These are pinned against the live assets:
    //   Windows ZIP:  commandcode-proxy-v<ver>-windows-x86_64.zip
    //   Linux tarball: commandcode-proxy-<ver>-linux-x86_64.tar.gz   (no `v`)
    //   Debian pkgs:   commandcode-proxy_<ver>-1_amd64.deb
    //                  commandcode-proxy-tray_<ver>-1_amd64.deb
    // Renaming any of these 404s the updater and AUR PKGBUILD.
    let release = read(".github/workflows/release.yml");
    assert!(
        release.contains("windows-x86_64.zip"),
        "release.yml no longer publishes the windows .zip asset name"
    );
    // The Linux tarball asset name must NOT contain "v<ver>": it is
    // `commandcode-proxy-<ver>-linux-x86_64.tar.gz` (see the actual v0.7.11
    // release). The Windows ZIP keeps its `v` prefix; do not "unify" them.
    assert!(
        release.contains("linux-x86_64.tar.gz"),
        "release.yml no longer publishes the linux tar.gz asset name"
    );
    assert!(
        !release.contains("commandcode-proxy-v*-linux-x86_64.tar.gz"),
        "linux tarball asset name must not include a `v` prefix (matches published Releases)"
    );
    // Debian packages follow <pkg>_<ver>-1_amd64.deb (Debian policy, no `v`).
    assert!(
        release.contains("dpkg-deb --build")
            && release.contains("_amd64.deb")
            && release.contains("commandcode-proxy_")
            && release.contains("commandcode-proxy-tray_"),
        "release.yml must build both commandcode-proxy_*_amd64.deb and \
         commandcode-proxy-tray_*_amd64.deb"
    );
}

#[test]
fn systemd_dest_is_consistent_across_layout_pkgbuild_and_workflow() {
    // layout.toml ships the unit at `lib/systemd/system/...` (relative to the
    // staged root). The AUR PKGBUILD and release.yml's systemd-analyze verify
    // must reference that same relative path — not `usr/lib/systemd/system/...`
    // which would only be true after install on a merged-/usr system.
    let layout = read("packaging/layout.toml");
    let pkgbuild = read("packaging/aur/PKGBUILD.template");
    let release = read(".github/workflows/release.yml");

    assert!(
        layout.contains("lib/systemd/system/commandcode-proxy.service"),
        "layout.toml must ship the systemd unit at lib/systemd/system/..."
    );
    // The PKGBUILD now uses `cp -a usr lib etc` so it does not hardcode the
    // systemd path; but it must NOT reintroduce a usr/lib/systemd path.
    assert!(
        !pkgbuild.contains("usr/lib/systemd"),
        "PKGBUILD must not hardcode usr/lib/systemd (layout ships lib/systemd)"
    );
    // The workflow's systemd-analyze verify must point at the staged root path.
    assert!(
        release.contains("dist/deb/commandcode-proxy/lib/systemd/system/commandcode-proxy.service"),
        "release.yml systemd-analyze verify must use the layout's lib/systemd/system path"
    );
}

#[test]
fn the_linux_debs_split_into_proxy_and_tray_packages() {
    // The split is now declared in packaging/layout.toml (deb-proxy vs deb-tray
    // targets), not in per-crate Cargo.toml metadata.
    let layout = read("packaging/layout.toml");

    // Proxy target: FHS proxy binary, WebUI tree, systemd unit, env.example.
    assert!(
        layout.contains("deb-proxy"),
        "layout must declare a deb-proxy target"
    );
    assert!(
        layout.contains("usr/bin/commandcode-proxy"),
        "proxy deb must ship /usr/bin/commandcode-proxy"
    );
    assert!(
        layout.contains("usr/share/commandcode-proxy/webui"),
        "proxy deb must ship the WebUI tree"
    );
    assert!(
        layout.contains("lib/systemd/system"),
        "proxy deb must ship the systemd unit"
    );
    assert!(
        layout.contains("etc/commandcode-proxy"),
        "proxy deb must ship env.example"
    );
    // The proxy target must not list tray-only files.
    let deb_proxy_block: String = layout
        .lines()
        .filter(|l| l.contains("deb-proxy") && !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !deb_proxy_block.contains("commandcode-proxy-tray")
            && !deb_proxy_block.contains(".desktop"),
        "proxy deb must not ship the tray binary or .desktop entry"
    );

    // Tray target: tray binary, .desktop, icon; depends on same-version proxy.
    assert!(
        layout.contains("deb-tray"),
        "layout must declare a deb-tray target"
    );
    assert!(
        layout.contains("usr/bin/commandcode-proxy-tray"),
        "tray deb must ship /usr/bin/commandcode-proxy-tray"
    );
    assert!(
        layout.contains("usr/share/applications/"),
        "tray deb must ship the .desktop entry"
    );
    assert!(
        layout.contains("usr/share/icons/"),
        "tray deb must ship the icon"
    );
    let deb_tray_block: String = layout
        .lines()
        .filter(|l| l.contains("deb-tray") && !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !deb_tray_block.contains("webui")
            && !deb_tray_block.contains("systemd")
            && !deb_tray_block.contains("env.example"),
        "tray deb must not ship WebUI, systemd unit or env file"
    );
}

#[test]
fn every_local_webui_file_referenced_by_index_html_exists() {
    let root = webui_dir();
    assert!(
        root.join("index.html").is_file(),
        "webui/index.html must exist; the package ships this directory"
    );
    let refs = ccproxy::webui_site::referenced_files(&root);
    assert!(
        !refs.is_empty(),
        "index.html references no CSS/JS assets at all"
    );

    let mut missing = Vec::new();
    for path in &refs {
        if path.is_file() {
            continue;
        }
        // `referenced_files` joins every href/src onto the root. Inlined
        // `data:` URIs and remote URLs are not files on disk and are allowed;
        // anything else is a local reference that has leaked out of the tree.
        let rel = path.strip_prefix(&root).unwrap_or(path);
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.starts_with("data:") || rel.starts_with("http://") || rel.starts_with("https://") {
            continue;
        }
        missing.push(rel);
    }
    assert!(
        missing.is_empty(),
        "index.html references local files that are missing on disk: {missing:?}"
    );
}
