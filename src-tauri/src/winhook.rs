//! Windows: shortcuts the normal hotkey API can't have (Raycast-style).
//!
//! `RegisterHotKey` refuses combinations Windows or another app already owns (Win+Space,
//! many Win+letter combos...), and some never even reach a window (Alt+Space opens the
//! system menu). For those, memeity falls back to a low-level keyboard hook: it watches
//! for exactly its combination and swallows it before Windows or the focused app sees it.
//! The same hook records shortcuts in Settings, so those combinations can be picked at all.
//!
//! The hook only runs while it's needed (a fallback shortcut is set, or Settings is
//! recording), never just in case. It looks only at the key that completes the combination
//! (and the modifiers held with it), and keeps nothing.
//!
//! The hook procedure must return quickly (Windows drops slow hooks), so it only decides
//! "ours or not" and hands everything else to a worker thread.

use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tauri_plugin_global_shortcut::{Modifiers, Shortcut};
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_CONTROL, VK_ESCAPE,
    VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage, HC_ACTION, KBDLLHOOKSTRUCT,
    LLKHF_INJECTED, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

/// Modifier set, as plain flags (cheap to compare inside the hook).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
struct Mods {
    ctrl: bool,
    alt: bool,
    shift: bool,
    win: bool,
}

impl Mods {
    fn from_shortcut(m: Modifiers) -> Self {
        Mods {
            ctrl: m.contains(Modifiers::CONTROL),
            alt: m.contains(Modifiers::ALT),
            shift: m.contains(Modifiers::SHIFT),
            win: m.contains(Modifiers::SUPER),
        }
    }

    fn is_empty(self) -> bool {
        self == Mods::default()
    }

    /// Releasing Alt or Win after a swallowed key would open the window menu / Start.
    fn needs_mask(self) -> bool {
        self.alt || self.win
    }

    fn accelerator_prefix(self) -> String {
        let mut s = String::new();
        for (on, name) in [(self.ctrl, "control+"), (self.alt, "alt+"), (self.shift, "shift+"), (self.win, "super+")] {
            if on {
                s.push_str(name);
            }
        }
        s
    }
}

#[derive(Default)]
struct HookState {
    /// The shortcut the hook opens the pane with (only when it's the fallback).
    target: Option<(Mods, u16)>,
    /// Settings is recording: the next combination is captured, not acted on.
    recording: bool,
    /// The key of a combination we swallowed, until it's released (its repeats and
    /// key-up are swallowed too).
    held: Option<u16>,
}

enum Event {
    /// The fallback shortcut was pressed.
    Trigger { mask: bool },
    /// Settings recorded a combination ("alt+Space"), or None if it was cancelled (Esc).
    Captured { accelerator: Option<String>, mask: bool },
}

fn state() -> &'static Mutex<HookState> {
    static STATE: OnceLock<Mutex<HookState>> = OnceLock::new();
    STATE.get_or_init(Default::default)
}

static EVENTS: OnceLock<Sender<Event>> = OnceLock::new();
static INSTALLED: OnceLock<bool> = OnceLock::new();

/// Starts the worker that acts on hook events. Call once at startup (it's idle until
/// the hook is installed).
pub fn init(app: &AppHandle) {
    let (tx, rx) = mpsc::channel::<Event>();
    if EVENTS.set(tx).is_err() {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        for event in rx {
            match event {
                Event::Trigger { mask } => {
                    if mask {
                        mask_modifier_release();
                    }
                    crate::toggle_pane(&app);
                }
                Event::Captured { accelerator, mask } => {
                    if mask {
                        mask_modifier_release();
                    }
                    let _ = app.emit_to(crate::settings::WINDOW, "shortcut-captured", accelerator);
                }
            }
        }
    });
}

/// Installs the hook (once; it then stays). False if Windows refused it.
fn ensure_hook() -> bool {
    *INSTALLED.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || unsafe {
            let module = GetModuleHandleW(std::ptr::null());
            let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), module, 0);
            let _ = tx.send(!hook.is_null());
            if hook.is_null() {
                return;
            }
            // Low-level hooks are called on this thread, through its message loop.
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });
        let ok = rx.recv_timeout(Duration::from_secs(2)).unwrap_or(false);
        if !ok {
            log::warn!("couldn't install the keyboard hook for shortcuts Windows reserves");
        }
        ok
    })
}

/// Use the hook for `shortcut` (Some), or stop using it for the pane (None). Returns
/// false if the shortcut's key isn't one the hook knows, or the hook couldn't start.
pub fn set_target(shortcut: Option<Shortcut>) -> bool {
    let target = match shortcut {
        None => {
            state().lock().unwrap().target = None;
            return true;
        }
        Some(s) => match vk_for(&s.key.to_string()) {
            Some(vk) => (Mods::from_shortcut(s.mods), vk),
            None => return false,
        },
    };
    if !ensure_hook() {
        return false;
    }
    state().lock().unwrap().target = Some(target);
    true
}

/// Settings started recording: capture the next combination system-wide. False if the
/// hook isn't available (Settings then records from its own key events, as elsewhere).
pub fn start_capture() -> bool {
    if !ensure_hook() {
        return false;
    }
    state().lock().unwrap().recording = true;
    true
}

pub fn stop_capture() {
    state().lock().unwrap().recording = false;
}

fn is_down(vk: u16) -> bool {
    unsafe { GetAsyncKeyState(i32::from(vk)) as u16 & 0x8000 != 0 }
}

fn current_mods() -> Mods {
    Mods {
        ctrl: is_down(VK_CONTROL),
        alt: is_down(VK_MENU),
        shift: is_down(VK_SHIFT),
        win: is_down(VK_LWIN) || is_down(VK_RWIN),
    }
}

fn is_modifier(vk: u16) -> bool {
    matches!(
        vk,
        VK_CONTROL | VK_LCONTROL | VK_RCONTROL | VK_MENU | VK_LMENU | VK_RMENU | VK_SHIFT | VK_LSHIFT | VK_RSHIFT | VK_LWIN | VK_RWIN
    )
}

/// Decides whether to swallow one key event. Must stay quick.
fn handle(msg: u32, vk: u16) -> bool {
    let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
    let mut st = state().lock().unwrap();

    if st.held == Some(vk) {
        if up {
            st.held = None;
        }
        return true; // the rest of a combination we took: repeats and the key-up
    }
    if !down || is_modifier(vk) {
        return false;
    }
    let mods = current_mods();
    let send = |event| {
        if let Some(tx) = EVENTS.get() {
            let _ = tx.send(event);
        }
    };

    if st.recording {
        let accelerator = if vk == VK_ESCAPE && mods.is_empty() {
            None // Esc alone cancels, as in the Settings window
        } else {
            match code_for(vk) {
                Some(code) => Some(format!("{}{code}", mods.accelerator_prefix())),
                None => return false, // a key memeity can't use: keep waiting
            }
        };
        st.recording = false;
        st.held = Some(vk);
        send(Event::Captured { accelerator, mask: mods.needs_mask() });
        return true;
    }

    if st.target == Some((mods, vk)) {
        st.held = Some(vk);
        send(Event::Trigger { mask: mods.needs_mask() });
        return true;
    }
    false
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for WH_KEYBOARD_LL with HC_ACTION, lparam points to a KBDLLHOOKSTRUCT.
        let kb = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        // Ignore synthetic input (ours, and instant insert's Ctrl+V).
        if kb.flags & LLKHF_INJECTED == 0 && handle(wparam as u32, kb.vkCode as u16) {
            return 1;
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

/// Taps an unassigned key, so the coming release of Alt/Win isn't a "lone" tap: that
/// would open the window menu bar (Alt) or the Start menu (Win).
fn mask_modifier_release() {
    const VK_UNASSIGNED: u16 = 0xE8;
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_UNASSIGNED, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    let inputs = [key(0), key(KEYEVENTF_KEYUP)];
    unsafe {
        SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32);
    }
}

// ---------- key names <-> Windows virtual-key codes ----------

/// Keys (by their browser `code` name, as shortcuts are stored) with fixed codes.
const NAMED: &[(&str, u16)] = &[
    ("Space", 0x20),
    ("Enter", 0x0D),
    ("Tab", 0x09),
    ("Backspace", 0x08),
    ("Delete", 0x2E),
    ("Insert", 0x2D),
    ("Home", 0x24),
    ("End", 0x23),
    ("PageUp", 0x21),
    ("PageDown", 0x22),
    ("ArrowLeft", 0x25),
    ("ArrowUp", 0x26),
    ("ArrowRight", 0x27),
    ("ArrowDown", 0x28),
    ("Backquote", 0xC0),
    ("Minus", 0xBD),
    ("Equal", 0xBB),
    ("BracketLeft", 0xDB),
    ("BracketRight", 0xDD),
    ("Backslash", 0xDC),
    ("Semicolon", 0xBA),
    ("Quote", 0xDE),
    ("Comma", 0xBC),
    ("Period", 0xBE),
    ("Slash", 0xBF),
];

fn vk_for(code: &str) -> Option<u16> {
    if let Some(c) = code.strip_prefix("Key").filter(|c| c.len() == 1) {
        let c = c.chars().next()?;
        return c.is_ascii_uppercase().then_some(c as u16);
    }
    if let Some(d) = code.strip_prefix("Digit").filter(|d| d.len() == 1) {
        let d = d.chars().next()?;
        return d.is_ascii_digit().then_some(d as u16);
    }
    if let Some(n) = code.strip_prefix('F').and_then(|n| n.parse::<u16>().ok()).filter(|n| (1..=24).contains(n)) {
        return Some(0x70 + n - 1);
    }
    NAMED.iter().find(|(name, _)| *name == code).map(|&(_, vk)| vk)
}

fn code_for(vk: u16) -> Option<String> {
    match vk {
        0x41..=0x5A => Some(format!("Key{}", vk as u8 as char)),
        0x30..=0x39 => Some(format!("Digit{}", vk as u8 as char)),
        0x70..=0x87 => Some(format!("F{}", vk - 0x70 + 1)),
        _ => NAMED.iter().find(|&&(_, v)| v == vk).map(|&(name, _)| name.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_round_trip() {
        for name in ["KeyM", "Digit7", "F12", "Space", "Backquote", "ArrowUp", "Slash"] {
            let vk = vk_for(name).unwrap_or_else(|| panic!("{name} has a code"));
            assert_eq!(code_for(vk).as_deref(), Some(name));
        }
        assert_eq!(vk_for("NumpadAdd"), None);
        assert_eq!(code_for(0xFF), None);
    }

    #[test]
    fn accelerators_match_the_settings_format() {
        let mods = Mods { ctrl: true, alt: true, shift: false, win: false };
        assert_eq!(format!("{}{}", mods.accelerator_prefix(), "Space"), "control+alt+Space");
        let s: Shortcut = "alt+Space".parse().unwrap();
        assert_eq!(Mods::from_shortcut(s.mods), Mods { alt: true, ..Default::default() });
        let s: Shortcut = "super+shift+KeyS".parse().unwrap();
        assert_eq!(Mods::from_shortcut(s.mods), Mods { win: true, shift: true, ..Default::default() });
    }
}
