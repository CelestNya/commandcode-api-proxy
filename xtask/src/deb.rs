//! Debian package root assembly (spec §6.3).
//!
//! xtask stages the FHS tree from `packaging/layout.toml` (deb-proxy / deb-tray
//! targets), writes `DEBIAN/control` from the `[deb.*]` metadata table, and
//! copies maintainer scripts from `packaging/deb/scripts/`. The actual
//! `dpkg-deb --build` runs on the Linux runner; this crate does not invoke
//! dpkg. The split is fixed by the layout:
//!
//! * `deb-proxy` carries proxy + WebUI + systemd + env.
//! * `deb-tray` carries tray + desktop + icon and depends on the
//!   same-version-proxy package.

use std::path::Path;

use crate::{layout::DebMeta, DistError, Result};

/// Metadata for a `DEBIAN/control` file.
#[derive(Debug, Clone)]
pub struct Control {
    pub package: String,
    pub version: String,
    pub architecture: String,
    pub maintainer: String,
    pub section: String,
    pub priority: String,
    pub depends: Option<String>,
    pub description: String,
}

impl Control {
    /// Render the control file text (no trailing blank lines; `\n` line endings).
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("Package: {}\n", self.package));
        out.push_str(&format!("Version: {}\n", self.version));
        out.push_str(&format!("Architecture: {}\n", self.architecture));
        out.push_str(&format!("Maintainer: {}\n", self.maintainer));
        out.push_str(&format!("Section: {}\n", self.section));
        out.push_str(&format!("Priority: {}\n", self.priority));
        if let Some(depends) = &self.depends {
            out.push_str(&format!("Depends: {depends}\n"));
        }
        out.push_str(&format!("Description: {}\n", self.description));
        out
    }
}

/// Build a `Control` from layout metadata. The Debian version is
/// `<upstream>-<revision>` (e.g. `0.7.11-1`); the tray's static dependency
/// is expanded to `<depends> (= <deb-version>)`.
pub fn control_from_meta(meta: &DebMeta, upstream_version: &str) -> Control {
    let deb_version = format!("{upstream_version}-{}", meta.revision);
    let depends = meta
        .depends
        .as_ref()
        .map(|pkg| format!("{pkg} (= {deb_version})"));
    Control {
        package: meta.package.clone(),
        version: deb_version,
        architecture: meta.architecture.clone(),
        maintainer: meta.maintainer.clone(),
        section: meta.section.clone(),
        priority: meta.priority.clone(),
        depends,
        description: meta.description.clone(),
    }
}

/// The published Debian binary name: `<pkg>_<deb-version>_<arch>.deb`.
/// (Debian policy: no `v` prefix on the upstream part inside the filename.)
pub fn deb_filename(meta: &DebMeta, upstream_version: &str) -> String {
    format!(
        "{}_{}-{}_{}.deb",
        meta.package, upstream_version, meta.revision, meta.architecture
    )
}

/// Write `DEBIAN/control` into the staged deb root, and copy maintainer scripts
/// from `packaging/deb/scripts/`. The FHS files themselves are staged by
/// `crate::stage::stage` against the `deb-proxy` / `deb-tray` layout targets.
pub fn finalize_deb_root(
    root: &Path,
    package: &str,
    upstream_version: &str,
    workspace: &Path,
    deb_proxy: Option<&DebMeta>,
    deb_tray: Option<&DebMeta>,
) -> Result<()> {
    let meta = match package {
        "commandcode-proxy" => deb_proxy
            .ok_or_else(|| DistError("layout.toml is missing [deb.proxy] metadata".to_string()))?,
        "commandcode-proxy-tray" => deb_tray
            .ok_or_else(|| DistError("layout.toml is missing [deb.tray] metadata".to_string()))?,
        other => return Err(DistError(format!("unknown deb package `{other}`"))),
    };
    let control = control_from_meta(meta, upstream_version);

    let debian = root.join("DEBIAN");
    std::fs::create_dir_all(&debian)?;
    std::fs::write(debian.join("control"), control.render().as_bytes())?;

    // Copy maintainer scripts verbatim; chmod 755 (dpkg requires them executable).
    let scripts_dir = workspace.join("packaging").join("deb").join("scripts");
    if !scripts_dir.is_dir() {
        return Err(DistError(format!(
            "maintainer scripts dir missing: {}",
            scripts_dir.display()
        )));
    }
    for name in ["preinst", "postinst", "prerm", "postrm"] {
        let src = scripts_dir.join(name);
        if !src.is_file() {
            return Err(DistError(format!(
                "maintainer script missing: {}",
                src.display()
            )));
        }
        let bytes = std::fs::read(&src)?;
        let dst = debian.join(name);
        std::fs::write(&dst, bytes)?;
        set_executable(&dst)?;
    }
    // control must be 644.
    set_mode_644(&debian.join("control"))?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(p)?.permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(p, perm)?;
    Ok(())
}

#[cfg(unix)]
fn set_mode_644(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(p)?.permissions();
    perm.set_mode(0o644);
    std::fs::set_permissions(p, perm)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_p: &Path) -> Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn set_mode_644(_p: &Path) -> Result<()> {
    Ok(())
}

/// AppRun shell script written into the AppDir root. Points `CC_WEBUI_DIR` at
/// the bundled webui and execs the proxy binary.
pub const APP_RUN: &str = "#!/bin/sh\n# AppImage entry point for commandcode-proxy.\nexport CC_WEBUI_DIR=\"$APPDIR/usr/share/commandcode-proxy/webui\"\nexec \"$APPDIR/usr/bin/commandcode-proxy\" \"$@\"\n";

/// Top-level desktop file appimagetool expects at the AppDir root.
pub const APP_DESKTOP: &str = "[Desktop Entry]\nType=Application\nName=commandcode-proxy\nComment=commandcode.ai API translation proxy\nExec=commandcode-proxy\nIcon=commandcode-proxy\nTerminal=true\nCategories=Network;\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy_meta() -> DebMeta {
        DebMeta {
            package: "commandcode-proxy".into(),
            maintainer: "CelestNya <c@e>".into(),
            section: "net".into(),
            priority: "optional".into(),
            architecture: "amd64".into(),
            revision: "1".into(),
            description: "API translation proxy for commandcode.ai".into(),
            depends: None,
        }
    }
    fn tray_meta() -> DebMeta {
        DebMeta {
            package: "commandcode-proxy-tray".into(),
            maintainer: "CelestNya <c@e>".into(),
            section: "net".into(),
            priority: "optional".into(),
            architecture: "amd64".into(),
            revision: "1".into(),
            description: "System tray companion".into(),
            depends: Some("commandcode-proxy".into()),
        }
    }

    #[test]
    fn control_renders_with_revision() {
        let c = control_from_meta(&proxy_meta(), "0.7.11");
        let text = c.render();
        assert!(text.contains("Package: commandcode-proxy"));
        assert!(text.contains("Version: 0.7.11-1"));
        assert!(text.contains("Architecture: amd64"));
        assert!(!text.contains("\n\n"));
    }

    #[test]
    fn tray_depends_on_same_debian_version() {
        let c = control_from_meta(&tray_meta(), "0.7.11");
        assert!(c.depends.as_deref() == Some("commandcode-proxy (= 0.7.11-1)"));
    }

    #[test]
    fn deb_filename_follows_debian_policy() {
        assert_eq!(
            deb_filename(&proxy_meta(), "0.7.11"),
            "commandcode-proxy_0.7.11-1_amd64.deb"
        );
        assert_eq!(
            deb_filename(&tray_meta(), "0.7.11"),
            "commandcode-proxy-tray_0.7.11-1_amd64.deb"
        );
    }
}
