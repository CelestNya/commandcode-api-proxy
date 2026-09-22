//! Supervising the proxy child process.
//!
//! The proxy is a single binary the tray launches and restarts on unexpected
//! exit. Two things must not go wrong: it must not be left orphaned holding the
//! port (the Job Object handles that), and a handover must not race a restart
//! (the `manual_stop` flag handles that).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use crate::settings;

/// A rotating line log. Personal scale, so rotation is a truncate.
pub struct LogFile {
    path: PathBuf,
    max_bytes: u64,
}

impl LogFile {
    const MAX_BYTES: u64 = 2 * 1024 * 1024;

    #[must_use]
    pub fn new(dir: &Path) -> Self {
        let _ = std::fs::create_dir_all(dir);
        Self {
            // The tray's lifecycle lines and the proxy's own lines are two
            // different streams; the proxy now writes `proxy.log` itself, so
            // the tray keeps its own file and they no longer interleave.
            path: dir.join("tray.log"),
            max_bytes: Self::MAX_BYTES,
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one line, timestamped to the second.
    pub fn append(&self, line: &str) {
        if let Ok(meta) = std::fs::metadata(&self.path) {
            if meta.len() > self.max_bytes {
                let _ = std::fs::write(&self.path, "");
            }
        }
        let stamp = ccproxy::now_iso8601();
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(file, "{stamp} {line}");
        }
    }
}

/// The proxy child, with the lifecycle the tray must enforce.
pub struct ProxyProcess {
    child: Option<Child>,
    /// Set while the tray itself is stopping the proxy, so the exit handler
    /// does not treat it as a crash and restart it.
    manual_stop: bool,
    binary: PathBuf,
    root: PathBuf,
    port: u16,
    log: std::sync::Arc<LogFile>,
}

impl ProxyProcess {
    #[must_use]
    pub fn new(binary: PathBuf, root: PathBuf, port: u16, log: std::sync::Arc<LogFile>) -> Self {
        Self {
            child: None,
            manual_stop: false,
            binary,
            root,
            port,
            log,
        }
    }

    #[must_use]
    pub fn is_running(&mut self) -> bool {
        match self.child.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// Launch the proxy. No-op when it is already running.
    ///
    /// The egress environment is applied here rather than at startup because
    /// the system proxy can change while the tray is running.
    ///
    /// Returns `false` when the port guard refused: the caller has already been
    /// told why, and must not treat the proxy as running.
    #[must_use]
    pub fn start(&mut self, job: Option<&crate::win::job::KillOnClose>) -> bool {
        if self.is_running() {
            return true;
        }
        self.manual_stop = false;

        // The port is a contract and is defended before anything is launched:
        // a foreign process holding it is never displaced.
        let child_pid = self.child.as_ref().map(std::process::Child::id);
        let decision =
            crate::portguard::ensure_available(self.port, child_pid, &self.root, &self.log);
        if decision == crate::portguard::Decision::Refuse {
            return false;
        }

        let mut cmd = Command::new(&self.binary);
        cmd.current_dir(&self.root)
            .env("PORT", self.port.to_string())
            .env("NODE_USE_ENV_PROXY", "1")
            .env("NO_PROXY", settings::build_no_proxy())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // The proxy is a console program; a GUI parent launching it without
        // this flag makes Windows allocate a console window for it. That was a
        // black window flashing on every start — and the reason the packaged
        // tray looked broken when it was only doing its job.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        if let Some(url) = settings::resolve_proxy_url() {
            cmd.env("HTTPS_PROXY", &url).env("HTTP_PROXY", &url);
            self.log.append(&format!("[tray] 出站代理: {url}"));
        } else {
            self.log
                .append("[tray] 出站代理: 直连（系统代理未开或 CC_PROXY=off）");
        }
        // A namespaced instance must not write into the production ledger.
        let ns = settings::namespace();
        if !ns.is_empty() {
            cmd.env("CC_TRAY_NS", ns);
        }
        if let Ok(level) = std::env::var("CC_LOG_LEVEL") {
            cmd.env("LOG_LEVEL", level);
        }

        match cmd.spawn() {
            Ok(mut child) => {
                // The child writes its own `proxy.log`, so its stdout/stderr are
                // NOT forwarded line-by-line — that would duplicate every line
                // into tray.log. Only the process-level facts belong here: the
                // tray's file records when the proxy started, died, or failed
                // to start, which is what makes a crash's timeline readable
                // next to the proxy's own account of the same window.
                //
                // The pipes are still taken and drained so a chatty child
                // cannot fill its pipe buffer and block.
                if let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) {
                    drain(out);
                    drain(err);
                }
                if let Some(job) = job {
                    // SAFETY: `as_raw_handle` is safe but returns a pointer
                    // whose validity the caller must respect — the child
                    // outlives this call.
                    let handle = std::os::windows::io::AsRawHandle::as_raw_handle(&child);
                    if !job.assign(handle.cast()) {
                        self.log.append(
                            "[tray] 警告：挂入 Job Object 失败，托盘退出后可能留下孤儿进程",
                        );
                    }
                }
                self.log
                    .append(&format!("[tray] 代理已启动 pid={}", child.id()));
                self.child = Some(child);
                true
            }
            Err(err) => {
                self.log.append(&format!("[tray] 代理启动失败: {err}"));
                false
            }
        }
    }

    /// Stop the proxy on purpose: no restart follows.
    pub fn stop(&mut self) {
        self.manual_stop = true;
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child = None;
    }

    /// If the child exited on its own, reap it and say whether to restart.
    ///
    /// A crash must be restarted (the tray exists to keep the service up), but
    /// a deliberate stop must not be — that distinction is the whole reason
    /// `manual_stop` exists, and it is what stops a handover from racing a
    /// restart and leaving two servers.
    pub fn take_unexpected_exit(&mut self) -> bool {
        if self.manual_stop {
            return false;
        }
        let exited = match self.child.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(Some(_))),
            None => false,
        };
        if exited {
            self.child = None;
            self.log.append("[tray] 代理意外退出 —— 3 秒后自动重启");
        }
        exited
    }
}

/// Pump a child stream into the log on its own thread.
/// Consume a child's output stream and discard it.
///
/// The child writes its own log file, so these lines are already recorded; the
/// thread exists only so the pipe is read instead of filling up and blocking
/// the child.
fn drain<R: std::io::Read + Send + 'static>(reader: R) {
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 4096];
        while matches!(std::io::Read::read(&mut reader, &mut buf), Ok(n) if n > 0) {}
    });
}

/// Shared handle to the supervisor, for the UI thread.
pub type SharedProxy = std::sync::Arc<Mutex<ProxyProcess>>;

#[must_use]
pub fn shared(proxy: ProxyProcess) -> SharedProxy {
    std::sync::Arc::new(Mutex::new(proxy))
}
