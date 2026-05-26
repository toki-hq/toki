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
}

/// Persisted server address and identity. Defaults match the original
/// hard-coded form values so first-launch behavior is unchanged.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ConnectionConfig {
    pub server: String,
    pub display_name: String,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            server: "http://127.0.0.1:50051".into(),
            display_name: "anon".into(),
        }
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
}

fn default_gain() -> f32 {
    1.0
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            input_gain: 1.0,
            output_gain: 1.0,
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
    /// `keyboard_types::Code` variant name, e.g. `"Backquote"`, `"F8"`.
    /// `None` when the bound input is a mouse button.
    #[serde(default = "default_key")]
    pub key: Option<String>,
    /// Stable mouse button label (`"Left"`, `"Right"`, `"Middle"`,
    /// `"Mouse4"`, …). `None` when the bound input is a keyboard key.
    #[serde(default)]
    pub mouse_button: Option<String>,
}

fn default_key() -> Option<String> {
    Some("Backquote".into())
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            key: Some("Backquote".into()),
            mouse_button: None,
        }
    }
}

impl HotkeyConfig {
    /// Parsed form suitable for handing to [`crate::hotkey::install`].
    /// Returns `None` if neither field is set, or the set field is
    /// unparseable. Mouse takes precedence if both are somehow set.
    pub fn to_input(&self) -> Option<crate::hotkey::Input> {
        if let Some(label) = self.mouse_button.as_deref() {
            return crate::hotkey::MouseButton::from_label(label).map(crate::hotkey::Input::Mouse);
        }
        let key = self.key.as_deref()?;
        let code = Code::from_str(key).ok()?;
        Some(crate::hotkey::Input::Key(code))
    }

    /// Serialize a single [`Input`] back into the on-disk form.
    pub fn from_input(input: crate::hotkey::Input) -> Self {
        match input {
            crate::hotkey::Input::Key(code) => Self {
                key: Some(code.to_string()),
                mouse_button: None,
            },
            crate::hotkey::Input::Mouse(b) => Self {
                key: None,
                mouse_button: Some(b.label()),
            },
        }
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
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not serialize config"),
        }
    }
}
