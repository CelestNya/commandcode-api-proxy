//! Raw Win32 bindings, and the only place in this crate that is allowed to be
//! `unsafe`.
//!
//! The crate is `#![deny(unsafe_code)]` rather than `forbid`, so each module
//! that needs it carries one `#[expect(unsafe_code)]` naming the invariant it
//! relies on. Everything here is a thin wrapper whose safety argument is "the
//! handle came from the matching constructor and is still open" or "the buffer
//! is sized by the API's own two-call protocol".

#![expect(unsafe_code)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::UI::WindowsAndMessaging::{WNDCLASSW, WNDPROC};

/// A NUL-terminated UTF-16 buffer.
///
/// Win32 takes `PCWSTR` (a bare pointer), so the buffer has to outlive the call
/// that consumes it. Keeping it in one type means the conversion is written
/// once and the temporary cannot be dropped too early.
pub struct Wide(Vec<u16>);

impl Wide {
    #[must_use]
    pub fn new(s: impl AsRef<OsStr>) -> Self {
        let mut buf: Vec<u16> = s.as_ref().encode_wide().collect();
        buf.push(0);
        Self(buf)
    }

    #[must_use]
    pub fn as_ptr(&self) -> *const u16 {
        self.0.as_ptr()
    }
}

/// A fixed-size UTF-16 field, as embedded in `NOTIFYICONDATAW::szTip`.
pub fn write_wide_field(field: &mut [u16], s: &str) {
    for (slot, unit) in field.iter_mut().zip(s.encode_utf16()) {
        *slot = unit;
    }
    // The terminator is written last so a truncated string still ends cleanly;
    // the field is zero-initialised, so a shorter string is already terminated.
    if let Some(last) = field.last_mut() {
        *last = 0;
    }
}

/// A kernel handle that may be shared across threads.
///
/// `windows-sys` types handles as bare pointers, which are `!Send`/`!Sync` by
/// default. Kernel handles do not have thread affinity — any thread may wait on
/// or signal them — so the auto-derived negative impls are simply wrong here.
/// The wrapper marks that explicitly rather than making every static a
/// `OnceLock<Option<_>>` full of raw pointers.
#[derive(Clone, Copy, Debug)]
pub struct SharedHandle(pub HANDLE);

// SAFETY: a Win32 kernel handle is a process-wide object reference with no
// thread affinity. Every API used through this wrapper (wait, set, reset) is
// documented as callable from any thread; the mutex's *ownership* is what is
// thread-bound, and that is confined to the ownership thread in `mutex`.
unsafe impl Send for SharedHandle {}
unsafe impl Sync for SharedHandle {}

pub mod job {
    //! Job Objects: kill the child when the tray dies, even on a hard kill.

    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// A job object that kills its members when the last handle closes.
    ///
    /// Holding the handle for the tray's lifetime is the whole mechanism: a
    /// hard-killed tray never gets to run cleanup, but closing the handle does
    /// happen, and the kernel then terminates everything assigned to the job.
    pub struct KillOnClose(HANDLE);

    // SAFETY: a job object handle is a process-wide kernel reference with no
    // thread affinity, so sharing it across threads is sound. It is never
    // closed explicitly — the process exit closes it, which is exactly the
    // moment the kernel should kill the members.
    unsafe impl Send for KillOnClose {}
    unsafe impl Sync for KillOnClose {}

    impl KillOnClose {
        /// Create the job. `None` when the OS refuses (e.g. already inside a
        /// non-nestable job) — the tray then runs without orphan protection
        /// rather than refusing to start.
        #[must_use]
        pub fn create() -> Option<Self> {
            // SAFETY: no attributes, no name; the returned handle is checked
            // for NULL/INVALID before use.
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if job.is_null() || job == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `info` is a correctly-typed, initialised value that
            // outlives the call, and the size passed is its own size.
            let ok = unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    std::ptr::addr_of!(info).cast(),
                    u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                        .unwrap_or(u32::MAX),
                )
            };
            if ok == 0 {
                return None;
            }
            Some(Self(job))
        }

        /// Assign a process to the job.
        ///
        /// A failure is reported, not fatal: the proxy still runs, it is just
        /// no longer guaranteed to die with the tray.
        #[must_use]
        pub fn assign(&self, process: HANDLE) -> bool {
            // SAFETY: both handles are live — the job is owned by `self`, and
            // the caller passes a process handle it obtained from CreateProcess.
            unsafe { AssignProcessToJobObject(self.0, process) != 0 }
        }
    }
}

pub mod tcp {
    //! Port ownership via `GetExtendedTcpTable`.
    //!
    //! Preferred over parsing `netstat` output, which depends on the English
    //! "LISTENING" wording and the column layout, and costs a process per poll.

    use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID, MIB_TCPROW_OWNER_PID,
        MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
    };

    const AF_INET: u32 = 2;
    const AF_INET6: u32 = 23;

    /// The PID listening on `port`, or `None` if nothing is.
    #[must_use]
    pub fn port_owner(port: u16) -> Option<u32> {
        owner_v4(port).or_else(|| owner_v6(port))
    }

    fn owner_v4(port: u16) -> Option<u32> {
        let buf = query(AF_INET)?;
        // SAFETY: `query(AF_INET)` returns a buffer the API filled with a
        // MIB_TCPTABLE_OWNER_PID, whose header is at offset 0. The row count is
        // read from the header, and rows are indexed only within the bounds the
        // API reported.
        unsafe {
            let table = buf.as_ptr().cast::<MIB_TCPTABLE_OWNER_PID>();
            let count = (*table).dwNumEntries as usize;
            let rows = std::ptr::addr_of!((*table).table).cast::<MIB_TCPROW_OWNER_PID>();
            for i in 0..count {
                let row = &*rows.add(i);
                if network_order_port(row.dwLocalPort) == port && row.dwOwningPid > 0 {
                    return Some(row.dwOwningPid);
                }
            }
        }
        None
    }

    fn owner_v6(port: u16) -> Option<u32> {
        let buf = query(AF_INET6)?;
        // SAFETY: as above, with the IPv6 row layout.
        unsafe {
            let table = buf.as_ptr().cast::<MIB_TCP6TABLE_OWNER_PID>();
            let count = (*table).dwNumEntries as usize;
            let rows = std::ptr::addr_of!((*table).table).cast::<MIB_TCP6ROW_OWNER_PID>();
            for i in 0..count {
                let row = &*rows.add(i);
                if network_order_port(row.dwLocalPort) == port && row.dwOwningPid > 0 {
                    return Some(row.dwOwningPid);
                }
            }
        }
        None
    }

    /// The two-call protocol: ask for the size, then ask for the table.
    ///
    /// Returns a byte buffer rather than a typed table because the table is
    /// variable-length; callers reinterpret it with the layout matching the
    /// address family they asked for.
    fn query(family: u32) -> Option<Vec<u8>> {
        let mut size = 0u32;
        // SAFETY: a null table pointer with a zero size is the documented way
        // to ask for the required size; it cannot write anything.
        unsafe {
            GetExtendedTcpTable(
                std::ptr::null_mut(),
                &raw mut size,
                0,
                family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            );
        }
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        // SAFETY: `buf` is exactly `size` bytes, and `size` says so. On success
        // the API writes at most that many bytes.
        let status = unsafe {
            GetExtendedTcpTable(
                buf.as_mut_ptr().cast(),
                &raw mut size,
                0,
                family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if status == NO_ERROR {
            return Some(buf);
        }
        // The table grew between the two calls: retry once with the new size.
        // Not `ERROR_INSUFFICIENT_BUFFER` here means a real failure.
        if status == ERROR_INSUFFICIENT_BUFFER {
            return None;
        }
        None
    }

    /// The port as the API reports it is in network byte order in a u32.
    fn network_order_port(raw: u32) -> u16 {
        let b = raw.to_be_bytes();
        u16::from_be_bytes([b[2], b[3]])
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_port_is_read_out_of_network_byte_order() {
            // 8787 = 0x2253. In network order in a u32 on a little-endian
            // machine the API stores the two bytes in the high half.
            let raw = u32::from_be_bytes([0, 0, 0x22, 0x53]);
            assert_eq!(network_order_port(raw), 8787);
        }

        #[test]
        fn a_free_port_has_no_owner() {
            // Port 1 is never a listener on a normal machine; if it were, the
            // test would still only be asserting the query returns a pid.
            assert!(port_owner(1).is_none_or(|pid| pid > 0));
        }
    }
}

/// The named mutex that admits one tray, with the dedicated ownership thread
/// Win32 mutexes require.
///
/// A mutex has thread affinity: releasing it from a thread that did not acquire
/// it fails, and the failure is silent if the error is ignored — the lock is
/// then never released and no successor can ever take over. The C# build hit
/// exactly this. So every acquire and release is posted to one thread that owns
/// the handle for the process's lifetime.
pub mod mutex {
    #![expect(unsafe_code)]

    use std::sync::mpsc::{channel, Sender};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    use super::Wide;
    use crate::settings;
    use windows_sys::Win32::Foundation::{
        GetLastError, ERROR_ALREADY_EXISTS, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

    /// An operation for the ownership thread to perform.
    enum Op {
        Acquire {
            timeout: Duration,
            reply: Sender<bool>,
        },
        Release {
            reply: Sender<()>,
        },
    }

    struct Ownership {
        tx: Sender<Op>,
    }

    static OWNERSHIP: OnceLock<Option<Ownership>> = OnceLock::new();

    /// Create the ownership thread and the mutex. `true` when this instance
    /// created the mutex, i.e. it is the incumbent.
    ///
    /// Returns `None` if the thread cannot start, which is fatal: without it
    /// there is no way to know whether another tray is running.
    pub fn init(name: &str) -> Option<bool> {
        let (tx, rx) = channel::<Op>();
        let (ready_tx, ready_rx) = channel::<bool>();
        let name = name.to_string();

        let spawned = std::thread::Builder::new()
            .name("ccproxy-tray-owner".into())
            .spawn(move || {
                let wide = Wide::new(&name);
                // SAFETY: `wide` outlives the call; no attributes are wanted,
                // and initially-owned is exactly the state being requested.
                let handle = unsafe { CreateMutexW(std::ptr::null(), 1, wide.as_ptr()) };
                // SAFETY: no preconditions.
                let created_new = unsafe { GetLastError() } != ERROR_ALREADY_EXISTS;
                if handle.is_null() {
                    let _ = ready_tx.send(false);
                    return;
                }
                let _ = ready_tx.send(created_new);

                while let Ok(op) = rx.recv() {
                    match op {
                        Op::Acquire { timeout, reply } => {
                            let start = Instant::now();
                            let mut acquired = false;
                            loop {
                                // SAFETY: `handle` is owned by this thread's
                                // frame and stays open for its lifetime.
                                let status = unsafe { WaitForSingleObject(handle, 0) };
                                if status == WAIT_OBJECT_0 {
                                    acquired = true;
                                    break;
                                }
                                // A lingering other state is not success.
                                if status != WAIT_TIMEOUT {
                                    break;
                                }
                                if start.elapsed() >= timeout {
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(200));
                            }
                            let _ = reply.send(acquired);
                        }
                        Op::Release { reply } => {
                            // SAFETY: acquired by this same thread, so this is
                            // the documented release path.
                            unsafe {
                                ReleaseMutex(handle);
                            }
                            let _ = reply.send(());
                        }
                    }
                }
            })
            .ok()?;
        let _ = spawned;

        let created_new = ready_rx.recv_timeout(Duration::from_secs(10)).ok()?;
        let installed = OWNERSHIP.get_or_init(|| Some(Ownership { tx }));
        if installed.is_none() {
            return None;
        }
        Some(created_new)
    }

    /// Acquire, waiting up to `timeout`.
    #[must_use]
    pub fn try_acquire(timeout: Duration) -> bool {
        let Some(Some(ownership)) = OWNERSHIP.get() else {
            return false;
        };
        let (reply, rx) = channel();
        if ownership.tx.send(Op::Acquire { timeout, reply }).is_err() {
            return false;
        }
        // Saturating: an absurd timeout degrades to "wait as long as asked",
        // never to a wrapped-around zero.
        let budget = timeout.saturating_add(Duration::from_secs(5));
        rx.recv_timeout(budget).unwrap_or(false)
    }

    /// Release. Safe to call when not held — the ownership thread's failure is
    /// ignored on purpose, because the alternative is aborting a shutdown.
    pub fn release() {
        let Some(Some(ownership)) = OWNERSHIP.get() else {
            return;
        };
        let (reply, rx) = channel();
        if ownership.tx.send(Op::Release { reply }).is_err() {
            return;
        }
        let _ = rx.recv_timeout(Duration::from_secs(5));
    }

    /// Probe for another instance without disturbing it (for `--selfcheck`).
    #[must_use]
    pub fn is_held_by_another() -> bool {
        // Creating a second handle to the same name reports "already exists"
        // whether or not the owner currently holds the lock; that is exactly
        // the question being asked, and it touches nothing.
        use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let wide = Wide::new(settings::ns_name("cc-proxy-tray"));
        // SAFETY: `wide` outlives the call.
        let probe = unsafe { CreateMutexW(std::ptr::null(), 1, wide.as_ptr()) };
        if probe.is_null() {
            return false;
        }
        // SAFETY: no preconditions.
        let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if !exists {
            // SAFETY: we just created it and hold it on this thread.
            unsafe {
                ReleaseMutex(probe);
            }
        }
        // SAFETY: the handle is no longer needed.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(probe);
        }
        exists
    }
}

/// The three named events of the handover protocol.
///
/// Namespaced like the mutex, so an isolated test instance has its own protocol
/// and cannot signal the production tray.
pub mod event {
    #![expect(unsafe_code)]

    use std::sync::OnceLock;

    use super::Wide;
    use crate::settings;
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{
        CreateEventW, ResetEvent, SetEvent, WaitForSingleObject,
    };

    #[derive(Clone, Copy)]
    pub enum Which {
        Standby,
        Commit,
        Abort,
    }

    struct Handles {
        standby: super::SharedHandle,
        commit: super::SharedHandle,
        abort: super::SharedHandle,
    }

    static EVENTS: OnceLock<Option<Handles>> = OnceLock::new();

    fn handles() -> Option<&'static Handles> {
        EVENTS
            .get_or_init(|| {
                let make = |suffix: &str| -> super::SharedHandle {
                    let name = settings::ns_name(suffix);
                    let wide = Wide::new(name);
                    // Manual-reset: the state is a latch the other side reads
                    // whenever it gets around to it, not a queued signal.
                    // SAFETY: `wide` outlives the call.
                    super::SharedHandle(unsafe {
                        CreateEventW(std::ptr::null(), 1, 0, wide.as_ptr())
                    })
                };
                let handles = Handles {
                    standby: make("cc-proxy-tray-standby"),
                    commit: make("cc-proxy-tray-commit"),
                    abort: make("cc-proxy-tray-abort"),
                };
                if handles.standby.0.is_null()
                    || handles.commit.0.is_null()
                    || handles.abort.0.is_null()
                {
                    None
                } else {
                    Some(handles)
                }
            })
            .as_ref()
    }

    fn handle_of(which: Which) -> Option<super::SharedHandle> {
        let h = handles()?;
        Some(match which {
            Which::Standby => h.standby,
            Which::Commit => h.commit,
            Which::Abort => h.abort,
        })
    }

    /// Latch an event.
    pub fn set(which: Which) {
        if let Some(handle) = handle_of(which) {
            // SAFETY: the handle names a manual-reset event created above.
            unsafe {
                SetEvent(handle.0);
            }
        }
    }

    /// Clear a latched event.
    pub fn reset(which: Which) {
        if let Some(handle) = handle_of(which) {
            // SAFETY: as above.
            unsafe {
                ResetEvent(handle.0);
            }
        }
    }

    /// Whether the event is currently set.
    #[must_use]
    pub fn is_set(which: Which) -> bool {
        handle_of(which).is_some_and(|handle| {
            // SAFETY: as above; a zero timeout makes this a pure query.
            unsafe { WaitForSingleObject(handle.0, 0) == WAIT_OBJECT_0 }
        })
    }

    /// Wait up to `timeout` for the event to be set.
    #[must_use]
    pub fn wait(which: Which, timeout: std::time::Duration) -> bool {
        let Some(handle) = handle_of(which) else {
            return false;
        };
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: the handle is live for the process lifetime.
        unsafe { WaitForSingleObject(handle.0, millis) == WAIT_OBJECT_0 }
    }
}

/// A modal message box. The tray has no console, so this is the only way to
/// tell the user something went wrong.
pub fn message_box(text: &str, warning: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_ICONWARNING, MB_OK,
    };
    let body = Wide::new(text);
    let title = Wide::new("CC Proxy Tray");
    let style = MB_OK
        | if warning {
            MB_ICONWARNING
        } else {
            MB_ICONERROR
        };
    // SAFETY: both strings outlive the call; a null owner is allowed.
    unsafe {
        MessageBoxW(std::ptr::null_mut(), body.as_ptr(), title.as_ptr(), style);
    }
}

/// Register a window class and create the message-only window the tray icon
/// hangs off. Kept beside the FFI it wraps rather than in the UI module.
pub mod window {
    use super::{job, Wide, WNDCLASSW, WNDPROC};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, RegisterClassW, MSG, WINDOW_EX_STYLE, WINDOW_STYLE,
        WS_OVERLAPPED,
    };

    /// A created window, plus the fact that its class is registered.
    pub struct Window(HWND);

    impl Window {
        /// Register `class_name` with `proc` and create a window of it.
        #[must_use]
        pub fn create(class_name: &str, proc: WNDPROC) -> Option<Self> {
            // SAFETY: a null module name asks for this executable's handle,
            // which always exists for a running process.
            let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
            let class = Wide::new(class_name);
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: proc,
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
            };
            // SAFETY: `wc` is fully initialised and outlives the call.
            let atom = unsafe { RegisterClassW(&raw const wc) };
            if atom == 0 {
                return None;
            }
            let title = Wide::new("CC Proxy");
            // SAFETY: the class is registered above; the remaining arguments are
            // a zero style, zero-size window, no parent, no menu, and no param.
            let hwnd = unsafe {
                CreateWindowExW(
                    0 as WINDOW_EX_STYLE,
                    class.as_ptr(),
                    title.as_ptr(),
                    WS_OVERLAPPED as WINDOW_STYLE,
                    0,
                    0,
                    0,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    instance,
                    std::ptr::null(),
                )
            };
            if hwnd.is_null() {
                return None;
            }
            Some(Self(hwnd))
        }

        #[must_use]
        pub fn hwnd(&self) -> HWND {
            self.0
        }
    }

    impl Drop for Window {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateWindowExW above and is
            // destroyed exactly once, here.
            unsafe {
                DestroyWindow(self.0);
            }
        }
    }

    /// Create the icon window, run the message loop, and dispatch actions.
    ///
    /// The window is a message-only window: it is never shown, and the shell
    /// only needs it as an address for icon callbacks. The message loop is
    /// `PeekMessage`-driven rather than blocking `GetMessage`, because the tray
    /// also has to notice the child exiting to restart it.
    /// `on_tick` runs on every idle pass (where the supervisor checks for an
    /// unexpected child exit), and `should_exit` ends the loop when the
    /// handover thread has decided this instance must stand down.
    pub fn run_ui(
        ui: &crate::tray::UiState,
        actions: impl Fn(crate::tray::Action),
        mut on_tick: impl FnMut(),
        should_exit: impl Fn() -> bool,
    ) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, TranslateMessage, PM_REMOVE, WM_QUIT,
        };
        let Some(window) = Window::create("CCProxyTrayWnd", Some(crate::tray::wnd_proc)) else {
            return;
        };
        let hwnd = window.hwnd();
        ui.attach(hwnd);

        let mut msg = MSG::default();
        let mut last_running = (ui.running)();
        loop {
            // SAFETY: `msg` is live; a null hwnd filter means "this thread".
            let had = unsafe { PeekMessageW(&raw mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) };
            if had != 0 {
                if msg.message == WM_QUIT {
                    break;
                }
                // SAFETY: `msg` is a message the API filled in.
                unsafe {
                    TranslateMessage(&raw const msg);
                    DispatchMessageW(&raw const msg);
                }
                continue;
            }
            let action = ui.take_action();
            if action != crate::tray::Action::None {
                actions(action);
                if action == crate::tray::Action::Quit {
                    break;
                }
            }
            // A completed handover ends this process: the successor is serving
            // and this instance has released the lock. Without this the loop
            // ran forever and a "stood down" incumbent kept its icon and its
            // mutex, so the next handover had two claimants.
            if should_exit() {
                break;
            }
            // Idle pass: the supervisor notices an unexpected child exit here.
            on_tick();
            // The icon is repainted only when the state actually changed,
            // because `Shell_NotifyIconW` is a syscall and this loop is hot.
            let running = (ui.running)();
            if running != last_running {
                last_running = running;
                ui.update_icon(hwnd, running);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        ui.detach(hwnd);
    }

    /// Silence the unused-import warning for a type only named in signatures.
    #[allow(dead_code)]
    fn _uses_job(_: &job::KillOnClose) {}
}

/// Small registry reader/writer for the two keys the tray cares about:
/// the Internet Settings proxy, and the Run key for autostart.
pub mod registry {
    #![expect(unsafe_code)]

    use super::Wide;
    use windows_sys::core::PCWSTR;
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
        RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_SZ,
    };

    /// An open key, closed on drop.
    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: the handle came from RegOpenKeyExW/RegCreateKeyExW and is
            // closed exactly once, here.
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }

    fn open(subkey: &str, access: u32) -> Option<Key> {
        let path = Wide::new(subkey);
        let mut handle: HKEY = std::ptr::null_mut();
        // SAFETY: `path` outlives the call and `handle` is a live out-param;
        // the access mask is one of the two the callers pass.
        let status =
            unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, access, &raw mut handle) };
        if status == ERROR_SUCCESS {
            Some(Key(handle))
        } else {
            None
        }
    }

    /// Read a value's raw bytes. `None` when the value is absent.
    #[must_use]
    pub fn read_bytes(subkey: &str, value: &str) -> Option<Vec<u8>> {
        let key = open(subkey, KEY_READ)?;
        let name = Wide::new(value);
        let mut kind = 0u32;
        let mut size = 0u32;
        // SAFETY: a null data pointer with a zero size asks for the length;
        // the API cannot write through a null in that form.
        let status = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                std::ptr::null(),
                &raw mut kind,
                std::ptr::null_mut(),
                &raw mut size,
            )
        };
        if status != ERROR_SUCCESS || size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        // SAFETY: `buf` is exactly `size` bytes as reported by the call above.
        let status = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                std::ptr::null(),
                &raw mut kind,
                buf.as_mut_ptr(),
                &raw mut size,
            )
        };
        if status == ERROR_SUCCESS {
            buf.truncate(size as usize);
            Some(buf)
        } else {
            None
        }
    }

    /// Read a REG_SZ value as a string.
    #[must_use]
    pub fn read_string(subkey: &str, value: &str) -> Option<String> {
        let bytes = read_bytes(subkey, value)?;
        // `chunks_exact(2)` guarantees each chunk is a full pair, so the
        // destructuring below cannot fail; it is written as a match rather than
        // by index to make that evident to the compiler and the reader.
        let mut units: Vec<u16> = bytes
            .chunks_exact(2)
            .filter_map(|c| match *c {
                [lo, hi] => Some(u16::from_ne_bytes([lo, hi])),
                _ => None,
            })
            .collect();
        while units.last().copied() == Some(0) {
            units.pop();
        }
        String::from_utf16(&units).ok()
    }

    /// Write a REG_SZ value, creating the key if needed.
    #[must_use]
    pub fn write_string(subkey: &str, value: &str, data: &str) -> bool {
        let path = Wide::new(subkey);
        let mut handle: HKEY = std::ptr::null_mut();
        // SAFETY: `path` outlives the call; the rest are the documented
        // defaults for creating-or-opening under HKCU.
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                std::ptr::null(),
                0,
                KEY_SET_VALUE,
                std::ptr::null(),
                &raw mut handle,
                std::ptr::null_mut(),
            )
        };
        if status != ERROR_SUCCESS {
            return false;
        }
        let key = Key(handle);
        let name = Wide::new(value);
        let mut data_wide: Vec<u16> = data.encode_utf16().collect();
        data_wide.push(0);
        let bytes = data_wide.len().saturating_mul(2);
        // SAFETY: `data_wide` is a NUL-terminated UTF-16 buffer and `bytes` is
        // exactly its size in bytes, which is what REG_SZ expects.
        let status = unsafe {
            RegSetValueExW(
                key.0,
                name.as_ptr(),
                0,
                REG_SZ,
                data_wide.as_ptr().cast(),
                u32::try_from(bytes).unwrap_or(u32::MAX),
            )
        };
        status == ERROR_SUCCESS
    }

    /// Delete a value. Succeeds when it was already absent.
    #[must_use]
    pub fn delete_value(subkey: &str, value: &str) -> bool {
        let Some(key) = open(subkey, KEY_SET_VALUE) else {
            return true;
        };
        let name = Wide::new(value);
        // SAFETY: `name` outlives the call; the handle is live via `key`.
        let status = unsafe { RegDeleteValueW(key.0, name.as_ptr()) };
        status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND
    }

    /// Exhaustive match over the error codes this module can see, so the
    /// unused-import lint does not hide a real copy/paste error.
    #[allow(dead_code)]
    fn _error_codes(status: WIN32_ERROR) -> &'static str {
        match status {
            ERROR_SUCCESS => "ok",
            ERROR_FILE_NOT_FOUND => "absent",
            _ => "other",
        }
    }

    #[allow(dead_code)]
    fn _uses_pcwstr(_: PCWSTR) {}
}
