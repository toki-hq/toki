//! Persisted user preferences. Stored as TOML at the platform's standard
//! config location (`~/Library/Application Support/toki/config.toml` on
//! macOS, `~/.config/toki/config.toml` on Linux, `%APPDATA%\toki\config.toml`
//! on Windows). All loads and saves are best-effort: failures fall back to
//! defaults and log a warning, never panic.

use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

use global_hotkey::hotkey::Code;
use serde::{Deserialize, Serialize};

const FILENAME: &str = "config.toml";

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Config {
    #[serde(default)]
    pub hotkey: HotkeyConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub connection: ConnectionConfig,
    #[serde(default)]
    pub beeps: BeepConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub update: UpdateConfig,
}

/// Auto-update checker preferences. The checker is notify-only — it
/// surfaces a newer GitHub release and opens the download page; it never
/// replaces the binary itself.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UpdateConfig {
    /// Check GitHub for a newer release on launch and periodically.
    /// On by default; the manual "Check for updates" button works
    /// regardless of this setting.
    #[serde(default = "default_true")]
    pub auto_check: bool,
    /// A version the user explicitly dismissed ("skip this version") so
    /// the banner stops nagging until something newer ships. Absent when
    /// nothing is skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_version: Option<String>,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            auto_check: true,
            skip_version: None,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Four quick-recall frequency presets (M1–M4), persisted across
/// restarts like a real radio's memory channels. Each slot holds a
/// canonical frequency label (e.g. `"446.05"`) or is absent when the
/// slot is free.
///
/// Stored as four individually-optional keys rather than an array
/// because TOML can't represent a null array element — an empty slot
/// is simply an omitted key, and `skip_serializing_if` keeps the
/// written file clean (no `m3 = ""` noise for unused presets).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MemoryConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub m1: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub m2: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub m3: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub m4: Option<String>,
}

impl MemoryConfig {
    /// The four slots as a positional array of clones, for indexed
    /// access in the UI (`slots()[i]`).
    pub fn slots(&self) -> [Option<String>; 4] {
        [
            self.m1.clone(),
            self.m2.clone(),
            self.m3.clone(),
            self.m4.clone(),
        ]
    }

    /// Set slot `i` (0-based) to `value`. Out-of-range indices are a
    /// no-op — callers only ever pass 0..4.
    pub fn set(&mut self, i: usize, value: Option<String>) {
        match i {
            0 => self.m1 = value,
            1 => self.m2 = value,
            2 => self.m3 = value,
            3 => self.m4 = value,
            _ => {}
        }
    }
}

/// Customizable "roger beeps" — the short tones the radio plays
/// locally whenever someone takes or clears the floor in our current
/// frequency room.
///
/// Tone choice (the two pitches + their duration) is selected from
/// the static preset table in [`crate::audio::BeepPreset::ALL`]; the
/// config persists the *preset id* rather than the raw values so a
/// preset's tuning can be refined later without forcing every user
/// to retune. An unknown id resolves to the first preset
/// (`"default"`) at load time.
///
/// Volume sits outside the preset because it's a loudness preference
/// rather than a tonal one — users should be able to trim it without
/// disturbing the preset they've picked.
///
/// Legacy configs that stored `acquire_hz` / `release_hz` /
/// `duration_ms` fields here are silently ignored by serde; they'll
/// pick up whatever the current default preset's values are on next
/// load.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BeepConfig {
    #[serde(default = "default_beep_preset")]
    pub preset: String,
    #[serde(default = "default_beep_volume")]
    pub volume: f32,
}

fn default_beep_preset() -> String {
    "default".into()
}
fn default_beep_volume() -> f32 {
    0.05
}

impl Default for BeepConfig {
    fn default() -> Self {
        Self {
            preset: default_beep_preset(),
            volume: default_beep_volume(),
        }
    }
}

/// Persisted server address, identity, and last-selected frequency.
/// Defaults match the original hard-coded form values so first-launch
/// behavior is unchanged.
///
/// `host` + `port` replaced the older single `server` string. Configs
/// written by previous versions are migrated on load by
/// [`ConnectionConfigDe::into_canonical`] — they keep working without
/// the user having to retype their server address.
#[derive(Serialize, Clone, Debug, serde::Deserialize)]
#[serde(from = "ConnectionConfigDe")]
pub struct ConnectionConfig {
    pub host: String,
    pub port: u16,
    pub display_name: String,
    /// Last frequency the user was on. Stored as `"446.05"`-style
    /// string for stability across float-formatting changes; parsed
    /// into a channel index by the UI on load. Defaults to the
    /// middle of the band.
    #[serde(default = "default_frequency")]
    pub frequency: String,
    /// Shared-secret password for servers that gate registration.
    /// Empty string (the default) is treated as "no password" both
    /// here and on the server — the wire field is still sent, the
    /// server just ignores it in open mode. Stored in plaintext
    /// alongside the rest of the user's settings — same threat model
    /// as Wi-Fi credentials in the OS keychain would have, but
    /// without the keychain integration.
    #[serde(default)]
    pub password: String,
}

impl ConnectionConfig {
    /// `host:port` as a single string. Used wherever the runtime
    /// wants a one-shot endpoint identifier (logs, the Quick Connect
    /// summary on the strip, etc.).
    pub fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

fn default_frequency() -> String {
    "447.00".into()
}

fn default_host() -> String {
    "127.0.0.1".into()
}

fn default_port() -> u16 {
    50051
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            display_name: "anon".into(),
            frequency: default_frequency(),
            password: String::new(),
        }
    }
}

/// Wire shape used for deserialisation only. Accepts both:
///   * The current form (`host = "…", port = 50051`).
///   * The legacy form (`server = "host:port"` or
///     `server = "http://host:port"`), present in config files
///     written before this split.
///
/// `host` / `port` win when both are set; otherwise the legacy
/// `server` is parsed for the host / port pair. If neither side
/// provides usable values we fall back to the regular defaults.
#[derive(serde::Deserialize)]
struct ConnectionConfigDe {
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    /// Legacy field (`server = "…"`) — read for migration only,
    /// never written.
    #[serde(default)]
    server: Option<String>,
    #[serde(default = "default_display_name")]
    display_name: String,
    #[serde(default = "default_frequency")]
    frequency: String,
    #[serde(default)]
    password: String,
}

fn default_display_name() -> String {
    "anon".into()
}

impl From<ConnectionConfigDe> for ConnectionConfig {
    fn from(d: ConnectionConfigDe) -> Self {
        let (host, port) = if let Some(host) = d.host {
            (host, d.port.unwrap_or_else(default_port))
        } else if let Some(server) = d.server.as_deref() {
            parse_legacy_server(server).unwrap_or_else(|| (default_host(), default_port()))
        } else {
            (default_host(), default_port())
        };
        Self {
            host,
            port,
            display_name: d.display_name,
            frequency: d.frequency,
            password: d.password,
        }
    }
}

/// Parse a legacy `server = "…"` value into the new `(host, port)`
/// pair. Accepts the historic forms:
///   * `"host:port"`
///   * `"http://host:port"`
///   * `"https://host:port"`
///
/// Returns `None` if the string doesn't parse cleanly; the caller
/// then falls back to the defaults rather than panicking on a typo.
fn parse_legacy_server(s: &str) -> Option<(String, u16)> {
    let stripped = s
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let (host, port_str) = stripped.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_slots_round_trip_through_toml() {
        // Two slots set, two empty. Empty slots must be omitted from
        // the serialized form (TOML can't hold nulls), and a reparse
        // must recover the same positional layout.
        let mut m = MemoryConfig::default();
        m.set(0, Some("446.05".into()));
        m.set(2, Some("447.50".into()));

        let toml_str = toml::to_string(&m).unwrap();
        assert!(toml_str.contains("m1 = \"446.05\""));
        assert!(toml_str.contains("m3 = \"447.50\""));
        // Empty slots omitted entirely.
        assert!(!toml_str.contains("m2"));
        assert!(!toml_str.contains("m4"));

        let back: MemoryConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(
            back.slots(),
            [
                Some("446.05".to_string()),
                None,
                Some("447.50".to_string()),
                None
            ]
        );
    }

    #[test]
    fn memory_defaults_to_all_empty() {
        let m = MemoryConfig::default();
        assert_eq!(m.slots(), [None, None, None, None]);
        // A config with no [memory] table still parses.
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.memory.slots(), [None, None, None, None]);
    }

    #[test]
    fn memory_hotkeys_round_trip_and_default_unbound() {
        use crate::hotkey::{Input, MouseButton};
        let mut hk = HotkeyConfig::default();
        // Bind M1 to a key, M3 to a mouse button; leave M2/M4 unbound.
        hk.set_memory(0, HotkeyBinding::from_input(Input::Key(Code::F1)));
        hk.set_memory(
            2,
            HotkeyBinding::from_input(Input::Mouse(MouseButton::Middle)),
        );

        let inputs = hk.memory_inputs();
        assert_eq!(inputs[0], Some(Input::Key(Code::F1)));
        assert_eq!(inputs[1], None);
        assert_eq!(inputs[2], Some(Input::Mouse(MouseButton::Middle)));
        assert_eq!(inputs[3], None);

        // Survives a serialize → parse round-trip.
        let s = toml::to_string(&hk).unwrap();
        let back: HotkeyConfig = toml::from_str(&s).unwrap();
        assert_eq!(back.memory_inputs(), inputs);
    }

    #[test]
    fn audio_balance_defaults_centered_and_round_trips() {
        // Default config is centered.
        assert_eq!(AudioConfig::default().balance, 0.0);
        // Missing key parses to the default (centered).
        let cfg: Config = toml::from_str("[audio]\n").unwrap();
        assert_eq!(cfg.audio.balance, 0.0);
        // A set value survives a round-trip.
        let raw = "[audio]\nbalance = -1.0\n";
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.audio.balance, -1.0);
        let s = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.audio.balance, -1.0);
    }

    #[test]
    fn dsp_toggles_default_on_and_round_trip() {
        // Absent keys (every pre-DSP config) resolve to on — existing
        // users get the processed-mic experience without editing TOML.
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.audio.noise_suppression);
        assert!(cfg.audio.agc);
        // An explicit opt-out survives a save/load round trip.
        let raw = "[audio]\nnoise_suppression = false\nagc = false\n";
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(!cfg.audio.noise_suppression);
        assert!(!cfg.audio.agc);
        let s = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert!(!back.audio.noise_suppression);
        assert!(!back.audio.agc);
    }

    #[test]
    fn output_dirty_defaults_off_and_round_trips() {
        // Absent keys (every pre-FX config) resolve to off, with a
        // moderate amount waiting behind the toggle.
        let cfg: Config = toml::from_str("").unwrap();
        assert!(!cfg.audio.output_dirty);
        assert_eq!(cfg.audio.output_dirty_amount, default_dirty_amount());
        // An explicit on + amount survives a save/load round trip.
        let raw = "[audio]\noutput_dirty = true\noutput_dirty_amount = 0.85\n";
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(cfg.audio.output_dirty);
        assert_eq!(cfg.audio.output_dirty_amount, 0.85);
        let s = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert!(back.audio.output_dirty);
        assert_eq!(back.audio.output_dirty_amount, 0.85);
    }

    #[test]
    fn freq_hotkeys_round_trip() {
        use crate::hotkey::Input;
        let mut hk = HotkeyConfig::default();
        // Unbound by default.
        assert_eq!(hk.freq_inputs(), [None, None]);

        hk.freq_up = HotkeyBinding::from_input(Input::Key(Code::Equal));
        hk.freq_down = HotkeyBinding::from_input(Input::Key(Code::Minus));
        assert_eq!(
            hk.freq_inputs(),
            [Some(Input::Key(Code::Equal)), Some(Input::Key(Code::Minus))]
        );

        let s = toml::to_string(&hk).unwrap();
        let back: HotkeyConfig = toml::from_str(&s).unwrap();
        assert_eq!(back.freq_inputs(), hk.freq_inputs());
    }

    #[test]
    fn set_ptt_preserves_memory_bindings() {
        use crate::hotkey::Input;
        let mut hk = HotkeyConfig::default();
        hk.set_memory(1, HotkeyBinding::from_input(Input::Key(Code::F2)));
        // Rebinding PTT must not clobber the M2 hotkey.
        hk.set_ptt(Input::Key(Code::KeyT));
        // set_ptt now writes the tagged `binding` form and clears legacy.
        assert_eq!(hk.binding.as_deref(), Some("key:KeyT"));
        assert_eq!(hk.key, None);
        assert_eq!(hk.to_input(), Some(Input::Key(Code::KeyT)));
        assert_eq!(hk.memory_inputs()[1], Some(Input::Key(Code::F2)));
    }

    #[test]
    fn tagged_binding_round_trips_for_a_gamepad() {
        use crate::hotkey::{GamepadButton, GamepadCode, Input};
        let mut hk = HotkeyConfig::default();
        let input = Input::Gamepad(GamepadButton {
            button: GamepadCode::South,
            index: 0,
        });
        hk.set_ptt(input);
        hk.set_memory(0, HotkeyBinding::from_input(input));

        let s = toml::to_string(&hk).unwrap();
        let back: HotkeyConfig = toml::from_str(&s).unwrap();
        assert_eq!(back.to_input(), Some(input));
        assert_eq!(back.memory_inputs()[0], Some(input));
    }

    #[test]
    fn legacy_key_only_config_still_parses() {
        use crate::hotkey::Input;
        // A config written before any-device binding: only `key`, no
        // `binding`. PTT must still resolve.
        let raw = "[hotkey]\nkey = \"F8\"\n";
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.hotkey.to_input(), Some(Input::Key(Code::F8)));
    }

    #[test]
    fn legacy_mouse_only_config_still_parses() {
        use crate::hotkey::{Input, MouseButton};
        let raw = "[hotkey]\nmouse_button = \"Middle\"\n";
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(
            cfg.hotkey.to_input(),
            Some(Input::Mouse(MouseButton::Middle))
        );
    }

    #[test]
    fn tagged_binding_wins_over_legacy_fields() {
        use crate::hotkey::Input;
        // When both the new `binding` and a legacy `key` are present,
        // the tagged form takes precedence.
        let raw = "[hotkey]\nbinding = \"key:F8\"\nkey = \"Backquote\"\n";
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.hotkey.to_input(), Some(Input::Key(Code::F8)));
    }

    #[test]
    fn legacy_memory_binding_still_parses() {
        use crate::hotkey::Input;
        // A memory slot written in the legacy shape (no `binding`).
        let raw = "[hotkey.m1]\nkey = \"F1\"\n";
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.hotkey.memory_inputs()[0], Some(Input::Key(Code::F1)));
    }

    #[test]
    fn secondary_ptt_round_trips_and_defaults_unbound() {
        use crate::hotkey::Input;
        let mut hk = HotkeyConfig::default();
        // Unbound by default and absent from the serialized form.
        assert_eq!(hk.to_input_secondary(), None);
        assert!(!toml::to_string(&hk).unwrap().contains("secondary"));

        hk.set_ptt_secondary(Some(Input::Key(Code::Backquote)));
        let s = toml::to_string(&hk).unwrap();
        let back: HotkeyConfig = toml::from_str(&s).unwrap();
        assert_eq!(back.to_input_secondary(), Some(Input::Key(Code::Backquote)));

        // Clearing removes it again.
        let mut back = back;
        back.set_ptt_secondary(None);
        assert_eq!(back.to_input_secondary(), None);
    }

    #[test]
    fn broadcast_ptt_defaults_unbound_and_absent_from_toml() {
        let hk = HotkeyConfig::default();
        // Default: unbound.
        assert_eq!(hk.broadcast_ptt_input(), None);
        // Not serialized when unset.
        let s = toml::to_string(&hk).unwrap();
        assert!(
            !s.contains("broadcast_ptt"),
            "absent key must not appear: {s}"
        );

        // A TOML file without broadcast_ptt still deserializes fine.
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.hotkey.broadcast_ptt_input(), None);
    }

    #[test]
    fn broadcast_ptt_round_trips() {
        use crate::hotkey::Input;
        use global_hotkey::hotkey::Code;
        let mut hk = HotkeyConfig::default();
        hk.set_broadcast_ptt(Some(Input::Key(Code::F9)));
        assert_eq!(hk.broadcast_ptt_input(), Some(Input::Key(Code::F9)));

        let s = toml::to_string(&hk).unwrap();
        assert!(
            s.contains("broadcast_ptt"),
            "bound key must be serialized: {s}"
        );
        let back: HotkeyConfig = toml::from_str(&s).unwrap();
        assert_eq!(back.broadcast_ptt_input(), Some(Input::Key(Code::F9)));

        // Clearing removes it again.
        let mut back = back;
        back.set_broadcast_ptt(None);
        assert_eq!(back.broadcast_ptt_input(), None);
    }

    #[test]
    fn parse_legacy_server_accepts_bare_host_port() {
        assert_eq!(
            parse_legacy_server("127.0.0.1:50051"),
            Some(("127.0.0.1".into(), 50051))
        );
    }

    #[test]
    fn parse_legacy_server_strips_http_scheme() {
        assert_eq!(
            parse_legacy_server("http://example.com:8080"),
            Some(("example.com".into(), 8080))
        );
        assert_eq!(
            parse_legacy_server("https://example.com:8443"),
            Some(("example.com".into(), 8443))
        );
    }

    #[test]
    fn parse_legacy_server_rejects_bad_inputs() {
        assert!(parse_legacy_server("").is_none());
        assert!(parse_legacy_server("no-port").is_none());
        assert!(parse_legacy_server(":50051").is_none()); // empty host
        assert!(parse_legacy_server("host:not-a-port").is_none());
        assert!(parse_legacy_server("host:99999").is_none()); // > u16
    }

    #[test]
    fn legacy_server_field_migrates_into_host_port() {
        // Old config shape: a single `server` string. The new
        // `host` / `port` pair should appear after deserialisation.
        let raw = r#"
            server = "192.168.1.50:60000"
            display_name = "TOKI-1"
        "#;
        let cfg: ConnectionConfig = toml::from_str(raw).unwrap();
        assert_eq!(cfg.host, "192.168.1.50");
        assert_eq!(cfg.port, 60000);
        assert_eq!(cfg.display_name, "TOKI-1");
    }

    #[test]
    fn new_host_port_form_round_trips() {
        let raw = r#"
            host = "toki.example"
            port = 1234
            display_name = "FOX"
        "#;
        let cfg: ConnectionConfig = toml::from_str(raw).unwrap();
        assert_eq!(cfg.host, "toki.example");
        assert_eq!(cfg.port, 1234);
        // Endpoint helper smooths over the format() boilerplate that
        // every call site would otherwise duplicate.
        assert_eq!(cfg.endpoint(), "toki.example:1234");
    }
}

/// Persisted audio device preferences. `None` means "use the host's
/// default device" (which is also what we do if the saved name no longer
/// matches any enumerated device, e.g. an unplugged USB headset).
///
/// Gains are linear multipliers applied in the i16 sample path. 1.0 means
/// passthrough; 0.0 is silence; values >1.0 amplify (and may clip — we
/// hard-clamp at the i16 boundary in the callback). The UI exposes
/// 0.0 – 2.0 which we found is enough headroom for quiet mics without
/// turning every consonant into a square wave.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AudioConfig {
    #[serde(default)]
    pub input_device: Option<String>,
    #[serde(default)]
    pub output_device: Option<String>,
    #[serde(default = "default_gain")]
    pub input_gain: f32,
    #[serde(default = "default_gain")]
    pub output_gain: f32,
    /// Stereo playback balance in `[-1.0, 1.0]`: `-1` = full left,
    /// `0` = centered, `+1` = full right. Lets the operator route
    /// received audio into a single ear to mimic a mono walkie-talkie
    /// earpiece. Default `0.0` (centered). No effect on mono outputs.
    #[serde(default = "default_balance")]
    pub balance: f32,
    /// Capture-side RNNoise noise suppression (see `crate::dsp`).
    /// Default on — the "just works in a noisy room" experience;
    /// toggle off in Settings for the raw, unprocessed mic character.
    #[serde(default = "default_true")]
    pub noise_suppression: bool,
    /// Capture-side automatic gain control. Default on, same
    /// rationale (and same Settings toggle) as `noise_suppression`.
    #[serde(default = "default_true")]
    pub agc: bool,
    /// Transmit-side "radio FX" dirtying (band-pass + saturation + static)
    /// baked into the operator's *outgoing* voice so peers hear it — see
    /// `crate::dsp::OutputDsp`. Default **off**: it's a deliberate flavour
    /// effect, not something to impose on a fresh install (the bare
    /// `#[serde(default)]` gives `false`, so every pre-existing config also
    /// stays clean). The field name is kept for config back-compat.
    #[serde(default)]
    pub output_dirty: bool,
    /// How hard the radio FX dirties, `[0.0, 1.0]`. Only meaningful when
    /// `output_dirty` is on. Defaults to a moderate setting so flipping
    /// the toggle with no prior value is audibly "a radio" rather than
    /// either subtle or extreme.
    #[serde(default = "default_dirty_amount")]
    pub output_dirty_amount: f32,
}

fn default_gain() -> f32 {
    1.0
}

fn default_balance() -> f32 {
    0.0
}

fn default_dirty_amount() -> f32 {
    0.6
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            input_gain: 1.0,
            output_gain: 1.0,
            balance: 0.0,
            noise_suppression: true,
            agc: true,
            output_dirty: false,
            output_dirty_amount: default_dirty_amount(),
        }
    }
}

/// Persisted PTT binding. Exactly one of `key` or `mouse_button` is
/// set at a time (we don't enforce this in the type — the resolution
/// logic in [`HotkeyConfig::to_input`] picks mouse-then-key if both
/// happen to be present, but normal usage always clears one when
/// setting the other).
///
/// Extra fields from older configs (`ctrl`, `shift`, `alt`, `meta`)
/// are silently dropped by serde — Toki no longer supports modifier
/// chords. The `key` value is just a physical key code.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HotkeyConfig {
    /// Tagged PTT binding for any peripheral (e.g. `"key:Backquote"`,
    /// `"gamepad:0:South"`, `"streamdeck:0x0fd9:0x0080:3"`). Preferred
    /// over the legacy `key` / `mouse_button` fields; written by all new
    /// saves. Absent on configs written before any-device binding — the
    /// resolver then falls back to `key`/`mouse_button` below.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    /// Legacy: `keyboard_types::Code` variant name, e.g. `"Backquote"`,
    /// `"F8"`. Read-only fallback when `binding` is absent. Still
    /// defaults to `Backquote` so a brand-new config keeps PTT on the
    /// backquote key.
    #[serde(default = "default_key")]
    pub key: Option<String>,
    /// Legacy: stable mouse button label (`"Left"`, `"Middle"`,
    /// `"Mouse4"`, …). Read-only fallback when `binding` is absent.
    #[serde(default)]
    pub mouse_button: Option<String>,
    /// Optional secondary/fallback PTT binding (tagged form, any
    /// peripheral). PTT engages while either the primary `binding` or
    /// this is held — e.g. a keyboard key backing up a gamepad button.
    /// Unbound (omitted) by default; no legacy equivalent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary: Option<String>,
    /// Optional global hotkeys that recall memory presets M1–M4. Each
    /// is an independent [`HotkeyBinding`]; an unbound slot is an empty
    /// (omitted) table. Pressing the bound key/button switches the
    /// tuner to that preset's saved frequency, exactly as a left-click
    /// on the on-screen M-button would.
    #[serde(default)]
    pub m1: HotkeyBinding,
    #[serde(default)]
    pub m2: HotkeyBinding,
    #[serde(default)]
    pub m3: HotkeyBinding,
    #[serde(default)]
    pub m4: HotkeyBinding,
    /// Optional global hotkeys that step the tuner one channel up /
    /// down — the keyboard equivalent of the ◀ ▶ chevrons. Unbound by
    /// default.
    #[serde(default)]
    pub freq_up: HotkeyBinding,
    #[serde(default)]
    pub freq_down: HotkeyBinding,
    /// Optional dedicated global-broadcast PTT binding. When held, sends a
    /// broadcast PTT (distinct from the normal PTT) that the server fans out
    /// to every frequency room. Unbound by default; only effective once an
    /// admin grants global-broadcast capability. Same peripheral support as
    /// the normal PTT binding. Stored as a tagged token (e.g. `"key:F9"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broadcast_ptt: Option<String>,
}

/// One binding for any peripheral, used for the four memory-recall and
/// the freq up/down hotkeys. All fields `None` = unbound.
///
/// `binding` is the current tagged form (e.g. `"gamepad:0:South"`,
/// `"key:F8"`) and is what new saves write. The legacy `key` /
/// `mouse_button` fields are kept **read-only** for back-compat with
/// configs written before any-device binding existed: when `binding`
/// is absent we fall through to them.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct HotkeyBinding {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mouse_button: Option<String>,
}

impl HotkeyBinding {
    /// Parse into an [`Input`], preferring the tagged `binding` form and
    /// falling back to the legacy `mouse_button` then `key` fields.
    /// `None` when unbound or unparseable.
    pub fn to_input(&self) -> Option<crate::hotkey::Input> {
        resolve_binding(
            self.binding.as_deref(),
            self.mouse_button.as_deref(),
            self.key.as_deref(),
        )
    }

    /// Build from a captured [`Input`]. Writes the tagged `binding`
    /// form and leaves the legacy fields empty.
    pub fn from_input(input: crate::hotkey::Input) -> Self {
        Self {
            binding: Some(input.to_token()),
            key: None,
            mouse_button: None,
        }
    }
}

/// Shared resolution used by both [`HotkeyBinding`] and the PTT slot on
/// [`HotkeyConfig`]: tagged `binding` wins, then legacy `mouse_button`,
/// then legacy `key`. Centralized so the precedence can't drift between
/// the two call sites.
fn resolve_binding(
    binding: Option<&str>,
    mouse_button: Option<&str>,
    key: Option<&str>,
) -> Option<crate::hotkey::Input> {
    if let Some(token) = binding {
        return crate::hotkey::Input::from_token(token);
    }
    if let Some(label) = mouse_button {
        return crate::hotkey::MouseButton::from_label(label).map(crate::hotkey::Input::Mouse);
    }
    let code = Code::from_str(key?).ok()?;
    Some(crate::hotkey::Input::Key(code))
}

fn default_key() -> Option<String> {
    Some("Backquote".into())
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            binding: None,
            key: Some("Backquote".into()),
            mouse_button: None,
            secondary: None,
            m1: HotkeyBinding::default(),
            m2: HotkeyBinding::default(),
            m3: HotkeyBinding::default(),
            m4: HotkeyBinding::default(),
            freq_up: HotkeyBinding::default(),
            freq_down: HotkeyBinding::default(),
            broadcast_ptt: None,
        }
    }
}

impl HotkeyConfig {
    /// Parsed PTT form suitable for handing to [`crate::hotkey::install`].
    /// Prefers the tagged `binding`, falling back to the legacy
    /// `mouse_button` then `key`. `None` if nothing parses.
    pub fn to_input(&self) -> Option<crate::hotkey::Input> {
        resolve_binding(
            self.binding.as_deref(),
            self.mouse_button.as_deref(),
            self.key.as_deref(),
        )
    }

    /// Update the PTT binding in place, leaving the memory bindings
    /// untouched. Writes the tagged `binding` form and clears the legacy
    /// fields so the two can't disagree.
    pub fn set_ptt(&mut self, input: crate::hotkey::Input) {
        self.binding = Some(input.to_token());
        self.key = None;
        self.mouse_button = None;
    }

    /// Parsed secondary/fallback PTT binding, or `None` when unbound.
    pub fn to_input_secondary(&self) -> Option<crate::hotkey::Input> {
        crate::hotkey::Input::from_token(self.secondary.as_deref()?)
    }

    /// Set (or clear, with `None`) the secondary/fallback PTT binding.
    pub fn set_ptt_secondary(&mut self, input: Option<crate::hotkey::Input>) {
        self.secondary = input.map(|i| i.to_token());
    }

    /// Borrow the memory-recall binding for slot `i` (0..4).
    pub fn memory(&self, i: usize) -> &HotkeyBinding {
        match i {
            0 => &self.m1,
            1 => &self.m2,
            2 => &self.m3,
            _ => &self.m4,
        }
    }

    /// Replace the memory-recall binding for slot `i` (0..4).
    pub fn set_memory(&mut self, i: usize, b: HotkeyBinding) {
        match i {
            0 => self.m1 = b,
            1 => self.m2 = b,
            2 => self.m3 = b,
            3 => self.m4 = b,
            _ => {}
        }
    }

    /// The four memory-recall bindings parsed into `Input`s, for
    /// seeding the input poller at startup.
    pub fn memory_inputs(&self) -> [Option<crate::hotkey::Input>; 4] {
        [
            self.m1.to_input(),
            self.m2.to_input(),
            self.m3.to_input(),
            self.m4.to_input(),
        ]
    }

    /// The tune up/down bindings parsed into `Input`s (`[up, down]`),
    /// for seeding the poller at startup.
    pub fn freq_inputs(&self) -> [Option<crate::hotkey::Input>; 2] {
        [self.freq_up.to_input(), self.freq_down.to_input()]
    }

    /// Parsed global-broadcast PTT binding, or `None` when unbound.
    /// Uses the same tagged-token resolution as the primary PTT binding;
    /// an unparseable or absent token returns `None`.
    pub fn broadcast_ptt_input(&self) -> Option<crate::hotkey::Input> {
        crate::hotkey::Input::from_token(self.broadcast_ptt.as_deref()?)
    }

    /// Set (or clear, with `None`) the broadcast PTT binding. Called
    /// from the Settings panel's BROADCAST PTT bind row.
    pub fn set_broadcast_ptt(&mut self, input: Option<crate::hotkey::Input>) {
        self.broadcast_ptt = input.map(|i| i.to_token());
    }
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("toki").join(FILENAME))
}

impl Config {
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        match fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                tracing::warn!(error = %e, path = %path.display(), "could not parse config, using defaults");
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "could not read config");
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                tracing::warn!(error = %e, path = %parent.display(), "could not create config dir");
                return;
            }
        }
        match toml::to_string_pretty(self) {
            Ok(s) => {
                if let Err(e) = fs::write(&path, s) {
                    tracing::warn!(error = %e, path = %path.display(), "could not write config");
                    return;
                }
                // The config persists the server password in plaintext;
                // tighten the file mode so other local users on a
                // shared box can't read it. Best-effort — a failure
                // here just logs (file was still written successfully).
                tighten_permissions(&path);
            }
            Err(e) => tracing::warn!(error = %e, "could not serialize config"),
        }
    }
}

/// On Unix, set the config file to `0600` so the shared-secret
/// password isn't world-readable on a multi-user box. No-op on
/// Windows — NTFS ACL inheritance from the user's profile already
/// limits access to the account that owns it.
#[cfg(unix)]
fn tighten_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let perms = fs::Permissions::from_mode(0o600);
    if let Err(e) = fs::set_permissions(path, perms) {
        tracing::warn!(error = %e, path = %path.display(), "could not tighten config permissions");
    }
}

#[cfg(not(unix))]
fn tighten_permissions(_path: &std::path::Path) {
    // Windows / other: rely on the user-profile ACL inheritance.
}
