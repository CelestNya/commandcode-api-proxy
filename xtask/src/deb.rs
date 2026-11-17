//! Debian package root assembly (spec §6.3).
//!
//! xtask stages the FHS tree from `packaging/layout.toml` (deb-proxy / deb-tray
//! targets), writes `DEBIAN/control`, and copies maintainer scripts from
//! `packaging/deb/scripts/`. The actual `dpkg-deb --build` runs on the Linux
//! runner; this crate does not invoke dpkg. The split is fixed by the layout:
//! `deb-proxy` carries proxy + WebUI + systemd + env, and `deb-tray` carries
//! tray + desktop + icon and depends on the same-version proxy package.

use std::path::Path;

use crate::{DistError, Result};

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

/// The control file for the proxy package. Depends is left for the CI step to
/// fill via `dpkg-shlibdeps`; on Windows we just write a placeholder.
pub fn proxy_control(version: &str) -> Control {
    Control {
        package: "commandcode-proxy".to_string(),
        version: version.to_string(),
        architecture: "amd64".to_string(),
        maintainer: "CelestNya <celestnya@users.noreply.github.com>".to_string(),
        section: "net".to_string(),
        priority: "optional".to_string(),
        // CI runs `dpkg-shlibdeps -O -e usr/bin/commandcode-proxy` and writes
        // the resulting Depends: line into DEBIAN/control. Empty here.
        depends: None,
        description: "API translation proxy for commandcode.ai".to_string(),
    }
}

/// The control file for the tray package, depending on the same-version proxy.
pub fn tray_control(version: &str) -> Control {
    Control {
        package: "commandcode-proxy-tray".to_string(),
        version: version.to_string(),
        architecture: "amd64".to_string(),
        maintainer: "CelestNya <celestnya@users.noreply.github.com>".to_string(),
        section: "net".to_string(),
        priority: "optional".to_string(),
        depends: Some(format!("commandcode-proxy (= {version})")),
        description: "System tray companion for commandcode-proxy".to_string(),
    }
}

/// Write `DEBIAN/control` into the staged deb root, and copy maintainer scripts
/// from `packaging/deb/scripts/`. The FHS files themselves are staged by
/// `crate::stage::stage` against the `deb-proxy` / `deb-tray` layout targets.
pub fn finalize_deb_root(
    root: &Path,
    package: &str,
    version: &str,
    workspace: &Path,
) -> Result<()> {
    let debian = root.join("DEBIAN");
    std::fs::create_dir_all(&debian)?;

    let control = match package {
        "commandcode-proxy" => proxy_control(version),
        "commandcode-proxy-tray" => tray_control(version),
        other => return Err(DistError(format!("unknown deb package `{other}`"))),
    };
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

    #[test]
    fn control_renders_required_fields() {
        let c = tray_control("0.7.11");
        let text = c.render();
        assert!(text.contains("Package: commandcode-proxy-tray"));
        assert!(text.contains("Version: 0.7.11"));
        assert!(text.contains("Depends: commandcode-proxy (= 0.7.11)"));
        assert!(text.contains("Architecture: amd64"));
        assert!(!text.contains("\n\n"));
    }

    #[test]
    fn proxy_control_has_no_tray_dependency() {
        let c = proxy_control("0.7.11");
        assert!(!c.render().contains("tray"));
    }
}
