//! Silent update detection, and the staging half of a seamless takeover.
//!
//! Two rules shape this module:
//!
//! * **Detection never interrupts.** A newer release is discovered in the
//!   background and recorded in a small state file; the tray menu and the
//!   WebUI show "有更新可用" and nothing else happens. No dialog, no toast, no
//!   restart. An update is an action the user takes, not one the proxy takes.
//! * **Nothing about the running install changes until the switch.** Staging
//!   downloads, unpacks into a *sibling* directory, inherits the config and
//!   copies the logs — all while the current process keeps serving. The only
//!   step left for the switch is stop→start, which is the tray's job and takes
//!   well under the client's retry budget (bili gives up after ~4.5 s of
//!   connection refusals; a measurable 502 in the field came from a 30 s
//!   manual window).
//!
//! A/B by directory, not by rewriting files in place: the new version lands in
//! `CCProxy-v<new>-rust` beside the old one, so a failed handover leaves a
//! completely intact previous install to fall back to, and the old directory
//! can be deleted by hand once the new one is known good. In-place overwrite
//! would make a bad download unrecoverable.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The repository whose releases are the update source.
pub const REPO: &str = "CelestNya/commandcode-api-proxy";
/// How often a background check is allowed to hit the API. Unauthenticated
/// GitHub allows 60 requests/hour per address; 6 hours is both polite and
/// sufficient for a tool that is launched most days.
pub const CHECK_INTERVAL_SECS: u64 = 6 * 60 * 60;
/// Refuse a download larger than this. The Windows package is ~8 MB.
const MAX_DOWNLOAD_BYTES: u64 = 128 * 1024 * 1024;

/// What the last check found, persisted so a restart does not re-ask.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct State {
    /// Unix seconds of the last completed check.
    #[serde(default)]
    pub checked_at: u64,
    /// The newest release tag seen, without the leading `v`.
    #[serde(default)]
    pub latest: String,
    /// Direct download URL of the asset for this platform, if the release has
    /// one. Absent means "newer exists but nothing to install from here".
    #[serde(default)]
    pub url: String,
    /// Byte size of that asset, for the progress display.
    #[serde(default)]
    pub size: u64,
    /// A finished staging run waiting for the handover.
    #[serde(default)]
    pub staged: Option<Staged>,
    /// Last failure, cleared by the next success.
    #[serde(default)]
    pub error: String,
}

/// A staged install: unpacked, config inherited, ready to be switched to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Staged {
    pub version: String,
    /// Absolute path of the staged install root.
    pub root: String,
    /// The tray executable inside it.
    pub tray: String,
    pub at: u64,
}

impl State {
    /// Whether a newer release exists than `current`.
    #[must_use]
    pub fn available(&self, current: &str) -> bool {
        is_newer(&self.latest, current)
    }

    /// The JSON the WebUI and the tray read.
    #[must_use]
    pub fn to_json(&self, current: &str) -> serde_json::Value {
        serde_json::json!({
            "current": current,
            "latest": self.latest,
            "available": self.available(current),
            "checkedAt": if self.checked_at == 0 { serde_json::Value::Null } else { serde_json::Value::from(self.checked_at) },
            "size": self.size,
            "staged": self.staged.as_ref().map(|s| serde_json::json!({
                "version": s.version,
                "root": s.root,
                "at": s.at,
            })),
            "error": self.error,
            // The page must not promise an install it cannot perform.
            "installable": self.available(current) && !self.url.is_empty(),
        })
    }
}

/// The state file that belongs to the settings file at `config`.
///
/// The tray reads it through the *proxy's* config path, the same way the proxy
/// writes it — resolving from the caller's own executable would split the two
/// again (tray at `<root>/`, proxy at `<root>/service/`).
#[must_use]
pub fn state_path_for(config: &Path) -> PathBuf {
    config.with_file_name(".ccproxy-update.json")
}

/// The state file, beside this process's settings file.
#[must_use]
pub fn state_path() -> Option<PathBuf> {
    let config = crate::settings::config_path()?;
    Some(state_path_for(&config))
}

/// Read the persisted state; an absent or unreadable file is "never checked".
#[must_use]
pub fn load() -> State {
    crate::settings::config_path()
        .map(|c| load_at(&c))
        .unwrap_or_default()
}

/// Read the state recorded beside the settings file at `config`.
#[must_use]
pub fn load_at(config: &Path) -> State {
    std::fs::read_to_string(state_path_for(config))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Persist `state` atomically.
pub fn save(state: &State) -> Result<(), String> {
    let Some(path) = state_path() else {
        return Err("无法定位更新状态文件".to_string());
    };
    let text = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text.as_bytes()).map_err(|e| format!("写入状态失败：{e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("替换状态失败：{e}"))
}

/// Compare dotted numeric versions. Anything unparsable compares as zero, and
/// a preprocessing suffix (`-rc1`) is ignored: the release channel here has no
/// pre-releases, and refusing to update because a tag had a suffix would be
/// worse than the suffix being ignored.
#[must_use]
pub fn is_newer(candidate: &str, current: &str) -> bool {
    parse_version(candidate) > parse_version(current)
}

fn parse_version(raw: &str) -> (u64, u64, u64) {
    let trimmed = raw.trim().trim_start_matches('v');
    let core = trimmed.split(['-', '+']).next().unwrap_or("");
    let mut parts = core.split('.');
    let mut next = || {
        parts
            .next()
            .and_then(|p| p.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    (next(), next(), next())
}

/// The asset name a release publishes for this platform.
#[must_use]
pub fn asset_suffix() -> &'static str {
    if cfg!(windows) {
        "windows-x86_64.zip"
    } else {
        "linux-x86_64.tar.gz"
    }
}

/// One published asset: `(name, download URL, byte size)`.
pub type Asset = (String, String, u64);

/// Pick the download for this platform out of a release's asset list.
///
/// `assets` is the `assets` array of the GitHub release JSON, as
/// `(name, browser_download_url, size)` triples — parsed by the caller so this
/// stays testable without a network.
#[must_use]
pub fn pick_asset(assets: &[Asset]) -> Option<Asset> {
    let suffix = asset_suffix();
    assets
        .iter()
        .find(|(name, _, _)| name.ends_with(suffix))
        .cloned()
}

/// Parse the release JSON into `(tag, assets)`.
pub fn parse_release(body: &str) -> Result<(String, Vec<Asset>), String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("发布信息不是有效 JSON：{e}"))?;
    let tag = value
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "发布信息缺少 tag_name".to_string())?
        .trim_start_matches('v')
        .to_string();
    let assets = value
        .get("assets")
        .and_then(serde_json::Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|a| {
                    let name = a.get("name")?.as_str()?.to_string();
                    let url = a.get("browser_download_url")?.as_str()?.to_string();
                    let size = a
                        .get("size")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    Some((name, url, size))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok((tag, assets))
}

/// Fetch the newest release. Blocking; the caller runs it off the UI thread.
///
/// Egress goes through the same agent factory as upstream traffic, so an
/// install behind a proxy (or one that has fallen back to direct) behaves the
/// same for updates as for requests.
pub fn fetch_latest(current: &str) -> Result<State, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let agent = crate::proxy::agent_with_timeouts(&url, 10_000, 10_000, 20_000);
    let response = agent
        .get(&url)
        .set("Accept", "application/vnd.github+json")
        // GitHub rejects requests without a User-Agent.
        .set("User-Agent", "commandcode-proxy-updater")
        .call()
        .map_err(|e| format!("检查更新失败：{e}"))?;
    let body = response
        .into_string()
        .map_err(|e| format!("读取发布信息失败：{e}"))?;
    let (tag, assets) = parse_release(&body)?;
    let picked = pick_asset(&assets);
    let mut state = load();
    // Never invent an "available" from a tag we could not match to bytes: the
    // page would offer an install button that cannot work.
    if !is_newer(&tag, current) {
        state.staged = None;
    }
    state.latest = tag;
    state.checked_at = crate::now_epoch_secs();
    state.error.clear();
    if let Some((_, url, size)) = picked {
        state.url = url;
        state.size = size;
    } else {
        state.url.clear();
        state.size = 0;
    }
    save(&state)?;
    Ok(state)
}

/// Whether a background check is due. A state that was never checked is
/// always due, which is what makes the first launch after an upgrade ask once.
#[must_use]
pub fn check_due(state: &State, now: u64) -> bool {
    if state.checked_at == 0 {
        return true;
    }
    now.saturating_sub(state.checked_at) >= CHECK_INTERVAL_SECS
}

/// The install root this process belongs to: the directory above `service/`
/// when the packaged layout is in use, else the executable's own directory.
#[must_use]
pub fn install_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    if dir.file_name().map(|n| n == "service").unwrap_or(false) {
        return dir.parent().map(Path::to_path_buf);
    }
    Some(dir.to_path_buf())
}

/// Where version `version` would be staged: a sibling of the current install.
///
/// Sibling rather than child: an install directory that contains the old and
/// the new version nesting inside it makes "delete the old one" ambiguous, and
/// the handover launches the new tray by absolute path anyway.
#[must_use]
pub fn staged_root_for(version: &str) -> Option<PathBuf> {
    let root = install_root()?;
    let parent = root.parent()?;
    Some(parent.join(format!("CCProxy-v{version}-rust")))
}

/// Copy the parts of the current install that are not in the package.
///
/// The package ships binaries and a default config; everything a user has
/// accumulated lives beside it. `logs/` is copied because the tray and the
/// WebUI both point at files inside the install directory — losing the history
/// on every update would make a post-update problem impossible to diagnose.
/// The ledger and the model-catalog cache are already outside the install (in
/// the per-user data directory), so they survive by construction.
pub fn inherit(from_root: &Path, to_root: &Path) -> Vec<String> {
    inherit_with(
        from_root,
        to_root,
        crate::settings::config_path().as_deref(),
    )
}

/// [`inherit`] with the source config named explicitly, so the copy can be
/// tested without a process-global settings path.
fn inherit_with(from_root: &Path, to_root: &Path, from_config: Option<&Path>) -> Vec<String> {
    let mut notes = Vec::new();
    // Config: the package's copy is a fresh default, which would silently turn
    // image compression off and reset the egress mode. The *source* is the
    // file this process actually read (`settings::config_path`), not a guessed
    // path — the packaged layout puts it under `service/`, a flat build puts it
    // beside the executable, and only the running process knows which.
    if let Some(from_config) = from_config {
        if from_config.is_file() {
            // The package decides where the config goes: if it shipped a
            // `service/` directory, that is where the proxy it launches will
            // look.
            let to_config = if to_root.join("service").is_dir() {
                to_root.join("service").join("ccproxy.json")
            } else {
                to_root.join("ccproxy.json")
            };
            let result = if to_config.parent().is_some_and(Path::exists) {
                std::fs::copy(from_config, &to_config)
            } else {
                // No directory to copy into is still recoverable: create it.
                std::fs::create_dir_all(to_root)
                    .and_then(|()| std::fs::copy(from_config, &to_config))
            };
            match result {
                Ok(_) => notes.push(format!("配置已继承（{}）", from_config.display())),
                Err(e) => notes.push(format!("配置继承失败：{e}")),
            }
        }
    }
    // Logs: the proxy's directory under `service/`, the tray's at the root of
    // the install, and — for a flat build — whatever directory this process
    // resolved for itself. Copied file by file so a locked or huge file cannot
    // fail the whole staging run.
    for (from, relative) in log_sources(from_root) {
        let to = to_root.join(relative);
        if std::fs::create_dir_all(&to).is_err() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&from) else {
            continue;
        };
        let mut copied = 0usize;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name() else {
                continue;
            };
            if std::fs::copy(&path, to.join(name)).is_ok() {
                copied = copied.saturating_add(1);
            }
        }
        if copied > 0 {
            notes.push(format!("日志已继承 {copied} 个文件（{relative}）"));
        }
    }
    notes
}

/// The log directories to carry over, as `(source, path inside the install)`.
///
/// The two fixed layouts come first; the process's own resolved directory is
/// appended when it is a third one (a flat build, or `CC_PROXY_LOG_DIR`).
fn log_sources(from_root: &Path) -> Vec<(PathBuf, &'static str)> {
    let mut out: Vec<(PathBuf, &'static str)> = Vec::new();
    for (relative, slot) in [("service/logs", "service/logs"), ("logs", "logs")] {
        let dir = from_root.join(relative);
        if dir.is_dir() {
            out.push((dir, slot));
        }
    }
    if let Some(own) = crate::config::log_path().parent() {
        if !out.iter().any(|(p, _)| p == own) {
            let slot = if own.ends_with("service/logs") || own.ends_with("service\\logs") {
                "service/logs"
            } else {
                "logs"
            };
            out.push((own.to_path_buf(), slot));
        }
    }
    out
}

/// Download, unpack and inherit. Blocking; the caller runs it off the UI
/// thread and reports progress through [`Job`].
///
/// Idempotent in the useful direction: if the target directory already exists
/// and contains a tray executable, the download is skipped and only the
/// inheritance is repeated. A half-written staging directory (the process died
/// mid-extract) is wiped first — its contents cannot be trusted to be complete.
pub fn stage(state: &State, progress: &dyn Fn(&str)) -> Result<Staged, String> {
    if state.url.is_empty() {
        return Err("没有可用的下载地址".to_string());
    }
    let Some(target) = staged_root_for(&state.latest) else {
        return Err("无法确定暂存目录".to_string());
    };
    let Some(from_root) = install_root() else {
        return Err("无法确定当前安装目录".to_string());
    };
    if target == from_root {
        return Err("暂存目录与当前安装目录相同".to_string());
    }
    let tray = target.join(if cfg!(windows) {
        "CCProxyTray.exe"
    } else {
        "CCProxyTray"
    });

    if !tray.is_file() {
        progress("正在下载更新包…");
        let bytes = download(&state.url, state.size)?;
        progress("正在解压…");
        // Unpack beside the target, then rename: an interrupted extract must
        // never look like a usable install.
        let staging = target.with_extension("partial");
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_dir_all(&target);
        crate::archive::extract(&bytes, &staging).map_err(|e| format!("解压失败：{e}"))?;
        if !staging
            .join(if cfg!(windows) {
                "CCProxyTray.exe"
            } else {
                "CCProxyTray"
            })
            .is_file()
        {
            let _ = std::fs::remove_dir_all(&staging);
            return Err("更新包里没有托盘程序".to_string());
        }
        std::fs::rename(&staging, &target).map_err(|e| format!("安装目录就位失败：{e}"))?;
    }

    progress("正在继承配置与日志…");
    for note in inherit(&from_root, &target) {
        progress(&note);
    }

    let staged = Staged {
        version: state.latest.clone(),
        root: target.display().to_string(),
        tray: tray.display().to_string(),
        at: crate::now_epoch_secs(),
    };
    let mut persisted = load();
    persisted.staged = Some(staged.clone());
    persisted.error.clear();
    save(&persisted)?;
    Ok(staged)
}

/// Fetch the asset into memory.
///
/// The packages are single-digit megabytes, so buffering is simpler than a
/// streamed file and lets the size cap be enforced before anything is written.
fn download(url: &str, expected: u64) -> Result<Vec<u8>, String> {
    if expected > MAX_DOWNLOAD_BYTES {
        return Err(format!("更新包体积异常（{expected} 字节）"));
    }
    let agent = crate::proxy::agent_with_timeouts(url, 15_000, 15_000, 300_000);
    let response = agent
        .get(url)
        .set("User-Agent", "commandcode-proxy-updater")
        .call()
        .map_err(|e| format!("下载失败：{e}"))?;
    if response.status() != 200 {
        return Err(format!("下载失败：HTTP {}", response.status()));
    }
    let mut bytes = Vec::new();
    use std::io::Read;
    let read = response
        .into_reader()
        .take(MAX_DOWNLOAD_BYTES.saturating_add(1))
        .read_to_end(&mut bytes);
    if let Err(e) = read {
        return Err(format!("读取更新包失败：{e}"));
    }
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_DOWNLOAD_BYTES {
        return Err("更新包超过体积上限".to_string());
    }
    if expected > 0 && u64::try_from(bytes.len()).unwrap_or(0) != expected {
        return Err(format!(
            "更新包大小不符（期望 {expected}，实际 {}）",
            bytes.len()
        ));
    }
    Ok(bytes)
}

/// Env var that turns detection off entirely (CI, tests, air-gapped installs).
pub const DISABLE_ENV: &str = "CC_NO_UPDATE_CHECK";

/// Start the background detector: check now, then every
/// [`CHECK_INTERVAL_SECS`].
///
/// Silent by design. A failure is one `warn` line in the proxy log and an
/// `error` field in the state file — never a dialog. The panel and the tray
/// display the result; nothing changes on its own.
pub fn spawn_detector() {
    if std::env::var(DISABLE_ENV)
        .map(|v| !v.trim().is_empty() && v.trim() != "0")
        .unwrap_or(false)
    {
        crate::log::info("自动更新检测已关闭（CC_NO_UPDATE_CHECK）");
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("ccproxy-update-check".into())
        .spawn(|| loop {
            let state = load();
            if check_due(&state, crate::now_epoch_secs()) {
                match fetch_latest(crate::proxy_version()) {
                    Ok(fresh) => {
                        if fresh.available(crate::proxy_version()) {
                            crate::log::info(&format!(
                                "有新版本可用：v{}（当前 v{}）",
                                fresh.latest,
                                crate::proxy_version()
                            ));
                        } else {
                            crate::log::debug("更新检测：已是最新版本");
                        }
                    }
                    // Network trouble is normal (offline, proxy down); the next
                    // interval retries. Recorded so the panel can explain it.
                    Err(err) => {
                        crate::log::warn(&format!("更新检测失败：{err}"));
                        let mut persisted = load();
                        persisted.error = err;
                        let _ = save(&persisted);
                    }
                }
            }
            // Sleep in one-minute steps: a long single `sleep` would keep the
            // process alive for hours after the server has stopped.
            for _ in 0..(CHECK_INTERVAL_SECS / 60) {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        });
    if spawned.is_err() {
        crate::log::warn("无法启动更新检测线程");
    }
}

/// Progress of an install, for the WebUI's poll.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Job {
    /// `idle` / `running` / `done` / `failed`.
    pub phase: &'static str,
    /// The last progress line, or the error.
    pub message: String,
    /// Unix seconds the run started.
    pub started_at: u64,
}

impl Job {
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "phase": self.phase,
            "message": self.message,
            "startedAt": if self.started_at == 0 { serde_json::Value::Null } else { serde_json::Value::from(self.started_at) },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically_not_lexically() {
        assert!(is_newer("0.8.0", "0.7.7"));
        assert!(is_newer("0.7.10", "0.7.9"), "10 is newer than 9");
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.7.7", "0.7.7"));
        assert!(!is_newer("0.7.6", "0.7.7"));
        // A `v` prefix and a suffix are tolerated rather than fatal.
        assert!(is_newer("v0.8.0", "0.7.7"));
        assert!(is_newer("0.8.0-rc1", "0.7.7"));
        assert!(!is_newer("", "0.7.7"));
        assert!(!is_newer("garbage", "0.7.7"));
    }

    #[test]
    fn the_release_json_is_parsed_into_a_tag_and_assets() {
        let body = r#"{
            "tag_name": "v0.8.0",
            "assets": [
                {"name": "commandcode-proxy-v0.8.0-windows-x86_64.zip",
                 "browser_download_url": "https://example.invalid/win.zip", "size": 8123456},
                {"name": "commandcode-proxy-v0.8.0-linux-x86_64.tar.gz",
                 "browser_download_url": "https://example.invalid/linux.tgz", "size": 7654321}
            ]
        }"#;
        let (tag, assets) = parse_release(body).expect("parsed");
        assert_eq!(tag, "0.8.0");
        assert_eq!(assets.len(), 2);
        let picked = pick_asset(&assets).expect("an asset for this platform");
        assert!(
            picked.0.ends_with(asset_suffix()),
            "picked {} for {}",
            picked.0,
            asset_suffix()
        );
    }

    #[test]
    fn a_release_without_a_matching_asset_is_not_installable() {
        let body = r#"{"tag_name": "v0.8.0", "assets": [
            {"name": "source.tar.gz", "browser_download_url": "https://example.invalid/src", "size": 1}
        ]}"#;
        let (_, assets) = parse_release(body).expect("parsed");
        assert!(pick_asset(&assets).is_none());
        let mut state = State {
            latest: "0.8.0".to_string(),
            ..State::default()
        };
        let json = state.to_json("0.7.7");
        assert_eq!(json["available"], serde_json::Value::Bool(true));
        assert_eq!(
            json["installable"],
            serde_json::Value::Bool(false),
            "a newer tag with no asset must not offer an install button"
        );
        state.url = "https://example.invalid/win.zip".to_string();
        assert_eq!(
            state.to_json("0.7.7")["installable"],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn a_check_is_due_only_after_the_interval() {
        let state = State {
            checked_at: 1_000,
            ..State::default()
        };
        assert!(!check_due(&state, 1_000));
        assert!(!check_due(&state, 1_000 + CHECK_INTERVAL_SECS - 1));
        assert!(check_due(&state, 1_000 + CHECK_INTERVAL_SECS));
        // A never-checked state is always due.
        assert!(check_due(&State::default(), 0));
    }

    #[test]
    fn staging_is_a_sibling_of_the_install() {
        // The packaged layout: `<parent>/CCProxy-v0.7.7-rust/service/ccproxy.exe`.
        let root = install_root().expect("a test binary has a directory");
        let target = staged_root_for("9.9.9").expect("staging target");
        assert_eq!(
            target.file_name().map(|n| n.to_owned()),
            Some("CCProxy-v9.9.9-rust".into())
        );
        assert_eq!(
            target.parent(),
            root.parent(),
            "the new version must not nest inside the old install"
        );
        assert_ne!(target, root);
    }

    #[test]
    fn inheritance_copies_config_and_logs_but_not_binaries() {
        let base = std::env::temp_dir().join(format!("ccproxy-inherit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let from = base.join("CCProxy-v0.7.7-rust");
        let to = base.join("CCProxy-v0.8.0-rust");
        std::fs::create_dir_all(from.join("service").join("logs")).expect("from");
        std::fs::create_dir_all(from.join("logs")).expect("tray logs");
        std::fs::create_dir_all(to.join("service")).expect("to");
        std::fs::write(
            from.join("service").join("ccproxy.json"),
            r#"{"imageCompress":true}"#,
        )
        .expect("config");
        std::fs::write(
            to.join("service").join("ccproxy.json"),
            r#"{"proxy":"default"}"#,
        )
        .expect("default config");
        std::fs::write(
            from.join("service").join("logs").join("proxy.log"),
            b"old lines",
        )
        .expect("log");
        std::fs::write(from.join("service").join("ccproxy.exe"), b"binary").expect("binary");

        let notes = inherit_with(&from, &to, Some(&from.join("service").join("ccproxy.json")));
        assert!(
            notes.iter().any(|n| n.contains("配置已继承")),
            "notes: {notes:?}"
        );
        assert_eq!(
            std::fs::read_to_string(to.join("service").join("ccproxy.json")).expect("read"),
            r#"{"imageCompress":true}"#,
            "the live config replaces the package default"
        );
        assert_eq!(
            std::fs::read(to.join("service").join("logs").join("proxy.log")).expect("read"),
            b"old lines"
        );
        assert!(
            !to.join("service").join("ccproxy.exe").exists(),
            "binaries come from the package, never from the old install"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_flat_install_inherits_its_config_beside_the_executable() {
        // The regression this pins: `inherit` used to look only for
        // `<root>/service/ccproxy.json`, so a build whose config sits beside
        // the executable copied nothing and the staged tree kept the package
        // default. Measured live — a staged install ended up with
        // `{"proxy":"default"}` while the running one had imageCompress on and
        // a custom quality, i.e. the update would have silently changed
        // behaviour it was supposed to preserve.
        let base =
            std::env::temp_dir().join(format!("ccproxy-inherit-flat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let from = base.join("flat-install");
        let to = base.join("staged");
        std::fs::create_dir_all(&from).expect("from");
        std::fs::create_dir_all(&to).expect("to");
        std::fs::write(from.join("ccproxy.json"), r#"{"imageCompressQuality":72}"#).expect("cfg");

        let notes = inherit_with(&from, &to, Some(&from.join("ccproxy.json")));
        assert!(
            notes.iter().any(|n| n.contains("配置已继承")),
            "notes: {notes:?}"
        );
        assert_eq!(
            std::fs::read_to_string(to.join("ccproxy.json")).expect("read"),
            r#"{"imageCompressQuality":72}"#,
            "with no service/ in the package the config goes beside the executable"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
