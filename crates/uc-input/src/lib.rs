//! Input injection through `SendInput`, batched: a whole action (move + click, a chord,
//! a string) is ONE syscall with one `INPUT[]`, exactly as the Python reference
//! (`executor/sendinput.py`) does — but with no interpreter between the loop and user32.
//!
//! Keyboard: printable ASCII goes as virtual-key + **scan code** (looks like a physical
//! key to DirectInput/Raw-Input consumers); everything else (Polish diacritics, emoji)
//! as `KEYEVENTF_UNICODE`. Long text goes through the clipboard + Ctrl+V.

use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, VkKeyScanW, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};

const CF_UNICODETEXT: u32 = 13;
const WHEEL_DELTA: i32 = 120;

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("SendInput injected {sent} of {wanted} events (blocked by UIPI/secure desktop?)")]
    Partial { sent: u32, wanted: u32 },
    #[error("unknown key name: {0}")]
    UnknownKey(String),
    #[error("clipboard busy")]
    ClipboardBusy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    fn flags(self) -> (MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS) {
        match self {
            Button::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            Button::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
            Button::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        }
    }
}

fn mouse(dx: i32, dy: i32, flags: MOUSE_EVENT_FLAGS, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> Result<u32, InputError> {
    if inputs.is_empty() {
        return Ok(0);
    }
    // SAFETY: slice of fully initialised INPUT structs; size passed explicitly.
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        return Err(InputError::Partial {
            sent,
            wanted: inputs.len() as u32,
        });
    }
    Ok(sent)
}

/// Screen px → 0..65535 across the virtual desktop (all monitors).
fn absolute(x: i32, y: i32) -> (i32, i32) {
    let [vx, vy, vw, vh] = uc_win32::virtual_screen();
    let nx = ((x - vx) as i64 * 65535 / (vw.max(2) - 1) as i64) as i32;
    let ny = ((y - vy) as i64 * 65535 / (vh.max(2) - 1) as i64) as i32;
    (nx, ny)
}

const MOVE_ABS: MOUSE_EVENT_FLAGS =
    MOUSE_EVENT_FLAGS(MOUSEEVENTF_MOVE.0 | MOUSEEVENTF_ABSOLUTE.0 | MOUSEEVENTF_VIRTUALDESK.0);

pub fn move_to(x: i32, y: i32) -> Result<u32, InputError> {
    let (nx, ny) = absolute(x, y);
    send(&[mouse(nx, ny, MOVE_ABS, 0)])
}

/// Move + press + release in one syscall. `clicks` = 1 or 2 (double).
pub fn click(x: i32, y: i32, button: Button, clicks: u8) -> Result<u32, InputError> {
    let (nx, ny) = absolute(x, y);
    let (down, up) = button.flags();
    let mut seq = vec![mouse(nx, ny, MOVE_ABS, 0)];
    for _ in 0..clicks.max(1) {
        seq.push(mouse(0, 0, down, 0));
        seq.push(mouse(0, 0, up, 0));
    }
    send(&seq)
}

/// Drag from the current cursor position to (x, y) in `steps` interpolated moves.
pub fn drag_to(x: i32, y: i32, button: Button, steps: u32) -> Result<u32, InputError> {
    let (sx, sy) = {
        let (cx, cy) = uc_win32::cursor_pos();
        absolute(cx, cy)
    };
    let (ex, ey) = absolute(x, y);
    let (down, up) = button.flags();
    let n = steps.max(1) as i64;
    let mut seq = vec![mouse(sx, sy, MOVE_ABS, 0), mouse(0, 0, down, 0)];
    for i in 1..=n {
        let ix = sx as i64 + (ex - sx) as i64 * i / n;
        let iy = sy as i64 + (ey - sy) as i64 * i / n;
        seq.push(mouse(ix as i32, iy as i32, MOVE_ABS, 0));
    }
    seq.push(mouse(0, 0, up, 0));
    send(&seq)
}

/// `notches` > 0 scrolls down. Optional move first.
pub fn scroll(notches: i32, at: Option<(i32, i32)>) -> Result<u32, InputError> {
    let mut seq = Vec::with_capacity(2);
    if let Some((x, y)) = at {
        let (nx, ny) = absolute(x, y);
        seq.push(mouse(nx, ny, MOVE_ABS, 0));
    }
    seq.push(mouse(0, 0, MOUSEEVENTF_WHEEL, -notches * WHEEL_DELTA));
    send(&seq)
}

fn vk_of(name: &str) -> Result<u16, InputError> {
    let vk = match name.to_ascii_lowercase().as_str() {
        "backspace" => 0x08,
        "tab" => 0x09,
        "enter" | "return" => 0x0D,
        "shift" => 0x10,
        "ctrl" | "control" => 0x11,
        "alt" | "menu" => 0x12,
        "pause" => 0x13,
        "capslock" => 0x14,
        "esc" | "escape" => 0x1B,
        "space" => 0x20,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "end" => 0x23,
        "home" => 0x24,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "printscreen" => 0x2C,
        "insert" => 0x2D,
        "delete" | "del" => 0x2E,
        "win" | "lwin" => 0x5B,
        "rwin" => 0x5C,
        "apps" => 0x5D,
        s if s.len() >= 2 && s.starts_with('f') && s[1..].chars().all(|c| c.is_ascii_digit()) => {
            let n: u16 = s[1..]
                .parse()
                .map_err(|_| InputError::UnknownKey(name.into()))?;
            if !(1..=24).contains(&n) {
                return Err(InputError::UnknownKey(name.into()));
            }
            0x6F + n
        }
        s if s.chars().count() == 1 => {
            let ch = s.chars().next().unwrap() as u16;
            // SAFETY: pure lookup.
            let r = unsafe { VkKeyScanW(ch) };
            if r == -1 {
                return Err(InputError::UnknownKey(name.into()));
            }
            (r & 0xFF) as u16
        }
        _ => return Err(InputError::UnknownKey(name.into())),
    };
    Ok(vk)
}

fn scan_of(vk: u16) -> u16 {
    // SAFETY: pure lookup.
    unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) as u16 }
}

/// Press and release a named key (`enter`, `esc`, `f5`, `a`).
pub fn press(name: &str) -> Result<u32, InputError> {
    let vk = vk_of(name)?;
    let sc = scan_of(vk);
    send(&[
        key(vk, sc, KEYEVENTF_SCANCODE),
        key(vk, sc, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP),
    ])
}

/// A chord such as `["ctrl", "s"]`: all downs then all ups (reversed), one syscall.
pub fn hotkey(names: &[&str]) -> Result<u32, InputError> {
    let vks: Vec<(u16, u16)> = names
        .iter()
        .map(|n| vk_of(n).map(|vk| (vk, scan_of(vk))))
        .collect::<Result<_, _>>()?;
    let mut seq = Vec::with_capacity(vks.len() * 2);
    for (vk, sc) in &vks {
        seq.push(key(*vk, *sc, KEYEVENTF_SCANCODE));
    }
    for (vk, sc) in vks.iter().rev() {
        seq.push(key(*vk, *sc, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP));
    }
    send(&seq)
}

fn char_inputs(ch: char, seq: &mut Vec<INPUT>) {
    let code = ch as u32;
    if (32..127).contains(&code) {
        // SAFETY: pure lookup.
        let r = unsafe { VkKeyScanW(code as u16) };
        if r != -1 {
            let vk = (r & 0xFF) as u16;
            let shift = (r >> 8) & 0x01 != 0;
            let sc = scan_of(vk);
            if shift {
                seq.push(key(0x10, 0x2A, KEYEVENTF_SCANCODE));
            }
            seq.push(key(vk, sc, KEYEVENTF_SCANCODE));
            seq.push(key(vk, sc, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP));
            if shift {
                seq.push(key(0x10, 0x2A, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP));
            }
            return;
        }
    }
    let mut units = [0u16; 2];
    for u in ch.encode_utf16(&mut units) {
        seq.push(key(0, *u, KEYEVENTF_UNICODE));
        seq.push(key(0, *u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
    }
}

/// Type text as key events, all in one `SendInput`.
pub fn type_text(text: &str) -> Result<u32, InputError> {
    let mut seq = Vec::with_capacity(text.len() * 2);
    for ch in text.chars() {
        char_inputs(ch, &mut seq);
    }
    send(&seq)
}

/// Clipboard + Ctrl+V — the fastest channel for long dictated text.
pub fn paste_text(text: &str) -> Result<u32, InputError> {
    set_clipboard_text(text)?;
    hotkey(&["ctrl", "v"])
}

/// `auto`: ≥ 16 chars → paste, otherwise key events.
pub fn type_auto(text: &str) -> Result<u32, InputError> {
    if text.chars().count() >= 16 {
        paste_text(text)
    } else {
        type_text(text)
    }
}

pub fn set_clipboard_text(text: &str) -> Result<(), InputError> {
    let mut data: Vec<u16> = text.encode_utf16().collect();
    data.push(0);
    let bytes = data.len() * 2;
    // SAFETY: clipboard protocol — open, empty, hand a moveable global block to the
    // system (ownership transfers on SetClipboardData success), close.
    unsafe {
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(None).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        if !opened {
            return Err(InputError::ClipboardBusy);
        }
        let result = (|| -> Result<(), InputError> {
            EmptyClipboard().map_err(|_| InputError::ClipboardBusy)?;
            let h: HGLOBAL =
                GlobalAlloc(GMEM_MOVEABLE, bytes).map_err(|_| InputError::ClipboardBusy)?;
            let p = GlobalLock(h);
            if p.is_null() {
                return Err(InputError::ClipboardBusy);
            }
            std::ptr::copy_nonoverlapping(data.as_ptr() as *const u8, p as *mut u8, bytes);
            let _ = GlobalUnlock(h);
            SetClipboardData(CF_UNICODETEXT, Some(HANDLE(h.0)))
                .map_err(|_| InputError::ClipboardBusy)?;
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}
