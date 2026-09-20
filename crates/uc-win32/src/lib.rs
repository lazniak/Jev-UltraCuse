//! Coarse Win32 state ("scene") gathered in microseconds, with no COM involved.
//!
//! Everything here is a single user32/kernel32 call; the expensive structural
//! perception (UI Automation) lives in `uc-uia`. Coordinates are physical pixels:
//! call [`ensure_dpi_aware`] once at process start, before any window is created.

use serde::Serialize;
use windows::core::PWSTR;
pub use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::{CloseHandle, POINT, RECT};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetCursorPos, GetForegroundWindow, GetSystemMetrics, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
    SetForegroundWindow, ShowWindow, SwitchToThisWindow, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_RESTORE,
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
