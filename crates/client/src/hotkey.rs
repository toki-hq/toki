//! Global PTT input — one binding, any peripheral, passive observation.
//!
//! ## Design
//!
//! A single [`Input`] value represents the bound PTT trigger. It can be
//! a keyboard key, a mouse button, a game-controller button, an Elgato
//! Stream Deck key, or a button on an arbitrary USB HID device. No
//! modifier chords (one button, period), no SPACE fallback. The user
//! picks whichever physical button feels natural and that's the only PTT.
//! The same goes for the memory-recall (M1–M4) and frequency up/down
//! hotkeys — any of them can bind any of those device kinds.
//!
//! ## Three backends, one shared state
//!
//! Each device class is polled on its own background thread, but all of
//! them feed the **same** [`Shared`] state, so the bind-capture
//! ("hold 1 s to commit") and edge-detection logic in [`process_tick`]
//! is reused, not duplicated:
//!
//! - **keyboard + mouse** → `device_query` (see below).
//! - **game controllers / joysticks** → `gilrs`. Passive and
//!   non-exclusive, so a focused game still receives the input.
//! - **Stream Deck + generic HID** → `hidapi`. Opened **non-exclusively**
//!   (the `macos-shared-device` feature is mandatory — without it,
//!   opening a HID device on macOS would grab it and stop the OS / the
//!   vendor software from seeing it).
//!
//! Whichever backend reports the bound input drives PTT; whichever one
//! first sees a held input reach [`HOLD_DURATION`] wins the bind.
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
//! ## Passthrough
//!
//! Keyboard and mouse bindings are **passthrough**: `device_query` only
//! reads OS state, never consumes the event, so the focused app still
//! gets the keystroke. Gamepad / Stream Deck / generic-HID buttons are
//! **capture-only** — the OS does not route those buttons to focused
//! apps in the first place — but we never open the device exclusively,
//! so a game still receives joystick input concurrently.
//!
//! ## Trade-offs that remain
//!
//! - **Permission**: macOS may require "Input Monitoring" permission
//!   (System Settings → Privacy & Security → Input Monitoring). Both
//!   `device_query` polling and `hidapi` reads fall under this same TCC
//!   category. Without it, polling silently returns "no input pressed"
//!   — no crash, just no PTT until the user grants permission. gilrs
//!   (GameController framework) does not need Input Monitoring.
//! - **Wayland**: device_query uses X11 on Linux. Wayland sessions
//!   without XWayland will see no events. The clickable PTT button
//!   still works.
//! - **Multiple identical devices**: two of the same gamepad / HID
//!   gadget are matched first-found (lowest enumeration order). The
//!   gamepad binding's `index` field allows an explicit second-device
//!   binding; HID/Stream Deck always match the first device of that
//!   VID/PID.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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

/// How long a key or button must be held continuously before it gets
/// committed as the new PTT binding. The polling thread tracks the
/// press-start time of every currently-held input and commits the
/// first one to reach this duration.
///
/// Why hold-to-bind: a plain "first new press" capture loses against
/// the on-screen Cancel button — the OS polling thread sees the
/// Cancel mouse-down before egui sees the matching mouse-up click,
/// so any click on Cancel gets captured as a Left-mouse binding.
/// Requiring 1 s of sustained hold means a Cancel *click* (~100 ms)
/// can't qualify, while an intentional bind (deliberately held) does.
const HOLD_DURATION: Duration = Duration::from_secs(1);

/// Keys that are never bindable as PTT. Space and Enter collide with
/// the universal "activate focused control" semantics in dialogs and
/// forms; Escape is reserved as the always-on cancel for the bind
/// flow itself. Pressing any of these during recording is silently
/// ignored (no hold timer starts), and a loaded config that points at
/// one of them is treated as "no PTT bound" at match time as a
/// belt-and-braces guard against hand-edited config files.
const RESTRICTED_KEYS: &[Keycode] = &[Keycode::Space, Keycode::Enter, Keycode::Escape];

/// `true` if `code` resolves to a keyboard key on our restricted
/// list. Mouse / gamepad / Stream Deck / generic-HID bindings have no
/// restrictions — those buttons don't collide with dialog semantics.
pub fn is_restricted(input: Input) -> bool {
    match input {
        Input::Key(code) => RESTRICTED_KEYS
            .iter()
            .any(|kc| device_to_code(*kc) == Some(code)),
        Input::Mouse(_) | Input::Gamepad(_) | Input::StreamDeck { .. } | Input::Hid(_) => false,
    }
}

/// One PTT binding from any peripheral. The user picks exactly one.
///
/// Every variant is `Copy + Eq + Hash` because `Input` is used as a
/// `HashMap` key in the bind-capture hold-timer and compared on every
/// poll tick. New device kinds therefore identify themselves with
/// small scalar fields only — no `String`/`Vec` — so a binding still
/// round-trips through TOML and re-matches the same physical button
/// after a replug.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Input {
    Key(Code),
    Mouse(MouseButton),
    /// A button on a game controller / joystick, identified by its
    /// mapped *semantic* (South, LeftTrigger2, …) rather than a raw HID
    /// usage — so the same physical button re-matches after replug and
    /// on a different-but-similar pad.
    Gamepad(GamepadButton),
    /// A key on an Elgato Stream Deck. `key` is the 0-based index from
    /// the device's key-state input report; `pid` selects the model so
    /// the report is parsed at the right offset.
    StreamDeck {
        pid: u16,
        key: u8,
    },
    /// A digital input on an arbitrary USB HID device, identified by
    /// device class (VID/PID) plus the `(byte, bit)` position in the
    /// input report that flips 0→1 on press.
    Hid(HidButton),
}

/// Stable game-controller button identity. `index` disambiguates
/// multiple identical pads (0 = first-found / lowest gilrs id).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GamepadButton {
    pub button: GamepadCode,
    pub index: u8,
}

/// Our own catalogue of game-controller buttons, mirroring the subset
/// of `gilrs::Button` we care about. Kept independent of gilrs's enum
/// (and `#[repr(u8)]`) so bindings serialize stably across gilrs
/// version bumps and stay `Copy + Hash`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum GamepadCode {
    South,
    East,
    North,
    West,
    LeftTrigger,
    LeftTrigger2,
    RightTrigger,
    RightTrigger2,
    Select,
    Start,
    Mode,
    LeftThumb,
    RightThumb,
    DPadUp,
    DPadDown,
    DPadLeft,
    DPadRight,
}

impl GamepadCode {
    /// Stable lowercase token used in the serialized binding string.
    fn token(self) -> &'static str {
        match self {
            GamepadCode::South => "South",
            GamepadCode::East => "East",
            GamepadCode::North => "North",
            GamepadCode::West => "West",
            GamepadCode::LeftTrigger => "LeftTrigger",
            GamepadCode::LeftTrigger2 => "LeftTrigger2",
            GamepadCode::RightTrigger => "RightTrigger",
            GamepadCode::RightTrigger2 => "RightTrigger2",
            GamepadCode::Select => "Select",
            GamepadCode::Start => "Start",
            GamepadCode::Mode => "Mode",
            GamepadCode::LeftThumb => "LeftThumb",
            GamepadCode::RightThumb => "RightThumb",
            GamepadCode::DPadUp => "DPadUp",
            GamepadCode::DPadDown => "DPadDown",
            GamepadCode::DPadLeft => "DPadLeft",
            GamepadCode::DPadRight => "DPadRight",
        }
    }

    fn from_token(s: &str) -> Option<Self> {
        Some(match s {
            "South" => GamepadCode::South,
            "East" => GamepadCode::East,
            "North" => GamepadCode::North,
            "West" => GamepadCode::West,
            "LeftTrigger" => GamepadCode::LeftTrigger,
            "LeftTrigger2" => GamepadCode::LeftTrigger2,
            "RightTrigger" => GamepadCode::RightTrigger,
            "RightTrigger2" => GamepadCode::RightTrigger2,
            "Select" => GamepadCode::Select,
            "Start" => GamepadCode::Start,
            "Mode" => GamepadCode::Mode,
            "LeftThumb" => GamepadCode::LeftThumb,
            "RightThumb" => GamepadCode::RightThumb,
            "DPadUp" => GamepadCode::DPadUp,
            "DPadDown" => GamepadCode::DPadDown,
            "DPadLeft" => GamepadCode::DPadLeft,
            "DPadRight" => GamepadCode::DPadRight,
            _ => return None,
        })
    }

    /// Short human-facing label for the Settings UI (Xbox-style).
    fn short_label(self) -> &'static str {
        match self {
            GamepadCode::South => "A",
            GamepadCode::East => "B",
            GamepadCode::North => "Y",
            GamepadCode::West => "X",
            GamepadCode::LeftTrigger => "LB",
            GamepadCode::LeftTrigger2 => "LT",
            GamepadCode::RightTrigger => "RB",
            GamepadCode::RightTrigger2 => "RT",
            GamepadCode::Select => "Select",
            GamepadCode::Start => "Start",
            GamepadCode::Mode => "Mode",
            GamepadCode::LeftThumb => "LS",
            GamepadCode::RightThumb => "RS",
            GamepadCode::DPadUp => "D-Up",
            GamepadCode::DPadDown => "D-Down",
            GamepadCode::DPadLeft => "D-Left",
            GamepadCode::DPadRight => "D-Right",
        }
    }
}

/// A digital input on an arbitrary USB HID device. The `(byte, bit)`
/// pair is the *physical* report position, which is stable for a given
/// device model across replug — we deliberately exclude the OS device
/// path / serial (those differ per port and per replug).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HidButton {
    pub vid: u16,
    pub pid: u16,
    pub byte: u8,
    pub bit: u8,
}

/// Elgato's USB vendor id. Any HID device with this VID is treated as a
/// Stream Deck and bound via [`Input::StreamDeck`] rather than the
/// generic [`Input::Hid`] path.
pub const ELGATO_VID: u16 = 0x0fd9;

/// Stable, serializable mouse button identity. `Other(n)` covers
/// platform-specific extra buttons that show up as indices ≥3 in
/// device_query's `MouseState::button_pressed` vec (typically X1/X2
/// on Windows mice).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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

    /// Inverse of [`MouseButton::from_index`]. Matching no longer
    /// indexes by position (the poll loop builds a pressed-set keyed by
    /// `MouseButton` directly), so this only backs the round-trip test.
    #[cfg(test)]
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

impl Input {
    /// Encode this binding as a stable, colon-delimited token for TOML
    /// persistence. The first field is a kind tag; the rest are the
    /// scalar identity fields. Kept here (next to [`Input`]) so
    /// `config.rs` stays a thin serializer and the grammar is unit-
    /// testable alongside the type.
    ///
    /// Grammar:
    /// - `key:<Code>` e.g. `key:Backquote`
    /// - `mouse:<label>` e.g. `mouse:Middle`, `mouse:Mouse4`
    /// - `gamepad:<index>:<button>` e.g. `gamepad:0:South`
    /// - `streamdeck:<vid>:<pid>:<key>` e.g. `streamdeck:0x0fd9:0x0080:3`
    /// - `hid:<vid>:<pid>:<byte>:<bit>` e.g. `hid:0x046d:0xc52b:2:4`
    pub fn to_token(self) -> String {
        match self {
            Input::Key(code) => format!("key:{code}"),
            Input::Mouse(b) => format!("mouse:{}", b.label()),
            Input::Gamepad(g) => format!("gamepad:{}:{}", g.index, g.button.token()),
            Input::StreamDeck { pid, key } => {
                format!("streamdeck:{ELGATO_VID:#06x}:{pid:#06x}:{key}")
            }
            Input::Hid(h) => format!("hid:{:#06x}:{:#06x}:{}:{}", h.vid, h.pid, h.byte, h.bit),
        }
    }

    /// Parse a token produced by [`Input::to_token`]. Returns `None`
    /// for an unknown tag, a missing/extra field, or an unparseable
    /// value — the caller treats that as "unbound".
    pub fn from_token(s: &str) -> Option<Self> {
        let (tag, rest) = s.split_once(':')?;
        match tag {
            "key" => Code::from_str(rest).ok().map(Input::Key),
            "mouse" => MouseButton::from_label(rest).map(Input::Mouse),
            "gamepad" => {
                let (index, button) = rest.split_once(':')?;
                Some(Input::Gamepad(GamepadButton {
                    index: index.parse().ok()?,
                    button: GamepadCode::from_token(button)?,
                }))
            }
            "streamdeck" => {
                let mut it = rest.split(':');
                let _vid = parse_u16(it.next()?)?; // implicitly ELGATO_VID
                let pid = parse_u16(it.next()?)?;
                let key = it.next()?.parse().ok()?;
                if it.next().is_some() {
                    return None; // trailing garbage
                }
                Some(Input::StreamDeck { pid, key })
            }
            "hid" => {
                let mut it = rest.split(':');
                let vid = parse_u16(it.next()?)?;
                let pid = parse_u16(it.next()?)?;
                let byte = it.next()?.parse().ok()?;
                let bit = it.next()?.parse().ok()?;
                if it.next().is_some() {
                    return None; // trailing garbage
                }
                Some(Input::Hid(HidButton {
                    vid,
                    pid,
                    byte,
                    bit,
                }))
            }
            _ => None,
        }
    }
}

/// Parse a `u16` written either as a hex literal (`0x0fd9`) or plain
/// decimal. Used by [`Input::from_token`] for VID/PID fields.
fn parse_u16(s: &str) -> Option<u16> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16).ok()
    } else {
        s.parse().ok()
    }
}

/// Cross-thread input state, shared by every backend poller. One
/// instance is built in [`install`] and cloned (cheap `Arc` bumps)
/// into each device thread, so a bind, PTT edge, recall, or tune
/// fires no matter which peripheral produced the input. All fields
/// are individually synchronized, so concurrent writes from the
/// keyboard/mouse, gamepad, and HID threads are safe.
#[derive(Clone)]
struct Shared {
    /// The PTT bindings: `[primary, secondary]`. PTT engages while
    /// **either** slot is pressed, so the secondary acts as a fallback
    /// (e.g. a keyboard key backing up a gamepad button). `None` per
    /// slot = unbound. Backends read this each tick; the UI updates it
    /// via `rebind` / `rebind_secondary`.
    current: Arc<Mutex<[Option<Input>; 2]>>,
    /// Latest input captured while `recording` was true. The UI polls
    /// each frame and takes the value.
    recorded: Arc<Mutex<Option<Input>>>,
    recording: Arc<AtomicBool>,
    /// Progress of the currently-longest held input toward
    /// [`HOLD_DURATION`], stored as `f32` bits. Written by whichever
    /// backend currently holds the longest input; cleared to 0 when
    /// recording ends.
    hold_progress: Arc<AtomicU32>,
    /// The four memory-recall bindings (M1–M4), `None` per slot when
    /// unbound. Backends watch these for press edges and push the slot
    /// index into `recalls`.
    memory: Arc<Mutex<[Option<Input>; 4]>>,
    /// Press-edge events for memory recalls, drained each frame by the
    /// UI via [`InstalledHotkey::take_recalls`]. A queue rather than a
    /// single slot so two quick presses can't be lost.
    recalls: Arc<Mutex<Vec<usize>>>,
    /// Tune up / down bindings (`[up, down]`), `None` per slot when
    /// unbound. Watched for press edges like the memory hotkeys.
    freq: Arc<Mutex<[Option<Input>; 2]>>,
    /// Net pending tune steps: each up-press adds 1, each down-press
    /// subtracts 1. Drained by the UI via
    /// [`InstalledHotkey::take_freq_delta`].
    freq_delta: Arc<Mutex<i32>>,
}

impl Shared {
    fn new(
        initial: Option<Input>,
        initial_secondary: Option<Input>,
        initial_memory: [Option<Input>; 4],
        initial_freq: [Option<Input>; 2],
    ) -> Self {
        Shared {
            current: Arc::new(Mutex::new([initial, initial_secondary])),
            recorded: Arc::new(Mutex::new(None)),
            recording: Arc::new(AtomicBool::new(false)),
            hold_progress: Arc::new(AtomicU32::new(0)),
            memory: Arc::new(Mutex::new(initial_memory)),
            recalls: Arc::new(Mutex::new(Vec::new())),
            freq: Arc::new(Mutex::new(initial_freq)),
            freq_delta: Arc::new(Mutex::new(0)),
        }
    }
}

/// Per-thread mutable state carried across ticks by a single backend.
/// Each backend owns its own `BackendState` (so its prev-frame edges
/// and hold timers are independent), but commits into the shared
/// state — that's what lets "hold to bind" and edge detection work
/// uniformly across keyboard, gamepad, and HID without duplication.
#[derive(Default)]
struct BackendState {
    /// `true` if this backend's bound input was pressed last tick —
    /// for PTT down/up edge detection.
    ptt_down: bool,
    /// Whether `recording` was active on this backend's previous tick,
    /// so it can observe the false→true / true→false edges itself.
    was_recording: bool,
    /// Previous-frame pressed state of each memory hotkey (recall on
    /// the down edge only).
    mem_prev: [bool; 4],
    /// Same, for the tune up/down hotkeys (`[up, down]`).
    freq_prev: [bool; 2],
    /// While recording, the earliest-press timestamp of every
    /// currently-held input from this backend. Cleared on entry into
    /// recording mode so anything pressed *before* "Bind" isn't
    /// pre-credited.
    held: HashMap<Input, Instant>,
}

/// Drive one backend's per-tick logic from a device-agnostic snapshot
/// of the inputs it currently sees pressed. Handles the bind-capture
/// hold-timer (in recording mode) and PTT/memory/freq edge detection
/// (in normal mode), committing into `shared`. `now` is injected
/// rather than read from the clock so the logic is unit-testable.
///
/// Returns `false` when `cmd_tx` is closed (the runtime has gone away)
/// — the caller should stop its loop.
fn process_tick(
    shared: &Shared,
    pressed: &HashSet<Input>,
    state: &mut BackendState,
    now: Instant,
    cmd_tx: &UnboundedSender<Cmd>,
) -> bool {
    let is_recording = shared.recording.load(Ordering::Relaxed);

    if is_recording {
        // On the false→true edge, ignore whatever was already pressed
        // at the moment recording started (the Bind click's mouse-down,
        // a stale key, a held gamepad button). Only presses that
        // *begin* during recording count.
        if !state.was_recording {
            state.held.clear();
            shared.hold_progress.store(0u32, Ordering::Relaxed);
        }

        // Drop anything released (timer resets); start timers for the
        // newly held.
        state.held.retain(|input, _| pressed.contains(input));
        for input in pressed {
            state.held.entry(*input).or_insert(now);
        }

        // Longest-held input past the threshold commits. Picking the
        // longest-held (not first-seen) means a modifier grabbed first
        // wins over a letter pressed a moment later — matches muscle
        // memory.
        let committed = state
            .held
            .iter()
            .filter(|(_, &start)| now.duration_since(start) >= HOLD_DURATION)
            .max_by_key(|(_, &start)| now.duration_since(start))
            .map(|(input, _)| *input);

        // Progress for the UI: max across all currently-held inputs.
        let max_progress = state
            .held
            .values()
            .map(|&start| now.duration_since(start).as_secs_f32() / HOLD_DURATION.as_secs_f32())
            .fold(0.0_f32, f32::max)
            .clamp(0.0, 1.0);
        // Only advance the shared bar — don't stomp a higher value a
        // different backend may have written this tick (e.g. a longer
        // hold on another device). Last-writer races are harmless for a
        // progress bar, but max keeps the bar monotone in the common
        // single-device case.
        let prev = f32::from_bits(shared.hold_progress.load(Ordering::Relaxed));
        shared
            .hold_progress
            .store(max_progress.max(prev).to_bits(), Ordering::Relaxed);

        if let Some(input) = committed {
            *shared.recorded.lock().unwrap() = Some(input);
            shared.recording.store(false, Ordering::Relaxed);
            state.held.clear();
            shared.hold_progress.store(0u32, Ordering::Relaxed);
        }
        // While recording, do NOT fire PTT — the held press would
        // otherwise leak into the previous binding's transmit gate.
        state.was_recording = is_recording;
        return true;
    }

    // ── Normal mode ───────────────────────────────────────────────
    // Clean up on recording-exit so a future Bind starts fresh.
    if state.was_recording {
        state.held.clear();
        shared.hold_progress.store(0u32, Ordering::Relaxed);
    }

    // PTT: down/up edge. PTT engages while EITHER the primary or the
    // secondary binding is pressed — the secondary is a fallback, not a
    // chord. Each backend ORs over the slots it can see; a binding on a
    // device this backend doesn't observe simply never matches here, and
    // its own backend drives it. The runtime de-dupes the resulting
    // edges, so two backends both reporting "down" is harmless.
    let bound = *shared.current.lock().unwrap();
    let now_pressed = bound.iter().flatten().any(|input| pressed.contains(input));
    if now_pressed != state.ptt_down {
        state.ptt_down = now_pressed;
        let cmd = if now_pressed {
            Cmd::PttDown
        } else {
            Cmd::PttUp
        };
        if cmd_tx.send(cmd).is_err() {
            // Runtime gone — app shutting down.
            return false;
        }
    }

    // `just_exited` primes the prev-state arrays on the first frame
    // after a bind so the input the user was holding to bind doesn't
    // immediately fire its own action.
    let just_exited = state.was_recording;

    // Memory recalls: fire once on the down edge.
    let mem = *shared.memory.lock().unwrap();
    for (i, slot) in mem.iter().enumerate() {
        let is_pressed = slot.map(|inp| pressed.contains(&inp)).unwrap_or(false);
        if is_pressed && !state.mem_prev[i] && !just_exited {
            shared.recalls.lock().unwrap().push(i);
        }
        state.mem_prev[i] = is_pressed;
    }

    // Tune up/down: accumulate a net step delta (+up / -down).
    let fr = *shared.freq.lock().unwrap();
    for (i, (slot, step)) in fr.iter().zip([1i32, -1i32]).enumerate() {
        let is_pressed = slot.map(|inp| pressed.contains(&inp)).unwrap_or(false);
        if is_pressed && !state.freq_prev[i] && !just_exited {
            *shared.freq_delta.lock().unwrap() += step;
        }
        state.freq_prev[i] = is_pressed;
    }

    state.was_recording = is_recording;
    true
}

/// Active PTT state. Holds the [`Shared`] state every backend writes
/// into, plus whether at least one poller spawned.
pub struct InstalledHotkey {
    shared: Shared,
    /// `true` once at least the keyboard/mouse poller has spawned. If
    /// it never starts, global PTT is unavailable on this system.
    available: bool,
}

impl InstalledHotkey {
    /// Set the primary PTT binding.
    pub fn rebind(&mut self, input: Input) -> Result<(), &'static str> {
        if !self.available {
            return Err("input poller unavailable");
        }
        self.shared.current.lock().unwrap()[0] = Some(input);
        info!(?input, "primary PTT rebound");
        Ok(())
    }

    /// Set (or clear, with `None`) the secondary/fallback PTT binding.
    /// PTT then engages while either the primary or this input is held.
    pub fn rebind_secondary(&mut self, input: Option<Input>) -> Result<(), &'static str> {
        if !self.available {
            return Err("input poller unavailable");
        }
        self.shared.current.lock().unwrap()[1] = input;
        info!(?input, "secondary PTT rebound");
        Ok(())
    }

    /// Set (or clear, with `None`) the memory-recall binding for slot
    /// `i` (0..4). A no-op for out-of-range indices.
    pub fn rebind_memory(&mut self, i: usize, input: Option<Input>) {
        if i < 4 {
            self.shared.memory.lock().unwrap()[i] = input;
            info!(slot = i, ?input, "memory hotkey rebound");
        }
    }

    /// Drain any pending memory-recall events (slot indices) captured
    /// since the last call. The UI applies each as a preset switch.
    pub fn take_recalls(&self) -> Vec<usize> {
        std::mem::take(&mut *self.shared.recalls.lock().unwrap())
    }

    /// Set (or clear, with `None`) the tune-up (`up = true`) or
    /// tune-down binding.
    pub fn rebind_freq(&mut self, up: bool, input: Option<Input>) {
        self.shared.freq.lock().unwrap()[if up { 0 } else { 1 }] = input;
        info!(up, ?input, "freq hotkey rebound");
    }

    /// Drain the net pending tune steps (positive = up). The UI steps
    /// the channel index by this amount, with wraparound.
    pub fn take_freq_delta(&self) -> i32 {
        std::mem::take(&mut *self.shared.freq_delta.lock().unwrap())
    }

    /// Begin recording the next press. The recorded press is captured
    /// inside the polling thread so the user can release naturally
    /// without it being counted as a separate event.
    pub fn start_recording(&mut self) -> bool {
        if !self.available {
            return false;
        }
        self.shared.recording.store(true, Ordering::Relaxed);
        *self.shared.recorded.lock().unwrap() = None;
        true
    }

    pub fn cancel_recording(&self) {
        self.shared.recording.store(false, Ordering::Relaxed);
        *self.shared.recorded.lock().unwrap() = None;
        self.shared.hold_progress.store(0u32, Ordering::Relaxed);
    }

    /// `0.0..=1.0` — how far the longest-currently-held input is
    /// toward triggering a bind. UI uses this to render a progress
    /// bar that fills as the user holds.
    pub fn hold_progress(&self) -> f32 {
        f32::from_bits(self.shared.hold_progress.load(Ordering::Relaxed))
    }

    /// Consume a captured input, if any. Returns `Some` exactly once
    /// per recording session.
    pub fn take_recorded(&self) -> Option<Input> {
        let mut slot = self.shared.recorded.lock().unwrap();
        if slot.is_some() {
            self.shared.recording.store(false, Ordering::Relaxed);
        }
        slot.take()
    }

    /// `false` if the polling thread couldn't be spawned. UI uses this
    /// to grey out the bind affordance.
    #[allow(dead_code)]
    pub fn available(&self) -> bool {
        self.available
    }
}

/// Install PTT state and spawn the input backends. The keyboard/mouse
/// poller always spawns (device_query polling is benign — it just
/// queries OS state every 10 ms). The gamepad (gilrs) and HID (hidapi)
/// backends spawn additionally and feed the same shared state; if
/// neither has a device, they idle cheaply. `available` reflects only
/// the keyboard/mouse poller, since that's the baseline every system
/// is expected to have.
pub fn install(
    cmd_tx: UnboundedSender<Cmd>,
    initial: Option<Input>,
    initial_secondary: Option<Input>,
    initial_memory: [Option<Input>; 4],
    initial_freq: [Option<Input>; 2],
) -> InstalledHotkey {
    // Belt-and-braces: a hand-edited config could point at Space /
    // Enter / Escape even though the bind UI refuses to record them.
    // Drop the value silently — the user can rebind from the
    // Settings panel and the rest of the app behaves as "no PTT".
    let restriction_guard = |i: Input| {
        let ok = !is_restricted(i);
        if !ok {
            warn!(?i, "ignoring restricted binding from config");
        }
        ok
    };
    let initial = initial.filter(|i| restriction_guard(*i));
    let initial_secondary = initial_secondary.filter(|i| restriction_guard(*i));
    // Same restriction guard for the memory + freq hotkeys.
    let initial_memory = initial_memory.map(|i| i.filter(|i| !is_restricted(*i)));
    let initial_freq = initial_freq.map(|i| i.filter(|i| !is_restricted(*i)));

    let shared = Shared::new(initial, initial_secondary, initial_memory, initial_freq);

    let available = spawn_kbm_poller(shared.clone(), cmd_tx.clone());
    spawn_gamepad_poller(shared.clone(), cmd_tx.clone());
    spawn_hid_poller(shared.clone(), cmd_tx);

    InstalledHotkey { shared, available }
}

/// Spawn the keyboard + mouse poller. Returns whether the spawn itself
/// succeeded; if `device_query::DeviceState::new()` panics inside the
/// thread (rare, but seen on some Linux setups without X11) we catch
/// it and log — the thread exits cleanly and `available` stays true
/// at the API level, but no events will fire.
fn spawn_kbm_poller(shared: Shared, cmd_tx: UnboundedSender<Cmd>) -> bool {
    let build = thread::Builder::new().name("toki-input-kbm".into());
    let result = build.spawn(move || {
        // device_query's constructor can panic on some Linux configs
        // where it can't open an X11 display. Catch it so the panic
        // doesn't propagate up and abort the process.
        let ds = match std::panic::catch_unwind(DeviceState::new) {
            Ok(ds) => ds,
            Err(_) => {
                warn!("device_query failed to initialize — keyboard/mouse PTT disabled");
                return;
            }
        };
        run_kbm_loop(ds, shared, cmd_tx);
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

/// Keyboard + mouse poll loop. Builds the device-agnostic pressed-set
/// and delegates the bind/edge logic to [`process_tick`]. Escape-cancel
/// of the bind flow lives here because it is a keyboard concept and
/// needs the per-tick key *edge* against `prev_keys`.
fn run_kbm_loop(ds: DeviceState, shared: Shared, cmd_tx: UnboundedSender<Cmd>) {
    let mut prev_keys: HashSet<Keycode> = HashSet::new();
    let mut state = BackendState::default();

    loop {
        thread::sleep(POLL_INTERVAL);
        let now = Instant::now();
        let keys: HashSet<Keycode> = ds.get_keys().into_iter().collect();
        let mouse = ds.get_mouse();

        // Escape doubles as the always-on cancel for the bind flow.
        // Handled here (before process_tick) because it needs the key
        // down-edge. Other restricted keys (Space, Enter) are simply
        // excluded from the pressed-set below.
        if shared.recording.load(Ordering::Relaxed)
            && keys.difference(&prev_keys).any(|k| *k == Keycode::Escape)
        {
            shared.recording.store(false, Ordering::Relaxed);
            *shared.recorded.lock().unwrap() = None;
            shared.hold_progress.store(0u32, Ordering::Relaxed);
            state.held.clear();
            state.was_recording = false;
            prev_keys = keys;
            continue;
        }

        // Build the pressed-set (keyboard + mouse). Restricted keys are
        // excluded so we never commit or fire one.
        let mut pressed: HashSet<Input> = HashSet::new();
        for kc in &keys {
            if RESTRICTED_KEYS.contains(kc) {
                continue;
            }
            if let Some(code) = device_to_code(*kc) {
                pressed.insert(Input::Key(code));
            }
        }
        for (i, &is_pressed) in mouse.button_pressed.iter().enumerate() {
            if is_pressed {
                pressed.insert(Input::Mouse(MouseButton::from_index(i)));
            }
        }

        if !process_tick(&shared, &pressed, &mut state, now, &cmd_tx) {
            break; // runtime gone
        }
        prev_keys = keys;
    }
}

/// The `gilrs::Button` variants we surface as bindings, paired with our
/// own [`GamepadCode`]. Anything not in this list (e.g. `Button::C`,
/// `Button::Z`, `Button::Unknown`) simply isn't bindable.
const GAMEPAD_BUTTONS: &[(gilrs::Button, GamepadCode)] = &[
    (gilrs::Button::South, GamepadCode::South),
    (gilrs::Button::East, GamepadCode::East),
    (gilrs::Button::North, GamepadCode::North),
    (gilrs::Button::West, GamepadCode::West),
    (gilrs::Button::LeftTrigger, GamepadCode::LeftTrigger),
    (gilrs::Button::LeftTrigger2, GamepadCode::LeftTrigger2),
    (gilrs::Button::RightTrigger, GamepadCode::RightTrigger),
    (gilrs::Button::RightTrigger2, GamepadCode::RightTrigger2),
    (gilrs::Button::Select, GamepadCode::Select),
    (gilrs::Button::Start, GamepadCode::Start),
    (gilrs::Button::Mode, GamepadCode::Mode),
    (gilrs::Button::LeftThumb, GamepadCode::LeftThumb),
    (gilrs::Button::RightThumb, GamepadCode::RightThumb),
    (gilrs::Button::DPadUp, GamepadCode::DPadUp),
    (gilrs::Button::DPadDown, GamepadCode::DPadDown),
    (gilrs::Button::DPadLeft, GamepadCode::DPadLeft),
    (gilrs::Button::DPadRight, GamepadCode::DPadRight),
];

/// Spawn the gamepad / joystick poller. Best-effort: if gilrs can't
/// initialize (no backend, missing permissions) we log and skip it —
/// the keyboard/mouse poller still provides PTT, so this never affects
/// `available`. gilrs reads passively and non-exclusively, so a focused
/// game keeps receiving the same controller input.
fn spawn_gamepad_poller(shared: Shared, cmd_tx: UnboundedSender<Cmd>) {
    let build = thread::Builder::new().name("toki-input-gamepad".into());
    let spawn = build.spawn(move || {
        // gilrs init can fail or panic on systems without a gamepad
        // backend; treat any failure as "no gamepads" and exit quietly.
        let mut gilrs = match std::panic::catch_unwind(gilrs::Gilrs::new) {
            Ok(Ok(g)) => g,
            _ => {
                info!("gilrs unavailable — gamepad PTT disabled");
                return;
            }
        };
        info!("gamepad PTT poller started (gilrs, 10 ms cadence)");
        run_gamepad_loop(&mut gilrs, shared, cmd_tx);
    });
    if let Err(e) = spawn {
        warn!(error = %e, "could not spawn gamepad input thread");
    }
}

/// Gamepad poll loop. Drains gilrs events (to keep button state fresh),
/// builds the device-agnostic pressed-set, and delegates to
/// [`process_tick`]. The enumeration order of `gamepads()` is monotone
/// in `GamepadId`, so the iteration index is the stable "first-found =
/// index 0" identity used by [`GamepadButton::index`].
fn run_gamepad_loop(gilrs: &mut gilrs::Gilrs, shared: Shared, cmd_tx: UnboundedSender<Cmd>) {
    let mut state = BackendState::default();
    loop {
        thread::sleep(POLL_INTERVAL);
        // Pump pending events so `is_pressed` reflects current state.
        while let Some(event) = gilrs.next_event() {
            gilrs.update(&event);
        }

        let now = Instant::now();
        let mut pressed: HashSet<Input> = HashSet::new();
        for (index, (_id, pad)) in gilrs.gamepads().enumerate() {
            // Cap the index at u8 — no one has 256 pads, and a binding
            // beyond that simply wouldn't have been recordable.
            let Ok(index) = u8::try_from(index) else {
                continue;
            };
            for &(btn, code) in GAMEPAD_BUTTONS {
                if pad.is_pressed(btn) {
                    pressed.insert(Input::Gamepad(GamepadButton {
                        button: code,
                        index,
                    }));
                }
            }
        }

        if !process_tick(&shared, &pressed, &mut state, now, &cmd_tx) {
            break; // runtime gone
        }
    }
}

/// How often the HID thread re-enumerates devices to pick up hot-plug.
/// Coarser than the poll cadence — enumeration is comparatively
/// expensive and a freshly-plugged device appearing within a second is
/// imperceptible for a binding workflow.
const HID_RESCAN_INTERVAL: Duration = Duration::from_secs(1);

/// HID Usage Pages (USB HID Usage Tables §3). We only treat devices on
/// pages that carry **buttons/controls** as bindable; anything else
/// (sensors, vendor-defined telemetry, Apple-internal housekeeping) is
/// skipped so its idle report chatter can't be mistaken for a press.
const HID_USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x01;
const HID_USAGE_PAGE_BUTTON: u16 = 0x09;
const HID_USAGE_PAGE_CONSUMER: u16 = 0x0c;

/// Generic-Desktop usages we must NOT bind via the raw-HID bitmap path
/// because device_query already owns them: Pointer (0x01), Mouse (0x02),
/// Keyboard (0x06), Keypad (0x07).
///
/// Joystick (0x04) and Game Pad (0x05) are intentionally **kept** here:
/// gilrs is the preferred backend for them, but it doesn't map every
/// device (notably DirectInput flight sticks like the Thrustmaster
/// T.16000M), so the raw-HID path is their only route to being bound.
/// Their report contains analog axes, so the generic collector debounces
/// changed bits to reject axis jitter — see [`OpenHid::collect_generic`].
const HID_USAGE_POINTER: u16 = 0x01;
const HID_USAGE_MOUSE: u16 = 0x02;
const HID_USAGE_KEYBOARD: u16 = 0x06;
const HID_USAGE_KEYPAD: u16 = 0x07;

/// Apple's USB vendor id. Apple internal devices (keyboard, trackpad,
/// ambient-light / T2 housekeeping) enumerate as HID and emit reports
/// with bits set at rest — they are already handled by device_query and
/// must never be bound, or BIND captures a phantom press immediately.
const APPLE_VID: u16 = 0x05ac;

/// How many consecutive ticks (at [`POLL_INTERVAL`] = 10 ms) a
/// generic-HID bit must stay changed-from-baseline before it counts as
/// pressed. Analog axes (on joysticks like the Thrustmaster T.16000M)
/// toggle their low bits every tick as the value dithers, so the set of
/// changed bits never holds steady; a real button press holds it steady.
/// 4 ticks ≈ 40 ms — below human PTT-latency perception, but long enough
/// to reject axis flicker.
const HID_DEBOUNCE_TICKS: u8 = 4;

/// An opened HID device plus the bookkeeping needed to turn its raw
/// input reports into pressed [`Input`]s.
struct OpenHid {
    device: hidapi::HidDevice,
    vid: u16,
    pid: u16,
    /// Elgato Stream Decks are parsed as keyed devices; everything else
    /// is treated as a generic bitmap of digital inputs.
    is_streamdeck: bool,
    /// Last input report read this tick (Stream Deck state / generic
    /// bitmap source).
    prev_report: Vec<u8>,
    /// The device's **resting** report — the first stable report seen.
    /// A generic-HID bit only counts as pressed when it *differs* from
    /// this baseline, so a bit that's high at rest (common on composite
    /// keyboards and sensors) never registers as a press. `None` until
    /// the first report arrives.
    baseline: Option<Vec<u8>>,
    /// **Per-byte** debounce state for the generic-HID path: for each
    /// report byte, the changed-from-baseline mask seen last tick and how
    /// many consecutive ticks it has been identical. A byte's bits are
    /// reported only once *that byte* has held steady for
    /// [`HID_DEBOUNCE_TICKS`]. Per-byte (not whole-report) is essential:
    /// on a flight stick the axis bytes churn continuously, but a held
    /// button keeps *its own* byte steady, so it still binds.
    debounce_mask: Vec<u8>,
    debounce_ticks: Vec<u8>,
    /// Scratch read buffer, reused each tick.
    buf: Vec<u8>,
}

/// Byte offset at which a Stream Deck's per-key state begins in its
/// input report. The "Expanded" family (XL / MK.2 / Plus / Neo, and any
/// future Elgato PID we don't recognise) prefixes a 4-byte header
/// (`01 00 len_lo len_hi`); the Classic/Mini family starts the key
/// bytes at offset 1. We default unknown Elgato PIDs to the Expanded
/// layout — newer models all use it — and log nothing, since an
/// off-by-a-few key map still lets the user bind *a* key.
fn streamdeck_key_offset(pid: u16) -> usize {
    // Classic (0x0060), Mini (0x0063), Mini MK.2 (0x0090) use the
    // headerless layout; everything else uses the 4-byte header.
    match pid {
        0x0060 | 0x0063 | 0x0090 => 1,
        _ => 4,
    }
}

/// Spawn the Stream Deck + generic HID poller. Best-effort, like the
/// gamepad poller: a hidapi init failure logs and skips. Devices are
/// opened **non-exclusively** (the `macos-shared-device` Cargo feature
/// guarantees this on macOS), so vendor software and the OS keep
/// seeing the device.
fn spawn_hid_poller(shared: Shared, cmd_tx: UnboundedSender<Cmd>) {
    let build = thread::Builder::new().name("toki-input-hid".into());
    let spawn = build.spawn(move || {
        let mut api = match std::panic::catch_unwind(hidapi::HidApi::new) {
            Ok(Ok(api)) => api,
            _ => {
                info!("hidapi unavailable — Stream Deck / HID PTT disabled");
                return;
            }
        };
        info!("HID PTT poller started (hidapi, 10 ms cadence)");
        run_hid_loop(&mut api, shared, cmd_tx);
    });
    if let Err(e) = spawn {
        warn!(error = %e, "could not spawn HID input thread");
    }
}

/// `true` if a device with this VID / usage page / usage should be
/// handled by the generic-HID path. This path is the catch-all for
/// button-bearing devices the *other* backends can't drive, so it
/// excludes:
///   * the Elgato VID (Stream Decks get their own keyed path);
///   * Apple internal devices (keyboard/trackpad/sensors — device_query
///     owns those and they report bits set at rest);
///   * pointer / keyboard / keypad usages (device_query's).
///
/// Joysticks and gamepads on the Generic Desktop page are **kept** —
/// gilrs is preferred but doesn't map every stick, so this is their
/// fallback. Button boxes (Button page) and remotes / foot pedals
/// (Consumer page) are kept too. Axis noise from sticks is handled by
/// the debounce in [`OpenHid::collect_generic`], not by excluding it.
fn hid_is_bindable_generic(vid: u16, usage_page: u16, usage: u16) -> bool {
    if vid == ELGATO_VID || vid == APPLE_VID {
        return false;
    }
    match usage_page {
        HID_USAGE_PAGE_GENERIC_DESKTOP => !matches!(
            usage,
            HID_USAGE_POINTER | HID_USAGE_MOUSE | HID_USAGE_KEYBOARD | HID_USAGE_KEYPAD
        ),
        HID_USAGE_PAGE_BUTTON | HID_USAGE_PAGE_CONSUMER => true,
        // Sensor pages, vendor-defined telemetry, LED pages, etc. — not
        // button inputs, so never bindable.
        _ => false,
    }
}

/// HID poll loop. Periodically (re)enumerates to track hot-plug, reads
/// each open device non-blocking, turns reports into pressed
/// [`Input`]s, and delegates to [`process_tick`].
fn run_hid_loop(api: &mut hidapi::HidApi, shared: Shared, cmd_tx: UnboundedSender<Cmd>) {
    let mut state = BackendState::default();
    let mut open: Vec<OpenHid> = Vec::new();
    let mut last_scan: Option<Instant> = None;

    loop {
        thread::sleep(POLL_INTERVAL);
        let now = Instant::now();

        // (Re)enumerate on first run and every HID_RESCAN_INTERVAL. We
        // rebuild the open set wholesale — simpler than diffing, and a
        // device that's still present just gets reopened. The previous
        // report state is carried over for devices that survive.
        if last_scan.is_none_or(|t| now.duration_since(t) >= HID_RESCAN_INTERVAL) {
            last_scan = Some(now);
            open = reopen_hid_devices(api, open);
        }

        let mut pressed: HashSet<Input> = HashSet::new();
        for dev in &mut open {
            dev.collect_pressed(&mut pressed);
        }

        if !process_tick(&shared, &pressed, &mut state, now, &cmd_tx) {
            break; // runtime gone
        }
    }
}

/// Prior per-device HID state carried across a rescan, keyed by
/// `(vid, pid)`: the last report seen and the resting baseline.
type HidPriorState = HashMap<(u16, u16), (Vec<u8>, Option<Vec<u8>>)>;

/// Refresh the device list and (re)open everything we care about,
/// preserving prior report state for devices that are still present so
/// generic-HID edge detection doesn't glitch across a rescan.
fn reopen_hid_devices(api: &mut hidapi::HidApi, prev: Vec<OpenHid>) -> Vec<OpenHid> {
    if let Err(e) = api.refresh_devices() {
        warn!(error = %e, "hidapi refresh failed");
        return prev;
    }

    // Index prior state by (vid, pid) so a surviving device keeps both
    // its last report and its resting baseline across a rescan.
    let mut prior: HidPriorState = HashMap::new();
    for d in prev {
        prior.insert((d.vid, d.pid), (d.prev_report, d.baseline));
    }

    let mut out: Vec<OpenHid> = Vec::new();
    let mut seen: HashSet<(u16, u16)> = HashSet::new();
    for info in api.device_list() {
        let vid = info.vendor_id();
        let pid = info.product_id();
        let is_streamdeck = vid == ELGATO_VID;
        if !is_streamdeck && !hid_is_bindable_generic(vid, info.usage_page(), info.usage()) {
            continue;
        }
        // One handle per (vid, pid) — multiple identical devices match
        // first-found, the documented behavior.
        if !seen.insert((vid, pid)) {
            continue;
        }
        let device = match info.open_device(api) {
            Ok(d) => d,
            Err(e) => {
                // Common on macOS without Input Monitoring; log once per
                // rescan at debug level so it isn't noisy.
                tracing::debug!(vid, pid, error = %e, "could not open HID device");
                continue;
            }
        };
        // Non-blocking reads so the poll tick never stalls waiting on a
        // quiet device.
        let _ = device.set_blocking_mode(false);
        // A device that survived the rescan keeps its baseline (and last
        // report) so its resting state isn't recaptured mid-press. The
        // debounce state is transient (re-stabilizes in a few ms), so we
        // don't bother carrying it across a rescan.
        let (prev_report, baseline) = prior.remove(&(vid, pid)).unwrap_or_default();
        out.push(OpenHid {
            device,
            vid,
            pid,
            is_streamdeck,
            prev_report,
            baseline,
            debounce_mask: Vec::new(),
            debounce_ticks: Vec::new(),
            buf: vec![0u8; 64],
        });
    }
    out
}

impl OpenHid {
    /// Read whatever reports are queued and add any currently-pressed
    /// inputs to `pressed`. Drains all pending reports each tick so a
    /// burst can't leave us a frame behind.
    fn collect_pressed(&mut self, pressed: &mut HashSet<Input>) {
        // Drain the device's report queue; keep the last full report.
        loop {
            match self.device.read_timeout(&mut self.buf, 0) {
                Ok(0) => break, // nothing queued
                Ok(n) => {
                    self.prev_report = self.buf[..n].to_vec();
                    // Latch the resting baseline from the very first
                    // report. The user can't have a generic button held
                    // the instant we open the device, so this captures
                    // the at-rest bit pattern — and any bit that's high
                    // here is treated as "not pressed" forever after.
                    if self.baseline.is_none() {
                        self.baseline = Some(self.prev_report.clone());
                    }
                }
                Err(_) => break, // device went away; rescan will drop it
            }
        }
        // Even if no new report arrived this tick, fall through using
        // the last known report so a held button keeps PTT engaged.

        if self.is_streamdeck {
            self.collect_streamdeck(pressed);
        } else {
            self.collect_generic(pressed);
        }
    }

    /// Stream Deck: each key is one byte (`0x00`/`0x01`) starting at a
    /// model-dependent offset. Key state is unambiguous (no rest-high
    /// bits), so no baseline is needed here.
    fn collect_streamdeck(&self, pressed: &mut HashSet<Input>) {
        let off = streamdeck_key_offset(self.pid);
        if self.prev_report.len() <= off {
            return;
        }
        for (i, &b) in self.prev_report[off..].iter().enumerate() {
            if b != 0 {
                let Ok(key) = u8::try_from(i) else { continue };
                pressed.insert(Input::StreamDeck { pid: self.pid, key });
            }
        }
    }

    /// Generic HID: report the bits that have changed from rest and held
    /// steady long enough to be a button rather than axis jitter. Debounce
    /// runs **per byte** so a held button (steady in its own byte) still
    /// registers while other bytes (axes) churn. Updates this device's
    /// debounce state, then emits the stably-changed bits.
    fn collect_generic(&mut self, pressed: &mut HashSet<Input>) {
        // No resting state yet → report nothing (better a one-tick delay
        // than a phantom capture).
        let Some(baseline) = self.baseline.as_deref() else {
            return;
        };
        let mask = changed_mask(&self.prev_report, baseline);

        // Resize the per-byte debounce state to match the report.
        self.debounce_mask.resize(mask.len(), 0);
        self.debounce_ticks.resize(mask.len(), 0);

        for (i, &m) in mask.iter().enumerate() {
            let stable = debounce_byte(m, &mut self.debounce_mask[i], &mut self.debounce_ticks[i]);
            if !stable || m == 0 {
                continue;
            }
            let Ok(byte) = u8::try_from(i) else { continue };
            emit_changed_bits(self.vid, self.pid, byte, m, pressed);
        }
    }
}

/// Advance one byte's debounce counter. `m` is this tick's changed-mask
/// for the byte; `prev`/`ticks` are its carried state. Returns `true`
/// once the same non-trivial mask has held for [`HID_DEBOUNCE_TICKS`]
/// consecutive ticks — i.e. the byte is steady, not dithering.
fn debounce_byte(m: u8, prev: &mut u8, ticks: &mut u8) -> bool {
    if m == *prev {
        *ticks = ticks.saturating_add(1);
    } else {
        *prev = m;
        *ticks = 1;
    }
    *ticks >= HID_DEBOUNCE_TICKS
}

/// Per-byte XOR of `report` against `baseline` — the set of bits that
/// differ from the device's resting state. A press flips a bit (changed
/// = 1); a steady rest-high bit XORs to 0. Bytes with more than two
/// changed bits are zeroed: that many simultaneous flips is almost
/// certainly an analog value, not buttons.
fn changed_mask(report: &[u8], baseline: &[u8]) -> Vec<u8> {
    report
        .iter()
        .enumerate()
        .map(|(i, &b)| {
            let rest = baseline.get(i).copied().unwrap_or(0);
            let changed = b ^ rest;
            if changed.count_ones() > 2 {
                0
            } else {
                changed
            }
        })
        .collect()
}

/// Insert an [`Input::Hid`] for every set bit of `m` in report `byte`.
fn emit_changed_bits(vid: u16, pid: u16, byte: u8, m: u8, pressed: &mut HashSet<Input>) {
    for bit in 0..8u8 {
        if m & (1 << bit) != 0 {
            pressed.insert(Input::Hid(HidButton {
                vid,
                pid,
                byte,
                bit,
            }));
        }
    }
}

/// Map device_query's `Keycode` to our serialization-friendly
/// `keyboard_types::Code`. Returns `None` for keys we haven't
/// catalogued — those simply can't be bound (the listener silently
/// ignores them in both recording and matching modes).
fn device_to_code(k: Keycode) -> Option<Code> {
    use Keycode as K;
    Some(match k {
        K::A => Code::KeyA,
        K::B => Code::KeyB,
        K::C => Code::KeyC,
        K::D => Code::KeyD,
        K::E => Code::KeyE,
        K::F => Code::KeyF,
        K::G => Code::KeyG,
        K::H => Code::KeyH,
        K::I => Code::KeyI,
        K::J => Code::KeyJ,
        K::K => Code::KeyK,
        K::L => Code::KeyL,
        K::M => Code::KeyM,
        K::N => Code::KeyN,
        K::O => Code::KeyO,
        K::P => Code::KeyP,
        K::Q => Code::KeyQ,
        K::R => Code::KeyR,
        K::S => Code::KeyS,
        K::T => Code::KeyT,
        K::U => Code::KeyU,
        K::V => Code::KeyV,
        K::W => Code::KeyW,
        K::X => Code::KeyX,
        K::Y => Code::KeyY,
        K::Z => Code::KeyZ,
        K::Key0 => Code::Digit0,
        K::Key1 => Code::Digit1,
        K::Key2 => Code::Digit2,
        K::Key3 => Code::Digit3,
        K::Key4 => Code::Digit4,
        K::Key5 => Code::Digit5,
        K::Key6 => Code::Digit6,
        K::Key7 => Code::Digit7,
        K::Key8 => Code::Digit8,
        K::Key9 => Code::Digit9,
        K::F1 => Code::F1,
        K::F2 => Code::F2,
        K::F3 => Code::F3,
        K::F4 => Code::F4,
        K::F5 => Code::F5,
        K::F6 => Code::F6,
        K::F7 => Code::F7,
        K::F8 => Code::F8,
        K::F9 => Code::F9,
        K::F10 => Code::F10,
        K::F11 => Code::F11,
        K::F12 => Code::F12,
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
/// and the PTT button's hint text. Works from the stored [`Input`]
/// alone, so a binding to a currently-disconnected device still
/// renders its label.
pub fn format(input: Input) -> String {
    match input {
        Input::Key(c) => format_code(c),
        Input::Mouse(b) => format_mouse(b),
        Input::Gamepad(g) => {
            let base = format!("Pad {}", g.button.short_label());
            if g.index > 0 {
                format!("{base} #{}", g.index + 1)
            } else {
                base
            }
        }
        // Display the key 1-based — matches how Stream Deck keys are
        // numbered on the physical device and in Elgato's software.
        Input::StreamDeck { key, .. } => format!("StreamDeck K{}", key as u16 + 1),
        Input::Hid(h) => format!("HID {:04x}:{:04x} b{}.{}", h.vid, h.pid, h.byte, h.bit),
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
        "AltLeft" => {
            if cfg!(target_os = "macos") {
                "LOpt".into()
            } else {
                "LAlt".into()
            }
        }
        "AltRight" => {
            if cfg!(target_os = "macos") {
                "ROpt".into()
            } else {
                "RAlt".into()
            }
        }
        "MetaLeft" => {
            if cfg!(target_os = "macos") {
                "LCmd".into()
            } else {
                "LSuper".into()
            }
        }
        "MetaRight" => {
            if cfg!(target_os = "macos") {
                "RCmd".into()
            } else {
                "RSuper".into()
            }
        }
        _ => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_button_indices_round_trip() {
        for b in [
            MouseButton::Left,
            MouseButton::Right,
            MouseButton::Middle,
            MouseButton::Other(4),
        ] {
            let round = MouseButton::from_index(b.to_index());
            assert_eq!(b, round);
        }
    }

    #[test]
    fn mouse_button_labels_round_trip() {
        for label in ["Left", "Right", "Middle", "Mouse4", "Mouse7"] {
            let parsed = MouseButton::from_label(label).expect("label should parse");
            assert_eq!(parsed.label(), label);
        }
    }

    #[test]
    fn mouse_button_label_rejects_garbage() {
        assert!(MouseButton::from_label("").is_none());
        assert!(MouseButton::from_label("MouseLeft").is_none());
        assert!(MouseButton::from_label("Mouse-abc").is_none());
    }

    #[test]
    fn restricted_keys_are_rejected() {
        // Space / Enter / Escape are explicitly blacklisted — the
        // binding flow drops them silently rather than capturing
        // them as PTT, since they collide with "activate focused
        // control" semantics in every dialog and form.
        assert!(is_restricted(Input::Key(Code::Space)));
        assert!(is_restricted(Input::Key(Code::Enter)));
        assert!(is_restricted(Input::Key(Code::Escape)));
    }

    #[test]
    fn ordinary_keys_pass_restriction_check() {
        assert!(!is_restricted(Input::Key(Code::Backquote)));
        assert!(!is_restricted(Input::Key(Code::KeyA)));
        assert!(!is_restricted(Input::Key(Code::F8)));
    }

    #[test]
    fn mouse_inputs_are_never_restricted() {
        for b in [
            MouseButton::Left,
            MouseButton::Right,
            MouseButton::Middle,
            MouseButton::Other(5),
        ] {
            assert!(!is_restricted(Input::Mouse(b)));
        }
    }

    /// All `GamepadCode` variants, at index 0 and a non-zero index,
    /// survive a `to_token` → `from_token` round-trip.
    #[test]
    fn gamepad_token_round_trips() {
        let codes = [
            GamepadCode::South,
            GamepadCode::East,
            GamepadCode::North,
            GamepadCode::West,
            GamepadCode::LeftTrigger,
            GamepadCode::LeftTrigger2,
            GamepadCode::RightTrigger,
            GamepadCode::RightTrigger2,
            GamepadCode::Select,
            GamepadCode::Start,
            GamepadCode::Mode,
            GamepadCode::LeftThumb,
            GamepadCode::RightThumb,
            GamepadCode::DPadUp,
            GamepadCode::DPadDown,
            GamepadCode::DPadLeft,
            GamepadCode::DPadRight,
        ];
        for code in codes {
            for index in [0u8, 2u8] {
                let input = Input::Gamepad(GamepadButton {
                    button: code,
                    index,
                });
                let token = input.to_token();
                assert_eq!(Input::from_token(&token), Some(input), "token={token}");
            }
        }
    }

    #[test]
    fn streamdeck_token_round_trips() {
        let input = Input::StreamDeck {
            pid: 0x0080,
            key: 3,
        };
        let token = input.to_token();
        assert_eq!(token, "streamdeck:0x0fd9:0x0080:3");
        assert_eq!(Input::from_token(&token), Some(input));
    }

    #[test]
    fn hid_token_round_trips() {
        let input = Input::Hid(HidButton {
            vid: 0x046d,
            pid: 0xc52b,
            byte: 2,
            bit: 4,
        });
        let token = input.to_token();
        assert_eq!(token, "hid:0x046d:0xc52b:2:4");
        assert_eq!(Input::from_token(&token), Some(input));
    }

    #[test]
    fn key_and_mouse_tokens_round_trip() {
        for input in [
            Input::Key(Code::Backquote),
            Input::Key(Code::F8),
            Input::Mouse(MouseButton::Middle),
            Input::Mouse(MouseButton::Other(4)),
        ] {
            let token = input.to_token();
            assert_eq!(Input::from_token(&token), Some(input), "token={token}");
        }
    }

    #[test]
    fn from_token_rejects_garbage() {
        for s in [
            "",
            "key",                              // no value
            "bogus:South",                      // unknown tag
            "gamepad:0",                        // missing button
            "gamepad:South",                    // missing index
            "gamepad:x:South",                  // non-numeric index
            "gamepad:0:Nonsense",               // unknown button
            "streamdeck:0x0fd9:0x0080",         // missing key
            "streamdeck:0x0fd9:0x0080:3:extra", // trailing garbage
            "hid:0x046d:0xc52b:2",              // missing bit
            "hid:zz:0xc52b:2:4",                // non-hex vid
            "hid:0x046d:0xc52b:2:4:5",          // trailing garbage
        ] {
            assert_eq!(Input::from_token(s), None, "should reject {s:?}");
        }
    }

    /// `format()` must return a non-empty label for every variant — a
    /// guard against the match silently regressing.
    #[test]
    fn format_covers_all_variants() {
        let inputs = [
            Input::Key(Code::KeyA),
            Input::Mouse(MouseButton::Left),
            Input::Gamepad(GamepadButton {
                button: GamepadCode::South,
                index: 0,
            }),
            Input::Gamepad(GamepadButton {
                button: GamepadCode::RightTrigger2,
                index: 1,
            }),
            Input::StreamDeck {
                pid: 0x0080,
                key: 0,
            },
            Input::Hid(HidButton {
                vid: 0x046d,
                pid: 0xc52b,
                byte: 0,
                bit: 0,
            }),
        ];
        for input in inputs {
            assert!(!format(input).is_empty(), "empty label for {input:?}");
        }
    }

    #[test]
    fn new_device_inputs_are_never_restricted() {
        assert!(!is_restricted(Input::Gamepad(GamepadButton {
            button: GamepadCode::South,
            index: 0,
        })));
        assert!(!is_restricted(Input::StreamDeck {
            pid: 0x0080,
            key: 0,
        }));
        assert!(!is_restricted(Input::Hid(HidButton {
            vid: 0x046d,
            pid: 0xc52b,
            byte: 0,
            bit: 0,
        })));
    }

    #[test]
    fn streamdeck_offset_branches_on_model() {
        // Classic / Mini family is headerless (offset 1).
        assert_eq!(streamdeck_key_offset(0x0060), 1);
        assert_eq!(streamdeck_key_offset(0x0063), 1);
        assert_eq!(streamdeck_key_offset(0x0090), 1);
        // Everything else (XL, MK.2, Plus, unknown future PIDs) uses
        // the 4-byte header layout.
        assert_eq!(streamdeck_key_offset(0x0080), 4);
        assert_eq!(streamdeck_key_offset(0xFFFF), 4);
    }

    #[test]
    fn hid_generic_filter_excludes_kbm_elgato_apple_and_sensors() {
        // Stream Decks are handled on their own path, never as generic.
        assert!(!hid_is_bindable_generic(
            ELGATO_VID,
            HID_USAGE_PAGE_CONSUMER,
            0x0001
        ));
        // Apple internal devices (the 0x05ac:0x8104 from the bug report
        // and its siblings) must never be bindable — they report bits
        // high at rest and are already device_query's.
        assert!(!hid_is_bindable_generic(
            APPLE_VID,
            HID_USAGE_PAGE_GENERIC_DESKTOP,
            0x0006
        ));
        assert!(!hid_is_bindable_generic(APPLE_VID, 0xff00, 0x0001));
        // Keyboard / mouse usages on the generic-desktop page belong to
        // the device_query backend.
        assert!(!hid_is_bindable_generic(
            0x046d,
            HID_USAGE_PAGE_GENERIC_DESKTOP,
            HID_USAGE_MOUSE
        ));
        assert!(!hid_is_bindable_generic(
            0x046d,
            HID_USAGE_PAGE_GENERIC_DESKTOP,
            HID_USAGE_KEYBOARD
        ));
        // Vendor-defined / sensor pages are not button inputs.
        assert!(!hid_is_bindable_generic(0x046d, 0xff00, 0x0001));
        assert!(!hid_is_bindable_generic(0x046d, 0x0020, 0x0001)); // Sensors
                                                                   // Joysticks / gamepads on the generic-desktop page ARE bindable
                                                                   // here — gilrs is preferred but doesn't map every stick (e.g. the
                                                                   // Thrustmaster T.16000M, 0x044f:0xb10a, usage 0x04), so raw HID is
                                                                   // their fallback. Axis noise is handled by debounce, not exclusion.
        assert!(hid_is_bindable_generic(
            0x044f,
            HID_USAGE_PAGE_GENERIC_DESKTOP,
            0x0004
        ));
        assert!(hid_is_bindable_generic(
            0x045e,
            HID_USAGE_PAGE_GENERIC_DESKTOP,
            0x0005
        ));
        // Bindable: a button-box on the Button page and a foot pedal /
        // remote on the Consumer page.
        assert!(hid_is_bindable_generic(
            0x1234,
            HID_USAGE_PAGE_BUTTON,
            0x0001
        ));
        assert!(hid_is_bindable_generic(
            0x046d,
            HID_USAGE_PAGE_CONSUMER,
            0x0001
        ));
    }

    #[test]
    fn changed_mask_ignores_rest_high_bits_and_dense_bytes() {
        // Rest state: byte 2 bit 0 high. A report identical to rest has
        // an all-zero changed mask (the rest-high bit XORs out).
        let rest = [0, 0, 0b0000_0001, 0];
        assert_eq!(changed_mask(&rest, &rest), vec![0, 0, 0, 0]);

        // A press flips byte 1 bit 3; byte 2 bit 0 stays high (ignored).
        let report = [0, 0b0000_1000, 0b0000_0001, 0];
        assert_eq!(changed_mask(&report, &rest), vec![0, 0b0000_1000, 0, 0]);

        // A byte with >2 changed bits is treated as an analog value and
        // zeroed, never a chord of buttons.
        let axis = [0b1011_0100, 0, 0, 0];
        assert_eq!(changed_mask(&axis, &[0, 0, 0, 0]), vec![0, 0, 0, 0]);
    }

    #[test]
    fn emit_changed_bits_maps_set_bits_to_hid_inputs() {
        let mut pressed = HashSet::new();
        emit_changed_bits(0x044f, 0xb10a, 1, 0b0000_1000, &mut pressed);
        assert_eq!(
            pressed,
            HashSet::from([Input::Hid(HidButton {
                vid: 0x044f,
                pid: 0xb10a,
                byte: 1,
                bit: 3,
            })])
        );
    }

    /// Run a sequence of per-byte changed-masks through `debounce_byte`
    /// and return whether the final tick reports a steady, non-zero mask.
    fn debounce_reports(masks: &[u8]) -> bool {
        let mut prev = u8::MAX; // sentinel: nothing seen yet
        let mut ticks = 0u8;
        let mut last_stable = false;
        for &m in masks {
            last_stable = debounce_byte(m, &mut prev, &mut ticks) && m != 0;
        }
        last_stable
    }

    #[test]
    fn debounce_rejects_axis_jitter_accepts_steady_hold() {
        // Axis dither: the changed bit toggles every tick → never steady.
        let jitter = [0b1, 0, 0b1, 0, 0b1, 0, 0b1, 0];
        assert!(!debounce_reports(&jitter));

        // A held button: the same bit stays changed for ≥ N ticks.
        let hold = [0b10; HID_DEBOUNCE_TICKS as usize];
        assert!(debounce_reports(&hold));

        // Held but not yet long enough → not reported.
        let brief = [0b10; (HID_DEBOUNCE_TICKS - 1) as usize];
        assert!(!debounce_reports(&brief));
    }

    #[test]
    fn debounce_is_per_byte_so_a_held_button_survives_axis_churn() {
        // Simulate two bytes: byte 0 = a held button (steady 0b10),
        // byte 1 = an axis dithering every tick. The button byte must
        // become stable even though the axis byte never does.
        let mut btn_prev = u8::MAX;
        let mut btn_ticks = 0u8;
        let mut axis_prev = u8::MAX;
        let mut axis_ticks = 0u8;
        let mut button_reported = false;
        let mut axis_reported = false;
        for tick in 0..HID_DEBOUNCE_TICKS {
            let btn_stable = debounce_byte(0b10, &mut btn_prev, &mut btn_ticks);
            let axis_mask = if tick % 2 == 0 { 0b1 } else { 0b0 };
            let axis_stable = debounce_byte(axis_mask, &mut axis_prev, &mut axis_ticks);
            button_reported |= btn_stable;
            axis_reported |= axis_stable && axis_mask != 0;
        }
        assert!(button_reported, "held button should stabilize");
        assert!(!axis_reported, "dithering axis should never report");
    }

    // ── process_tick: edge detection ───────────────────────────────

    /// Build a `Shared` seeded with a primary PTT binding (and optional
    /// memory / freq bindings), for driving `process_tick` in tests.
    fn shared_with(
        ptt: Option<Input>,
        memory: [Option<Input>; 4],
        freq: [Option<Input>; 2],
    ) -> Shared {
        Shared::new(ptt, None, memory, freq)
    }

    /// Collect every `Cmd` available on the receiver right now.
    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Cmd>) -> Vec<Cmd> {
        let mut out = Vec::new();
        while let Ok(c) = rx.try_recv() {
            out.push(c);
        }
        out
    }

    fn pressed(inputs: &[Input]) -> HashSet<Input> {
        inputs.iter().copied().collect()
    }

    #[test]
    fn ptt_fires_on_down_and_up_edges_only() {
        let bind = Input::Gamepad(GamepadButton {
            button: GamepadCode::South,
            index: 0,
        });
        let shared = shared_with(Some(bind), Default::default(), Default::default());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = BackendState::default();
        let t0 = Instant::now();

        // Not pressed → no events.
        process_tick(&shared, &pressed(&[]), &mut state, t0, &tx);
        assert!(drain(&mut rx).is_empty());

        // Down edge → exactly one PttDown.
        process_tick(&shared, &pressed(&[bind]), &mut state, t0, &tx);
        assert!(matches!(drain(&mut rx).as_slice(), [Cmd::PttDown]));

        // Still held → no further events.
        process_tick(&shared, &pressed(&[bind]), &mut state, t0, &tx);
        assert!(drain(&mut rx).is_empty());

        // Up edge → exactly one PttUp.
        process_tick(&shared, &pressed(&[]), &mut state, t0, &tx);
        assert!(matches!(drain(&mut rx).as_slice(), [Cmd::PttUp]));
    }

    #[test]
    fn memory_recall_fires_once_per_press_edge() {
        let m0 = Input::StreamDeck {
            pid: 0x0080,
            key: 0,
        };
        let shared = shared_with(None, [Some(m0), None, None, None], Default::default());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = BackendState::default();
        let t0 = Instant::now();

        process_tick(&shared, &pressed(&[m0]), &mut state, t0, &tx); // down edge
        process_tick(&shared, &pressed(&[m0]), &mut state, t0, &tx); // held
        assert_eq!(shared.recalls.lock().unwrap().as_slice(), &[0]);

        process_tick(&shared, &pressed(&[]), &mut state, t0, &tx); // release
        process_tick(&shared, &pressed(&[m0]), &mut state, t0, &tx); // down again
        assert_eq!(shared.recalls.lock().unwrap().as_slice(), &[0, 0]);
    }

    #[test]
    fn freq_up_down_accumulate_net_delta() {
        let up = Input::Hid(HidButton {
            vid: 1,
            pid: 2,
            byte: 0,
            bit: 0,
        });
        let down = Input::Hid(HidButton {
            vid: 1,
            pid: 2,
            byte: 0,
            bit: 1,
        });
        let shared = shared_with(None, Default::default(), [Some(up), Some(down)]);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = BackendState::default();
        let t0 = Instant::now();

        // Two distinct up presses (with releases) → +2.
        process_tick(&shared, &pressed(&[up]), &mut state, t0, &tx);
        process_tick(&shared, &pressed(&[]), &mut state, t0, &tx);
        process_tick(&shared, &pressed(&[up]), &mut state, t0, &tx);
        process_tick(&shared, &pressed(&[]), &mut state, t0, &tx);
        // One down press → −1. Net +1.
        process_tick(&shared, &pressed(&[down]), &mut state, t0, &tx);
        assert_eq!(*shared.freq_delta.lock().unwrap(), 1);
    }

    // ── process_tick: hold-to-bind ─────────────────────────────────

    #[test]
    fn hold_commits_after_threshold() {
        let bind = Input::Gamepad(GamepadButton {
            button: GamepadCode::RightTrigger2,
            index: 0,
        });
        let shared = shared_with(None, Default::default(), Default::default());
        shared.recording.store(true, Ordering::Relaxed);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = BackendState::default();
        let t0 = Instant::now();

        // Press starts the hold timer; nothing committed yet.
        process_tick(&shared, &pressed(&[bind]), &mut state, t0, &tx);
        assert!(shared.recorded.lock().unwrap().is_none());

        // Just under the threshold → still nothing.
        let almost = t0 + HOLD_DURATION - Duration::from_millis(1);
        process_tick(&shared, &pressed(&[bind]), &mut state, almost, &tx);
        assert!(shared.recorded.lock().unwrap().is_none());

        // At/after the threshold → committed, and recording ends.
        let after = t0 + HOLD_DURATION;
        process_tick(&shared, &pressed(&[bind]), &mut state, after, &tx);
        assert_eq!(*shared.recorded.lock().unwrap(), Some(bind));
        assert!(!shared.recording.load(Ordering::Relaxed));
    }

    #[test]
    fn brief_tap_does_not_commit() {
        let bind = Input::Mouse(MouseButton::Left);
        let shared = shared_with(None, Default::default(), Default::default());
        shared.recording.store(true, Ordering::Relaxed);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = BackendState::default();
        let t0 = Instant::now();

        // Press for ~100 ms then release — a Cancel click, not a bind.
        process_tick(&shared, &pressed(&[bind]), &mut state, t0, &tx);
        let t1 = t0 + Duration::from_millis(100);
        process_tick(&shared, &pressed(&[]), &mut state, t1, &tx);
        let t2 = t0 + HOLD_DURATION + Duration::from_millis(10);
        process_tick(&shared, &pressed(&[]), &mut state, t2, &tx);
        assert!(shared.recorded.lock().unwrap().is_none());
        assert!(shared.recording.load(Ordering::Relaxed)); // still recording
    }

    #[test]
    fn longest_held_input_wins_the_bind() {
        let early = Input::Gamepad(GamepadButton {
            button: GamepadCode::LeftTrigger,
            index: 0,
        });
        let late = Input::Gamepad(GamepadButton {
            button: GamepadCode::South,
            index: 0,
        });
        let shared = shared_with(None, Default::default(), Default::default());
        shared.recording.store(true, Ordering::Relaxed);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = BackendState::default();
        let t0 = Instant::now();

        // `early` pressed first.
        process_tick(&shared, &pressed(&[early]), &mut state, t0, &tx);
        // `late` joins a bit later; both held.
        let t1 = t0 + Duration::from_millis(200);
        process_tick(&shared, &pressed(&[early, late]), &mut state, t1, &tx);
        // Past the threshold relative to `early` — it has been held
        // longest, so it wins.
        let after = t0 + HOLD_DURATION;
        process_tick(&shared, &pressed(&[early, late]), &mut state, after, &tx);
        assert_eq!(*shared.recorded.lock().unwrap(), Some(early));
    }

    #[test]
    fn no_ptt_fires_while_recording() {
        let bind = Input::Mouse(MouseButton::Left);
        let shared = shared_with(Some(bind), Default::default(), Default::default());
        shared.recording.store(true, Ordering::Relaxed);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = BackendState::default();
        let t0 = Instant::now();
        // Even though `bind` is the active PTT, pressing it while
        // recording must not emit PttDown.
        process_tick(&shared, &pressed(&[bind]), &mut state, t0, &tx);
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn secondary_binding_acts_as_a_ptt_fallback() {
        let primary = Input::Gamepad(GamepadButton {
            button: GamepadCode::RightTrigger2,
            index: 0,
        });
        let secondary = Input::Key(Code::Backquote);
        // Within a single backend the OR is exercised directly (e.g. the
        // user binds both to keyboard-visible inputs); cross-device, each
        // backend ORs over what it can see and the runtime de-dupes.
        let shared = Shared::new(
            Some(primary),
            Some(secondary),
            Default::default(),
            Default::default(),
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = BackendState::default();
        let t0 = Instant::now();

        // Secondary alone engages PTT.
        process_tick(&shared, &pressed(&[secondary]), &mut state, t0, &tx);
        assert!(matches!(drain(&mut rx).as_slice(), [Cmd::PttDown]));

        // Primary also pressed → no new edge (already down).
        process_tick(
            &shared,
            &pressed(&[secondary, primary]),
            &mut state,
            t0,
            &tx,
        );
        assert!(drain(&mut rx).is_empty());

        // Release secondary while primary still held → stays down.
        process_tick(&shared, &pressed(&[primary]), &mut state, t0, &tx);
        assert!(drain(&mut rx).is_empty());

        // Release the last one → single PttUp.
        process_tick(&shared, &pressed(&[]), &mut state, t0, &tx);
        assert!(matches!(drain(&mut rx).as_slice(), [Cmd::PttUp]));
    }
}
