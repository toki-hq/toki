//! Global PTT hotkey — fires PTT events even when the Toki window doesn't
//! have keyboard focus.
//!
//! Default: backtick (`). Configurable at runtime via the Settings panel.
//! See `config::HotkeyConfig` for persistence.
//!
//! Platform notes
//! ──────────────
//! - **macOS**: needs Accessibility permission (System Settings → Privacy
//!   & Security → Accessibility). Without it, registration appears to
//!   succeed but no events fire. The in-window SPACE handler keeps
//!   working regardless.
//! - **Linux/Wayland**: `global-hotkey` typically fails to install. We
//!   log a warning and fall back to in-window SPACE only.
//! - **Windows, Linux/X11**: just work.

use std::thread;

use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use crate::runtime::Cmd;

pub use global_hotkey::GlobalHotKeyManager;
pub use global_hotkey::hotkey::{Code, HotKey, Modifiers};

pub const DEFAULT_KEY: Code = Code::Backquote;

/// Active hotkey registration. Holds the manager and the currently-bound
/// `HotKey` so we can swap bindings at runtime via [`InstalledHotkey::rebind`].
pub struct InstalledHotkey {
    manager: GlobalHotKeyManager,
    current: HotKey,
}

impl InstalledHotkey {
    /// Replace the active hotkey. Registers the new one first; only if
    /// that succeeds do we unregister the old, so a rejected rebind (e.g.
    /// another app already owns the key) leaves the existing binding
    /// untouched.
    pub fn rebind(&mut self, code: Code, mods: Modifiers) -> Result<(), global_hotkey::Error> {
        let new_hotkey = HotKey::new(Some(mods), code);
        if new_hotkey.id() == self.current.id() {
            return Ok(()); // same key+mods, nothing to do
        }
        self.manager.register(new_hotkey)?;
        // Best-effort: the new binding is already live; old binding will
        // be cleaned up by Drop if this fails (uncommon).
        let _ = self.manager.unregister(self.current);
        self.current = new_hotkey;
        info!(?code, ?mods, "global PTT hotkey rebound");
        Ok(())
    }
}

/// Install the global PTT hotkey and spawn a listener that forwards
/// debounced press/release into the runtime as `Cmd::PttDown` / `Cmd::PttUp`.
///
/// Returns the [`InstalledHotkey`] on success; the caller must keep it
/// alive for the hotkey to stay registered. `None` if the platform
/// doesn't support global hotkeys (e.g. Wayland) or registration failed —
/// the in-window SPACE handler still works in either case.
pub fn install(cmd_tx: UnboundedSender<Cmd>, initial: HotKey) -> Option<InstalledHotkey> {
    let manager = match GlobalHotKeyManager::new() {
        Ok(m) => m,
        Err(e) => {
            warn!(
                error = %e,
                "global hotkey manager unavailable — in-window SPACE still works"
            );
            return None;
        }
    };

    if let Err(e) = manager.register(initial) {
        warn!(error = %e, "failed to register initial global PTT hotkey");
        return None;
    }
    info!("global PTT hotkey registered");

    // Drain hotkey events on a dedicated OS thread and forward to the
    // tokio runtime as commands. We edge-detect locally so OS key-repeat
    // (which fires Pressed repeatedly while the user holds the key) only
    // produces a single PttDown / PttUp pair per physical hold.
    thread::Builder::new()
        .name("toki-hotkey".into())
        .spawn(move || {
            let receiver = GlobalHotKeyEvent::receiver();
            let mut down = false;
            loop {
                match receiver.recv() {
                    Ok(evt) => {
                        let cmd = match (evt.state, down) {
                            (HotKeyState::Pressed, false) => {
                                down = true;
                                Some(Cmd::PttDown)
                            }
                            (HotKeyState::Released, true) => {
                                down = false;
                                Some(Cmd::PttUp)
                            }
                            // Pressed-while-already-down (auto-repeat) and
                            // spurious Released-while-up are dropped.
                            _ => None,
                        };
                        if let Some(cmd) = cmd {
                            if cmd_tx.send(cmd).is_err() {
                                break; // runtime gone — app shutting down
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .expect("spawn hotkey listener thread");

    Some(InstalledHotkey {
        manager,
        current: initial,
    })
}

/// Map an egui keystroke to a `keyboard_types::Code` suitable for
/// registering as a global hotkey. Returns `None` for keys we can't
/// usefully bind (Caps Lock, modifier keys alone, anything we haven't
/// catalogued).
pub fn from_egui_key(k: egui::Key) -> Option<Code> {
    use egui::Key as E;
    Some(match k {
        E::A => Code::KeyA, E::B => Code::KeyB, E::C => Code::KeyC, E::D => Code::KeyD,
        E::E => Code::KeyE, E::F => Code::KeyF, E::G => Code::KeyG, E::H => Code::KeyH,
        E::I => Code::KeyI, E::J => Code::KeyJ, E::K => Code::KeyK, E::L => Code::KeyL,
        E::M => Code::KeyM, E::N => Code::KeyN, E::O => Code::KeyO, E::P => Code::KeyP,
        E::Q => Code::KeyQ, E::R => Code::KeyR, E::S => Code::KeyS, E::T => Code::KeyT,
        E::U => Code::KeyU, E::V => Code::KeyV, E::W => Code::KeyW, E::X => Code::KeyX,
        E::Y => Code::KeyY, E::Z => Code::KeyZ,
        E::Num0 => Code::Digit0, E::Num1 => Code::Digit1, E::Num2 => Code::Digit2,
        E::Num3 => Code::Digit3, E::Num4 => Code::Digit4, E::Num5 => Code::Digit5,
        E::Num6 => Code::Digit6, E::Num7 => Code::Digit7, E::Num8 => Code::Digit8,
        E::Num9 => Code::Digit9,
        E::F1 => Code::F1, E::F2 => Code::F2, E::F3 => Code::F3, E::F4 => Code::F4,
        E::F5 => Code::F5, E::F6 => Code::F6, E::F7 => Code::F7, E::F8 => Code::F8,
        E::F9 => Code::F9, E::F10 => Code::F10, E::F11 => Code::F11, E::F12 => Code::F12,
        E::Space => Code::Space,
        E::Backtick => Code::Backquote,
        E::Tab => Code::Tab,
        E::Enter => Code::Enter,
        E::Backspace => Code::Backspace,
        E::Escape => Code::Escape,
        E::Insert => Code::Insert,
        E::Delete => Code::Delete,
        E::Home => Code::Home,
        E::End => Code::End,
        E::PageUp => Code::PageUp,
        E::PageDown => Code::PageDown,
        E::ArrowUp => Code::ArrowUp,
        E::ArrowDown => Code::ArrowDown,
        E::ArrowLeft => Code::ArrowLeft,
        E::ArrowRight => Code::ArrowRight,
        E::Minus => Code::Minus,
        E::Equals => Code::Equal,
        E::OpenBracket => Code::BracketLeft,
        E::CloseBracket => Code::BracketRight,
        E::Backslash => Code::Backslash,
        E::Semicolon => Code::Semicolon,
        E::Quote => Code::Quote,
        E::Comma => Code::Comma,
        E::Period => Code::Period,
        E::Slash => Code::Slash,
        _ => return None,
    })
}

/// Map egui modifier flags to `global_hotkey::Modifiers`. egui's `command`
/// field is platform-aware (Ctrl on Win/Linux, Cmd on Mac) — we ignore it
/// here and use the physical flags so the binding survives moving the
/// config between platforms.
pub fn from_egui_modifiers(m: egui::Modifiers) -> Modifiers {
    let mut mods = Modifiers::empty();
    if m.ctrl {
        mods |= Modifiers::CONTROL;
    }
    if m.shift {
        mods |= Modifiers::SHIFT;
    }
    if m.alt {
        mods |= Modifiers::ALT;
    }
    if m.mac_cmd {
        mods |= Modifiers::META;
    }
    mods
}

/// Human-readable label for a `Code` + `Modifiers` pair, e.g. `Ctrl+F8`,
/// `` ` ``, `Cmd+Space`. Used by the Settings UI.
pub fn format(code: Code, mods: Modifiers) -> String {
    let mut s = String::new();
    if mods.contains(Modifiers::CONTROL) {
        s.push_str("Ctrl+");
    }
    if mods.contains(Modifiers::SHIFT) {
        s.push_str("Shift+");
    }
    if mods.contains(Modifiers::ALT) {
        s.push_str(if cfg!(target_os = "macos") { "Opt+" } else { "Alt+" });
    }
    if mods.contains(Modifiers::META) {
        s.push_str(if cfg!(target_os = "macos") { "Cmd+" } else { "Super+" });
    }
    s.push_str(&format_code(code));
    s
}

fn format_code(c: Code) -> String {
    let s = c.to_string();
    if let Some(letter) = s.strip_prefix("Key") {
        return letter.to_string();
    }
    if let Some(digit) = s.strip_prefix("Digit") {
        return digit.to_string();
    }
    match s.as_str() {
        "Backquote" => "`".to_string(),
        _ => s,
    }
}
