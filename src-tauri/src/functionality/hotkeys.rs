#[cfg(target_os = "windows")]
use livesplit_hotkey::Hotkey;
use livesplit_hotkey::{ConsumePreference, Hook};
use serde::{Deserialize, Serialize};
use std::{
  collections::HashMap,
  sync::{
    Arc, LazyLock, Mutex,
    atomic::{AtomicBool, Ordering},
  },
  time::{Duration, Instant},
};
use tauri::{Emitter, Listener};

use crate::{
  config::{get_config, set_config},
  functionality::keyboard::keystructs_to_hotkey,
  log,
};

use super::keyboard::{KeyStruct, KeybindChangedEvent};
#[cfg(target_os = "windows")]
use super::powertoys;

pub static KEYBINDS_CHANGED: AtomicBool = AtomicBool::new(false);
pub static PTT_ENABLED: AtomicBool = AtomicBool::new(false);
static RECENT_ACTIONS: LazyLock<Mutex<HashMap<String, (Instant, Option<bool>)>>> =
  LazyLock::new(|| Mutex::new(HashMap::new()));

#[tauri::command]
pub fn get_keybinds() -> HashMap<String, Vec<KeyStruct>> {
  let config = get_config();
  config.keybinds.unwrap_or_default()
}

#[tauri::command]
pub fn set_keybinds(keybinds: HashMap<String, Vec<KeyStruct>>) {
  let mut config = get_config();
  config.keybinds = Some(keybinds);

  set_config(config);

  KEYBINDS_CHANGED.store(true, std::sync::atomic::Ordering::Relaxed);
}

#[tauri::command]
pub fn set_keybind(action: String, keys: Vec<KeyStruct>) {
  let mut keybinds = get_keybinds();
  keybinds.insert(action, keys);

  set_keybinds(keybinds);
}

#[tauri::command]
#[cfg(target_os = "windows")]
pub fn trigger_keys_pressed(
  win: tauri::WebviewWindow<crate::Runtime>,
  keys: Vec<KeyStruct>,
  pressed: bool,
) {
  let keybinds = get_keybinds();

  // Convert the input keys to a hotkey for comparison
  let mut input_hotkey = match keystructs_to_hotkey(&keys) {
    Some(hotkey) => hotkey,
    None => {
      return;
    }
  };

  #[cfg(target_os = "windows")]
  let keymap = powertoys::get();

  // PowerToys remaps keys before Dorion sees them, but the webview still
  // receives the physical key. Translate it so focused keybinds match too.
  #[cfg(target_os = "windows")]
  if let Some(translated) = keymap.translate(&input_hotkey) {
    input_hotkey = translated;
  }

  // Find matching action(s) for these keys
  for (action, action_keys) in &keybinds {
    if let Some(action_hotkey) = keystructs_to_hotkey(action_keys) {
      // A keybind using a remapped source key no longer exists in the keymap.
      #[cfg(target_os = "windows")]
      if keymap.is_source(&action_hotkey) {
        continue;
      }

      if hotkeys_match(&input_hotkey, &action_hotkey) {
        log!("Key combination matched action: {}", action);

        if action.starts_with("PUSH") {
          handle_push_action(&win, action, pressed);
        } else {
          // For regular actions, only trigger on key press
          if pressed {
            handle_regular_action(&win, action);
          }
        }

        // Only trigger the first matching action to avoid conflicts
        return;
      }
    }
  }
}

pub fn start_keybind_watcher(win: &tauri::WebviewWindow<crate::Runtime>) {
  PTT_ENABLED.store(
    get_config().push_to_talk.unwrap_or(false),
    std::sync::atomic::Ordering::Relaxed,
  );
  let win_hook = win.clone();
  let hook = match new_hook(win_hook.clone()) {
    Ok(hook) => hook,
    Err(e) => {
      log!("Failed to create new keybind hook: {}", e);
      return;
    }
  };
  let hook = Arc::new(Mutex::new(hook));

  let keybind_hook = hook.clone();
  let keybind_win = win_hook.clone();
  win.listen("keybinds_changed", move |evt| {
    let payload = evt.payload();
    if payload.is_empty() {
      return;
    }

    let keybinds: Vec<KeybindChangedEvent> = serde_json::from_str(payload).unwrap();
    let mut keybinds_map = HashMap::new();

    for keybind in keybinds {
      keybinds_map.insert(keybind.key, keybind.keys);
    }

    set_keybinds(keybinds_map);

    // Drop and recreate the hook to apply new keybinds
    *keybind_hook.lock().unwrap() = match new_hook(keybind_win.clone()) {
      Ok(new_hook) => new_hook,
      Err(e) => {
        log!("Failed to recreate keybind hook: {}", e);
        return;
      }
    };

    KEYBINDS_CHANGED.store(true, std::sync::atomic::Ordering::Relaxed);
  });

  // PowerToys can change its remaps while Dorion is running. Re-read its
  // config and rebuild the hook so keybinds immediately follow the new keymap.
  #[cfg(target_os = "windows")]
  {
    let watch_win = win.clone();
    std::thread::spawn(move || {
      let mut last_mtime = powertoys::config_mtime();

      loop {
        std::thread::sleep(Duration::from_secs(5));

        let mtime = powertoys::config_mtime();
        if mtime == last_mtime {
          continue;
        }

        last_mtime = mtime;

        powertoys::reload();
        let _ = watch_win.emit("powertoys_keymap_changed", ());
      }
    });

    let keymap_hook = hook.clone();
    let keymap_win = win_hook.clone();
    win.listen("powertoys_keymap_changed", move |_| {
      *keymap_hook.lock().unwrap() = match new_hook(keymap_win.clone()) {
        Ok(new_hook) => new_hook,
        Err(e) => {
          log!(
            "Failed to recreate keybind hook after PowerToys keymap change: {}",
            e
          );
          return;
        }
      };
    });
  }

  win.listen("ptt_toggled", |evt| {
    #[derive(Serialize, Deserialize)]
    struct PTTPayload {
      state: bool,
    }

    let payload = evt.payload();

    log!("PTT enabled: {:?}", payload);

    if payload.is_empty() {
      return;
    }

    let state = serde_json::from_str::<PTTPayload>(payload).unwrap();
    PTT_ENABLED.store(state.state, std::sync::atomic::Ordering::Relaxed);

    let mut config = get_config();
    config.push_to_talk = Some(state.state);
    set_config(config);
  });
}

fn new_hook(
  win: tauri::WebviewWindow<crate::Runtime>,
) -> Result<Arc<Hook>, Box<dyn std::error::Error>> {
  let hook = Arc::new(
    match Hook::with_consume_preference(ConsumePreference::MustNotConsume) {
      Ok(hook) => Ok(hook),
      Err(e) => {
        log!(
          "Failed to create Hook with MustNotConsume preference: {}. Global keybinds will still work, but may now eat your inputs.",
          e
        );
        // PreferNoConsume if we can't force no consumption
        Hook::with_consume_preference(ConsumePreference::PreferNoConsume)
      }
    }?,
  );
  // Register keybinds
  register_all_keybinds(&win, &hook, &get_keybinds());
  Ok(hook)
}

fn register_all_keybinds(
  win: &tauri::WebviewWindow<crate::Runtime>,
  hook: &Arc<Hook>,
  keybinds: &HashMap<String, Vec<KeyStruct>>,
) {
  #[cfg(target_os = "windows")]
  let keymap = powertoys::reload();

  for (action, keys) in keybinds {
    let win = win.clone();
    let hotkey = match keystructs_to_hotkey(keys) {
      Some(hotkey) => hotkey,
      None => {
        log!("Invalid keybind for action {}: {:?}", action, keys);
        continue;
      }
    };

    // PowerToys replaces this key with a different one, so the old keybind no
    // longer exists. The remapped target is matched by the injected key event.
    #[cfg(target_os = "windows")]
    if keymap.is_source(&hotkey) {
      log!(
        "Skipping keybind for action {} because PowerToys remaps {}",
        action,
        hotkey
      );
      continue;
    }

    let action_clone = action.clone();

    if action.starts_with("PUSH") {
      let callback = move |pressed| {
        handle_push_action(&win, &action_clone, pressed);
      };

      match hook.register_specific(hotkey, callback) {
        Ok(_) => {
          log!("Registered PTT keybind: {:?}", hotkey);
        }
        Err(e) => {
          log!("Failed to register PTT keybind: {}: {}", hotkey, e);
        }
      }
    } else {
      let callback = move || {
        handle_regular_action(&win, &action_clone);
      };

      match hook.register(hotkey, callback) {
        Ok(_) => {
          log!("Registered keybind: {:?}", hotkey);
        }
        Err(e) => {
          log!("Failed to register keybind: {}: {}", hotkey, e);
        }
      };
    }
  }
}

/// Both the global hook and the focused-window webview listener can see the
/// same key press. Drop the duplicate so toggle actions don't fire twice.
fn should_emit(action: &str, pressed: Option<bool>) -> bool {
  const DEDUPE_WINDOW: Duration = Duration::from_millis(150);

  let now = Instant::now();
  let mut recent = RECENT_ACTIONS.lock().unwrap();

  if let Some((last, last_pressed)) = recent.get(action)
    && now.duration_since(*last) < DEDUPE_WINDOW
    && (pressed.is_none() || *last_pressed == pressed)
  {
    return false;
  }

  recent.insert(action.to_string(), (now, pressed));

  true
}

fn handle_push_action(win: &tauri::WebviewWindow<crate::Runtime>, action: &str, pressed: bool) {
  if !PTT_ENABLED.load(Ordering::Relaxed) && action == "PUSH_TO_TALK" {
    return;
  }

  if !should_emit(action, Some(pressed)) {
    return;
  }

  if pressed {
    win
      .emit("keybind_pressed", action)
      .expect("Failed to emit keybind_pressed event");
  } else {
    win
      .emit("keybind_released", action)
      .expect("Failed to emit keybind_released event");
  }
}

fn handle_regular_action(win: &tauri::WebviewWindow<crate::Runtime>, action: &str) {
  if !should_emit(action, None) {
    return;
  }

  log!("Regular action triggered: {}", action);

  win
    .emit("keybind_pressed", action)
    .expect("Failed to emit keybind_pressed event");
}

#[cfg(target_os = "windows")]
fn hotkeys_match(hotkey1: &Hotkey, hotkey2: &Hotkey) -> bool {
  hotkey1.key_code == hotkey2.key_code && hotkey1.modifiers == hotkey2.modifiers
}
