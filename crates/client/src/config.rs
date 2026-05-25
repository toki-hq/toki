//! Persisted user preferences. Stored as TOML at the platform's standard
//! config location (`~/Library/Application Support/toki/config.toml` on
//! macOS, `~/.config/toki/config.toml` on Linux, `%APPDATA%\toki\config.toml`
//! on Windows). All loads and saves are best-effort: failures fall back to
//! defaults and log a warning, never panic.

use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
};
use serde::{Deserialize, Serialize};

const FILENAME: &str = "config.toml";

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Config {
    #[serde(default)]
    pub hotkey: HotkeyConfig,
    #[serde(default)]
    pub audio: AudioConfig,
}

/// Persisted audio device preferences. `None` means "use the host's
/// default device" (which is also what we do if the saved name no longer
/// matches any enumerated device, e.g. an unplugged USB headset).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AudioConfig {
    #[serde(default)]
    pub input_device: Option<String>,
    #[serde(default)]
    pub output_device: Option<String>,
}

/// Serializable representation of a global hotkey. We don't reuse
/// `global_hotkey::HotKey` directly — its fields aren't public and the
/// underlying `Code` enum's stringly representation is easier to edit by
/// hand than a numeric bitfield.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HotkeyConfig {
    /// `keyboard_types::Code` variant name, e.g. `"Backquote"`, `"F8"`,
    /// `"KeyA"`. Round-trips via `Code::from_str` / `Display`.
    pub key: String,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub alt: bool,
    /// Cmd on macOS, Windows key on Windows, Super on X11.
    #[serde(default)]
    pub meta: bool,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            key: "Backquote".into(),
            ctrl: false,
            shift: false,
            alt: false,
            meta: false,
        }
    }
}

impl HotkeyConfig {
    pub fn to_hotkey(&self) -> Option<HotKey> {
        let code = Code::from_str(&self.key).ok()?;
        Some(HotKey::new(Some(self.modifiers()), code))
    }

    pub fn modifiers(&self) -> Modifiers {
        let mut m = Modifiers::empty();
        if self.ctrl {
            m |= Modifiers::CONTROL;
        }
        if self.shift {
            m |= Modifiers::SHIFT;
        }
        if self.alt {
            m |= Modifiers::ALT;
        }
        if self.meta {
            m |= Modifiers::META;
        }
        m
    }

    pub fn from_parts(code: Code, mods: Modifiers) -> Self {
        Self {
            key: code.to_string(),
            ctrl: mods.contains(Modifiers::CONTROL),
            shift: mods.contains(Modifiers::SHIFT),
            alt: mods.contains(Modifiers::ALT),
            meta: mods.contains(Modifiers::META),
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
