//! Settings format shared by the bridge and DS5Tee: ~/.config/ds5bridge.json =
//! {"default": {...}, "pads": {MAC: {...}}, "touchpad_mouse": bool, "bridge": bool, "bt_mic": bool, "preview": {...}}
use serde_json::{Map, Value, json};

pub const STATUS_PATH: &str = "/run/ds5bridge/status.json"; // written by the bridge, read by DS5Tee

pub fn defaults() -> Value {
    json!({"speaker": 50, "haptics": 100, "rumble": 100, "trigger_strength_l": 100, "trigger_strength_r": 100,
           "trigger_mode_l": "game", "trigger_mode_r": "game", "color": "", "battery_smooth": true,
           // green outshines red on the LED: the extra 30 % point keeps the bar from looking orange when nearly empty
           "battery_points": [{"pct": 10, "color": "#ff0000"}, {"pct": 30, "color": "#ff3700"},
                              {"pct": 50, "color": "#ffff00"}, {"pct": 100, "color": "#00ff00"}],
           "brightness": 100, "player": 0, "idle_minutes": 15,
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

pub fn parse_color(c: &str) -> Option<[u8; 3]> {
    let h = c.strip_prefix('#')?;
    let v = u32::from_str_radix(h, 16).ok().filter(|_| h.len() == 6)?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

/// The "battery" lightbar's points as (percent, colour), sorted by percent.
pub fn battery_points(cfg: &Map<String, Value>) -> Vec<(f64, [u8; 3])> {
    let mut points: Vec<(f64, [u8; 3])> = cfg.get("battery_points").and_then(Value::as_array).into_iter().flatten()
        .filter_map(|p| Some((p["pct"].as_f64()?, parse_color(p["color"].as_str()?)?))).collect();
    points.sort_by(|a, b| a.0.total_cmp(&b.0));
    points
}

/// Colour at `pct`. Smooth: blends between the points. Otherwise each point's colour holds up to the next point.
pub fn battery_color(pct: f64, points: &[(f64, [u8; 3])], smooth: bool) -> [u8; 3] {
    let next = points.iter().position(|p| p.0 > pct);
    match next {
        _ if points.is_empty() => [255; 3],
        None => points[points.len() - 1].1,
        Some(0) => points[0].1,
        Some(i) if !smooth => points[i - 1].1,
        Some(i) => {
            let ((p0, c0), (p1, c1)) = (points[i - 1], points[i]);
            let mix = (pct - p0) / (p1 - p0);
            std::array::from_fn(|k| (c0[k] as f64 + (c1[k] as f64 - c0[k] as f64) * mix).round_ties_even() as u8)
        }
    }
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
