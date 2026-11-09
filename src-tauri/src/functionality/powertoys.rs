//! Microsoft PowerToys Keyboard Manager support.
//!
//! PowerToys remaps keys at the OS level before they reach Dorion. Dorion's
//! global keybind hook still sees the original key press (and, on Windows, the
//! remapped key that PowerToys injects), so without this module a key that has
//! been remapped away would still trigger its old Dorion keybind.
//!
//! This module reads the active Keyboard Manager configuration and translates
//! key presses the same way PowerToys does. Keybinds that use a remapped source
//! key are ignored, and input is translated to the remap target before matching.

use std::{
  collections::{HashMap, HashSet},
  fs,
  path::{Path, PathBuf},
  sync::{Arc, LazyLock, RwLock},
  time::SystemTime,
};

use livesplit_hotkey::{Hotkey, KeyCode, Modifiers};
use serde::Deserialize;
use sysinfo::{ProcessRefreshKind, RefreshKind, System};

use crate::log;

#[derive(Debug, Default)]
pub struct PowerToysKeymap {
  remaps: Vec<KeyRemap>,
  single_key_sources: HashSet<KeyCode>,
  shortcut_sources: HashSet<Hotkey>,
}

#[derive(Debug, Clone)]
struct KeyRemap {
  from: Hotkey,
  to: Option<Hotkey>,
  /// PowerToys "Remap a key" entries apply regardless of which modifiers are
  /// held. "Remap a shortcut" entries only apply to the exact combination.
  single_key: bool,
}

#[derive(Debug, Deserialize)]
struct KbmConfig {
  #[serde(rename = "remapKeys")]
  remap_keys: Option<RemapKeys>,
  #[serde(rename = "remapShortcuts")]
  remap_shortcuts: Option<RemapShortcuts>,
}

#[derive(Debug, Deserialize)]
struct RemapKeys {
  #[serde(rename = "inProcess")]
  in_process: Option<Vec<KbmEntry>>,
}

#[derive(Debug, Deserialize)]
struct RemapShortcuts {
  global: Option<Vec<KbmEntry>>,
  #[serde(rename = "appSpecific")]
  app_specific: Option<Vec<KbmEntry>>,
}

#[derive(Debug, Deserialize)]
struct KbmEntry {
  #[serde(rename = "originalKeys", default)]
  original_keys: String,
  #[serde(rename = "newRemapKeys", default)]
  new_remap_keys: String,
  #[serde(rename = "unicodeText", default)]
  new_remap_string: String,
  #[serde(rename = "secondKeyOfChord", default)]
  second_key_of_chord: u32,
  #[serde(rename = "operationType", default)]
  operation_type: i32,
  #[serde(rename = "targetApp", default)]
  target_app: String,
}

#[derive(Debug, Deserialize)]
struct KbmSettings {
  #[serde(default)]
  enabled: HashMap<String, bool>,
}

static KEYMAP: LazyLock<RwLock<Arc<PowerToysKeymap>>> =
  LazyLock::new(|| RwLock::new(Arc::new(PowerToysKeymap::default())));

/// Returns the last loaded keymap. Call [`reload`] to pick up changes.
pub fn get() -> Arc<PowerToysKeymap> {
  KEYMAP.read().unwrap().clone()
}

/// Reads the current PowerToys configuration and replaces the cached keymap.
pub fn reload() -> Arc<PowerToysKeymap> {
  let keymap = Arc::new(load());

  *KEYMAP.write().unwrap() = keymap.clone();

  keymap
}

/// Used by the Keybind watcher to detect PowerToys settings changes.
pub fn config_mtime() -> Option<SystemTime> {
  let dir = config_dir()?;
  let settings = fs::metadata(dir.join("settings.json"))
    .and_then(|metadata| metadata.modified())
    .ok();
  let profile = fs::metadata(dir.join(format!("{}.json", active_profile(&dir))))
    .and_then(|metadata| metadata.modified())
    .ok();

  match (settings, profile) {
    (Some(settings), Some(profile)) => Some(settings.max(profile)),
    (settings, profile) => settings.or(profile),
  }
}

impl PowerToysKeymap {
  /// Returns true if a Dorion keybind uses a key that PowerToys remaps away.
  pub fn is_source(&self, hotkey: &Hotkey) -> bool {
    self.single_key_sources.contains(&hotkey.key_code) || self.shortcut_sources.contains(hotkey)
  }

  /// Translates a physical key combination to the PowerToys remap target.
  pub fn translate(&self, hotkey: &Hotkey) -> Option<Hotkey> {
    for remap in &self.remaps {
      if remap.single_key {
        if remap.from.key_code != hotkey.key_code {
          continue;
        }

        return remap.to.map(|to| Hotkey {
          key_code: to.key_code,
          modifiers: hotkey.modifiers | to.modifiers,
        });
      } else if remap.from == *hotkey {
        return remap.to;
      }
    }

    None
  }

  pub fn is_empty(&self) -> bool {
    self.remaps.is_empty()
  }

  pub fn describe(&self) -> String {
    self
      .remaps
      .iter()
      .map(|remap| match remap.to {
        Some(to) => format!("{} -> {}", remap.from, to),
        None => format!("{} -> (disabled)", remap.from),
      })
      .collect::<Vec<_>>()
      .join(", ")
  }
}

fn load() -> PowerToysKeymap {
  let Some(dir) = config_dir() else {
    return PowerToysKeymap::default();
  };

  if !keyboard_manager_enabled(&dir) {
    log!("PowerToys Keyboard Manager is disabled, ignoring keymap");
    return PowerToysKeymap::default();
  }

  let profile = active_profile(&dir);
  let path = dir.join(format!("{profile}.json"));
  let path = if path.exists() {
    path
  } else {
    dir.join("default.json")
  };

  let contents = match fs::read_to_string(&path) {
    Ok(contents) => contents,
    Err(e) => {
      log!(
        "Failed to read PowerToys Keyboard Manager config at {}: {}",
        path.display(),
        e
      );
      return PowerToysKeymap::default();
    }
  };

  let config: KbmConfig = match serde_json::from_str(&contents) {
    Ok(config) => config,
    Err(e) => {
      log!(
        "Failed to parse PowerToys Keyboard Manager config at {}: {}",
        path.display(),
        e
      );
      return PowerToysKeymap::default();
    }
  };

  let mut remaps = Vec::new();

  if let Some(keys) = config.remap_keys.and_then(|keys| keys.in_process) {
    for entry in keys {
      if let Some(remap) = parse_entry(&entry, true) {
        remaps.push(remap);
      }
    }
  }

  if let Some(shortcuts) = config.remap_shortcuts {
    for entry in shortcuts.global.unwrap_or_default() {
      if let Some(remap) = parse_entry(&entry, false) {
        remaps.push(remap);
      }
    }

    for entry in shortcuts.app_specific.unwrap_or_default() {
      // App-specific remaps only matter if they target Dorion itself.
      if entry.target_app.trim().is_empty() || targets_dorion(&entry.target_app) {
        if let Some(remap) = parse_entry(&entry, false) {
          remaps.push(remap);
        }
      }
    }
  }

  let mut single_key_sources = HashSet::new();
  let mut shortcut_sources = HashSet::new();

  for remap in &remaps {
    if remap.single_key {
      single_key_sources.insert(remap.from.key_code);
    } else {
      shortcut_sources.insert(remap.from);
    }
  }

  let keymap = PowerToysKeymap {
    remaps,
    single_key_sources,
    shortcut_sources,
  };

  if keymap.is_empty() {
    log!(
      "No PowerToys Keyboard Manager remaps found at {}",
      path.display()
    );
  } else {
    log!(
      "Loaded {} PowerToys Keyboard Manager remap(s) from {}: {}",
      keymap.remaps.len(),
      path.display(),
      keymap.describe()
    );
  }

  keymap
}

fn parse_entry(entry: &KbmEntry, single_key: bool) -> Option<KeyRemap> {
  // Chords, text remaps, and run-program/URI remaps are not keybinds we can
  // represent, and their source keys should not be treated as normal keys.
  if entry.second_key_of_chord > 0
    || entry.operation_type != 0
    || !entry.new_remap_string.trim().is_empty()
  {
    return None;
  }

  let from = parse_hotkey(&entry.original_keys)?;
  let to = if entry.new_remap_keys.trim().is_empty() {
    None
  } else {
    parse_hotkey(&entry.new_remap_keys)
  };

  Some(KeyRemap {
    from,
    to,
    single_key,
  })
}

fn parse_hotkey(keys: &str) -> Option<Hotkey> {
  let keys: Vec<u32> = keys.split(';').filter_map(parse_vk).collect();
  let (key, modifiers) = keys.split_last()?;
  let key_code = vk_to_keycode(*key)?;
  let mut modifier_bits = Modifiers::empty();

  for modifier in modifiers {
    modifier_bits.insert(vk_to_modifier(*modifier)?);
  }

  Some(Hotkey {
    key_code,
    modifiers: modifier_bits,
  })
}

fn parse_vk(value: &str) -> Option<u32> {
  let value = value.trim();

  if let Some(hex) = value
    .strip_prefix("0x")
    .or_else(|| value.strip_prefix("0X"))
  {
    u32::from_str_radix(hex, 16).ok()
  } else {
    value.parse().ok()
  }
}

fn vk_to_modifier(vk: u32) -> Option<Modifiers> {
  Some(match vk {
    // Generic and left/right variants
    0x10 | 0xA0 | 0xA1 => Modifiers::SHIFT,
    0x11 | 0xA2 | 0xA3 => Modifiers::CONTROL,
    0x12 | 0xA4 | 0xA5 => Modifiers::ALT,
    0x5B | 0x5C => Modifiers::META,
    _ => return None,
  })
}

fn vk_to_keycode(vk: u32) -> Option<KeyCode> {
  use KeyCode::*;

  Some(match vk {
    0x08 => Backspace,
    0x09 => Tab,
    0x0D => Enter,
    0x10 => ShiftLeft,
    0x11 => ControlLeft,
    0x12 => AltLeft,
    0x13 => Pause,
    0x14 => CapsLock,
    0x1B => Escape,
    0x20 => Space,
    0x21 => PageUp,
    0x22 => PageDown,
    0x23 => End,
    0x24 => Home,
    0x25 => ArrowLeft,
    0x26 => ArrowUp,
    0x27 => ArrowRight,
    0x28 => ArrowDown,
    0x2C => PrintScreen,
    0x2D => Insert,
    0x2E => Delete,
    0x30 => Digit0,
    0x31 => Digit1,
    0x32 => Digit2,
    0x33 => Digit3,
    0x34 => Digit4,
    0x35 => Digit5,
    0x36 => Digit6,
    0x37 => Digit7,
    0x38 => Digit8,
    0x39 => Digit9,
    0x41 => KeyA,
    0x42 => KeyB,
    0x43 => KeyC,
    0x44 => KeyD,
    0x45 => KeyE,
    0x46 => KeyF,
    0x47 => KeyG,
    0x48 => KeyH,
    0x49 => KeyI,
    0x4A => KeyJ,
    0x4B => KeyK,
    0x4C => KeyL,
    0x4D => KeyM,
    0x4E => KeyN,
    0x4F => KeyO,
    0x50 => KeyP,
    0x51 => KeyQ,
    0x52 => KeyR,
    0x53 => KeyS,
    0x54 => KeyT,
    0x55 => KeyU,
    0x56 => KeyV,
    0x57 => KeyW,
    0x58 => KeyX,
    0x59 => KeyY,
    0x5A => KeyZ,
    0x5B => MetaLeft,
    0x5C => MetaRight,
    0x5D => ContextMenu,
    0x60 => Numpad0,
    0x61 => Numpad1,
    0x62 => Numpad2,
    0x63 => Numpad3,
    0x64 => Numpad4,
    0x65 => Numpad5,
    0x66 => Numpad6,
    0x67 => Numpad7,
    0x68 => Numpad8,
    0x69 => Numpad9,
    0x6A => NumpadMultiply,
    0x6B => NumpadAdd,
    0x6D => NumpadSubtract,
    0x6E => NumpadDecimal,
    0x6F => NumpadDivide,
    0x70 => F1,
    0x71 => F2,
    0x72 => F3,
    0x73 => F4,
    0x74 => F5,
    0x75 => F6,
    0x76 => F7,
    0x77 => F8,
    0x78 => F9,
    0x79 => F10,
    0x7A => F11,
    0x7B => F12,
    0x7C => F13,
    0x7D => F14,
    0x7E => F15,
    0x7F => F16,
    0x80 => F17,
    0x81 => F18,
    0x82 => F19,
    0x83 => F20,
    0x84 => F21,
    0x85 => F22,
    0x86 => F23,
    0x87 => F24,
    0x90 => NumLock,
    0x91 => ScrollLock,
    0xA0 => ShiftLeft,
    0xA1 => ShiftRight,
    0xA2 => ControlLeft,
    0xA3 => ControlRight,
    0xA4 => AltLeft,
    0xA5 => AltRight,
    0xA6 => BrowserBack,
    0xA7 => BrowserForward,
    0xA8 => BrowserRefresh,
    0xA9 => BrowserStop,
    0xAA => BrowserSearch,
    0xAB => BrowserFavorites,
    0xAC => BrowserHome,
    0xAD => AudioVolumeMute,
    0xAE => AudioVolumeDown,
    0xAF => AudioVolumeUp,
    0xB0 => MediaTrackNext,
    0xB1 => MediaTrackPrevious,
    0xB2 => MediaStop,
    0xB3 => MediaPlayPause,
    0xB4 => LaunchMail,
    0xB5 => MediaSelect,
    0xB6 => LaunchApp1,
    0xB7 => LaunchApp2,
    0xBA => Semicolon,
    0xBB => Equal,
    0xBC => Comma,
    0xBD => Minus,
    0xBE => Period,
    0xBF => Slash,
    0xC0 => Backquote,
    0xDB => BracketLeft,
    0xDC => Backslash,
    0xDD => BracketRight,
    0xDE => Quote,
    0xE2 => IntlBackslash,
    _ => return None,
  })
}

fn config_dir() -> Option<PathBuf> {
  std::env::var_os("LOCALAPPDATA").map(|local_app_data| {
    PathBuf::from(local_app_data)
      .join("Microsoft")
      .join("PowerToys")
      .join("Keyboard Manager")
  })
}

fn active_profile(dir: &Path) -> String {
  let settings = fs::read_to_string(dir.join("settings.json")).ok();
  let profile = settings
    .and_then(|contents| serde_json::from_str::<serde_json::Value>(&contents).ok())
    .and_then(|settings| {
      settings
        .pointer("/properties/activeConfiguration/value")
        .and_then(|value| value.as_str())
        .map(str::to_string)
    })
    .unwrap_or_else(|| "default".to_string());

  // Do not let a profile name escape the Keyboard Manager directory.
  if profile.is_empty()
    || !profile
      .chars()
      .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ' '))
  {
    return "default".to_string();
  }

  profile
}

fn keyboard_manager_enabled(dir: &Path) -> bool {
  let settings_path = dir
    .parent()
    .map(|parent| parent.join("settings.json"))
    .filter(|path| path.exists());

  if let Some(settings_path) = settings_path {
    if let Ok(contents) = fs::read_to_string(settings_path) {
      if let Ok(settings) = serde_json::from_str::<KbmSettings>(&contents) {
        if let Some(enabled) = settings.enabled.get("Keyboard Manager") {
          if !*enabled {
            return false;
          }
        }
      }
    }
  }

  // PowerToys must be running for remaps to be active.
  let system = System::new_with_specifics(
    RefreshKind::nothing().with_processes(ProcessRefreshKind::everything()),
  );

  system.processes().values().any(|process| {
    let name = process.name().to_string_lossy().to_ascii_lowercase();

    name == "powertoys.exe"
      || name == "powertoys.keyboardmanagerengine.exe"
      || name == "powertoys"
      || name == "powertoys.keyboardmanagerengine"
  })
}

fn targets_dorion(target_app: &str) -> bool {
  let file_name = Path::new(target_app)
    .file_name()
    .unwrap_or_default()
    .to_string_lossy();

  file_name.eq_ignore_ascii_case("dorion.exe") || file_name.eq_ignore_ascii_case("dorion")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_single_key_remap() {
    let hotkey = parse_hotkey("220").unwrap();

    assert_eq!(hotkey.key_code, KeyCode::Backslash);
    assert_eq!(hotkey.modifiers, Modifiers::empty());
  }

  #[test]
  fn parses_shortcut_remap() {
    let hotkey = parse_hotkey("17;16;77").unwrap();

    assert_eq!(hotkey.key_code, KeyCode::KeyM);
    assert_eq!(hotkey.modifiers, Modifiers::CONTROL | Modifiers::SHIFT);
  }

  #[test]
  fn translates_remapped_keys() {
    let keymap = PowerToysKeymap {
      remaps: vec![KeyRemap {
        from: Hotkey {
          key_code: KeyCode::Backslash,
          modifiers: Modifiers::empty(),
        },
        to: Some(Hotkey {
          key_code: KeyCode::KeyA,
          modifiers: Modifiers::empty(),
        }),
        single_key: true,
      }],
      single_key_sources: HashSet::from([KeyCode::Backslash]),
      shortcut_sources: HashSet::new(),
    };

    let physical = Hotkey {
      key_code: KeyCode::Backslash,
      modifiers: Modifiers::CONTROL,
    };
    let translated = keymap.translate(&physical).unwrap();

    assert_eq!(translated.key_code, KeyCode::KeyA);
    assert_eq!(translated.modifiers, Modifiers::CONTROL);
    assert!(keymap.is_source(&physical));
  }
}
