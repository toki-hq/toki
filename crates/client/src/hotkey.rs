//! Global PTT input — one binding, any peripheral, passive observation.
//!
//! ## Design
//!
//! A single [`Input`] value represents the bound PTT trigger: either a
//! keyboard key or a mouse button. No modifier chords (one key, period),
//! no separate keyboard/mouse modes, no SPACE fallback. The user picks
//! whichever physical button feels natural and that's the only PTT.
//!
//! ## Detection: `device_query`, not `rdev` or `global-hotkey`
//!
//! `global-hotkey` consumes the bound key — wrong for PTT, since the
//! focused app should still see the keystroke.
//!
//! `rdev` is passive, but installs an OS-level event tap and converts
//! every event through Core Foundation calls on macOS — including
//! `CGEventKeyboardGetUnicodeString`, which has a long history of hard-
//! crashing on certain key sequences (modifier keys, IMEs, denied
//! Accessibility). We hit exactly that.
//!
//! `device_query` instead **polls** OS-level input state — `GetAsyncKeyState`
//! on Windows, `XQueryKeymap` on X11, `CGEventSourceKeyState` on macOS.
//! No tap, no event conversion, no callback into native code. The
//! trade-off is a 10 ms polling cadence in a background thread (which
//! is well below human perception of PTT latency) for a CPU cost of
//! roughly a syscall every 10 ms.
//!
//! ## Trade-offs that remain
//!
//! - **Permission**: macOS may require "Input Monitoring" permission
//!   (System Settings → Privacy & Security → Input Monitoring). Without
//!   it, polling silently returns "no keys pressed" — no crash, just
//!   no PTT until the user grants permission.
//! - **Wayland**: device_query uses X11 on Linux. Wayland sessions
//!   without XWayland will see no events. The clickable PTT button
//!   still works.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use device_query::{DeviceQuery, DeviceState, Keycode};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use crate::runtime::Cmd;

// Re-exported so config/UI can keep using these stable physical-key
// identifiers from `keyboard_types`.
pub use global_hotkey::hotkey::Code;

pub const DEFAULT_KEY: Code = Code::Backquote;

/// How often the input thread polls OS keyboard/mouse state. 10 ms
/// gives ~10 ms worst-case PTT latency — well below human perception
/// thresholds for "instant" — at the cost of ~100 syscalls/sec.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// One PTT binding from any peripheral. The user picks exactly one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    Key(Code),
    Mouse(MouseButton),
}

/// Stable, serializable mouse button identity. `Other(n)` covers
/// platform-specific extra buttons that show up as indices ≥3 in
/// device_query's `MouseState::button_pressed` vec (typically X1/X2
/// on Windows mice).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Other(u8),
}

impl MouseButton {
    fn from_index(i: usize) -> Self {
        match i {
            0 => MouseButton::Left,
            1 => MouseButton::Right,
            2 => MouseButton::Middle,
            n => MouseButton::Other(n as u8),
        }
    }

    fn to_index(self) -> usize {
        match self {
            MouseButton::Left => 0,
            MouseButton::Right => 1,
            MouseButton::Middle => 2,
            MouseButton::Other(n) => n as usize,
        }
    }

    pub fn label(self) -> String {
        match self {
            MouseButton::Left => "Left".into(),
            MouseButton::Right => "Right".into(),
            MouseButton::Middle => "Middle".into(),
            MouseButton::Other(n) => format!("Mouse{n}"),
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        match s {
            "Left" => Some(MouseButton::Left),
            "Right" => Some(MouseButton::Right),
            "Middle" => Some(MouseButton::Middle),
            other => other
                .strip_prefix("Mouse")
                .and_then(|n| n.parse::<u8>().ok())
                .map(MouseButton::Other),
        }
    }
}

/// Active PTT state. Holds the shared atomics the polling thread
/// reads on every tick, plus the sender it forwards `Cmd`s through.
pub struct InstalledHotkey {
    /// Current binding (`None` = no PTT bound). The polling thread
    /// reads this each tick; the UI updates it via `rebind`.
    current: Arc<Mutex<Option<Input>>>,

    /// Latest input captured while `recording` was true. The UI polls
    /// each frame and takes the value.
    recorded: Arc<Mutex<Option<Input>>>,
    recording: Arc<AtomicBool>,

    /// `true` once the polling thread has spawned. If it never starts,
    /// global PTT is unavailable on this system.
    available: bool,
}

impl InstalledHotkey {
    /// Set the bound input.
    pub fn rebind(&mut self, input: Input) -> Result<(), &'static str> {
        if !self.available {
            return Err("input poller unavailable");
        }
        *self.current.lock().unwrap() = Some(input);
        info!(?input, "global PTT rebound");
        Ok(())
    }

    /// Begin recording the next press. The recorded press is captured
    /// inside the polling thread so the user can release naturally
    /// without it being counted as a separate event.
    pub fn start_recording(&mut self) -> bool {
        if !self.available {
            return false;
        }
        self.recording.store(true, Ordering::Relaxed);
        *self.recorded.lock().unwrap() = None;
        true
    }

    pub fn cancel_recording(&self) {
        self.recording.store(false, Ordering::Relaxed);
        *self.recorded.lock().unwrap() = None;
    }

    /// Consume a captured input, if any. Returns `Some` exactly once
    /// per recording session.
    pub fn take_recorded(&self) -> Option<Input> {
        let mut slot = self.recorded.lock().unwrap();
        if slot.is_some() {
            self.recording.store(false, Ordering::Relaxed);
        }
        slot.take()
    }

    /// `false` if the polling thread couldn't be spawned. UI uses this
    /// to grey out the bind affordance.
    pub fn available(&self) -> bool {
        self.available
    }
}

/// Install PTT state and (unconditionally) spawn the polling thread.
/// Unlike rdev, device_query polling is benign — it just queries OS
/// state every 10 ms — so we don't need the lazy-spawn dance.
pub fn install(cmd_tx: UnboundedSender<Cmd>, initial: Option<Input>) -> InstalledHotkey {
    let current = Arc::new(Mutex::new(initial));
    let recorded = Arc::new(Mutex::new(None));
    let recording = Arc::new(AtomicBool::new(false));

    let available = spawn_poller(
        cmd_tx,
        current.clone(),
        recorded.clone(),
        recording.clone(),
    );

    InstalledHotkey {
        current,
        recorded,
        recording,
        available,
    }
}

/// Spawn the polling thread. Returns whether the spawn itself
/// succeeded; if `device_query::DeviceState::new()` panics inside the
/// thread (rare, but seen on some Linux setups without X11) we catch
/// it and log — the thread exits cleanly and `available` stays true
/// at the API level, but no events will fire.
fn spawn_poller(
    cmd_tx: UnboundedSender<Cmd>,
    current: Arc<Mutex<Option<Input>>>,
    recorded: Arc<Mutex<Option<Input>>>,
    recording: Arc<AtomicBool>,
) -> bool {
    let build = thread::Builder::new().name("toki-input".into());
    let result = build.spawn(move || {
        // device_query's constructor can panic on some Linux configs
        // where it can't open an X11 display. Catch it so the panic
        // doesn't propagate up and abort the process.
        let ds = match std::panic::catch_unwind(DeviceState::new) {
            Ok(ds) => ds,
            Err(_) => {
                warn!("device_query failed to initialize — global PTT disabled");
                return;
            }
        };
        run_poll_loop(ds, cmd_tx, current, recorded, recording);
    });
    match result {
        Ok(_) => {
            info!("global PTT poller started (device_query, 10 ms cadence)");
            true
        }
        Err(e) => {
            warn!(error = %e, "could not spawn input thread — global PTT disabled");
            false
        }
    }
}

fn run_poll_loop(
    ds: DeviceState,
    cmd_tx: UnboundedSender<Cmd>,
    current: Arc<Mutex<Option<Input>>>,
    recorded: Arc<Mutex<Option<Input>>>,
    recording: Arc<AtomicBool>,
) {
    let mut prev_keys: HashSet<Keycode> = HashSet::new();
    let mut prev_buttons: Vec<bool> = Vec::new();
    let mut ptt_down = false;

    loop {
        thread::sleep(POLL_INTERVAL);

        let keys: HashSet<Keycode> = ds.get_keys().into_iter().collect();
        let mouse = ds.get_mouse();

        // ── Recording mode: capture the first new press this tick ─
        if recording.load(Ordering::Relaxed) {
            // Prefer keyboard newly-pressed; fall back to mouse.
            let mut captured: Option<Input> = None;
            for kc in keys.difference(&prev_keys) {
                if let Some(code) = device_to_code(*kc) {
                    captured = Some(Input::Key(code));
                    break;
                }
            }
            if captured.is_none() {
                for (i, &pressed) in mouse.button_pressed.iter().enumerate() {
                    let was = prev_buttons.get(i).copied().unwrap_or(false);
                    if pressed && !was {
                        captured = Some(Input::Mouse(MouseButton::from_index(i)));
                        break;
                    }
                }
            }
            if let Some(input) = captured {
                *recorded.lock().unwrap() = Some(input);
                recording.store(false, Ordering::Relaxed);
            }
            // While recording, do NOT also fire PTT — the press would
            // otherwise leak into the previous binding's transmit gate.
        } else {
            // ── Normal: track held-state of the bound input ───────
            let bound = *current.lock().unwrap();
            if let Some(bound) = bound {
                let now_pressed = match bound {
                    Input::Key(code) => keys.iter().any(|kc| device_to_code(*kc) == Some(code)),
                    Input::Mouse(b) => mouse
                        .button_pressed
                        .get(b.to_index())
                        .copied()
                        .unwrap_or(false),
                };
                if now_pressed != ptt_down {
                    ptt_down = now_pressed;
                    let cmd = if now_pressed { Cmd::PttDown } else { Cmd::PttUp };
                    if cmd_tx.send(cmd).is_err() {
                        // Runtime gone — app shutting down.
                        break;
                    }
                }
            }
        }

        prev_keys = keys;
        prev_buttons = mouse.button_pressed;
    }
}

/// Map device_query's `Keycode` to our serialization-friendly
/// `keyboard_types::Code`. Returns `None` for keys we haven't
/// catalogued — those simply can't be bound (the listener silently
/// ignores them in both recording and matching modes).
fn device_to_code(k: Keycode) -> Option<Code> {
    use Keycode as K;
    Some(match k {
        K::A => Code::KeyA, K::B => Code::KeyB, K::C => Code::KeyC, K::D => Code::KeyD,
        K::E => Code::KeyE, K::F => Code::KeyF, K::G => Code::KeyG, K::H => Code::KeyH,
        K::I => Code::KeyI, K::J => Code::KeyJ, K::K => Code::KeyK, K::L => Code::KeyL,
        K::M => Code::KeyM, K::N => Code::KeyN, K::O => Code::KeyO, K::P => Code::KeyP,
        K::Q => Code::KeyQ, K::R => Code::KeyR, K::S => Code::KeyS, K::T => Code::KeyT,
        K::U => Code::KeyU, K::V => Code::KeyV, K::W => Code::KeyW, K::X => Code::KeyX,
        K::Y => Code::KeyY, K::Z => Code::KeyZ,
        K::Key0 => Code::Digit0, K::Key1 => Code::Digit1, K::Key2 => Code::Digit2,
        K::Key3 => Code::Digit3, K::Key4 => Code::Digit4, K::Key5 => Code::Digit5,
        K::Key6 => Code::Digit6, K::Key7 => Code::Digit7, K::Key8 => Code::Digit8,
        K::Key9 => Code::Digit9,
        K::F1 => Code::F1, K::F2 => Code::F2, K::F3 => Code::F3, K::F4 => Code::F4,
        K::F5 => Code::F5, K::F6 => Code::F6, K::F7 => Code::F7, K::F8 => Code::F8,
        K::F9 => Code::F9, K::F10 => Code::F10, K::F11 => Code::F11, K::F12 => Code::F12,
        // Modifier keys are catalogued so the user CAN bind them as a
        // PTT trigger if they want (e.g. Right Ctrl is a classic PTT
        // choice). Unlike rdev, polling them is crash-safe.
        K::LControl => Code::ControlLeft,
        K::RControl => Code::ControlRight,
        K::LShift => Code::ShiftLeft,
        K::RShift => Code::ShiftRight,
        K::LAlt => Code::AltLeft,
        K::RAlt => Code::AltRight,
        K::LMeta => Code::MetaLeft,
        K::RMeta => Code::MetaRight,
        K::Space => Code::Space,
        K::Grave => Code::Backquote,
        K::Tab => Code::Tab,
        K::Enter => Code::Enter,
        K::Backspace => Code::Backspace,
        K::Escape => Code::Escape,
        K::Insert => Code::Insert,
        K::Delete => Code::Delete,
        K::Home => Code::Home,
        K::End => Code::End,
        K::PageUp => Code::PageUp,
        K::PageDown => Code::PageDown,
        K::Up => Code::ArrowUp,
        K::Down => Code::ArrowDown,
        K::Left => Code::ArrowLeft,
        K::Right => Code::ArrowRight,
        K::Minus => Code::Minus,
        K::Equal => Code::Equal,
        K::LeftBracket => Code::BracketLeft,
        K::RightBracket => Code::BracketRight,
        K::BackSlash => Code::Backslash,
        K::Semicolon => Code::Semicolon,
        K::Apostrophe => Code::Quote,
        K::Comma => Code::Comma,
        K::Dot => Code::Period,
        K::Slash => Code::Slash,
        K::CapsLock => Code::CapsLock,
        _ => return None,
    })
}

/// Human-readable label for any bound input. Used by the Settings UI
/// and the PTT button's hint text.
pub fn format(input: Input) -> String {
    match input {
        Input::Key(c) => format_code(c),
        Input::Mouse(b) => format_mouse(b),
    }
}

fn format_mouse(b: MouseButton) -> String {
    match b {
        MouseButton::Left => "Mouse Left".into(),
        MouseButton::Right => "Mouse Right".into(),
        MouseButton::Middle => "Mouse Middle".into(),
        MouseButton::Other(n) => format!("Mouse{n}"),
    }
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
        "ControlLeft" => "LCtrl".into(),
        "ControlRight" => "RCtrl".into(),
        "ShiftLeft" => "LShift".into(),
        "ShiftRight" => "RShift".into(),
        "AltLeft" => if cfg!(target_os = "macos") { "LOpt".into() } else { "LAlt".into() },
        "AltRight" => if cfg!(target_os = "macos") { "ROpt".into() } else { "RAlt".into() },
        "MetaLeft" => if cfg!(target_os = "macos") { "LCmd".into() } else { "LSuper".into() },
        "MetaRight" => if cfg!(target_os = "macos") { "RCmd".into() } else { "RSuper".into() },
        _ => s,
    }
}
