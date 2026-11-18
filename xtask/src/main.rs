//! `cargo xtask` entry point: argument parsing and exit codes only.
//!
//! Subcommands (spec §5):
//!
//! ```text
//! cargo xtask layout --check
//! cargo xtask stage --platform <windows|linux> --target <t> --out <dir>
//! cargo xtask archive --platform <windows|linux> --input <dir> --out <file>
//! cargo xtask manifest --platform <w|l> --input <dir> --artifact <file> --out <file.json>
//! cargo xtask verify --platform <windows|linux> --target <t> --input <dir> [--artifact <file>]
//! cargo xtask dist --platform <windows|linux> [--skip-build]
//! ```

use std::path::PathBuf;

use xtask::{archive, deb, layout, manifest, stage, verify, DistError, Result};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        eprintln!("xtask: {e}");
        std::process::exit(1);
    }
}

/// Published artifact names.
///
/// These strings are the public contract: the updater (ccproxy::archive /
/// update::asset_suffix) matches on the Windows ZIP suffix, AUR PKGBUILD and
/// user scripts fetch the Linux tarball by name, and GitHub Releases already
/// ship these exact filenames for v0.7.9–v0.7.11. Changing any of them 404s
/// existing updaters. The names are derived here (and pinned by
/// `crates/ccproxy/tests/dist_contract.rs`) so they cannot drift between
/// xtask, CI and the AUR template.
///
/// Rules (verified against published Releases):
///
/// * Windows ZIP: `commandcode-proxy-v<ver>-windows-x86_64.zip`
/// * Linux tarball: `commandcode-proxy-<ver>-linux-x86_64.tar.gz` (no `v`)
/// * Debian packages: `<pkg>_<ver>-1_amd64.deb` (Debian policy, no `v`)
/// * AppImage: `commandcode-proxy-<ver>-x86_64.AppImage`
fn windows_zip_name(version: &str) -> String {
    format!("commandcode-proxy-v{version}-windows-x86_64.zip")
}
fn linux_tarball_name(version: &str) -> String {
    format!("commandcode-proxy-{version}-linux-x86_64.tar.gz")
}

fn run(args: &[String]) -> Result<()> {
    let cmd = args.first().map(String::as_str).unwrap_or("");
    match cmd {
        "layout" => {
            let check = args.iter().any(|a| a == "--check");
            let layout = layout::Layout::load(&xtask::workspace_root())?;
            if check {
                println!("layout OK: {} entries across targets", layout.entries.len());
            }
            Ok(())
        }
        "stage" => {
            let opts = Parse::from(&args[1..]);
            let target = opts.required("target")?;
            let out = PathBuf::from(opts.required("out")?);
            let layout = layout::Layout::load(&xtask::workspace_root())?;
            let files = stage::stage(&layout, &target, &out)?;
            println!("staged {} files into {}", files.len(), out.display());
            Ok(())
        }
        "archive" => {
            let opts = Parse::from(&args[1..]);
            let platform = opts.required("platform")?;
            let input = PathBuf::from(opts.required("input")?);
            let out = PathBuf::from(opts.required("out")?);
            let files = list_staged(&input)?;
            let bytes = match platform.as_str() {
                "windows" => archive::build_zip(&files)?,
                "linux" => archive::build_tar_gz(&files)?,
                other => return Err(DistError(format!("unknown platform `{other}`"))),
            };
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&out, bytes)?;
            println!("wrote {} ({} bytes)", out.display(), out.metadata()?.len());
            Ok(())
        }
        "manifest" => {
            let opts = Parse::from(&args[1..]);
            let input = PathBuf::from(opts.required("input")?);
            let out = PathBuf::from(opts.required("out")?);
            let platform = opts.required("platform")?;
            let artifact_path = opts
                .value("artifact")
                .map(PathBuf::from)
                .ok_or_else(|| DistError("missing required --artifact <path>".to_string()))?;
            let version = xtask::workspace_version()?;
            let files = list_staged(&input)?;
            let artifact = manifest::ArtifactInfo::from_file(&artifact_path)?;
            let sums = sums_for_manifest(&out);
            manifest::write(
                &files, &version, &platform, "x86_64", &artifact, &out, &sums,
            )?;
            println!("wrote {} (and {})", out.display(), sums.display());
            Ok(())
        }
        "verify" => {
            let opts = Parse::from(&args[1..]);
            let platform = opts.required("platform")?;
            let target = opts.required("target")?;
            let input = PathBuf::from(opts.required("input")?);
            let artifact = opts.value("artifact").map(PathBuf::from);
            let files = list_staged(&input)?;
            verify::stage(&target, &files)?;
            if platform == "windows" {
                if let Some(zip) = &artifact {
                    verify::windows_zip(zip, &files)?;
                }
            }
            if let Some(zip) = &artifact {
                let sidecar = sidecar_for(zip, "manifest.json");
                if sidecar.is_file() {
                    let m = manifest::Manifest::read(&sidecar)?;
                    verify::against_manifest(&files, &m, Some(zip))?;
                } else {
                    println!("warn: no sidecar manifest at {}", sidecar.display());
                }
            }
            println!("verify OK for {target}");
            Ok(())
        }
        "dist" => {
            let opts = Parse::from(&args[1..]);
            let platform = opts.required("platform")?;
            let skip_build = args.iter().any(|a| a == "--skip-build");
            let layout = layout::Layout::load(&xtask::workspace_root())?;
            let version = xtask::workspace_version()?;
            if !skip_build {
                build_release()?;
            }
            let (target, artifact_name) = match platform.as_str() {
                "windows" => ("windows-portable", windows_zip_name(&version)),
                "linux" => ("linux-portable", linux_tarball_name(&version)),
                other => return Err(DistError(format!("unknown platform `{other}`"))),
            };
            let out_dir = xtask::workspace_root()
                .join("dist")
                .join("stage")
                .join(target);
            let files = stage::stage(&layout, target, &out_dir)?;
            verify::stage(target, &files)?;

            let artifact_path = xtask::workspace_root().join("dist").join(&artifact_name);
            let bytes = match platform.as_str() {
                "windows" => archive::build_zip(&files)?,
                _ => archive::build_tar_gz(&files)?,
            };
            std::fs::write(&artifact_path, bytes)?;

            // Per-artifact manifest + sums (no shared SHA256SUMS between platforms).
            let artifact = manifest::ArtifactInfo::from_file(&artifact_path)?;
            let manifest_json = sidecar_for(&artifact_path, "manifest.json");
            let sums = sidecar_for(&artifact_path, "SHA256SUMS");
            manifest::write(
                &files,
                &version,
                &platform,
                "x86_64",
                &artifact,
                &manifest_json,
                &sums,
            )?;

            if platform == "windows" {
                verify::windows_zip(&artifact_path, &files)?;
            }
            verify::against_manifest(
                &files,
                &manifest::Manifest::read(&manifest_json)?,
                Some(&artifact_path),
            )?;

            if platform == "linux" {
                // Stage the two Debian roots (FHS tree + DEBIAN/control + scripts).
                // dpkg-deb --build runs on the Linux runner; we only lay out the root.
                let dist = xtask::workspace_root().join("dist");
                let mut deb_names: Vec<String> = Vec::new();
                for (target_name, package) in [
                    ("deb-proxy", "commandcode-proxy"),
                    ("deb-tray", "commandcode-proxy-tray"),
                ] {
                    let root = dist.join("deb").join(package);
                    let staged = stage::stage(&layout, target_name, &root)?;
                    verify::stage(target_name, &staged)?;
                    deb::finalize_deb_root(
                        &root,
                        package,
                        &version,
                        &xtask::workspace_root(),
                        layout.deb_proxy.as_ref(),
                        layout.deb_tray.as_ref(),
                    )?;
                    let filename = if package == "commandcode-proxy" {
                        layout.deb_proxy.as_ref()
                    } else {
                        layout.deb_tray.as_ref()
                    }
                    .map(|m| deb::deb_filename(m, &version))
                    .unwrap_or_else(|| format!("{package}.deb"));
                    deb_names.push(filename.clone());
                    println!(
                        "deb root: {} ({} files; published as {})",
                        root.display(),
                        staged.len(),
                        filename
                    );
                }
                // Emit the canonical deb filenames so the release workflow does not
                // re-derive them (and never reads the version from the git ref).
                std::fs::write(dist.join("deb-names.txt"), deb_names.join("\n").as_bytes())?;
                println!("wrote dist/deb-names.txt");
                // Stage the AppDir (FHS tree shared with linux-portable) plus
                // AppRun and a top-level .desktop that appimagetool requires.
                let appdir = dist.join("appimage").join("AppDir");
                let _ = stage::stage(&layout, "appimage", &appdir)?;
                std::fs::write(appdir.join("AppRun"), deb::APP_RUN.as_bytes())?;
                // The AppImage runtime execs AppRun straight out of the mounted
                // squashfs. Written by `fs::write` it would land at mode 0644 and
                // every launch fails with `execv error: Permission denied` — a
                // real-run-only failure that no static check catches (found by
                // the FUSE acceptance job). Mirror the deb maintainer scripts.
                deb::set_executable(&appdir.join("AppRun"))?;
                std::fs::write(
                    appdir.join("commandcode-proxy.desktop"),
                    deb::APP_DESKTOP.as_bytes(),
                )?;
                // AppImage requires a top-level icon matching the desktop file's
                // `Icon=` key (and a `.DirIcon` symlink/copy). Find the layout's
                // .svg entry targeting appimage and copy it to the AppDir root.
                let icon = layout
                    .entries
                    .iter()
                    .find(|e| e.targets.iter().any(|t| t == "appimage") && e.dest.ends_with(".svg"))
                    .ok_or_else(|| {
                        DistError(
                            "layout has no .svg entry targeting appimage; AppDir root icon is required"
                                .to_string(),
                        )
                    })?;
                let staged_icon = appdir.join(icon.dest.trim_start_matches('/'));
                if !staged_icon.is_file() {
                    return Err(DistError(format!(
                        "staged AppDir icon missing at {}",
                        staged_icon.display()
                    )));
                }
                let stem = std::path::Path::new(&icon.dest)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("commandcode-proxy");
                std::fs::copy(&staged_icon, appdir.join(format!("{stem}.svg")))?;
                std::fs::copy(&staged_icon, appdir.join(".DirIcon"))?;
                println!(
                    "appdir: {} (root icon: {stem}.svg, .DirIcon)",
                    appdir.display()
                );
            }
            println!(
                "dist OK: {} (manifest: {}, sums: {})",
                artifact_path.display(),
                manifest_json.display(),
                sums.display()
            );
            Ok(())
        }
        other => Err(format!(
            "unknown command `{other}` (expected layout|stage|archive|manifest|verify|dist)"
        )
        .into()),
    }
}

/// Parse `--flag value` pairs.
struct Parse {
    pairs: Vec<(String, String)>,
}

impl Parse {
    fn from(args: &[String]) -> Self {
        let mut pairs = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if let Some(name) = a.strip_prefix("--") {
                let value = args.get(i + 1).cloned().unwrap_or_default();
                pairs.push((name.to_string(), value));
                i += 2;
            } else {
                i += 1;
            }
        }
        Self { pairs }
    }
    fn value(&self, name: &str) -> Option<String> {
        self.pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    }
    fn required(&self, name: &str) -> Result<String> {
        self.value(name)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("missing required --{name}").into())
    }
}

/// Recursively list files under a staged directory as `StagedFile`.
fn list_staged(root: &std::path::Path) -> Result<Vec<stage::StagedFile>> {
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

fn walk(
    base: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<stage::StagedFile>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk(base, &path, out)?;
            continue;
        }
        let rel = path
            .strip_prefix(base)?
            .to_string_lossy()
            .replace('\\', "/");
        out.push(stage::StagedFile {
            rel,
            abs: path,
            mode: 0o644,
        });
    }
    Ok(())
}

/// Run `cargo build --release` via argv, never through a shell.
fn build_release() -> Result<()> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = std::process::Command::new(cargo)
        .args(["build", "--release", "--locked"])
        .status()?;
    if !status.success() {
        return Err("cargo build --release failed".into());
    }
    Ok(())
}

/// `<path>.<suffix>` — appended to the *full* file name, so
/// `x.tar.gz` becomes `x.tar.gz.manifest.json` (not `x.tar.manifest.json`).
fn sidecar_for(path: &std::path::Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}.{}", path.display(), suffix))
}

/// Given a manifest output path `<base>.manifest.json`, return the matching
/// sums file `<base>.SHA256SUMS`.
fn sums_for_manifest(manifest_out: &std::path::Path) -> PathBuf {
    let name = manifest_out
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let base = name.strip_suffix(".manifest.json").unwrap_or(&name);
    let mut p = manifest_out.to_path_buf();
    p.set_file_name(format!("{base}.SHA256SUMS"));
    p
}

#[cfg(test)]
mod appicon_tests {
    use super::*;

    /// Parse `Icon=` out of a desktop file blob.
    fn desktop_icon(blob: &str) -> Option<&str> {
        blob.lines()
            .find(|l| l.starts_with("Icon="))
            .map(|l| l.trim_start_matches("Icon=").trim())
    }

    #[test]
    fn appdir_icon_name_matches_layout_svg_entry() {
        // The AppDir root icon is copied from the layout's .svg entry that
        // targets appimage, and its stem must equal the desktop file's Icon=.
        // If the icon is renamed in layout.toml, update APP_DESKTOP to match.
        let workspace = xtask::workspace_root();
        let layout = layout::Layout::load(&workspace).expect("layout loads");
        let svg = layout
            .entries
            .iter()
            .find(|e| e.targets.iter().any(|t| t == "appimage") && e.dest.ends_with(".svg"))
            .expect("layout has an appimage-targeting .svg entry");
        let stem = std::path::Path::new(&svg.dest)
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("svg dest has a file stem");
        assert_eq!(
            desktop_icon(deb::APP_DESKTOP),
            Some(stem),
            "APP_DESKTOP Icon= must match the layout svg entry stem"
        );
    }
}
