//! Debian package root assembly (spec §6.3).
//!
//! M1 produces the debian *root* (control file + maintainer scripts alongside
//! the staged FHS tree); actually invoking `dpkg-deb --build` happens on the
//! Linux runner in a later milestone and is deliberately not done here. The
//! split is fixed by the layout: `deb-proxy` carries proxy + WebUI + systemd +
//! env, and `deb-tray` carries tray + desktop + icon and depends on the proxy
//! package.

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
    /// Render the control file text.
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

/// The control file for the proxy package, matching the current cargo-deb
/// metadata in `crates/ccproxy/Cargo.toml`.
pub fn proxy_control(version: &str) -> Control {
    Control {
        package: "commandcode-proxy".to_string(),
        version: version.to_string(),
        architecture: "amd64".to_string(),
        maintainer: "CelestNya <celestnya@users.noreply.github.com>".to_string(),
        section: "net".to_string(),
        priority: "optional".to_string(),
        depends: Some("$auto".to_string()),
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
        depends: Some("commandcode-proxy".to_string()),
        description: "System tray companion for commandcode-proxy".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Result;

    #[test]
    fn control_renders_required_fields() {
        let c = tray_control("0.7.11");
        let text = c.render();
        assert!(text.contains("Package: commandcode-proxy-tray"));
        assert!(text.contains("Version: 0.7.11"));
        assert!(text.contains("Depends: commandcode-proxy"));
        assert!(text.contains("Architecture: amd64"));
        // No blank lines inside the stanza.
        assert!(!text.contains("\n\n"));
    }

    #[test]
    fn proxy_control_has_no_tray_dependency() -> Result<()> {
        let c = proxy_control("0.7.11");
        assert!(!c.render().contains("tray"));
        Ok(())
    }
}
