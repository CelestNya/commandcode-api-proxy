//! The tray icon and its context menu.
//!
//! No window is ever shown: the icon is owned by a message-only window whose
//! procedure handles the `WM_APP` callbacks the shell sends for icon events.
//! The menu is rebuilt per right-click, so it always reflects current state.

#![expect(unsafe_code)]

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DefWindowProcW, DestroyMenu, GetCursorPos, GetWindowLongPtrW,
    PostQuitMessage, SetForegroundWindow, TrackPopupMenu, GWLP_USERDATA, MF_GRAYED, MF_SEPARATOR,
    MF_STRING, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_DESTROY,
    WM_LBUTTONDBLCLK, WM_RBUTTONUP,
};

use crate::win::{write_wide_field, Wide};

/// The callback message the shell sends for icon events.
const WM_TRAY: u32 = WM_APP + 1;

/// Menu command ids, non-zero and stable.
pub mod cmd {
    pub const START: usize = 1;
    pub const STOP: usize = 2;
    pub const OPEN_LOG: usize = 3;
    pub const AUTOSTART: usize = 4;
    pub const QUIT: usize = 5;
    pub const OPEN_WEBUI: usize = 6;
    pub const OPEN_TRAY_LOG: usize = 7;
    /// A read-only line, drawn greyed and never chosen.
    pub const LABEL: usize = 0;
    /// A separator.
    pub const SEPARATOR: usize = usize::MAX;
}

/// What the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    OpenLog,
    OpenTrayLog,
    OpenWebUi,
    ToggleAutostart,
    Quit,
    None,
}

fn action_from_id(id: usize) -> Action {
    match id {
        cmd::START => Action::Start,
        cmd::STOP => Action::Stop,
        cmd::OPEN_LOG => Action::OpenLog,
        cmd::OPEN_TRAY_LOG => Action::OpenTrayLog,
        cmd::OPEN_WEBUI => Action::OpenWebUi,
        cmd::AUTOSTART => Action::ToggleAutostart,
        cmd::QUIT => Action::Quit,
        _ => Action::None,
    }
}

/// Everything the window procedure needs, reached through the window's
/// user-data slot.
pub struct UiState {
    pub port: u16,
    pub version: String,
    pub icon: IconSet,
    /// Read on demand so the menu shows current numbers, not numbers from
    /// startup.
    pub stats: Box<dyn Fn() -> Option<ccproxy::billing::DailyStats> + Send + Sync>,
    pub running: Box<dyn Fn() -> bool + Send + Sync>,
    pub autostart: Box<dyn Fn() -> bool + Send + Sync>,
    last_action: std::sync::atomic::AtomicU32,
}

/// The state lives in a window's user-data slot, which Win32 types as a bare
/// pointer, so the `Sync` bound cannot be expressed to the compiler's
/// satisfaction even though exactly one thread ever touches it.
unsafe impl Send for UiState {}
unsafe impl Sync for UiState {}

impl UiState {
    #[must_use]
    pub fn new(
        port: u16,
        version: String,
        stats: impl Fn() -> Option<ccproxy::billing::DailyStats> + Send + Sync + 'static,
        running: impl Fn() -> bool + Send + Sync + 'static,
        autostart: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            port,
            version,
            icon: IconSet::new(),
            stats: Box::new(stats),
            running: Box::new(running),
            autostart: Box::new(autostart),
            last_action: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Store `self` in `hwnd`'s user-data slot and show the icon.
    ///
    /// The state is leaked into the slot and reclaimed by [`detach`]; the
    /// window outlives every use of the pointer, which is what makes the
    /// `&'static` reborrow in [`state_of`] sound.
    pub fn attach(&self, hwnd: HWND) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowLongPtrW, GWLP_USERDATA};
        let raw = std::ptr::from_ref(self).cast::<std::ffi::c_void>();
        // SAFETY: `hwnd` is live, and storing a pointer in the user-data slot
        // is exactly what the slot is for. `detach` clears it before the
        // window is destroyed.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
        }
        self.update_icon(hwnd, (self.running)());
        // SAFETY: `nid` is a stack value sized by the API's own measure.
        let nid = self.notify_data(hwnd);
        // SAFETY: `nid` is fully initialised above.
        unsafe {
            Shell_NotifyIconW(NIM_ADD, &raw const nid);
        }
    }

    /// Re-read state and update the tooltip and dot colour.
    pub fn update_icon(&self, hwnd: HWND, running: bool) {
        let mut nid = self.notify_data(hwnd);
        nid.hIcon = self.icon.handle_for(running);
        // SAFETY: `nid` is a fully initialised local.
        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &raw const nid);
        }
    }

    /// Clear the user-data slot. Must run before the window is destroyed.
    pub fn detach(&self, hwnd: HWND) {
        use windows_sys::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW;
        let nid = self.notify_data(hwnd);
        // SAFETY: the icon was added in `attach`; removing it here keeps the
        // shell from holding a stale handle to a freed icon.
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &raw const nid);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        }
    }

    fn notify_data(&self, hwnd: HWND) -> NOTIFYICONDATAW {
        let mut nid = NOTIFYICONDATAW {
            cbSize: u32::try_from(std::mem::size_of::<NOTIFYICONDATAW>()).unwrap_or(0),
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: self.icon.handle_for((self.running)()),
            ..Default::default()
        };
        let running = (self.running)();
        let state = if running {
            format!("CC Proxy — running (:{})", self.port)
        } else {
            "CC Proxy — stopped".to_string()
        };
        write_wide_field(&mut nid.szTip, &format!("{state}  v{}", self.version));
        nid
    }

    fn record(&self, action: Action) {
        let code = match action {
            Action::Start => cmd::START,
            Action::Stop => cmd::STOP,
            Action::OpenLog => cmd::OPEN_LOG,
            Action::OpenTrayLog => cmd::OPEN_TRAY_LOG,
            Action::OpenWebUi => cmd::OPEN_WEBUI,
            Action::ToggleAutostart => cmd::AUTOSTART,
            Action::Quit => cmd::QUIT,
            Action::None => 0,
        };
        self.last_action.store(
            u32::try_from(code).unwrap_or(0),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Take the action recorded by the last menu interaction, if any.
    pub fn take_action(&self) -> Action {
        action_from_id(
            usize::try_from(
                self.last_action
                    .swap(0, std::sync::atomic::Ordering::Relaxed),
            )
            .unwrap_or(0),
        )
    }

    /// Build and run the context menu; record the choice.
    fn show_menu(&self, hwnd: HWND) {
        let running = (self.running)();
        let entries = menu_entries(
            running,
            (self.autostart)(),
            &self.cache_lines(),
            &self.version,
        );

        // SAFETY: the menu handle is checked before use and destroyed on every
        // path out of this block.
        let chosen = unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            for (id, text, enabled) in &entries {
                if *id == cmd::SEPARATOR {
                    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
                    continue;
                }
                let wide = Wide::new(text);
                let flags = if *enabled {
                    MF_STRING
                } else {
                    MF_STRING | MF_GRAYED
                };
                AppendMenuW(menu, flags, *id, wide.as_ptr());
            }
            let mut point = POINT { x: 0, y: 0 };
            GetCursorPos(&raw mut point);
            // Required so the menu closes when the user clicks elsewhere.
            SetForegroundWindow(hwnd);
            let id = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_LEFTALIGN | TPM_BOTTOMALIGN,
                point.x,
                point.y,
                0,
                hwnd,
                std::ptr::null(),
            );
            DestroyMenu(menu);
            usize::try_from(id).unwrap_or(0)
        };
        self.record(action_from_id(chosen));
    }

    /// The two read-only lines at the top of the menu.
    fn cache_lines(&self) -> (String, String) {
        let Some(stats) = (self.stats)() else {
            return ("24h 缓存率：无数据".into(), " ".into());
        };
        if stats.rows == 0 {
            return ("24h 缓存率：无数据".into(), " ".into());
        }
        let rate = stats
            .cache_rate_percent()
            .map_or_else(|| "—".to_string(), |r| format!("{r}%"));
        (
            format!("24h 缓存率：{rate}（{} 次）", stats.rows),
            format!(
                "缓存 {} / {} tokens · 输出 {}",
                fmt_tokens(stats.cached_tokens),
                fmt_tokens(stats.prompt_tokens),
                fmt_tokens(stats.completion_tokens)
            ),
        )
    }
}

/// The window procedure. Handles icon callbacks; everything else is default.
///
/// # Safety
/// Called by Win32 with a live `hwnd`; the user-data slot may hold the
/// `UiState` installed by [`UiState::attach`].
pub unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAY => {
            // SAFETY: the caller is the message loop for this window.
            if let Some(state) = unsafe { state_of(hwnd) } {
                let event = u32::try_from(lparam).unwrap_or(0);
                match event {
                    WM_RBUTTONUP => state.show_menu(hwnd),
                    WM_LBUTTONDBLCLK => state.record(Action::OpenLog),
                    _ => {}
                }
            }
            0
        }
        WM_DESTROY => {
            // SAFETY: no preconditions beyond owning the message queue.
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => {
            // SAFETY: forwarded verbatim to the default handler.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

/// Reborrow the `UiState` installed in `hwnd`'s user-data slot.
///
/// # Safety
/// `hwnd` must be a window created by this module with `UiState::attach` having
/// run, and the state must still be alive.
unsafe fn state_of(hwnd: HWND) -> Option<&'static UiState> {
    // SAFETY: the caller guarantees the window belongs to this module.
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    if raw == 0 {
        return None;
    }
    // SAFETY: installed by `UiState::attach` from a reference that outlives
    // the window; cleared by `detach` before destruction.
    Some(unsafe { &*(raw as *const UiState) })
}

/// The context menu, top to bottom, as `(command id, label, enabled)`.
///
/// Split out of `show_menu` so the composition — which entries appear, and
/// which are greyed — is testable without a live shell: the Win32 calls that
/// consume it cannot run in a test.
#[must_use]
pub fn menu_entries(
    running: bool,
    autostart: bool,
    cache: &(String, String),
    version: &str,
) -> Vec<(usize, String, bool)> {
    let autostart_line = if autostart {
        "开机自启 ✓".to_string()
    } else {
        "开机自启".to_string()
    };
    vec![
        (cmd::LABEL, cache.0.clone(), false),
        (cmd::LABEL, cache.1.clone(), false),
        (cmd::SEPARATOR, String::new(), false),
        (cmd::START, "启动代理".into(), !running),
        (cmd::STOP, "停止代理".into(), running),
        (cmd::SEPARATOR, String::new(), false),
        // The page is served by the proxy itself, so there is nothing to open
        // while it is stopped — greyed rather than hidden, so the option stays
        // discoverable.
        (cmd::OPEN_WEBUI, "打开 WebUI".into(), running),
        (cmd::OPEN_LOG, "打开代理日志".into(), true),
        (cmd::OPEN_TRAY_LOG, "打开托盘日志".into(), true),
        (cmd::AUTOSTART, autostart_line, true),
        (cmd::SEPARATOR, String::new(), false),
        (cmd::QUIT, "退出（同时停止代理）".into(), true),
        (cmd::SEPARATOR, String::new(), false),
        (cmd::LABEL, format!("版本：{version}"), false),
    ]
}

/// Compact token counts for the menu (`12.3k`, `1.1M`).
#[must_use]
pub fn fmt_tokens(v: u64) -> String {
    if v >= 1_000_000 {
        format!("{:.1}M", v as f64 / 1_000_000.0)
    } else if v >= 1000 {
        format!("{:.1}k", v as f64 / 1000.0)
    } else {
        v.to_string()
    }
}

/// The two coloured dots, built once from GDI objects.
///
/// Drawing rather than shipping a `.ico` keeps the package to a single file and
/// means the colour cannot go stale against the code that picks it.
pub struct IconSet {
    gray: Hicon,
    green: Hicon,
}

type Hicon = windows_sys::Win32::UI::WindowsAndMessaging::HICON;

impl IconSet {
    #[must_use]
    pub fn new() -> Self {
        Self {
            gray: make_dot_icon(0x80, 0x80, 0x80),
            green: make_dot_icon(0x2E, 0xCC, 0x71),
        }
    }

    #[must_use]
    pub fn handle_for(&self, running: bool) -> Hicon {
        if running {
            self.green
        } else {
            self.gray
        }
    }
}

impl Default for IconSet {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for IconSet {
    fn drop(&mut self) {
        // SAFETY: both handles came from `make_dot_icon` and are destroyed
        // exactly once, here.
        unsafe {
            if !self.gray.is_null() {
                windows_sys::Win32::UI::WindowsAndMessaging::DestroyIcon(self.gray);
            }
            if !self.green.is_null() {
                windows_sys::Win32::UI::WindowsAndMessaging::DestroyIcon(self.green);
            }
        }
    }
}

/// Draw a filled 16×16 circle and wrap it as an `HICON`.
///
/// `ICONINFO.fIcon` is a `BOOL`, so it must be 1 rather than `true`. The DIB is
/// 32-bit with a real alpha channel expressed through the mask, which is what
/// makes the antialiased edge blend instead of showing a black box.
fn make_dot_icon(r: u8, g: u8, b: u8) -> Hicon {
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, CreatePen, CreateSolidBrush, DeleteDC, DeleteObject,
        Ellipse, GetDC, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        DIB_RGB_COLORS,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, ICONINFO};

    const SIZE: i32 = 16;
    // SAFETY: every GDI handle below is created here, checked for null before
    // use, and released on all paths. The DIB is 16×16×4 bytes and GDI writes
    // only within the bitmap it was told to create.
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        let dc = CreateCompatibleDC(screen);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: u32::try_from(std::mem::size_of::<BITMAPINFOHEADER>()).unwrap_or(0),
                biWidth: SIZE,
                biHeight: SIZE,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let color = CreateDIBSection(
            dc,
            &raw const info,
            DIB_RGB_COLORS,
            &raw mut bits,
            std::ptr::null_mut(),
            0,
        );
        let mask =
            windows_sys::Win32::Graphics::Gdi::CreateBitmap(SIZE, SIZE, 1, 1, std::ptr::null());
        if color.is_null() || mask.is_null() || dc.is_null() {
            if !color.is_null() {
                DeleteObject(color);
            }
            if !mask.is_null() {
                DeleteObject(mask);
            }
            if !dc.is_null() {
                DeleteDC(dc);
            }
            ReleaseDC(std::ptr::null_mut(), screen);
            return std::ptr::null_mut();
        }

        let old_bitmap = SelectObject(dc, color);
        let brush = CreateSolidBrush(rgb(r, g, b));
        let pen = CreatePen(0, 1, rgb(0x3C, 0x3C, 0x3C));
        let old_brush = SelectObject(dc, brush);
        let old_pen = SelectObject(dc, pen);
        Ellipse(dc, 2, 2, 14, 14);
        SelectObject(dc, old_pen);
        SelectObject(dc, old_brush);
        SelectObject(dc, old_bitmap);
        DeleteObject(brush);
        DeleteObject(pen);

        // GDI draws opaque colour but leaves the alpha bytes at 0, and the mask
        // bitmap was never filled — the shell then renders the untouched pixels
        // as an opaque black square. Promote every drawn pixel to alpha 255 and
        // leave the rest at 0 so the background stays truly transparent.
        const PIXEL_COUNT: usize = (SIZE * SIZE) as usize;
        let pixels = bits.cast::<u32>();
        for i in 0..PIXEL_COUNT {
            let p = pixels.add(i);
            if (*p & 0x00FF_FFFF) != 0 {
                *p |= 0xFF00_0000;
            }
        }

        let icon_info = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&raw const icon_info);
        DeleteObject(color);
        DeleteObject(mask);
        DeleteDC(dc);
        ReleaseDC(std::ptr::null_mut(), screen);
        icon
    }
}

/// `COLORREF` is 0x00BBGGRR, not RGB.
fn rgb(r: u8, g: u8, b: u8) -> u32 {
    u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_are_abbreviated() {
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1500), "1.5k");
        assert_eq!(fmt_tokens(2_400_000), "2.4M");
    }

    #[test]
    fn colorref_is_blue_green_red_ordered() {
        // Getting this backwards is invisible until someone reads the pixels,
        // so pin it: red lands in the low byte.
        assert_eq!(rgb(0xFF, 0, 0), 0x0000_00FF);
        assert_eq!(rgb(0, 0xFF, 0), 0x0000_FF00);
        assert_eq!(rgb(0, 0, 0xFF), 0x00FF_0000);
    }

    #[test]
    fn menu_ids_round_trip_to_actions() {
        assert_eq!(action_from_id(cmd::QUIT), Action::Quit);
        assert_eq!(action_from_id(cmd::START), Action::Start);
        assert_eq!(action_from_id(cmd::OPEN_WEBUI), Action::OpenWebUi);
        assert_eq!(action_from_id(cmd::OPEN_LOG), Action::OpenLog);
        assert_eq!(action_from_id(cmd::OPEN_TRAY_LOG), Action::OpenTrayLog);
        assert_eq!(action_from_id(cmd::LABEL), Action::None);
        assert_eq!(action_from_id(cmd::SEPARATOR), Action::None);
    }

    #[test]
    fn a_menu_label_is_never_an_action() {
        // Labels and separators share id 0 / usize::MAX so that TrackPopupMenu
        // cannot report them as a choice; if that ever changed, clicking the
        // stats line would do something.
        assert_eq!(action_from_id(0), Action::None);
        assert_ne!(cmd::LABEL, cmd::START);
    }

    fn webui_entry(running: bool) -> (String, bool) {
        let cache = ("24h 缓存率：—".to_string(), " ".to_string());
        let entry = menu_entries(running, false, &cache, "0.5.2")
            .into_iter()
            .find(|(id, ..)| *id == cmd::OPEN_WEBUI)
            .expect("the WebUI entry is always present");
        (entry.1, entry.2)
    }

    #[test]
    fn the_webui_entry_is_present_and_greyed_only_while_stopped() {
        assert_eq!(webui_entry(true), ("打开 WebUI".to_string(), true));
        assert_eq!(webui_entry(false), ("打开 WebUI".to_string(), false));
    }

    #[test]
    fn every_action_in_the_menu_has_an_entry() {
        // A menu that silently loses an entry is how "打开 WebUI" would go
        // missing again; every actionable id must appear exactly once.
        let cache = ("24h 缓存率：—".to_string(), " ".to_string());
        for (running, autostart) in [(true, true), (true, false), (false, true), (false, false)] {
            let ids: Vec<usize> = menu_entries(running, autostart, &cache, "0.5.2")
                .into_iter()
                .map(|(id, ..)| id)
                .collect();
            for id in [
                cmd::START,
                cmd::STOP,
                cmd::OPEN_LOG,
                cmd::OPEN_TRAY_LOG,
                cmd::OPEN_WEBUI,
                cmd::AUTOSTART,
                cmd::QUIT,
            ] {
                assert_eq!(
                    ids.iter().filter(|i| **i == id).count(),
                    1,
                    "id {id} must appear exactly once (running={running})"
                );
            }
        }
    }

    #[test]
    fn the_two_log_entries_are_distinct_and_both_present() {
        // The proxy log and the tray log live in different directories; the
        // whole point of splitting them is that a user can open either. If a
        // later edit collapses them, this names the regression.
        let cache = ("24h 缓存率：—".to_string(), " ".to_string());
        let entries = menu_entries(true, false, &cache, "0.6.2");
        let label = |id: usize| {
            entries
                .iter()
                .find(|(i, ..)| *i == id)
                .map(|(_, text, _)| text.clone())
        };
        assert_eq!(label(cmd::OPEN_LOG).as_deref(), Some("打开代理日志"));
        assert_eq!(label(cmd::OPEN_TRAY_LOG).as_deref(), Some("打开托盘日志"));
        assert_ne!(cmd::OPEN_LOG, cmd::OPEN_TRAY_LOG);
    }
}
