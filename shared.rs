//! Settings format shared by the bridge and DS5Tee: ~/.config/ds5bridge.json =
//! {"default": {...}, "pads": {MAC: {...}}, "touchpad_mouse": bool, "bridge": bool, "preview": {...}}
use serde_json::{Map, Value, json};

pub const STATUS_PATH: &str = "/run/ds5bridge/status.json"; // written by the bridge, read by DS5Tee

pub fn defaults() -> Value {
    json!({"speaker": 50, "haptics": 100, "rumble": 100, "trigger_strength_l": 100, "trigger_strength_r": 100,
           "trigger_mode_l": "game", "trigger_mode_r": "game", "color": "", "player": 0, "idle_minutes": 15,
           "gyro": "off", "gyro_sensitivity": 37, "gyro_invert_x": false, "gyro_invert_y": false})
}

/// Per-controller settings: defaults < "default" section < that pad's own section.
pub fn settings_for(config: &Value, mac: &str) -> Map<String, Value> {
    let mut out = match defaults() {
        Value::Object(m) => m,
        _ => unreachable!(),
    };
    let own = if mac.is_empty() { None } else { config["pads"].get(mac) };
    for section in [config.get("default"), own].into_iter().flatten() {
        if let Some(m) = section.as_object() {
            out.extend(m.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
    }
    out
}

pub fn load_json(path: &str) -> Value {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
}

#[allow(dead_code)] // DS5Tee only
/// Atomic write: readers never see half a file.
pub fn save_json(path: &str, v: &Value) -> std::io::Result<()> {
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    std::fs::rename(tmp, path)
}
