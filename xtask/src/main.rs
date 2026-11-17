//! `cargo xtask` entry point: argument parsing and exit codes only.
//!
//! Subcommands (spec §5):
//!
//! ```text
//! cargo xtask layout --check
//! cargo xtask stage --platform <windows|linux> --target <t> --out <dir>
//! cargo xtask archive --platform <windows|linux> --input <dir> --out <file>
//! cargo xtask manifest --input <dir> --out <file>
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
            let version = xtask::workspace_version()?;
            let files = list_staged(&input)?;
            let bytes = manifest::write(&files, &version, "windows", "x86_64", &input)?;
            std::fs::write(&out, bytes)?;
            println!("wrote {}", out.display());
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
                if let Some(zip) = artifact {
                    verify::windows_zip(&zip, &files)?;
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

            manifest::write(
                &files,
                &version,
                &platform,
                "x86_64",
                &xtask::workspace_root().join("dist"),
            )?;

            if platform == "windows" {
                verify::windows_zip(&artifact_path, &files)?;
            }
            // Also emit the debian control stanzas for the Linux split.
            if platform == "linux" {
                let proxy = deb::proxy_control(&version);
                let tray = deb::tray_control(&version);
                println!("proxy control:\n{}", proxy.render());
                println!("tray control:\n{}", tray.render());
            }
            println!("dist OK: {}", artifact_path.display());
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
