//! The control file: how the WebUI asks the tray for something only the tray
//! can do.
//!
//! The WebUI is served by the *proxy*, but two of its actions belong to the
//! supervisor: restart the proxy, and switch to a newly staged version. The
//! proxy cannot do either itself — restarting in place would leave the tray
//! supervising a child that no longer exists, and spawning a replacement would
//! put two proxies on one port.
//!
//! So the proxy writes a request next to its config and the tray consumes it on
//! its next tick. A file rather than a socket or a named pipe because the tray
//! is not always running, both processes already share this directory, and a
//! request that arrives while the tray is down is still there when it returns.
//! The request is *deleted before it is acted on* ([`take`]), so a crash
//! mid-restart cannot produce an endless restart loop on the next launch.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Beside the settings file, and hidden so it does not look like user config.
const CONTROL_NAME: &str = ".ccproxy-control.json";

/// The control file that belongs to the settings file at `config`.
#[must_use]
pub fn control_path(config: &Path) -> PathBuf {
    config.with_file_name(CONTROL_NAME)
}

/// A request to restart the proxy in place.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Restart {
    /// Unix seconds; informational (the tray logs it).
    pub at: u64,
    /// `settings` / `update` / … — why, for the log line.
    pub reason: String,
    /// Revision of the settings file the restart is meant to pick up.
    #[serde(default)]
    pub revision: String,
}

/// A request to switch to a staged install.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Handover {
    pub at: u64,
    /// The staged install root (contains `CCProxyTray.exe`).
    pub root: String,
    /// The tray executable to launch, for a clear error if the path is wrong.
    pub tray: String,
    pub version: String,
}

/// Whatever is pending. Absent fields are absent requests.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Request {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart: Option<Restart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handover: Option<Handover>,
}

impl Request {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.restart.is_none() && self.handover.is_none()
    }
}

/// Read the pending request without claiming it (for status displays).
#[must_use]
pub fn peek(config: &Path) -> Request {
    std::fs::read_to_string(control_path(config))
        .ok()
        .and_then(|text| serde_json::from_str::<Request>(&text).ok())
        .unwrap_or_default()
}

/// Claim the pending request: read it and delete the file, so acting on it
/// cannot be repeated.
///
/// `Err` means the file existed but could not be understood — a truncated write
/// from a non-atomic writer, or a hand-edited file with a typo. The file is
/// deleted either way (a request that cannot be parsed must not be replayed
/// forever), but the caller logs the reason: a dropped restart or handover that
/// says nothing is indistinguishable from a request that never arrived, and
/// that is precisely the kind of silence this project has been bitten by.
pub fn take(config: &Path) -> Result<Request, String> {
    let path = control_path(config);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Request::default());
    };
    // Delete first: if the process dies between here and the action, the
    // request is lost rather than replayed forever. A lost restart is
    // recoverable (the user clicks again); a restart loop is not.
    let _ = std::fs::remove_file(&path);
    serde_json::from_str::<Request>(&text)
        .map_err(|e| format!("{} 无法解析（{e}），已丢弃", path.display()))
}

/// Merge `request` into the control file, preserving the other field.
///
/// Read-modify-write rather than overwrite: the update path may stage while a
/// settings restart is pending, and dropping either would silently ignore a
/// user action.
pub fn merge(config: &Path, request: Request) -> std::io::Result<()> {
    let mut current = peek(config);
    if request.restart.is_some() {
        current.restart = request.restart;
    }
    if request.handover.is_some() {
        current.handover = request.handover;
    }
    write(config, &current)
}

fn write(config: &Path, request: &Request) -> std::io::Result<()> {
    let path = control_path(config);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(request).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text.as_bytes())?;
    std::fs::rename(&tmp, &path)
}

/// Ask for an in-place restart that picks up `revision` of the settings file.
pub fn request_restart(config: &Path, reason: &str, revision: &str) -> std::io::Result<()> {
    merge(
        config,
        Request {
            restart: Some(Restart {
                at: crate::now_epoch_secs(),
                reason: reason.to_string(),
                revision: revision.to_string(),
            }),
            handover: None,
        },
    )
}

/// Ask the tray to switch to a staged install.
pub fn request_handover(
    config: &Path,
    root: &Path,
    tray: &Path,
    version: &str,
) -> std::io::Result<()> {
    merge(
        config,
        Request {
            restart: None,
            handover: Some(Handover {
                at: crate::now_epoch_secs(),
                root: root.display().to_string(),
                tray: tray.display().to_string(),
                version: version.to_string(),
            }),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ccproxy-control-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("ccproxy.json")
    }

    #[test]
    fn the_control_file_sits_beside_the_config() {
        let config = Path::new("/srv/ccproxy/ccproxy.json");
        assert_eq!(
            control_path(config),
            Path::new("/srv/ccproxy/.ccproxy-control.json")
        );
    }

    #[test]
    fn a_restart_request_round_trips_and_is_claimed_once() {
        let config = temp_config("restart");
        request_restart(&config, "settings", "abc123").expect("write");

        let first = take(&config).expect("parsed");
        assert_eq!(
            first.restart.as_ref().map(|r| r.reason.as_str()),
            Some("settings")
        );
        assert_eq!(
            first.restart.as_ref().map(|r| r.revision.as_str()),
            Some("abc123")
        );
        assert!(
            take(&config).expect("parsed").restart.is_none(),
            "a claimed request is not served twice"
        );
        let _ = std::fs::remove_dir_all(config.parent().expect("parent"));
    }

    #[test]
    fn a_handover_does_not_erase_a_pending_restart() {
        // The update path can run while a settings restart is queued; both
        // actions must survive, each in its own field.
        let config = temp_config("merge");
        request_restart(&config, "settings", "rev1").expect("restart");
        request_handover(
            &config,
            Path::new("D:/CCProxy-v0.8.0-rust"),
            Path::new("D:/CCProxy-v0.8.0-rust/CCProxyTray.exe"),
            "0.8.0",
        )
        .expect("handover");

        let doc = peek(&config);
        assert!(doc.restart.is_some(), "the restart survives the merge");
        assert_eq!(
            doc.handover.as_ref().map(|h| h.version.as_str()),
            Some("0.8.0")
        );
        let _ = std::fs::remove_dir_all(config.parent().expect("parent"));
    }

    #[test]
    fn a_missing_file_reads_as_no_request() {
        let config = temp_config("missing");
        assert!(peek(&config).is_empty());
        assert!(take(&config).expect("absent is not an error").is_empty());
        let _ = std::fs::remove_dir_all(config.parent().expect("parent"));
    }

    #[test]
    fn a_corrupt_file_does_not_wedge_the_tray() {
        let config = temp_config("corrupt");
        std::fs::write(control_path(&config), b"{ not json").expect("seed");
        // Reported, not panicked on, and consumed so it stops being read: the
        // tray must say *something* rather than look like nothing happened.
        assert!(peek(&config).is_empty());
        let err = take(&config).expect_err("corrupt is reported");
        assert!(err.contains("无法解析"), "{err}");
        assert!(!control_path(&config).exists());
        let _ = std::fs::remove_dir_all(config.parent().expect("parent"));
    }
}
