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
use shared::{STATUS_PATH, battery_color, battery_points, load_json, parse_color, save_json, settings_for};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, mpsc};
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
    vec![(json!("off"), t("Aus", "Off")), (json!("aim"), t("Beim Zielen (L2)", "While aiming (L2)")),
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

    /// Middle click: disconnect all connected controllers.
    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        self.pads.iter().for_each(|p| disconnect(p.mac.clone()));
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

/// Live pad state for the input card, read from the pad's own evdev nodes.
#[derive(Clone, Copy)]
struct Input {
    online: bool,
    keys: u32, // bit = code - BTN_SOUTH
    abs: [i32; 6], // left x, y, L2, right x, y, R2 (evdev order)
    hat: [i32; 2],
    touch: [Option<[i32; 2]>; 2], // the pad tracks two fingers
    touch_at: [[i32; 2]; 2],
    slot: usize,
    click: bool,
}

const BTN_SOUTH: u16 = 0x130;
const BTN_X: u16 = 0x130;
const BTN_CIRCLE: u16 = 0x131;
const BTN_TRIANGLE: u16 = 0x133;
const BTN_SQUARE: u16 = 0x134;
const BTN_L1: u16 = 0x136;
const BTN_R1: u16 = 0x137;
const BTN_L2: u16 = 0x138;
const BTN_R2: u16 = 0x139;
const BTN_CREATE: u16 = 0x13A;
const BTN_OPTIONS: u16 = 0x13B;
const BTN_PS: u16 = 0x13C;
const BTN_L3: u16 = 0x13D;
const BTN_R3: u16 = 0x13E;

impl Input {
    fn new(online: bool) -> Self {
        Input { online, keys: 0, abs: [128, 128, 0, 128, 128, 0], hat: [0; 2], touch: [None; 2], touch_at: [[0; 2]; 2], slot: 0, click: false }
    }

    fn key(&self, code: u16) -> bool {
        self.keys & 1 << (code - BTN_SOUTH) != 0
    }

    /// One evdev event; true when the picture changes (sticks only past their resting jitter, or an open window redraws forever).
    fn apply(&mut self, touchpad: bool, kind: u16, code: u16, value: i32) -> bool {
        const EV_KEY: u16 = 1;
        const EV_ABS: u16 = 3;
        let old = (self.keys, self.abs, self.hat, self.touch, self.click);
        match (touchpad, kind, code) {
            (true, EV_KEY, 0x110) => self.click = value != 0,
            (true, EV_ABS, 0x2F) => self.slot = value.clamp(0, 1) as usize, // ABS_MT_SLOT
            (true, EV_ABS, 0x39) => self.touch[self.slot] = (value >= 0).then_some(self.touch_at[self.slot]), // ABS_MT_TRACKING_ID
            (true, EV_ABS, 0x35 | 0x36) => { // ABS_MT_POSITION_X, _Y
                let axis = code as usize - 0x35;
                self.touch_at[self.slot][axis] = value;
                if let Some(at) = &mut self.touch[self.slot] {
                    if at[axis].abs_diff(value) >= 8 {
                        at[axis] = value;
                    }
                }
            }
            (false, EV_KEY, 0x130..=0x13E) => {
                let bit = 1 << (code - BTN_SOUTH);
                self.keys = if value != 0 { self.keys | bit } else { self.keys & !bit };
            }
            (false, EV_ABS, 0..=5) if self.abs[code as usize].abs_diff(value) >= 3 || value == 0 || value == 255 => {
                self.abs[code as usize] = value;
            }
            (false, EV_ABS, 0x10 | 0x11) => self.hat[code as usize - 0x10] = value,
            _ => {}
        }
        old != (self.keys, self.abs, self.hat, self.touch, self.click)
    }
}

/// The virtual USB pad reports the real MAC with the locally-administered bit flipped (see the bridge's feature 0x09).
fn virtual_mac(mac: &str) -> String {
    let first = u8::from_str_radix(mac.get(..2).unwrap_or("0"), 16).unwrap_or(0) ^ 0x02;
    format!("{first:02x}{}", mac.get(2..).unwrap_or(""))
}

/// Gamepad and touchpad evdev nodes of a pad: (file, is touchpad). Fails silently on nodes the bridge hides.
fn input_nodes(mac: &str) -> Vec<(File, bool)> {
    let mut out = vec![];
    if mac.is_empty() {
        return out;
    }
    let (mac, virt) = (mac.to_lowercase(), virtual_mac(mac).to_lowercase());
    for e in std::fs::read_dir("/sys/class/input").into_iter().flatten().flatten() {
        let node = e.file_name().to_string_lossy().into_owned();
        if !node.starts_with("event") {
            continue;
        }
        let dev = e.path().join("device");
        let read = |f: &str| std::fs::read_to_string(dev.join(f)).unwrap_or_default().trim().to_lowercase();
        let (name, uniq) = (read("name"), read("uniq"));
        let touchpad = name.ends_with("dualsense wireless controller touchpad");
        if (uniq != mac && uniq != virt) || !(touchpad || name.ends_with("dualsense wireless controller")) {
            continue;
        }
        if let Ok(f) = OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(format!("/dev/input/{node}")) {
            out.push((f, touchpad));
        }
    }
    out
}

/// Earliest pending redraw. egui_software_backend 0.0.3 redraws at once on every request_repaint_after
/// (it ignores the delay), so a 1 s refresh turned into an endless CPU render loop. Delayed redraws go here instead.
static REDRAW_AT: Mutex<Option<Instant>> = Mutex::new(None);

fn redraw_after(delay: Duration) {
    let at = Instant::now() + delay;
    let mut due = REDRAW_AT.lock().unwrap();
    if due.is_none_or(|d| at < d) {
        *due = Some(at);
    }
}

/// Fires the pending redraw when it is due, checking 30 times a second.
fn redraw_ticker(ctx: egui::Context) {
    loop {
        std::thread::sleep(Duration::from_millis(33));
        if REDRAW_AT.lock().unwrap().take_if(|d| *d <= Instant::now()).is_some() {
            ctx.request_repaint();
        }
    }
}

/// Keeps `state` current for the pad named in `mac`; asks for a redraw at most about 30 times a second.
fn input_reader(mac: Arc<Mutex<String>>, state: Arc<Mutex<Input>>, ctx: egui::Context) {
    loop {
        let want = mac.lock().unwrap().clone();
        let mut nodes = input_nodes(&want);
        *state.lock().unwrap() = Input::new(!nodes.is_empty());
        ctx.request_repaint();
        if nodes.is_empty() {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        'open: while *mac.lock().unwrap() == want {
            let mut fds: Vec<libc::pollfd> = nodes.iter()
                .map(|(f, _)| libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLIN, revents: 0 }).collect();
            unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 500) };
            let mut changed = false;
            for (node, touchpad) in &mut nodes {
                let mut buf = [0u8; 24 * 64];
                match node.read(&mut buf) {
                    Ok(n) => for ev in buf[..n].chunks_exact(24) {
                        let kind = u16::from_ne_bytes([ev[16], ev[17]]);
                        let code = u16::from_ne_bytes([ev[18], ev[19]]);
                        let value = i32::from_ne_bytes([ev[20], ev[21], ev[22], ev[23]]);
                        changed |= state.lock().unwrap().apply(*touchpad, kind, code, value);
                    },
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => break 'open, // pad gone
                }
            }
            if changed {
                redraw_after(Duration::ZERO);
            }
        }
    }
}

type Curve = [[f32; 2]; 3]; // control, control, end

/// Cubic curves as a polyline, the end point included.
fn curve(start: [f32; 2], curves: &[Curve]) -> Vec<[f32; 2]> {
    let mut out = vec![];
    let mut from = start;
    for &[c1, c2, to] in curves {
        for k in 0..16 {
            let t = k as f32 / 16.0;
            let u = 1.0 - t;
            out.push(std::array::from_fn::<f32, 2, _>(|i| {
                u * u * u * from[i] + 3.0 * u * u * t * c1[i] + 3.0 * u * t * t * c2[i] + t * t * t * to[i]
            }));
        }
        from = to;
    }
    out.push(from);
    out
}

/// `half` (ending on the centre line) continued as its mirror image on the 240 wide grid.
fn mirrored(mut half: Vec<[f32; 2]>) -> Vec<[f32; 2]> {
    let other: Vec<[f32; 2]> = half.iter().rev().skip(1).map(|&[x, y]| [240.0 - x, y]).collect();
    half.extend(other);
    half
}

const GRIP_START: [f32; 2] = [28.0, 164.0];
const INNER_EDGE: [Curve; 2] = [
    [[40.0, 163.0], [50.0, 140.0], [64.0, 122.0]],
    [[76.0, 110.0], [92.0, 111.0], [120.0, 111.0]],
];

/// Shell outline, starting at the top centre and running down the left side.
fn shell_outline() -> Vec<[f32; 2]> {
    let mut half = curve([120.0, 28.0], &[
        [[100.0, 27.0], [84.0, 27.0], [72.0, 28.0]],
        [[62.0, 24.0], [46.0, 22.0], [38.0, 24.0]],
        [[28.0, 26.0], [22.0, 34.0], [18.0, 46.0]],
        [[10.0, 74.0], [4.0, 104.0], [6.0, 132.0]],
        [[7.0, 152.0], [16.0, 165.0], GRIP_START],
    ]);
    half.pop();
    half.extend(curve(GRIP_START, &INNER_EDGE));
    let mut all = mirrored(half);
    all.pop(); // closed: the last point is the first one
    all
}

/// The darker core between the seams: seam down to the grip, inner edge to the centre line, mirrored.
fn core_outline() -> Vec<[f32; 2]> {
    let mut half = seam();
    half.pop();
    half.extend(curve(GRIP_START, &INNER_EDGE));
    mirrored(half)
}

/// Left seam from the touchpad's corner to the grip.
fn seam() -> Vec<[f32; 2]> {
    curve([88.0, 66.0], &[[[74.0, 86.0], [50.0, 116.0], GRIP_START]])
}

fn touchpad_outline() -> Vec<[f32; 2]> {
    let mut half = curve([120.0, 29.0], &[
        [[104.0, 29.0], [88.0, 29.0], [76.0, 30.0]],
        [[78.0, 46.0], [82.0, 60.0], [92.0, 67.0]],
        [[100.0, 69.0], [110.0, 69.0], [120.0, 69.0]],
    ]);
    half.reverse();
    let mut all = mirrored(half);
    all.pop();
    all
}

type Filled = LazyLock<(Vec<[f32; 2]>, Vec<[u32; 3]>)>;
static SHELL: Filled = LazyLock::new(|| { let o = shell_outline(); let t = triangulate(&o); (o, t) });
static CORE: Filled = LazyLock::new(|| { let o = core_outline(); let t = triangulate(&o); (o, t) });
// Shoulder buttons as one tongue: L1 with a rounded end against the shell, L2 tapering away from it to its tip.
static L1_SHAPE: Filled = LazyLock::new(|| {
    let mut o = curve([25.0, 7.0], &[
        [[24.0, 17.0], [30.0, 25.0], [46.0, 25.0]],
        [[60.0, 25.0], [68.0, 17.0], [68.0, 7.0]],
        [[58.0, 4.0], [36.0, 4.0], [25.0, 7.0]],
    ]);
    o.pop();
    let t = triangulate(&o);
    (o, t)
});
const L2_BASE: f32 = 5.0;
const L2_TOP: f32 = -23.0;
static L2_SHAPE: Filled = LazyLock::new(|| {
    let mut o = curve([25.0, L2_BASE], &[
        [[36.0, 2.0], [58.0, 2.0], [68.0, L2_BASE]],
        [[68.0, -5.0], [62.0, -16.0], [54.0, -21.0]],
        [[51.0, L2_TOP], [46.0, L2_TOP], [43.0, -21.0]],
        [[34.0, -15.0], [26.0, -5.0], [25.0, L2_BASE]],
    ]);
    o.pop();
    let t = triangulate(&o);
    (o, t)
});
static TOUCHPAD: Filled = LazyLock::new(|| { let o = touchpad_outline(); let t = triangulate(&o); (o, t) });

fn cross(o: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

/// Ear clipping: egui only fills convex shapes, the shell is not.
fn triangulate(points: &[[f32; 2]]) -> Vec<[u32; 3]> {
    let area: f32 = (0..points.len()).map(|i| cross([0.0, 0.0], points[i], points[(i + 1) % points.len()])).sum();
    let mut left: Vec<usize> = (0..points.len()).collect();
    let mut out = vec![];
    while left.len() > 2 {
        let n = left.len();
        let ear = (0..n).find(|&i| {
            let (a, b, c) = (points[left[(i + n - 1) % n]], points[left[i]], points[left[(i + 1) % n]]);
            cross(a, b, c) * area > 0.0 && !left.iter().any(|&q| {
                let q = points[q];
                q != a && q != b && q != c
                    && cross(a, b, q) * area >= 0.0 && cross(b, c, q) * area >= 0.0 && cross(c, a, q) * area >= 0.0
            })
        });
        let Some(i) = ear else { break }; // only on a self-crossing outline
        out.push([left[(i + n - 1) % n] as u32, left[i] as u32, left[(i + 1) % n] as u32]);
        left.remove(i);
    }
    out
}

/// The controller as seen from above, lit where it is being used. Drawn on a 240 x 192 grid.
fn draw_input(ui: &mut egui::Ui, i: &Input) {
    use egui::{Color32, CornerRadius, Pos2, Rect, Stroke, StrokeKind, pos2, vec2};
    let width = ui.available_width().min(340.0);
    let scale = width / 240.0;
    let (mut area, _) = ui.allocate_exact_size(vec2(ui.available_width(), 192.0 * scale), egui::Sense::hover());
    area = Rect::from_center_size(area.center(), vec2(width, 192.0 * scale));
    let p = ui.painter_at(area);
    let at = |x: f32, y: f32| area.min + vec2(x, y + 24.0) * scale; // the shoulder buttons stand above y = 0
    let rect = |x0: f32, y0: f32, x1: f32, y1: f32| Rect::from_min_max(at(x0, y0), at(x1, y1));
    let (body, part, on) = (Color32::from_gray(0x2c), Color32::from_gray(0x4a), ACCENT);
    let fill = |pressed: bool| if pressed { on } else { part };
    let fill_in = |painter: &egui::Painter, shape: &Filled, mirror: bool, color: &dyn Fn(f32) -> Color32| {
        let (grid, triangles) = &**shape;
        let mut mesh = egui::epaint::Mesh::default();
        for &[x, y] in grid {
            mesh.colored_vertex(at(if mirror { 240.0 - x } else { x }, y), color(y));
        }
        triangles.iter().for_each(|&[a, b, c]| mesh.add_triangle(a, b, c));
        painter.add(egui::Shape::mesh(mesh));
    };
    let fill_shape = |shape: &Filled, color: &dyn Fn(f32) -> Color32| fill_in(&p, shape, false, color);
    let line = |grid: &[[f32; 2]], stroke: Stroke| {
        p.add(egui::Shape::line(grid.iter().map(|&[x, y]| at(x, y)).collect(), stroke));
    };
    // shoulder buttons behind the shell; L2 fills towards its tip with its travel
    let outline = |shape: &Filled, mirror: bool, pressed: bool| {
        let mut grid: Vec<[f32; 2]> = shape.0.iter().map(|&[x, y]| [if mirror { 240.0 - x } else { x }, y]).collect();
        grid.push(grid[0]);
        line(&grid, Stroke::new(1.0_f32 * scale, if pressed { on } else { Color32::from_gray(0x5A) }));
    };
    for (mirror, level, l1, l2) in [(false, i.abs[2], BTN_L1, BTN_L2), (true, i.abs[5], BTN_R1, BTN_R2)] {
        fill_in(&p, &L1_SHAPE, mirror, &|_| fill(i.key(l1)));
        outline(&L1_SHAPE, mirror, i.key(l1));
        fill_in(&p, &L2_SHAPE, mirror, &|_| body);
        let top = L2_BASE - (L2_BASE - L2_TOP) * level as f32 / 255.0;
        fill_in(&p.with_clip_rect(rect(0.0, top, 240.0, L2_BASE + 1.0)), &L2_SHAPE, mirror, &|_| on);
        outline(&L2_SHAPE, mirror, i.key(l2));
    }
    // shell slightly lighter towards the top, a darker core between the seams, the touchpad on top
    let shade = |y: f32| Color32::from_gray((0x3E as f32 - 0x18 as f32 * ((y - 24.0) / 142.0).clamp(0.0, 1.0)) as u8);
    fill_shape(&SHELL, &shade);
    fill_shape(&CORE, &|_| Color32::from_gray(0x20));
    let edge = Stroke::new(1.0_f32 * scale, Color32::from_gray(0x4C));
    let mut closed = SHELL.0.clone();
    closed.push(closed[0]);
    line(&closed, edge);
    let seam_stroke = Stroke::new(1.4_f32 * scale, Color32::from_gray(0x12));
    line(&seam(), seam_stroke);
    line(&seam().into_iter().map(|[x, y]| [240.0 - x, y]).collect::<Vec<_>>(), seam_stroke);
    let finger = |t: &[i32; 2]| {
        let v = t[1] as f32 / 1079.0;
        let half = 42.0 - 12.0 * v; // follows the narrowing pad
        at(120.0 + (t[0] as f32 / 1919.0 - 0.5) * 2.0 * half, 31.0 + 36.0 * v)
    };
    let fingers: Vec<Pos2> = i.touch.iter().flatten().map(finger).collect();
    // pressed down: lit around the fingers, the whole pad when the press has no tracked finger
    let pad_color = if i.click && fingers.is_empty() { on } else { part };
    fill_shape(&TOUCHPAD, &|_| pad_color);
    if i.click {
        // ponytail: clipped to a rect, not the pad's curve; the glow can touch the bottom corners by a few pixels
        let clipped = p.with_clip_rect(rect(84.0, 30.0, 156.0, 68.0));
        fingers.iter().for_each(|f| { clipped.circle_filled(*f, 11.0 * scale, on); });
    }
    fingers.iter().for_each(|f| { p.circle_filled(*f, 3.5 * scale, Color32::WHITE); });
    p.circle_filled(at(66.0, 42.0), 3.5 * scale, fill(i.key(BTN_CREATE)));
    p.circle_filled(at(174.0, 42.0), 3.5 * scale, fill(i.key(BTN_OPTIONS)));
    p.circle_filled(at(120.0, 88.0), 4.5 * scale, fill(i.key(BTN_PS)));
    // d-pad: four arms pointing at the centre
    let dpad = at(44.0, 70.0);
    for (d, pressed) in [(vec2(-1.0, 0.0), i.hat[0] < 0), (vec2(1.0, 0.0), i.hat[0] > 0),
                         (vec2(0.0, -1.0), i.hat[1] < 0), (vec2(0.0, 1.0), i.hat[1] > 0)] {
        let side = vec2(-d.y, d.x) * 5.0 * scale;
        let (near, far) = (dpad + d * 8.0 * scale, dpad + d * 19.0 * scale);
        p.add(egui::Shape::convex_polygon(vec![dpad + d * 3.0 * scale, near + side, far + side, far - side, near - side],
                                          fill(pressed), Stroke::new(0.5_f32, fill(pressed))));
    }
    // face buttons, each with its own small symbol
    let (fx, fy, gap, r) = (196.0, 70.0, 15.0, 8.0);
    let mark = Stroke::new(1.3_f32 * scale, Color32::WHITE);
    let dots: [(f32, f32, u16); 4] = [(0.0, -1.0, BTN_TRIANGLE), (1.0, 0.0, BTN_CIRCLE), (0.0, 1.0, BTN_X), (-1.0, 0.0, BTN_SQUARE)];
    for (dx, dy, code) in dots {
        let c: Pos2 = at(fx + dx * gap, fy + dy * gap);
        p.circle_filled(c, r * scale, fill(i.key(code)));
        let s = 3.2 * scale;
        match code {
            BTN_TRIANGLE => {
                let pts = [c + vec2(0.0, -s), c + vec2(s, s * 0.8), c + vec2(-s, s * 0.8)];
                for k in 0..3 {
                    p.line_segment([pts[k], pts[(k + 1) % 3]], mark);
                }
            }
            BTN_CIRCLE => {
                p.circle_stroke(c, s, mark);
            }
            BTN_X => {
                p.line_segment([c + vec2(-s, -s), c + vec2(s, s)], mark);
                p.line_segment([c + vec2(-s, s), c + vec2(s, -s)], mark);
            }
            _ => {
                p.rect_stroke(Rect::from_center_size(c, vec2(s, s) * 2.0), CornerRadius::ZERO, mark, StrokeKind::Middle);
            }
        }
    }
    // sticks: the knob follows the stick
    for (x, ax, ay, click) in [(84.0, 0, 1, BTN_L3), (156.0, 3, 4, BTN_R3)] {
        let center = at(x, 92.0);
        p.circle_filled(center, 15.0 * scale, Color32::from_gray(0x12));
        let off = vec2(i.abs[ax] as f32 - 128.0, i.abs[ay] as f32 - 128.0) / 128.0 * 7.0 * scale;
        p.circle_filled(center + off, 9.5 * scale, fill(i.key(click)));
    }
    if !i.online {
        p.text(pos2(area.center().x, area.center().y), egui::Align2::CENTER_CENTER, t("Kein Controller", "No controller"),
               egui::FontId::proportional(13.0), Color32::from_gray(0xa0));
    }
}

struct Settings {
    input: Arc<Mutex<Input>>,
    input_mac: Arc<Mutex<String>>,
    battery_editor: bool,
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
    fn new(ctx: egui::Context) -> Self {
        let pads = pads();
        let mac = pads.first().map(|p| p.mac.clone()).unwrap_or_default();
        let (input, input_mac) = (Arc::new(Mutex::new(Input::new(false))), Arc::new(Mutex::new(mac.clone())));
        let (reader_mac, reader_state) = (input_mac.clone(), input.clone());
        let ticker_ctx = ctx.clone();
        std::thread::spawn(move || redraw_ticker(ticker_ctx));
        std::thread::spawn(move || input_reader(reader_mac, reader_state, ctx));
        Settings { input, input_mac, battery_editor: false, cfg: load_json(&CONFIG), seen: mtime(&CONFIG), mac,
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
        if *self.input_mac.lock().unwrap() != self.mac {
            self.input_mac.lock().unwrap().clone_from(&self.mac);
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
        egui::ComboBox::from_id_salt(key).width(142.0).selected_text(text).show_ui(ui, |ui| {
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
        let r = switch(ui, &mut on, label);
        let r = if let Some(tip) = tip { r.on_hover_text(tip) } else { r };
        if r.changed() {
            self.change(Op::Top(key, json!(on)));
        }
    }

    fn segmented_setting(&mut self, ui: &mut egui::Ui, key: &'static str, choices: Choices) {
        let cur = self.get(key);
        let mut pick = None;
        segments(ui, |ui| {
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

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("DS5Tee").size(20.0).strong().color(egui::Color32::from_gray(0xF0)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(b) = self.pads.iter().find(|p| p.mac == self.mac).and_then(|p| p.battery) {
                    let level = b as f32 / 100.0;
                    ui.add(egui::ProgressBar::new(level).desired_width(96.0).corner_radius(9).fill(bar_color(level))
                        .text(egui::RichText::new(format!("{b} %")).color(egui::Color32::BLACK)));
                }
                let targets: Vec<(String, String)> = self.pads.iter().map(|p| (p.mac.clone(), pad_label(p)))
                    .chain([(String::new(), t("Standard für neue Controller", "Default for new controllers").into())]).collect();
                let current = targets.iter().find(|(m, _)| *m == self.mac).map_or(String::new(), |t| t.1.clone());
                egui::ComboBox::from_id_salt("target").width(320.0).selected_text(current).show_ui(ui, |ui| {
                    for (mac, label) in &targets {
                        ui.selectable_value(&mut self.mac, mac.clone(), label);
                    }
                });
            });
        });
        ui.add_space(8.0);
    }

    fn light_card(&mut self, ui: &mut egui::Ui) {
        card(ui, t("Licht", "Light"), |ui| {
            ui.label(t("Lichtleiste", "Light bar"));
            ui.horizontal(|ui| {
                let color = self.get("color").as_str().unwrap_or("").to_string();
                let mut rgb = parse_color(&color).unwrap_or([0x00, 0x50, 0xFF]);
                if color == "battery" {
                    // same button, same place: for the battery level it opens the gradient
                    let points = battery_points(&settings_for(&self.cfg, &self.mac));
                    let smooth = self.get("battery_smooth").as_bool().unwrap_or(true);
                    let (area, r) = ui.allocate_exact_size(ui.spacing().interact_size, egui::Sense::click());
                    gradient(ui.painter(), area, &points, smooth);
                    if r.on_hover_text(t("Akkufarben einstellen", "Set battery colours")).clicked() {
                        self.battery_editor = !self.battery_editor;
                    }
                } else if ui.color_edit_button_srgb(&mut rgb).on_hover_text(t("Feste Farbe", "Fixed colour")).changed() {
                    self.set("color", json!(format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])));
                }
                segments(ui, |ui| {
                    if ui.selectable_label(color.starts_with('#'), t("Feste Farbe", "Fixed")).clicked() && !color.starts_with('#') {
                        self.set("color", json!(format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])));
                    }
                    if ui.selectable_label(color == "battery", t("Akkustand", "Battery"))
                        .on_hover_text(t("Farbe folgt dem Akkustand, das Farbfeld links stellt den Verlauf ein",
                                         "Colour follows the battery level, the colour box on the left sets the gradient")).clicked() {
                        self.set("color", json!("battery"));
                    }
                    if ui.selectable_label(color.is_empty(), t("Vom Spiel", "From game")).clicked() {
                        self.set("color", json!(""));
                    }
                });
            });
            ui.end_row();
            ui.label(t("Helligkeit", "Brightness"));
            self.slider(ui, "brightness", 100);
            ui.end_row();
            ui.label("Controller-ID");
            self.segmented_setting(ui, "player", players());
            ui.end_row();
        });
    }

    /// Up to 5 points (percent + colour) for the "battery" lightbar, blended or as hard steps.
    fn battery_window(&mut self, ctx: &egui::Context) {
        let mut open = self.battery_editor;
        egui::Window::new(t("Akkufarben", "Battery colours")).open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            let cfg = settings_for(&self.cfg, &self.mac);
            let mut points = battery_points(&cfg);
            let mut smooth = self.get("battery_smooth").as_bool().unwrap_or(true);
            let (area, _) = ui.allocate_exact_size(egui::vec2(320.0, 18.0), egui::Sense::hover());
            gradient(ui.painter(), area, &points, smooth);
            ui.add_space(4.0);
            if switch(ui, &mut smooth, t("Automatischer Verlauf", "Automatic gradient"))
                .on_hover_text(t("Aus: Jede Farbe gilt ab ihrem Prozentwert bis zum nächsten Punkt",
                                 "Off: each colour holds from its percentage up to the next point")).changed() {
                self.set("battery_smooth", json!(smooth));
            }
            ui.add_space(4.0);
            let (mut changed, mut remove) = (false, None);
            // while dragging a percentage the rows must not swap under the pointer
            let dragging = ui.ctx().input(|i| i.pointer.any_down());
            if dragging {
                points = cfg["battery_points"].as_array().into_iter().flatten()
                    .filter_map(|p| Some((p["pct"].as_f64()?, parse_color(p["color"].as_str()?)?))).collect();
            }
            let many = points.len() > 2;
            for (n, (pct, rgb)) in points.iter_mut().enumerate() {
                ui.push_id(n, |ui| ui.horizontal(|ui| {
                    changed |= ui.color_edit_button_srgb(rgb).changed();
                    ui.spacing_mut().slider_width = 170.0;
                    let mut whole = *pct as i64;
                    if ui.add(egui::Slider::new(&mut whole, 0..=100).suffix(" %")).changed() {
                        *pct = whole as f64;
                        changed = true;
                    }
                    if ui.add_enabled(many, egui::Button::new("✖")).on_hover_text(t("Punkt entfernen", "Remove point")).clicked() {
                        remove = Some(n);
                    }
                }));
            }
            if let Some(n) = remove {
                points.remove(n);
                changed = true;
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(points.len() < 5, egui::Button::new(t("Punkt hinzufügen", "Add point"))).clicked() {
                    points.sort_by(|a, b| a.0.total_cmp(&b.0));
                    // in the middle of the widest gap, in the colour the gradient already has there
                    let edges: Vec<f64> = [0.0].into_iter().chain(points.iter().map(|p| p.0)).chain([100.0]).collect();
                    let gap = edges.windows(2).max_by(|a, b| (a[1] - a[0]).total_cmp(&(b[1] - b[0]))).unwrap_or(&[0.0, 100.0]);
                    let pct = ((gap[0] + gap[1]) / 2.0).round();
                    points.push((pct, battery_color(pct, &points, smooth)));
                    changed = true;
                }
                if ui.button(t("Standard", "Default")).clicked() {
                    self.set("battery_points", shared::defaults()["battery_points"].clone());
                    self.set("battery_smooth", json!(true));
                }
            });
            if changed {
                let list: Vec<Value> = points.iter().map(|(pct, c)| json!({
                    "pct": *pct as i64, "color": format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])})).collect();
                self.set("battery_points", Value::Array(list));
            }
        });
        self.battery_editor = open;
    }

    fn audio_card(&mut self, ui: &mut egui::Ui) {
        card(ui, t("Audio & Vibration", "Audio & rumble"), |ui| {
            for (label, key, max) in [(t("Lautsprecher", "Speaker"), "speaker", 100), (t("Haptik", "Haptics"), "haptics", 200),
                                      (t("Vibration", "Rumble"), "rumble", 100)] {
                ui.label(label);
                self.slider(ui, key, max);
                ui.end_row();
            }
            ui.label(t("Mikrofon", "Microphone"));
            self.check(ui, "bt_mic", "", false, Some(t(
                "Controller-Mikrofon über Bluetooth. Überträgt nur, solange eine App davon aufnimmt.",
                "Controller microphone over Bluetooth. Only streams while an app records from it.")));
            ui.end_row();
        });
    }

    fn trigger_card(&mut self, ui: &mut egui::Ui) {
        // mode combo and its strength slider share one row
        card(ui, t("Adaptive Trigger", "Adaptive triggers"), |ui| {
            ui.spacing_mut().slider_width = 92.0;
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
    }

    fn gyro_card(&mut self, ui: &mut egui::Ui) {
        card(ui, t("Gyro auf rechten Stick", "Gyro on right stick"), |ui| {
            ui.label(t("Modus", "Mode"));
            self.segmented_setting(ui, "gyro", gyro_modes());
            ui.end_row();
            ui.label(t("Empfindlichkeit", "Sensitivity"));
            self.slider(ui, "gyro_sensitivity", 300);
            ui.end_row();
            ui.label(t("Spiegeln", "Invert"));
            ui.horizontal(|ui| {
                for (key, label) in [("gyro_invert_x", "Horizontal"), ("gyro_invert_y", t("Vertikal", "Vertical"))] {
                    let mut on = self.get(key).as_bool().unwrap_or(false);
                    if switch(ui, &mut on, label).changed() {
                        self.set(key, json!(on));
                    }
                    ui.add_space(8.0);
                }
            });
            ui.end_row();
        });
    }

    fn form(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().slider_width = 200.0;
        self.battery_window(&ui.ctx().clone());
        self.header(ui);
        ui.columns(2, |c| {
            let input = *self.input.lock().unwrap();
            panel(&mut c[0], t("Eingaben", "Inputs"), |ui| draw_input(ui, &input));
            self.light_card(&mut c[0]);
            self.audio_card(&mut c[1]);
            self.trigger_card(&mut c[1]);
            self.gyro_card(&mut c[1]);
        });
        panel(ui, t("Verbindung", "Connection"), |ui| {
            ui.horizontal(|ui| {
                ui.label(t("Auto-Trennen", "Auto-disconnect"));
                self.combo(ui, "idle_minutes", idle_times());
                ui.add_space(16.0);
                self.check(ui, "touchpad_mouse", t("Touchpads als Maus", "Touchpads as mouse"), false,
                           Some(t("Am Desktop, gilt für alle Controller", "On the desktop, for all controllers")));
                ui.add_space(16.0);
                self.check(ui, "bridge", t("Kabel-Emulation", "Wired emulation"), true, Some(t(
                    "Versteckt den echten Controller. Aus: Der Controller ist ein normaler Bluetooth-Controller – ohne \
                     Haptik, Lautsprecher und adaptive Trigger in Spielen. Nach dem Umschalten das Spiel neu starten.",
                    "Hides the real controller. Off: the controller is a plain Bluetooth controller – no haptics, speaker \
                     or adaptive triggers in games. Restart the game after switching.")));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(!self.mac.is_empty(), egui::Button::new(t("Trennen", "Disconnect"))).clicked() {
                        disconnect(self.mac.clone());
                    }
                    if ui.button(t("Zurücksetzen", "Reset")).on_hover_text(t(
                        "Alles auf Standard: Lautsprecher 50 %, übrige Regler 100 %, Trigger/Farbe/ID vom Spiel, Gyro aus, Auto-Trennen 15 min",
                        "Everything to default: speaker 50 %, other sliders 100 %, triggers/colour/ID from game, gyro off, auto-disconnect 15 min"))
                        .clicked() {
                        self.change(Op::Reset(self.mac.clone()));
                    }
                });
            });
        });
    }
}

const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x00, 0x70, 0xD1); // PlayStation blue

/// Rounded section with a heading; the label column has a fixed width so all cards line up.
fn card(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    panel(ui, title, |ui| {
        let mut body = Some(body);
        egui::Grid::new(title).num_columns(2).min_col_width(92.0).spacing([12.0, 10.0])
            .show(ui, |ui| body.take().unwrap()(ui));
    });
}

fn panel(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new().fill(ui.visuals().faint_bg_color).corner_radius(14).inner_margin(14)
        .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(0x2C))).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(egui::RichText::new(title).size(14.0).strong().color(egui::Color32::from_gray(0xE8)));
        ui.add_space(4.0);
        body(ui);
    });
    ui.add_space(8.0);
}

/// The battery gradient left to right, 0 to 100 %.
fn gradient(p: &egui::Painter, area: egui::Rect, points: &[(f64, [u8; 3])], smooth: bool) {
    let slices = 40;
    for k in 0..slices {
        let (x0, x1) = (k as f32 / slices as f32, (k + 1) as f32 / slices as f32);
        let [r, g, b] = battery_color(x0 as f64 * 100.0, points, smooth);
        let slice = egui::Rect::from_min_max(egui::pos2(area.left() + x0 * area.width(), area.top()),
                                             egui::pos2(area.left() + x1 * area.width(), area.bottom()));
        p.rect_filled(slice, 0.0, egui::Color32::from_rgb(r, g, b));
    }
    p.rect_stroke(area, 3.0, egui::Stroke::new(1.0_f32, egui::Color32::from_gray(0x50)), egui::StrokeKind::Outside);
}

/// Choices side by side on one dark strip.
fn segments(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new().fill(ui.visuals().extreme_bg_color).corner_radius(8).inner_margin(2).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            body(ui);
        });
    });
}

/// On/off switch with its label; the whole row is clickable.
fn switch(ui: &mut egui::Ui, on: &mut bool, label: &str) -> egui::Response {
    use egui::{Color32, Sense, vec2};
    let galley = ui.painter().layout_no_wrap(label.into(), egui::FontId::proportional(14.0), ui.visuals().text_color());
    let track = vec2(32.0, 18.0);
    let size = vec2(track.x + 8.0 + galley.size().x, track.y.max(galley.size().y));
    let (area, mut r) = ui.allocate_exact_size(size, Sense::click());
    if r.clicked() {
        *on = !*on;
        r.mark_changed();
    }
    let knob = ui.ctx().animate_bool(r.id, *on);
    let rail = egui::Rect::from_min_size(area.left_center() - vec2(0.0, track.y / 2.0), track);
    let off = if r.hovered() { Color32::from_gray(0x50) } else { Color32::from_gray(0x3C) };
    let p = ui.painter();
    p.rect_filled(rail, 9.0, if *on { ACCENT } else { off });
    p.circle_filled(egui::pos2(rail.left() + 9.0 + knob * (track.x - 18.0), rail.center().y), 7.0, Color32::WHITE);
    p.galley(egui::pos2(rail.right() + 8.0, area.center().y - galley.size().y / 2.0), galley, ui.visuals().text_color());
    r
}

/// Same scale as the bridge's "battery" light bar: green, yellow at 50 %, deep red from 10 % down.
fn bar_color(level: f32) -> egui::Color32 {
    let l = level.clamp(0.0, 1.0);
    let (r, g) = if l > 0.5 { ((2.0 - 2.0 * l) * 255.0, 220.0) } else { (255.0, ((l - 0.1) / 0.4).max(0.0).powf(2.2) * 220.0) };
    egui::Color32::from_rgb(r as u8, g as u8, 60)
}

fn style(ctx: &egui::Context) {
    use egui::{Color32, CornerRadius, Stroke};
    let mut v = egui::Visuals::dark();
    v.panel_fill = Color32::from_rgb(0x10, 0x12, 0x16);
    v.faint_bg_color = Color32::from_rgb(0x1a, 0x1d, 0x23);
    v.extreme_bg_color = Color32::from_rgb(0x0b, 0x0d, 0x10);
    v.handle_shape = egui::style::HandleShape::Circle;
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    v.slider_trailing_fill = true;
    v.hyperlink_color = ACCENT;
    let w = &mut v.widgets;
    for (wv, fill) in [(&mut w.inactive, 0x2a), (&mut w.hovered, 0x36), (&mut w.active, 0x40), (&mut w.open, 0x30)] {
        wv.bg_fill = Color32::from_gray(fill);
        wv.weak_bg_fill = Color32::from_gray(fill);
        wv.corner_radius = CornerRadius::same(7);
        wv.bg_stroke = Stroke::NONE;
    }
    w.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    w.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    w.noninteractive.corner_radius = CornerRadius::same(7);
    ctx.set_visuals(v);
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.slider_rail_height = 6.0;
        s.spacing.combo_height = 300.0;
        for (style, size) in [(egui::TextStyle::Body, 14.0), (egui::TextStyle::Button, 14.0)] {
            if let Some(font) = s.text_styles.get_mut(&style) {
                font.size = size;
            }
        }
        s.spacing.button_padding = egui::vec2(10.0, 4.0);
        s.spacing.interact_size.y = 24.0;
    });
}

impl App for Settings {
    fn ui(&mut self, ui: &mut egui::Ui, _backend: &mut SoftwareBackend) {
        self.poll();
        egui::CentralPanel::default().show_inside(ui, |ui| egui::ScrollArea::vertical().show(ui, |ui| self.form(ui)));
        redraw_after(Duration::from_millis(if self.save_at.is_some() || self.preview.is_some() { 50 } else { 1000 }));
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
        .with_inner_size([960.0, 672.0]);
    let _ = egui_software_backend::run_app_with_software_backend(config, |ctx| {
        style(&ctx);
        Settings::new(ctx.clone())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_events() {
        let mut i = Input::new(true);
        assert!(i.apply(false, 1, BTN_X, 1) && i.key(BTN_X));
        assert!(i.apply(false, 1, BTN_X, 0) && !i.key(BTN_X));
        assert!(i.apply(false, 1, BTN_R3, 1) && i.key(BTN_R3));
        assert!(!i.apply(false, 3, 0, 129), "stick jitter must not redraw");
        assert!(i.apply(false, 3, 0, 255) && i.abs[0] == 255);
        assert!(i.apply(false, 3, 0x11, -1) && i.hat[1] == -1);
        assert!(i.apply(true, 3, 0x39, 7) && i.touch[0] == Some([0, 0]));
        assert!(i.apply(true, 3, 0x36, 500) && i.touch[0] == Some([0, 500]));
        i.apply(true, 3, 0x2F, 1); // second finger, first stays
        assert!(i.apply(true, 3, 0x39, 8) && i.touch[1].is_some());
        assert!(i.apply(true, 3, 0x35, 900) && i.touch[1] == Some([900, 0]) && i.touch[0] == Some([0, 500]));
        assert!(i.apply(true, 3, 0x39, -1) && i.touch[1].is_none() && i.touch[0].is_some());
        assert!(i.apply(true, 1, 0x110, 1) && i.click);
        assert_eq!(virtual_mac("48:00:00:00:00:01"), "4a:00:00:00:00:01");
        // the triangles cover exactly the shell
        for shape in [&SHELL, &CORE, &TOUCHPAD, &L1_SHAPE, &L2_SHAPE] {
            let (o, tris) = &**shape;
            let outline: f32 = (0..o.len()).map(|k| cross([0.0, 0.0], o[k], o[(k + 1) % o.len()])).sum::<f32>().abs() / 2.0;
            let covered: f32 = tris.iter().map(|&[a, b, c]| cross(o[a as usize], o[b as usize], o[c as usize]).abs() / 2.0).sum();
            assert!((outline - covered).abs() < 1.0, "{outline} vs {covered}");
        }
    }
}
