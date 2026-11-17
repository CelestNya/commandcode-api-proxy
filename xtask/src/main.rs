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
                "windows" => (
                    "windows-portable",
                    format!("commandcode-proxy-v{version}-windows-x86_64.zip"),
                ),
                "linux" => (
                    "linux-portable",
                    format!("commandcode-proxy-v{version}-linux-x86_64.tar.gz"),
                ),
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
                for (target_name, package) in [
                    ("deb-proxy", "commandcode-proxy"),
                    ("deb-tray", "commandcode-proxy-tray"),
                ] {
                    let root = dist.join("deb").join(package);
                    let staged = stage::stage(&layout, target_name, &root)?;
                    verify::stage(target_name, &staged)?;
                    deb::finalize_deb_root(&root, package, &version, &xtask::workspace_root())?;
                    println!("deb root: {} ({} files)", root.display(), staged.len());
                }
                // Stage the AppDir (FHS tree shared with linux-portable) plus
                // AppRun and a top-level .desktop that appimagetool requires.
                let appdir = dist.join("appimage").join("AppDir");
                let _ = stage::stage(&layout, "appimage", &appdir)?;
                std::fs::write(appdir.join("AppRun"), deb::APP_RUN.as_bytes())?;
                std::fs::write(
                    appdir.join("commandcode-proxy.desktop"),
                    deb::APP_DESKTOP.as_bytes(),
                )?;
                println!("appdir: {}", appdir.display());
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
