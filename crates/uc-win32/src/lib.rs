//! Coarse Win32 state ("scene") gathered in microseconds, with no COM involved.
//!
//! Everything here is a single user32/kernel32 call; the expensive structural
//! perception (UI Automation) lives in `uc-uia`. Coordinates are physical pixels:
//! call [`ensure_dpi_aware`] once at process start, before any window is created.

use serde::Serialize;
use windows::core::{BOOL, PWSTR};
pub use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::{CloseHandle, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::System::Console::{GetConsoleProcessList, GetConsoleWindow};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetCursorPos, GetForegroundWindow, GetSystemMetrics, GetWindow,
    GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, SetForegroundWindow, ShowWindow,
    SwitchToThisWindow, GWL_EXSTYLE, GWL_STYLE, GW_OWNER, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_HIDE, SW_RESTORE, WS_EX_TOOLWINDOW, WS_POPUP,
};

/// Rectangle as `[x, y, w, h]` in physical screen pixels.
pub type Rect = [i32; 4];

/// Make the process per-monitor-v2 DPI aware so every rectangle we read and every
/// coordinate we inject is in physical pixels. Returns `false` if the manifest or an
/// earlier call already fixed the awareness (harmless).
pub fn ensure_dpi_aware() -> bool {
    // SAFETY: plain Win32 call with a constant argument.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_ok() }
}

pub fn foreground_hwnd() -> Option<HWND> {
    // SAFETY: no arguments, returns a handle or null.
    let h = unsafe { GetForegroundWindow() };
    if h.0.is_null() {
        None
    } else {
        Some(h)
    }
}

/// Bring a window to the front (restoring it if minimised) and report whether it is
/// now the foreground window. `SwitchToThisWindow` gets past the foreground lock that
/// `SetForegroundWindow` hits when the caller has had no recent input.
pub fn bring_to_front(hwnd: HWND) -> bool {
    // SAFETY: plain user32 calls on a handle we do not own.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        // Check first: `SwitchToThisWindow(_, true)` has Alt-Tab semantics and toggles
        // *away* from a window that is already in front.
        for attempt in 0..10 {
            if GetForegroundWindow() == hwnd {
                return true;
            }
            if attempt % 2 == 0 {
                let _ = SetForegroundWindow(hwnd);
            } else {
                SwitchToThisWindow(hwnd, true);
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        GetForegroundWindow() == hwnd
    }
}

/// Does the handle still name a window? False once the target closed.
pub fn is_window(hwnd: HWND) -> bool {
    // SAFETY: pure query on a handle value.
    unsafe { IsWindow(Some(hwnd)).as_bool() }
}

pub fn window_title(hwnd: HWND) -> String {
    // SAFETY: buffer sized from GetWindowTextLengthW plus the terminator.
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

pub fn window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: fixed-size buffer passed by slice.
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub fn window_rect(hwnd: HWND) -> Option<Rect> {
    let mut r = RECT::default();
    // SAFETY: out-pointer to a stack RECT.
    unsafe { GetWindowRect(hwnd, &mut r).ok()? };
    Some([r.left, r.top, r.right - r.left, r.bottom - r.top])
}

pub fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    // SAFETY: out-pointer to a stack u32.
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    pid
}

/// Executable file name (e.g. `notepad.exe`) of a process, or `None` if not accessible.
pub fn process_exe(pid: u32) -> Option<String> {
    // SAFETY: handle is closed on every path after the query.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            h,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return None;
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        Some(full.rsplit('\\').next().unwrap_or(&full).to_string())
    }
}

pub fn cursor_pos() -> (i32, i32) {
    let mut p = POINT::default();
    // SAFETY: out-pointer to a stack POINT.
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    (p.x, p.y)
}

/// Virtual desktop bounds `[x, y, w, h]` spanning all monitors (SendInput absolute space).
pub fn virtual_screen() -> Rect {
    // SAFETY: GetSystemMetrics has no failure mode for these indices.
    unsafe {
        [
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        ]
    }
}

/// Is the virtual key currently held? Polled, hook-free (a low-level hook would add
/// latency to every input event in the system and can be silently dropped on timeout).
pub fn key_down(vk: i32) -> bool {
    // SAFETY: reads key state, no side effects.
    unsafe { (GetAsyncKeyState(vk) as u16 & 0x8000) != 0 }
}

/// Global kill-switch: Ctrl+Alt+K. Checked by the loop before every action.
pub fn kill_switch_pressed() -> bool {
    key_down(0x11) && key_down(0x12) && key_down(0x4B)
}

/// Coarse state of the foreground window — the `scene` part of the Jev state.
#[derive(Clone, Debug, Serialize)]
pub struct Scene {
    #[serde(skip)]
    pub hwnd: isize,
    pub app: String,
    pub title: String,
    #[serde(skip)]
    pub class: String,
    pub exe: String,
    pub pid: u32,
    #[serde(skip)]
    pub rect: Rect,
    #[serde(skip)]
    pub cursor: (i32, i32),
    pub fg: bool,
}

impl Scene {
    pub fn hwnd(&self) -> HWND {
        HWND(self.hwnd as *mut core::ffi::c_void)
    }
}

/// Snapshot of the foreground window. ~10–50 µs in practice.
pub fn scene() -> Option<Scene> {
    let hwnd = foreground_hwnd()?;
    let pid = window_pid(hwnd);
    let exe = process_exe(pid).unwrap_or_default();
    let app = exe.trim_end_matches(".exe").to_string();
    Some(Scene {
        hwnd: hwnd.0 as isize,
        app,
        title: window_title(hwnd),
        class: window_class(hwnd),
        exe,
        pid,
        rect: window_rect(hwnd).unwrap_or([0, 0, 0, 0]),
        cursor: cursor_pos(),
        fg: true,
    })
}

/// A top-level window a user could name as the target of a task.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub title: String,
    pub exe: String,
    pub pid: u32,
}

/// Visible, titled, non-tool, non-cloaked top-level windows of other processes, in
/// Z-order (front first). ~1 ms.
pub fn list_windows() -> Vec<WindowInfo> {
    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `Vec` passed by `list_windows`, alive for the whole
        // enumeration; every other call is a plain user32/dwmapi query on the handle.
        let out = &mut *(lparam.0 as *mut Vec<WindowInfo>);
        if !IsWindowVisible(hwnd).as_bool() {
            return true.into();
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 {
            return true.into();
        }
        let mut cloaked: u32 = 0;
        let _ = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        );
        if cloaked != 0 {
            return true.into();
        }
        let title = window_title(hwnd);
        if title.is_empty() {
            return true.into();
        }
        let pid = window_pid(hwnd);
        if pid == std::process::id() {
            return true.into();
        }
        out.push(WindowInfo {
            hwnd: hwnd.0 as isize,
            title,
            exe: process_exe(pid).unwrap_or_default(),
            pid,
        });
        true.into()
    }
    let mut out: Vec<WindowInfo> = Vec::with_capacity(32);
    // SAFETY: the callback only touches `out` through the pointer we pass here.
    let _ = unsafe { EnumWindows(Some(cb), LPARAM(&mut out as *mut Vec<WindowInfo> as isize)) };
    out
}

/// Union of two `[x, y, w, h]` rectangles.
pub fn rect_union(a: Rect, b: Rect) -> Rect {
    if a[2] <= 0 || a[3] <= 0 {
        return b;
    }
    if b[2] <= 0 || b[3] <= 0 {
        return a;
    }
    let x = a[0].min(b[0]);
    let y = a[1].min(b[1]);
    let r = (a[0] + a[2]).max(b[0] + b[2]);
    let btm = (a[1] + a[3]).max(b[1] + b[3]);
    [x, y, r - x, btm - y]
}

/// Visible pop-ups of process `pid` other than `main`: context menus (`#32768`),
/// drop-down lists, XAML flyouts, owned dialogs — every `WS_POPUP` or owned top-level
/// window with a non-empty rectangle; tooltips and minimized windows excluded (an
/// iconic window sits at −32000,−32000 and would blow the viewport up). Front-most first, at most
/// `max_n`. These never become the foreground window, so a scan of the foreground
/// alone would miss them. ~1 ms.
pub fn popups_of(pid: u32, main: HWND, max_n: usize) -> Vec<(HWND, Rect)> {
    struct Ctx {
        pid: u32,
        main: HWND,
        max_n: usize,
        out: Vec<(HWND, Rect)>,
    }
    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `Ctx` owned by `popups_of` for the whole enumeration;
        // everything else is a plain user32 query on the handle.
        let ctx = &mut *(lparam.0 as *mut Ctx);
        if ctx.out.len() >= ctx.max_n {
            return false.into();
        }
        if hwnd == ctx.main
            || !IsWindowVisible(hwnd).as_bool()
            || IsIconic(hwnd).as_bool()
            || window_pid(hwnd) != ctx.pid
        {
            return true.into();
        }
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let owned = GetWindow(hwnd, GW_OWNER).is_ok_and(|o| o == ctx.main);
        if style & WS_POPUP.0 == 0 && !owned {
            return true.into();
        }
        let class = window_class(hwnd);
        if class == "tooltips_class32" || class.starts_with("Shell_") {
            return true.into();
        }
        match window_rect(hwnd) {
            Some(r) if r[2] > 0 && r[3] > 0 => ctx.out.push((hwnd, r)),
            _ => {}
        }
        true.into()
    }
    let mut ctx = Ctx {
        pid,
        main,
        max_n,
        out: Vec::new(),
    };
    // SAFETY: the callback only touches `ctx` through the pointer we pass here.
    let _ = unsafe { EnumWindows(Some(cb), LPARAM(&mut ctx as *mut Ctx as isize)) };
    ctx.out
}

/// Hide the console when this process is its only owner (the exe was double-clicked);
/// leave it alone when launched from a terminal, so CLI output keeps working.
pub fn hide_own_console() {
    // SAFETY: console queries; a null handle simply means "no console".
    unsafe {
        let hwnd = GetConsoleWindow();
        if hwnd.0.is_null() {
            return;
        }
        let mut pids = [0u32; 4];
        if GetConsoleProcessList(&mut pids) == 1 {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}
