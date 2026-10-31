//! DS5Tee: tray icon + settings window for ds5bridge, per controller: speaker, haptics, rumble (each with a
//! short preview played by the bridge), trigger strength/mode per side, gyro on the right stick, lightbar colour
//! (fixed, by battery level or from the game), player LED, auto-disconnect time and a disconnect button, plus
//! whether KDE uses DualSense touchpads as a mouse.
//! Writes ~/.config/ds5bridge.json; the bridge applies it within 0.3 s. Battery/connection come from
//! /run/ds5bridge/status.json. The tray stays small; the window is its own process, started on demand
//! (`DS5Tee --settings`, or `DS5Tee` while the tray already runs) and gone again when closed.
#[path = "../../shared.rs"]
mod shared;

use egui_software_backend::{App, SoftwareBackend, SoftwareBackendAppConfiguration};
use serde_json::{Value, json};
use shared::{STATUS_PATH, load_json, save_json, settings_for};
use std::collections::HashMap;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, mpsc};
use std::time::{Duration, Instant, SystemTime};

static CONFIG: LazyLock<String> =
    LazyLock::new(|| format!("{}/.config/ds5bridge.json", std::env::var("HOME").unwrap_or_default()));
// German UI on a German system, English otherwise.
static DE: LazyLock<bool> = LazyLock::new(|| {
    ["LC_ALL", "LC_MESSAGES", "LANG"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .is_some_and(|v| v.starts_with("de"))
});

fn t(de: &'static str, en: &'static str) -> &'static str {
    if *DE { de } else { en }
}

type Choices = Vec<(Value, &'static str)>;

fn modes() -> Choices {
    vec![(json!("game"), t("Vom Spiel", "From game")), (json!("off"), t("Aus", "Off")),
         (json!("resistance"), t("Widerstand", "Resistance")),
         (json!("weapon"), t("Waffe (Druckpunkt)", "Weapon (trigger point)")), (json!("vibration"), "Vibration")]
}

fn players() -> Choices {
    vec![(json!(0), t("Vom Spiel", "From game")), (json!(1), "1"), (json!(2), "2"), (json!(3), "3"), (json!(4), "4")]
}

fn gyro_modes() -> Choices {
    vec![(json!("off"), t("Aus", "Off")), (json!("aim"), t("Beim Zielen (L2 gedrückt)", "While aiming (L2 held)")),
         (json!("always"), t("Immer", "Always"))]
}

fn idle_times() -> Choices {
    vec![(json!(0), t("Nie", "Never")), (json!(5), "5 min"), (json!(10), "10 min"), (json!(15), "15 min"),
         (json!(30), "30 min"), (json!(60), "60 min")]
}

fn bridge_label() -> &'static str {
    t("Kabel-Emulation (echten Controller verstecken)", "Wired emulation (hide the real controller)")
}

fn no_pad() -> &'static str {
    t("Kein DualSense verbunden", "No DualSense connected")
}

const PREVIEW: [&str; 3] = ["speaker", "haptics", "rumble"]; // sliders that play a sample in the controller

/// One change to the settings file. Every change is applied to a fresh read of the file, so the tray and the
/// window never overwrite each other's edits.
#[derive(Clone)]
enum Op {
    Set(String, &'static str, Value), // pad MAC ("" = the "default" section), key, value
    Top(&'static str, Value),
    Reset(String),
}

fn apply(cfg: &mut Value, op: &Op) {
    if !cfg.is_object() {
        *cfg = json!({});
    }
    for k in ["default", "pads"] {
        if !cfg[k].is_object() {
            cfg[k] = json!({});
        }
    }
    fn section<'a>(cfg: &'a mut Value, mac: &str) -> &'a mut Value {
        if mac.is_empty() { &mut cfg["default"] } else { &mut cfg["pads"][mac] }
    }
    match op {
        Op::Set(mac, key, v) => {
            let s = section(cfg, mac);
            if !s.is_object() {
                *s = json!({});
            }
            s[*key] = v.clone();
        }
        Op::Top(key, v) => cfg[*key] = v.clone(),
        Op::Reset(mac) => *section(cfg, mac) = shared::defaults(),
    }
}

fn update(ops: &[Op]) -> Value {
    let mut cfg = load_json(&CONFIG);
    for op in ops {
        apply(&mut cfg, op);
    }
    if let Some(dir) = std::path::Path::new(CONFIG.as_str()).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = save_json(&CONFIG, &cfg);
    cfg
}

fn mtime(path: &str) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[derive(Clone, PartialEq)]
struct Pad {
    mac: String,
    battery: Option<i64>,
    charging: bool,
}

fn pads() -> Vec<Pad> {
    load_json(STATUS_PATH)["pads"].as_array().map(|a| a.iter().map(|p| Pad {
        mac: p["mac"].as_str().unwrap_or("").into(),
        battery: p["battery"].as_i64(),
        charging: p["charging"].as_bool().unwrap_or(false),
    }).collect()).unwrap_or_default()
}

fn pad_label(p: &Pad) -> String {
    let bat = p.battery.map_or("?".into(), |b| format!("{b} %"));
    let tail = &p.mac[p.mac.len().saturating_sub(5)..];
    format!("DualSense …{tail} – {} {bat}{}", t("Akku", "battery"), if p.charging { " ⚡" } else { "" })
}

fn disconnect(mac: String) {
    std::thread::spawn(move || Command::new("bluetoothctl").args(["disconnect", &mac]).output());
}

/// Single instance lock in the runtime dir; held as long as the returned file lives.
fn lock(name: &str) -> Option<File> {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let f = File::create(format!("{dir}/{name}")).ok()?;
    (unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0).then_some(f)
}

/// DualSense touchpads as desktop pointer in KWin on/off (games still read them).
#[derive(Default)]
struct Touchpads {
    conn: Option<zbus::blocking::Connection>,
    names: HashMap<String, String>, // KWin input device (event N) -> name
    applied: HashMap<String, bool>,
}

impl Touchpads {
    fn proxy<'a>(conn: &'a zbus::blocking::Connection, path: &'a str, iface: &'a str)
                 -> zbus::Result<zbus::blocking::Proxy<'a>> {
        zbus::blocking::proxy::Builder::new(conn).destination("org.kde.KWin")?.path(path)?.interface(iface)?
            .cache_properties(zbus::proxy::CacheProperties::No).build()
    }

    fn apply(&mut self, on: bool) {
        if self.conn.is_none() {
            self.conn = zbus::blocking::Connection::session().ok();
        }
        let Some(conn) = &self.conn else { return };
        let Ok(devices) = Self::proxy(conn, "/org/kde/KWin/InputDevice", "org.kde.KWin.InputDeviceManager")
            .and_then(|p| p.get_property::<Vec<String>>("devicesSysNames")) else { return };
        self.names.retain(|k, _| devices.contains(k)); // removed devices; their event numbers get reused
        self.applied.retain(|k, _| devices.contains(k));
        for dev in devices {
            if self.applied.get(&dev) == Some(&on) {
                continue;
            }
            let path = format!("/org/kde/KWin/InputDevice/{dev}");
            let Ok(p) = Self::proxy(conn, &path, "org.kde.KWin.InputDevice") else { continue };
            let name = self.names.entry(dev.clone()).or_insert_with(|| p.get_property("name").unwrap_or_default());
            if !(name.contains("DualSense") && name.contains("Touchpad")) || p.set_property("enabled", on).is_ok() {
                self.applied.insert(dev, on);
            }
        }
    }
}

/// Arctis Sound Manager moves every Wine stream onto the headset, the pad's 4-channel haptics/speaker
/// stream too (the pad's audio endpoint has 4 channels). Pin those apps to the DualSense sink in ASM's own
/// override list, which its router re-reads every few seconds.
fn keep_haptics_on_pad() {
    let overrides_path = format!("{}/.config/arctis_manager/routing_overrides.json", std::env::var("HOME").unwrap_or_default());
    if !std::fs::exists(&overrides_path).unwrap_or(false) {
        return;
    }
    let Ok(out) = Command::new("pw-dump").output() else { return };
    let Ok(Value::Array(graph)) = serde_json::from_slice::<Value>(&out.stdout) else { return };
    let props: HashMap<i64, &Value> = graph.iter().filter(|o| o["type"] == "PipeWire:Interface:Node")
        .filter_map(|o| Some((o["id"].as_i64()?, &o["info"]["props"]))).collect();
    // ponytail: first DualSense sink; with two pads in two games each stream could need its own pad
    let Some(sink) = props.values().find(|p| p["media.class"] == "Audio/Sink"
        && p["node.name"].as_str().is_some_and(|n| n.contains("DualSense"))).and_then(|p| p["node.name"].as_str())
    else { return };
    let mut channels: HashMap<i64, usize> = HashMap::new();
    for o in &graph {
        if o["type"] == "PipeWire:Interface:Port" && o["info"]["direction"] == "output" {
            if let Some(id) = o["info"]["props"]["node.id"].as_i64() {
                *channels.entry(id).or_default() += 1;
            }
        }
    }
    // Wine opens the pad either via winepulse (FL FR RL RR, named after the .exe) or via its ALSA
    // driver ("PipeWire ALSA [wine64-preloader]", AUX0-3); game audio itself is stereo/5.1/7.1.
    let wine = ["wine64-preloader", "wine-preloader", "wine", "wine64"];
    let apps: Vec<&str> = props.iter().filter(|(id, p)| {
        let name = p["application.name"].as_str().unwrap_or("");
        p["media.class"] == "Stream/Output/Audio" && channels.get(id) == Some(&4)
            && (p["application.process.binary"].as_str().is_some_and(|b| wine.contains(&b)) || name.contains("wine"))
    }).filter_map(|(_, p)| p["application.name"].as_str().filter(|n| !n.is_empty())).collect();
    let mut overrides = load_json(&overrides_path);
    if !overrides.is_object() {
        overrides = json!({});
    }
    if apps.iter().all(|a| overrides[*a] == sink) {
        return;
    }
    for a in apps {
        overrides[a] = json!(sink);
    }
    let _ = save_json(&overrides_path, &overrides);
}

struct Tray {
    pads: Vec<Pad>,
    cfg: Value,
    settings_open: Arc<AtomicBool>,
    wake: mpsc::Sender<()>, // main loop: re-read the settings file now
}

fn open_settings(open: &Arc<AtomicBool>) {
    if open.swap(true, Ordering::SeqCst) {
        return; // already open
    }
    let open = open.clone();
    std::thread::spawn(move || {
        if let Ok(mut child) = std::env::current_exe().and_then(|exe| Command::new(exe).arg("--settings").spawn()) {
            let _ = child.wait();
        }
        open.store(false, Ordering::SeqCst);
    });
}

impl Tray {
    fn change(&mut self, ops: &[Op]) {
        self.cfg = update(ops);
        let _ = self.wake.send(());
    }

    fn bridge_on(&self) -> bool {
        self.cfg["bridge"].as_bool().unwrap_or(true)
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "DS5Tee".into()
    }

    fn title(&self) -> String {
        "DS5Tee".into()
    }

    fn icon_name(&self) -> String {
        "input-gamepad".into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let text = if !self.pads.is_empty() {
            self.pads.iter().map(pad_label).collect::<Vec<_>>().join("\n")
        } else if self.bridge_on() {
            no_pad().into()
        } else {
            t("Kabel-Emulation aus", "Wired emulation off").into()
        };
        ksni::ToolTip { title: "DS5Tee".into(), description: text, ..Default::default() }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        open_settings(&self.settings_open);
    }

    /// Per connected pad a submenu with trigger mode, player ID and disconnect.
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let mut items: Vec<MenuItem<Self>> = vec![
            StandardItem { label: t("Einstellungen …", "Settings …").into(),
                           activate: Box::new(|this: &mut Self| open_settings(&this.settings_open)), ..Default::default() }.into(),
            MenuItem::Separator,
        ];
        if self.pads.is_empty() {
            items.push(StandardItem { label: no_pad().into(), enabled: false, ..Default::default() }.into());
        }
        for p in &self.pads {
            let cfg = settings_for(&self.cfg, &p.mac);
            let mut sub = vec![];
            for (title, choices, keys) in [
                (t("Trigger-Modus (L + R)", "Trigger mode (L + R)"), modes(), &["trigger_mode_l", "trigger_mode_r"][..]),
                ("Controller-ID", players(), &["player"][..]),
            ] {
                let selected = choices.iter().position(|(v, _)| keys.iter().all(|k| cfg.get(*k) == Some(v)))
                    .unwrap_or(usize::MAX);
                let (mac, values): (String, Vec<Value>) = (p.mac.clone(), choices.iter().map(|c| c.0.clone()).collect());
                sub.push(SubMenu { label: title.into(), submenu: vec![RadioGroup {
                    selected,
                    select: Box::new(move |this: &mut Self, i| {
                        let ops: Vec<Op> = keys.iter().map(|k| Op::Set(mac.clone(), k, values[i].clone())).collect();
                        this.change(&ops);
                    }),
                    options: choices.iter().map(|(_, l)| RadioItem { label: (*l).into(), ..Default::default() }).collect(),
                }.into()], ..Default::default() }.into());
            }
            let mac = p.mac.clone();
            sub.push(StandardItem { label: t("Trennen", "Disconnect").into(),
                                    activate: Box::new(move |_: &mut Self| disconnect(mac.clone())), ..Default::default() }.into());
            items.push(SubMenu { label: pad_label(p), submenu: sub, ..Default::default() }.into());
        }
        let touch = self.cfg["touchpad_mouse"].as_bool().unwrap_or(false);
        let bridge = self.bridge_on();
        items.extend([
            MenuItem::Separator,
            CheckmarkItem { label: t("Touchpads als Maus", "Touchpads as mouse").into(), checked: touch,
                            activate: Box::new(move |this: &mut Self| this.change(&[Op::Top("touchpad_mouse", json!(!touch))])),
                            ..Default::default() }.into(),
            CheckmarkItem { label: bridge_label().into(), checked: bridge,
                            activate: Box::new(move |this: &mut Self| this.change(&[Op::Top("bridge", json!(!bridge))])),
                            ..Default::default() }.into(),
            StandardItem { label: t("Beenden", "Quit").into(), activate: Box::new(|_| std::process::exit(0)),
                           ..Default::default() }.into(),
        ]);
        items
    }
}

fn run_tray() {
    use ksni::blocking::TrayMethods;
    let (wake, rx) = mpsc::channel();
    let mut cfg = load_json(&CONFIG);
    let mut pads_now = pads();
    let settings_open = Arc::new(AtomicBool::new(false));
    let handle = Tray { pads: pads_now.clone(), cfg: cfg.clone(), settings_open, wake }.spawn().expect("tray");
    let mut touchpads = Touchpads::default();
    let (mut seen, mut last_status) = (mtime(&CONFIG), Instant::now());
    touchpads.apply(cfg["touchpad_mouse"].as_bool().unwrap_or(false));
    loop {
        let _ = rx.recv_timeout(Duration::from_secs(1)); // a stat per second catches the window's changes
        let m = mtime(&CONFIG);
        let full = last_status.elapsed() >= Duration::from_secs(5);
        if m == seen && !full {
            continue;
        }
        seen = m;
        let new_cfg = load_json(&CONFIG);
        let touch = new_cfg["touchpad_mouse"].as_bool().unwrap_or(false);
        let mut new_pads = pads_now.clone();
        if full {
            last_status = Instant::now();
            new_pads = pads();
            if !new_pads.is_empty() {
                keep_haptics_on_pad();
            }
        }
        touchpads.apply(touch); // also catches pads that reconnected
        if new_cfg != cfg || new_pads != pads_now {
            (cfg, pads_now) = (new_cfg, new_pads);
            let (c, p) = (cfg.clone(), pads_now.clone());
            handle.update(move |t: &mut Tray| (t.cfg, t.pads) = (c, p));
        }
    }
}

struct Settings {
    cfg: Value,
    seen: Option<SystemTime>,
    pads: Vec<Pad>,
    pads_at: Instant,
    mac: String, // "" = the "default" section: used by controllers without own settings
    pending: Vec<Op>,
    save_at: Option<Instant>, // coalesce slider drags
    preview: Option<(&'static str, Instant)>,
}

impl Settings {
    fn new() -> Self {
        let pads = pads();
        Settings { cfg: load_json(&CONFIG), seen: mtime(&CONFIG), mac: pads.first().map(|p| p.mac.clone()).unwrap_or_default(),
                   pads, pads_at: Instant::now(), pending: vec![], save_at: None, preview: None }
    }

    fn get(&self, key: &str) -> Value {
        settings_for(&self.cfg, &self.mac).get(key).cloned().unwrap_or(Value::Null)
    }

    fn change(&mut self, op: Op) {
        apply(&mut self.cfg, &op);
        self.pending.push(op);
        self.save_at = Some(Instant::now() + Duration::from_millis(150));
    }

    fn set(&mut self, key: &'static str, v: Value) {
        self.change(Op::Set(self.mac.clone(), key, v));
    }

    /// Ask the bridge for a sample of the setting that was just changed (it reads the config file).
    fn preview_soon(&mut self, kind: &'static str, ms: u64) {
        self.preview = Some((kind, Instant::now() + Duration::from_millis(ms))); // > save delay: new value is used
    }

    fn poll(&mut self) {
        let now = Instant::now();
        if self.save_at.is_some_and(|t| t <= now) {
            self.cfg = update(&std::mem::take(&mut self.pending));
            self.seen = mtime(&CONFIG);
            self.save_at = None;
        } else if self.save_at.is_none() && mtime(&CONFIG) != self.seen {
            self.seen = mtime(&CONFIG); // changed in the tray menu
            self.cfg = load_json(&CONFIG);
        }
        if let Some((kind, at)) = self.preview.filter(|(_, at)| *at <= now && self.save_at.is_none()) {
            let _ = at;
            let n = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs_f64();
            self.cfg = update(&[Op::Top("preview", json!({"mac": self.mac, "kind": kind, "n": n}))]);
            self.seen = mtime(&CONFIG);
            self.preview = None;
        }
        if now - self.pads_at >= Duration::from_secs(1) {
            self.pads_at = now;
            self.pads = pads();
            if !self.mac.is_empty() && !self.pads.iter().any(|p| p.mac == self.mac) {
                self.mac = self.pads.first().map(|p| p.mac.clone()).unwrap_or_default();
            }
        }
    }

    fn slider(&mut self, ui: &mut egui::Ui, key: &'static str, max: i64) {
        let mut v = self.get(key).as_i64().unwrap_or(0);
        let r = ui.add(egui::Slider::new(&mut v, 0..=max).suffix(" %"));
        let preview = PREVIEW.contains(&key);
        if r.changed() {
            self.set(key, json!(v));
            if preview && !r.dragged() {
                self.preview_soon(key, 1200); // groove click, wheel, arrow keys: wait until the user stops
            }
        }
        if preview && r.drag_stopped() {
            self.preview_soon(key, 200); // sample only when the drag ends, not while holding the handle
        }
    }

    fn combo(&mut self, ui: &mut egui::Ui, key: &'static str, choices: Choices) {
        let cur = self.get(key);
        let text = choices.iter().find(|(v, _)| *v == cur).map_or("?", |c| c.1);
        let mut pick = None;
        egui::ComboBox::from_id_salt(key).width(190.0).selected_text(text).show_ui(ui, |ui| {
            for (v, label) in &choices {
                if ui.selectable_label(*v == cur, *label).clicked() {
                    pick = Some(v.clone());
                }
            }
        });
        if let Some(v) = pick {
            self.set(key, v);
        }
    }

    fn check(&mut self, ui: &mut egui::Ui, key: &'static str, label: &str, default: bool, tip: Option<&str>) {
        let mut on = self.cfg[key].as_bool().unwrap_or(default);
        let r = ui.checkbox(&mut on, label);
        let r = if let Some(tip) = tip { r.on_hover_text(tip) } else { r };
        if r.changed() {
            self.change(Op::Top(key, json!(on)));
        }
    }

    fn form(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().slider_width = 170.0;
        ui.horizontal(|ui| {
            let targets: Vec<(String, String)> = self.pads.iter().map(|p| (p.mac.clone(), pad_label(p)))
                .chain([(String::new(), t("Standard für neue Controller", "Default for new controllers").into())]).collect();
            let current = targets.iter().find(|(m, _)| *m == self.mac).map_or(String::new(), |t| t.1.clone());
            egui::ComboBox::from_id_salt("target").width(360.0).selected_text(egui::RichText::new(current).strong())
                .show_ui(ui, |ui| {
                    for (mac, label) in &targets {
                        ui.selectable_value(&mut self.mac, mac.clone(), label);
                    }
                });
            if let Some(b) = self.pads.iter().find(|p| p.mac == self.mac).and_then(|p| p.battery) {
                let level = b as f32 / 100.0;
                ui.add(egui::ProgressBar::new(level).desired_width(110.0).corner_radius(6).fill(battery_color(level))
                    .text(egui::RichText::new(format!("{b} %")).color(egui::Color32::BLACK)));
            }
        });
        ui.add_space(4.0);
        card(ui, t("Audio & Vibration", "Audio & rumble"), |ui| {
            for (label, key, max) in [(t("Lautsprecher", "Speaker"), "speaker", 100), (t("Haptik", "Haptics"), "haptics", 200),
                                      (t("Vibration", "Rumble"), "rumble", 100)] {
                ui.label(label);
                self.slider(ui, key, max);
                ui.end_row();
            }
        });
        // mode combo and its strength slider share one row
        card(ui, t("Adaptive Trigger", "Adaptive triggers"), |ui| {
            for (label, mode, strength) in [("L2", "trigger_mode_l", "trigger_strength_l"),
                                            ("R2", "trigger_mode_r", "trigger_strength_r")] {
                ui.label(label);
                ui.horizontal(|ui| {
                    self.combo(ui, mode, modes());
                    self.slider(ui, strength, 100);
                });
                ui.end_row();
            }
        });
        card(ui, t("Gyro auf rechten Stick", "Gyro on right stick"), |ui| {
            ui.label(t("Modus", "Mode"));
            ui.horizontal(|ui| {
                self.combo(ui, "gyro", gyro_modes());
                self.slider(ui, "gyro_sensitivity", 300);
            });
            ui.end_row();
            ui.label(t("Spiegeln", "Invert"));
            ui.horizontal(|ui| {
                for (key, label) in [("gyro_invert_x", "Horizontal"), ("gyro_invert_y", t("Vertikal", "Vertical"))] {
                    let mut on = self.get(key).as_bool().unwrap_or(false);
                    if ui.checkbox(&mut on, label).changed() {
                        self.set(key, json!(on));
                    }
                }
            });
            ui.end_row();
        });
        card(ui, t("Licht & Verbindung", "Light & connection"), |ui| {
            ui.label(t("Lichtleiste", "Light bar"));
            ui.horizontal(|ui| {
                let color = self.get("color").as_str().unwrap_or("").to_string();
                let hex = color.strip_prefix('#').and_then(|h| u32::from_str_radix(h, 16).ok());
                let mut rgb = hex.map_or([0x00, 0x50, 0xFF], |v| [(v >> 16) as u8, (v >> 8) as u8, v as u8]);
                if ui.color_edit_button_srgb(&mut rgb).on_hover_text(t("Feste Farbe", "Fixed colour")).changed() {
                    self.set("color", json!(format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])));
                }
                if ui.selectable_label(color.starts_with('#'), t("Feste Farbe", "Fixed colour")).clicked() && !color.starts_with('#') {
                    self.set("color", json!(format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])));
                }
                if ui.selectable_label(color == "battery", t("Akkustand", "Battery level"))
                    .on_hover_text(t("Grün bei vollem Akku, über Gelb (50 %) bis Rot bei leerem Akku",
                                     "Green when full, through yellow (50 %) to red when empty")).clicked() {
                    self.set("color", json!("battery"));
                }
                if ui.selectable_label(color.is_empty(), t("Vom Spiel", "From game")).clicked() {
                    self.set("color", json!(""));
                }
            });
            ui.end_row();
            ui.label("Controller-ID");
            self.combo(ui, "player", players());
            ui.end_row();
            ui.label(t("Auto-Trennen", "Auto-disconnect"));
            self.combo(ui, "idle_minutes", idle_times());
            ui.end_row();
        });
        ui.add_space(4.0);
        self.check(ui, "touchpad_mouse", t("Touchpads als Maus am Desktop (alle Controller)",
                                           "Touchpads as desktop mouse (all controllers)"), false, None);
        self.check(ui, "bridge", bridge_label(), true, Some(t(
            "Aus: Der Controller ist ein normaler Bluetooth-Controller – ohne Haptik, Lautsprecher und adaptive \
             Trigger in Spielen. Nach dem Umschalten das Spiel neu starten.",
            "Off: the controller is a plain Bluetooth controller – no haptics, speaker or adaptive triggers in \
             games. Restart the game after switching.")));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button(t("Zurücksetzen", "Reset")).on_hover_text(t(
                "Alles auf Standard: Lautsprecher 50 %, übrige Regler 100 %, Trigger/Farbe/ID vom Spiel, Gyro aus, Auto-Trennen 15 min",
                "Everything to default: speaker 50 %, other sliders 100 %, triggers/colour/ID from game, gyro off, auto-disconnect 15 min"))
                .clicked() {
                self.change(Op::Reset(self.mac.clone()));
            }
            if ui.add_enabled(!self.mac.is_empty(), egui::Button::new(t("Controller trennen", "Disconnect controller"))).clicked() {
                disconnect(self.mac.clone());
            }
        });
    }
}

const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x00, 0x70, 0xD1); // PlayStation blue

/// Rounded section with a small heading; the label column has a fixed width so all cards line up.
fn card(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new().fill(ui.visuals().faint_bg_color).corner_radius(10).inner_margin(12).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(egui::RichText::new(title.to_uppercase()).size(11.5).strong().color(ACCENT));
        ui.add_space(2.0);
        let mut body = Some(body);
        egui::Grid::new(title).num_columns(2).min_col_width(110.0).spacing([14.0, 8.0])
            .show(ui, |ui| body.take().unwrap()(ui));
    });
    ui.add_space(6.0);
}

/// Same scale as the bridge's "battery" light bar: green, yellow at 50 %, red when empty.
fn battery_color(level: f32) -> egui::Color32 {
    let l = level.clamp(0.0, 1.0);
    let (r, g) = if l > 0.5 { ((2.0 - 2.0 * l) * 255.0, 220.0) } else { (255.0, l * 2.0 * 220.0) };
    egui::Color32::from_rgb(r as u8, g as u8, 60)
}

fn style(ctx: &egui::Context) {
    use egui::{Color32, CornerRadius, Stroke};
    let mut v = egui::Visuals::dark();
    v.panel_fill = Color32::from_rgb(0x15, 0x17, 0x1c);
    v.faint_bg_color = Color32::from_rgb(0x1f, 0x22, 0x29);
    v.extreme_bg_color = Color32::from_rgb(0x0f, 0x11, 0x15);
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    v.slider_trailing_fill = true;
    v.hyperlink_color = ACCENT;
    let w = &mut v.widgets;
    for (wv, fill) in [(&mut w.inactive, 0x2a), (&mut w.hovered, 0x36), (&mut w.active, 0x40), (&mut w.open, 0x30)] {
        wv.bg_fill = Color32::from_gray(fill);
        wv.weak_bg_fill = Color32::from_gray(fill);
        wv.corner_radius = CornerRadius::same(4);
        wv.bg_stroke = Stroke::NONE;
    }
    w.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    w.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    w.noninteractive.corner_radius = CornerRadius::same(4);
    ctx.set_visuals(v);
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(10.0, 8.0);
        s.spacing.button_padding = egui::vec2(10.0, 4.0);
        s.spacing.interact_size.y = 24.0;
    });
}

impl App for Settings {
    fn ui(&mut self, ui: &mut egui::Ui, _backend: &mut SoftwareBackend) {
        self.poll();
        egui::CentralPanel::default().show_inside(ui, |ui| egui::ScrollArea::vertical().show(ui, |ui| self.form(ui)));
        ui.ctx().request_repaint_after(Duration::from_millis(if self.save_at.is_some() || self.preview.is_some() { 50 } else { 1000 }));
    }

    fn on_exit(&mut self, _ctx: &egui::Context) {
        if !self.pending.is_empty() {
            update(&self.pending);
        }
    }
}

fn run_settings() {
    let Some(_lock) = lock("ds5tee-settings.lock") else { return }; // already open
    let mut config = SoftwareBackendAppConfiguration::default();
    config.viewport_builder = egui::ViewportBuilder::default().with_title("DS5Tee").with_app_id("DS5Tee")
        .with_inner_size([640.0, 690.0]);
    let _ = egui_software_backend::run_app_with_software_backend(config, |ctx| {
        style(&ctx);
        Settings::new()
    });
}

fn main() {
    if std::env::args().any(|a| a == "--settings") {
        return run_settings();
    }
    match lock("ds5tee.lock") {
        Some(_lock) => run_tray(),
        None => run_settings(), // the tray already runs: launching DS5Tee again opens the settings
    }
}
