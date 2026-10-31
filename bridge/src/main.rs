//! DualSense Bluetooth -> virtual wired USB DualSense (the DS5Dongle Pico, in software).
//!
//! Games only enable adaptive triggers, HD haptics and the controller speaker for a wired
//! DualSense, and they pair its HID and audio functions through the shared USB device.
//! So for each DualSense connected over Bluetooth this plays a complete USB DualSense
//! (DS5Dongle's descriptors: UAC1 4-ch speaker/haptics + 2-ch mic + HID) to the kernel
//! through USB/IP (vhci_hcd, attached over a local socketpair). Linux, PipeWire and Proton
//! then see a real USB device with one container, exactly like a cable.
//!
//!   BT input 0x31 (78 B)      -> HID interrupt IN 0x01 (64 B)
//!   HID output 0x02           -> BT output 0x31 with sequence tag + CRC32
//!   feature GET               -> forwarded to the controller
//!   feature SET               -> only 0x80 / 0x60-0x62 forwarded (CRC recomputed), like DS5Dongle
//!   iso OUT 4ch s16 48 kHz    -> BT 0x39: ch3/4 haptics at 3 kHz int8, ch1/2 speaker as Opus
//! Runs as root: needs vhci_hcd and hides the real Bluetooth nodes from users, so games and
//! Steam only see the USB pad. Other Bluetooth devices are untouched.
#[path = "../../shared.rs"]
mod shared;

use serde_json::{Map, Value, json};
use shared::{STATUS_PATH, settings_for};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, VecDeque};
use std::f64::consts::PI;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const RDESC: &[u8] = include_bytes!("../../usb_report_desc.bin"); // HID report descriptor (DS5Dongle)
const VHCI: &str = "/sys/devices/platform/vhci_hcd.0";
const AFRAME: usize = 512 * 4 * 2; // 512 frames x 4 ch x s16 = 10.67 ms -> one haptics + one speaker frame
const HAPTICS_GAIN: f64 = 2.0; // at haptics 100 %; DS5Dongle allows 1.0-2.0, 1.0 felt weak
const SPEAKER_GAIN: f64 = 2.5; // at speaker 100 %: digital boost before Opus, soft-clipped; the BT speaker path is quieter than a PS5
const GYRO_DPS: f64 = 60.0; // turn rate (deg/s) for full right-stick deflection at gyro sensitivity 100 %
const GYRO_LSB: f64 = 16.4; // raw gyro units per deg/s (+-2000 deg/s range)
const GYRO_NOISE: f64 = 1.5; // deg/s subtracted from the turn rate: hand tremor and sensor noise
const GAME_DEADZONE: f64 = 0.15; // usual game stick deadzone; gyro-only motion starts just past it so fine aim isn't swallowed
const SPEAKER_PREGAIN: u8 = 7; // speaker pre-amp gain 1-7 (DS5Dongle default 2); PS5 games sound louder than 2
const EPIPE: i32 = -32;
const ECONNRESET: i32 = -104;
const WATCHDOG_S: i32 = 5; // main loop silent this long: it is stuck in the kernel, cut the virtual USB pads loose
const PLAYER_LEDS: [(i64, u8); 4] = [(1, 0x04), (2, 0x0A), (3, 0x15), (4, 0x1B)];
// Real Bluetooth DualSense nodes root-only from the moment they appear. Hiding them afterwards is too late
// when Steam or a running game opens them first: they keep that handle, see a second pad and fight the
// bridge over lightbar and player LEDs. In /run, so it only applies while the bridge runs and is enabled.
const HIDE_RULE_PATH: &str = "/run/udev/rules.d/72-ds5bridge-hide.rules";
const HIDE_RULE: &str = "\
SUBSYSTEM==\"hidraw\", KERNELS==\"0005:054C:0CE6.*\", MODE=\"0600\", TAG-=\"uaccess\"
SUBSYSTEM==\"input\", KERNELS==\"0005:054C:0CE6.*\", MODE=\"0600\", TAG-=\"uaccess\"
";

static START: LazyLock<Instant> = LazyLock::new(Instant::now);
static STOP: AtomicBool = AtomicBool::new(false);

fn now() -> f64 {
    START.elapsed().as_secs_f64()
}

type Cfg = Map<String, Value>;

fn num(c: &Cfg, k: &str) -> f64 {
    c.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

fn text<'a>(c: &'a Cfg, k: &str) -> &'a str {
    c.get(k).and_then(Value::as_str).unwrap_or("")
}

fn flag(c: &Cfg, k: &str) -> bool {
    c.get(k).and_then(Value::as_bool).unwrap_or(false)
}

/// DS5Tee writes ~/.config/ds5bridge.json of whoever runs it; the newest one wins (DS5BRIDGE_CONFIG overrides).
struct Config {
    paths: Vec<String>,
    seen: Option<(String, i64)>, // file and mtime last read: unchanged files aren't parsed again
    value: Value,
}

impl Config {
    fn new() -> Self {
        let paths = match std::env::var("DS5BRIDGE_CONFIG") {
            Ok(p) if !p.is_empty() => vec![p],
            _ => fs::read_to_string("/etc/passwd").unwrap_or_default().lines().filter_map(|l| {
                let f: Vec<&str> = l.split(':').collect();
                let uid: u32 = f.get(2)?.parse().ok()?;
                let home = f.get(5)?;
                (uid == 0 || (1000..60000).contains(&uid)).then(|| format!("{home}/.config/ds5bridge.json"))
            }).collect(),
        };
        Config { paths, seen: None, value: Value::Null }
    }

    /// Re-read the tray tool's settings; returns true when they changed.
    fn load(&mut self) -> bool {
        let newest = self.paths.iter()
            .filter_map(|p| Some((p.clone(), fs::metadata(p).ok()?.mtime_nsec_total())))
            .max_by_key(|(_, m)| *m);
        if newest == self.seen {
            return false;
        }
        let new = newest.as_ref().map(|(p, _)| shared::load_json(p)).unwrap_or(Value::Null);
        self.seen = newest;
        let changed = new != self.value;
        self.value = new;
        changed
    }

    fn enabled(&self) -> bool {
        self.value.get("bridge").and_then(Value::as_bool).unwrap_or(true) // off: real pads stay plain Bluetooth pads
    }
}

trait MtimeNs {
    fn mtime_nsec_total(&self) -> i64;
}

impl MtimeNs for fs::Metadata {
    fn mtime_nsec_total(&self) -> i64 {
        self.mtime() * 1_000_000_000 + self.mtime_nsec()
    }
}

/// Bridge on: install the hide rule. Off: drop it and let udev give connected pads back to the user.
fn set_hiding(on: bool) {
    if on == fs::exists(HIDE_RULE_PATH).unwrap_or(false) {
        return;
    }
    if on {
        let _ = fs::create_dir_all("/run/udev/rules.d");
        let _ = fs::write(HIDE_RULE_PATH, HIDE_RULE);
    } else {
        let _ = fs::remove_file(HIDE_RULE_PATH);
    }
    let _ = Command::new("udevadm").args(["control", "--reload"]).status();
    if !on {
        let _ = Command::new("udevadm")
            .args(["trigger", "--action=change", "--subsystem-match=hidraw", "--subsystem-match=input"]).status();
    }
    println!("hiding real pads: {on}");
}

/// Scale the force of one trigger effect (11 bytes). The pad's own power reduction is shared by
/// both triggers, so per-side strength rescales the effect parameters of the common modes instead.
fn scale_effect(e: &mut [u8], pct: f64) {
    let f = pct / 100.0;
    let scale = |v: u8| (v as f64 * f).round_ties_even() as u8;
    let packed = |e: &mut [u8], lo: usize, hi: usize, zones: usize| {
        // 3-bit strengths (stored as strength-1) packed little-endian
        let mut v = e[lo..hi].iter().rev().fold(0u64, |a, &b| a << 8 | b as u64);
        for z in 0..zones {
            let st = (v >> (3 * z)) & 7;
            let new = (((st + 1) as f64 * f).round_ties_even() as i64 - 1).max(0) as u64;
            v = v & !(7 << (3 * z)) | new << (3 * z);
        }
        for (i, b) in e[lo..hi].iter_mut().enumerate() {
            *b = (v >> (8 * i)) as u8;
        }
    };
    match e[0] {
        0x01 => e[2] = scale(e[2]), // continuous resistance: start, force
        0x02 => e[3] = scale(e[3]), // section: start, end, force
        0x06 => e[2] = scale(e[2]), // vibration: frequency, amplitude, start
        0x21 | 0x26 => packed(e, 3, 7, 10), // feedback / vibration: zone mask, 10 packed strengths
        0x25 => packed(e, 3, 4, 1), // weapon: zone mask, strength
        _ => {} // ponytail: bow (0x22), galloping (0x23), machine (0x27) unscaled; add them if a game uses them
    }
}

/// Lightbar by battery: green at 100 %, yellow at 50 %, red at 0 %.
fn battery_color(pct: Option<u8>) -> [u8; 3] {
    let p = pct.unwrap_or(50) as f64;
    let r = if p <= 50.0 { 255.0 } else { (255.0 * (100.0 - p) / 50.0).round_ties_even() };
    let g = if p >= 50.0 { 255.0 } else { (255.0 * p / 50.0).round_ties_even() };
    [r as u8, g as u8, 0]
}

fn trigger_effect(mode: &str) -> &'static [u8] {
    // raw DualSense trigger effects (mode, params...), forced per trigger
    match mode {
        "resistance" => &[0x01, 0x40, 0xC0], // continuous: start position, force
        "weapon" => &[0x02, 0x40, 0xA0, 0xFF], // section: start, end, force
        "vibration" => &[0x06, 0x28, 0xC0, 0x30], // frequency, amplitude, start position
        _ => &[0x05], // off
    }
}

fn parse_color(c: &str) -> Option<[u8; 3]> {
    let h = c.strip_prefix('#')?;
    let v = u32::from_str_radix(h, 16).ok().filter(|_| h.len() == 6)?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

/// User overrides on an outgoing SetStateData (common part, 47 bytes, report id stripped).
fn apply_settings(body: &mut [u8], cfg: &Cfg, battery: Option<u8>) {
    if body[0] & 0x03 != 0 {
        // rumble motors: scale the values and the pad's own rumble power (high nibble)
        let rumble = num(cfg, "rumble") as i64;
        body[2] = (body[2] as i64 * rumble / 100) as u8;
        body[3] = (body[3] as i64 * rumble / 100) as u8;
        body[1] |= 0x40;
        body[36] = (body[36] & 0x0F) | (((100 - rumble) as f64 / 12.5).round_ties_even().min(7.0) as u8) << 4;
    }
    for (side, bit, lo) in [("r", 0x04, 10), ("l", 0x08, 21)] {
        let mode = text(cfg, &format!("trigger_mode_{side}"));
        if mode != "game" {
            body[0] |= bit;
            let e = trigger_effect(mode);
            body[lo..lo + 11].fill(0);
            body[lo..lo + e.len()].copy_from_slice(e);
        }
        let strength = num(cfg, &format!("trigger_strength_{side}"));
        if body[0] & bit != 0 && strength < 100.0 {
            scale_effect(&mut body[lo..lo + 11], strength);
        }
    }
    let color = text(cfg, "color");
    let rgb = if color == "battery" { Some(battery_color(battery)) } else { parse_color(color) };
    if let Some(rgb) = rgb {
        body[1] |= 0x04;
        body[44..47].copy_from_slice(&rgb);
    }
    let player = num(cfg, "player") as i64;
    if let Some(&(_, leds)) = PLAYER_LEDS.iter().find(|(p, _)| *p == player) {
        body[1] |= 0x10;
        body[43] = leds;
    }
}

fn usb_string(text: &str) -> Vec<u8> {
    let b: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    [vec![2 + b.len() as u8, 3], b].concat()
}

const DEVICE: [u8; 18] = [0x12, 0x01, 0x00, 0x02, 0, 0, 0, 0x40, 0x4C, 0x05, 0xE6, 0x0C, 0x00, 0x01, 1, 2, 0, 1];
const HID_DESC: [u8; 9] = [0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, RDESC.len() as u8, (RDESC.len() >> 8) as u8];

fn usb_config() -> Vec<u8> {
    let mut c = vec![
        // DS5Dongle usb_descriptors.cpp without CDC/wake interfaces
        0x09, 0x02, 0xE3, 0x00, 0x04, 0x01, 0x00, 0xC0, 0xFA,
        // interface 0: audio control
        0x09, 0x04, 0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x0A, 0x24, 0x01, 0x00, 0x01, 0x49, 0x00, 0x02, 0x01, 0x02,
        0x0C, 0x24, 0x02, 0x01, 0x01, 0x01, 0x06, 0x04, 0x33, 0x00, 0x00, 0x00, // IT 1: USB streaming, 4 ch
        0x0C, 0x24, 0x06, 0x02, 0x01, 0x01, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, // FU 2: mute, volume
        0x09, 0x24, 0x03, 0x03, 0x01, 0x03, 0x04, 0x02, 0x00, // OT 3: speaker
        0x0C, 0x24, 0x02, 0x04, 0x02, 0x04, 0x03, 0x02, 0x03, 0x00, 0x00, 0x00, // IT 4: headset mic, 2 ch
        0x09, 0x24, 0x06, 0x05, 0x04, 0x01, 0x03, 0x00, 0x00, // FU 5
        0x09, 0x24, 0x03, 0x06, 0x01, 0x01, 0x01, 0x05, 0x00, // OT 6: USB streaming
        // interface 1: speaker/haptics stream, EP1 OUT iso 4 ch s16 48 kHz
        0x09, 0x04, 0x01, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00,
        0x09, 0x04, 0x01, 0x01, 0x01, 0x01, 0x02, 0x00, 0x00,
        0x07, 0x24, 0x01, 0x01, 0x01, 0x01, 0x00,
        0x0B, 0x24, 0x02, 0x01, 0x04, 0x02, 0x10, 0x01, 0x80, 0xBB, 0x00,
        0x09, 0x05, 0x01, 0x09, 0x88, 0x01, 0x01, 0x00, 0x00,
        0x07, 0x25, 0x01, 0x00, 0x00, 0x00, 0x00,
        // interface 2: mic stream, EP2 IN iso 2 ch s16 48 kHz (answered with silence)
        0x09, 0x04, 0x02, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00,
        0x09, 0x04, 0x02, 0x01, 0x01, 0x01, 0x02, 0x00, 0x00,
        0x07, 0x24, 0x01, 0x06, 0x01, 0x01, 0x00,
        0x0B, 0x24, 0x02, 0x01, 0x02, 0x02, 0x10, 0x01, 0x80, 0xBB, 0x00,
        0x09, 0x05, 0x82, 0x05, 0xC4, 0x00, 0x01, 0x00, 0x00,
        0x07, 0x25, 0x01, 0x00, 0x00, 0x00, 0x00,
        // interface 3: HID, EP4 IN / EP3 OUT interrupt 64 B
        0x09, 0x04, 0x03, 0x00, 0x02, 0x03, 0x00, 0x00, 0x00,
    ];
    c.extend(HID_DESC);
    c.extend([0x07, 0x05, 0x84, 0x03, 0x40, 0x00, 0x01, 0x07, 0x05, 0x03, 0x03, 0x40, 0x00, 0x01]);
    assert_eq!(c.len(), 0xE3);
    c
}

fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
    // _IOC('H', nr, size)
    (dir << 30) | ((size as u64) << 16) | ((b'H' as u64) << 8) | nr
}

fn get_feature(f: &File, rid: u8, size: usize) -> io::Result<Vec<u8>> {
    // HIDIOCGFEATURE
    let mut buf = vec![0u8; size];
    buf[0] = rid;
    let n = unsafe { libc::ioctl(f.as_raw_fd(), ioc(3, 0x07, size) as _, buf.as_mut_ptr()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(n as usize);
    Ok(buf)
}

fn set_feature(f: &File, data: &mut [u8]) -> io::Result<()> {
    // HIDIOCSFEATURE
    let n = unsafe { libc::ioctl(f.as_raw_fd(), ioc(3, 0x06, data.len()) as _, data.as_mut_ptr()) };
    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB88320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// DualSense BT CRC32 (zlib) over a 1-byte HID header + report.
fn bt_crc(seed: u8, data: &[u8]) -> [u8; 4] {
    let mut c = !0u32;
    for &b in std::iter::once(&seed).chain(data) {
        c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    (!c).to_le_bytes()
}

fn read(path: &str) -> String {
    fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn dir_names(dir: &str) -> Vec<String> {
    fs::read_dir(dir).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default()
}

/// {hidraw path: (sysfs hid dir, MAC)} for DualSenses connected over Bluetooth.
fn bt_dualsenses() -> BTreeMap<String, (String, String)> {
    let mut found = BTreeMap::new();
    for h in dir_names("/sys/class/hidraw") {
        let Ok(dev) = fs::canonicalize(format!("/sys/class/hidraw/{h}/device")) else { continue };
        let dev = dev.to_string_lossy().into_owned();
        if dev.rsplit('/').next().unwrap_or("").to_uppercase().starts_with("0005:054C:0CE6") {
            let uniq = read(&format!("{dev}/uevent")).lines()
                .find_map(|l| l.strip_prefix("HID_UNIQ=")).unwrap_or("").to_uppercase();
            found.insert(format!("/dev/{h}"), (dev, uniq));
        }
    }
    found
}

/// Make the real Bluetooth nodes root-only so games/Steam don't see a second pad.
fn hide(hiddir: &str, hidraw: &str) {
    let mut nodes = vec![hidraw.to_string()];
    for input in dir_names(&format!("{hiddir}/input")).into_iter().filter(|n| n.starts_with("input")) {
        nodes.extend(dir_names(&format!("{hiddir}/input/{input}")).into_iter()
            .filter(|n| n.starts_with("event") || n.starts_with("js")).map(|n| format!("/dev/input/{n}")));
    }
    for n in nodes {
        if fs::metadata(&n).is_ok_and(|m| m.mode() & 0o077 != 0) {
            let _ = std::os::unix::fs::chown(&n, Some(0), Some(0));
            let _ = fs::set_permissions(&n, fs::Permissions::from_mode(0o600)); // also masks uaccess ACL entries
        }
    }
}

#[link(name = "opus")]
unsafe extern "C" {
    fn opus_encoder_create(fs: i32, channels: i32, application: i32, error: *mut i32) -> *mut libc::c_void;
    fn opus_encoder_ctl(st: *mut libc::c_void, request: i32, ...) -> i32;
    fn opus_encode(st: *mut libc::c_void, pcm: *const i16, frame_size: i32, data: *mut u8, max: i32) -> i32;
    fn opus_encoder_destroy(st: *mut libc::c_void);
}

/// 10 ms CBR Opus frames of 200 bytes, as the DualSense expects (DS5Dongle settings).
struct Opus(*mut libc::c_void);

impl Opus {
    fn new() -> io::Result<Self> {
        let mut err = 0;
        let enc = unsafe { opus_encoder_create(48000, 2, 2051, &mut err) }; // RESTRICTED_LOWDELAY = CELT
        if err != 0 || enc.is_null() {
            return Err(io::Error::other(format!("opus_encoder_create {err}")));
        }
        for (req, val) in [(4002, 160000), (4006, 0), (4010, 0), (4040, 5003)] { // bitrate, VBR off, complexity, 10 ms
            unsafe { opus_encoder_ctl(enc, req, val as i32) };
        }
        Ok(Opus(enc))
    }

    fn encode(&mut self, pcm: &[i16]) -> [u8; 200] {
        // 480 frames x 2 ch
        let mut out = [0u8; 200];
        unsafe { opus_encode(self.0, pcm.as_ptr(), 480, out.as_mut_ptr(), 200) };
        out // shorter frames stay zero-padded to 200 bytes
    }
}

impl Drop for Opus {
    fn drop(&mut self) {
        unsafe { opus_encoder_destroy(self.0) }
    }
}

fn be(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes(b[i * 4..i * 4 + 4].try_into().unwrap())
}

/// USB/IP RET_SUBMIT header.
fn ret_submit(seq: u32, status: i32, actual: usize, start: i32, npackets: i32) -> Vec<u8> {
    let mut h = Vec::with_capacity(48);
    for v in [3, seq, 0, 0, 0, status as u32, actual as u32, start as u32, npackets as u32, 0] {
        h.extend(v.to_be_bytes());
    }
    h.extend([0; 8]);
    h
}

/// The USB side: a USB/IP device served over a socketpair whose other end vhci_hcd owns.
struct Usb {
    sock: UnixStream,
    port: u32,
    int_in: VecDeque<(u32, usize)>, // pending HID IN URBs (seqnum, length)
    last_input: Option<[u8; 64]>,   // newest input report not yet delivered
    timed: BinaryHeap<Reverse<(u64, u32, Vec<u8>)>>, // (due ns, seqnum, reply) for paced iso completions
    iso_due: [f64; 2],              // OUT ep 1, IN ep 0x82
    config: Vec<u8>,
}

impl Usb {
    fn new() -> io::Result<Self> {
        let (sock, kernel_end) = UnixStream::pair()?;
        let _ = Command::new("modprobe").arg("vhci-hcd").status();
        let port = read(&format!("{VHCI}/status")).lines().skip(1).find_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() > 2 && f[0] == "hs" && f[2] == "004").then(|| f[1].parse().ok()).flatten() // free USB2 port
        }).ok_or_else(|| io::Error::other("no free vhci port"))?;
        // port sockfd devid speed(full)
        fs::write(format!("{VHCI}/attach"), format!("{port} {} {} 2", kernel_end.as_raw_fd(), (1 << 16) | 2))?;
        drop(kernel_end); // vhci holds its own reference now
        Ok(Usb { sock, port, int_in: VecDeque::new(), last_input: None, timed: BinaryHeap::new(), iso_due: [0.0; 2],
                 config: usb_config() })
    }

    fn close(&self) {
        let _ = fs::write(format!("{VHCI}/detach"), self.port.to_string());
        let _ = self.sock.shutdown(std::net::Shutdown::Both);
    }

    fn recv(&mut self, n: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0; n];
        self.sock.read_exact(&mut buf).map_err(|_| io::Error::other("vhci closed the connection"))?;
        Ok(buf)
    }

    fn reply(&mut self, seq: u32, status: i32, data: &[u8], actual: Option<usize>) -> io::Result<()> {
        let mut msg = ret_submit(seq, status, actual.unwrap_or(data.len()), 0, 0);
        msg.extend(data);
        self.sock.write_all(&msg)
    }

    /// New HID input report: complete a waiting IN URB or keep it for the next one.
    fn input(&mut self, report: [u8; 64]) -> io::Result<()> {
        match self.int_in.pop_front() {
            Some((seq, len)) => self.reply(seq, 0, &report[..len.min(64)], None),
            None => {
                self.last_input = Some(report);
                Ok(())
            }
        }
    }

    /// Send paced iso completions that are due; returns seconds until the next one.
    fn tick(&mut self) -> io::Result<Option<f64>> {
        let t = (now() * 1e9) as u64;
        while self.timed.peek().is_some_and(|Reverse((due, _, _))| *due <= t) {
            let Reverse((_, _, msg)) = self.timed.pop().unwrap();
            self.sock.write_all(&msg)?;
        }
        Ok(self.timed.peek().map(|Reverse((due, _, _))| (due - t) as f64 / 1e9))
    }
}

enum Step {
    Output(Vec<u8>),
    Audio(Vec<i16>, Vec<i16>), // two 512 x 4 blocks
}

struct Bridge {
    mac: String,
    real: File,
    seq: u8,
    packets: u8,
    headset: bool,
    battery: (Option<u8>, bool),
    cfg: Cfg,
    timers: Vec<(f64, Step)>, // previews
    controls: Option<[u8; 9]>, // for auto-disconnect when idle
    active_at: f64,
    gbias: [f64; 2], // gyro pitch/yaw at rest
    nout: u32,       // output reports per second, for the log
    tout: f64,
    pcm_buf: Vec<u8>,
    frames: Vec<Vec<i16>>,
    opus: Opus,
    usb: Usb,
}

impl Bridge {
    fn new(hidraw: &str, mac: &str, config: &Value) -> io::Result<Self> {
        let real = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(hidraw)?;
        let opus = Opus::new()?;
        let usb = Usb::new()?;
        let mut b = Bridge {
            mac: mac.into(), real, seq: 0, packets: 0, headset: false, battery: (None, false),
            cfg: settings_for(config, mac), timers: vec![], controls: None, active_at: now(), gbias: [0.0; 2],
            nout: 0, tout: now(), pcm_buf: vec![], frames: vec![], opus, usb,
        };
        println!("bridge {mac} {hidraw} -> vhci port {}", b.usb.port);
        b.audio_route()?;
        Ok(b)
    }

    fn close(&self) {
        self.usb.close();
        println!("closed {}", self.mac);
    }

    /// BT input report -> USB input report.
    fn on_real(&mut self) -> io::Result<()> {
        let mut r = [0u8; 128];
        loop {
            let n = match self.real.read(&mut r) {
                Ok(0) => return Err(io::Error::other("hidraw closed")),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            };
            if n < 65 || r[0] != 0x31 || r[1] & 0x02 != 0 {
                continue; // bit1 = mic audio frame
            }
            if (r[55] & 1 != 0) != self.headset {
                self.headset = !self.headset;
                self.audio_route()?;
            }
            let battery = (Some(((r[54] & 0x0F) * 10 + 5).min(100)), r[54] >> 4 == 1);
            let push_color = battery.0 != self.battery.0 && text(&self.cfg, "color") == "battery";
            self.battery = battery;
            if push_color {
                self.audio_route()?; // pushes the new lightbar colour
            }
            let mut controls = [0u8; 9]; // sticks, analog triggers, buttons
            controls[..6].copy_from_slice(&r[2..8]);
            controls[6..].copy_from_slice(&r[9..12]);
            if self.controls.is_none_or(|old| {
                controls[6..] != old[6..] || controls[..6].iter().zip(&old[..6]).any(|(a, b)| a.abs_diff(*b) > 26)
            }) {
                // > ~10 %: no jitter
                self.controls = Some(controls);
                self.active_at = now();
            }
            let mut report = [0u8; 64];
            report[0] = 0x01;
            report[1..].copy_from_slice(&self.gyro_aim(&r));
            self.usb.input(report)?;
        }
    }

    /// Gyro -> right stick, added to the thumb (Switch style: stick for coarse, tilting for fine aim).
    fn gyro_aim(&mut self, r: &[u8]) -> [u8; 63] {
        let mut out: [u8; 63] = r[2..65].try_into().unwrap();
        let pitch = i16::from_le_bytes([r[17], r[18]]) as f64;
        let yaw = i16::from_le_bytes([r[19], r[20]]) as f64;
        let rate = [pitch - self.gbias[0], yaw - self.gbias[1]];
        if rate[0].abs().max(rate[1].abs()) < 3.0 * GYRO_LSB {
            // held still: follow the sensor's slow bias drift
            self.gbias[0] += rate[0] / 2000.0;
            self.gbias[1] += rate[1] / 2000.0;
        }
        let mode = text(&self.cfg, "gyro");
        if mode == "off" || mode == "aim" && r[6] < 64 {
            return out; // aim = while L2 is pressed
        }
        let full = GYRO_DPS * GYRO_LSB * 100.0 / num(&self.cfg, "gyro_sensitivity").max(1.0);
        let inv = |k| if flag(&self.cfg, k) { -1.0 } else { 1.0 };
        let (mut gx, mut gy) = (-rate[1] / full * inv("gyro_invert_x"), -rate[0] / full * inv("gyro_invert_y"));
        let mut m = gx.hypot(gy);
        if m > 0.0 {
            let k = (m - GYRO_NOISE * GYRO_LSB / full).max(0.0) / m;
            (gx, gy, m) = (gx * k, gy * k, m * k);
        }
        let (mut sx, mut sy) = ((r[4] as f64 - 128.0) / 127.0, (r[5] as f64 - 128.0) / 127.0);
        if m > 0.0 && sx.hypot(sy) < GAME_DEADZONE {
            // thumb resting: lift the gyro motion past the game's deadzone
            let k = (GAME_DEADZONE + m * (1.0 - GAME_DEADZONE)) / m;
            (gx, gy, sx, sy) = (gx * k, gy * k, 0.0, 0.0);
        }
        out[2] = (128.0 + (sx + gx) * 127.0).round_ties_even().clamp(0.0, 255.0) as u8;
        out[3] = (128.0 + (sy + gy) * 127.0).round_ties_even().clamp(0.0, 255.0) as u8;
        out
    }

    /// USB output 0x02 -> BT output 0x31.
    fn output(&mut self, data: &[u8]) -> io::Result<()> {
        if data.first() != Some(&0x02) {
            println!("output ignored: id {:02x?} {} B", data.first(), data.len());
            return Ok(());
        }
        self.nout += 1;
        if now() - self.tout >= 5.0 {
            let last: String = data.iter().take(12).map(|b| format!("{b:02x}")).collect();
            println!("outputs/s {:.0}, last {last}", self.nout as f64 / (now() - self.tout));
            (self.nout, self.tout) = (0, now());
        }
        let mut r = [0u8; 78];
        (r[0], r[1], r[2]) = (0x31, self.seq << 4, 0x10);
        self.seq = (self.seq + 1) & 0x0F;
        let n = data.len().min(48);
        r[3..2 + n].copy_from_slice(&data[1..n]); // SetStateData common part, 47 bytes
        apply_settings(&mut r[3..50], &self.cfg, self.battery.0);
        let crc = bt_crc(0xA2, &r[..74]);
        r[74..].copy_from_slice(&crc);
        self.real.write_all(&r)
    }

    /// HID class control request; returns the reply bytes, empty to ack, None to stall.
    fn hid_request(&mut self, req: u8, rtype: u8, rnum: u8, data: &[u8], wlen: usize) -> io::Result<Option<Vec<u8>>> {
        Ok(match (req, rtype) {
            (0x0A, _) => Some(vec![]), // SET_IDLE
            (0x01, 3) => match get_feature(&self.real, rnum, wlen.max(64)) { // GET_REPORT feature
                Ok(mut out) => {
                    if rnum == 0x09 && out.len() >= 7 {
                        // hid-playstation refuses a second pad with the same MAC (the real BT one
                        // stays bound): set the locally-administered bit of the first octet
                        out[6] ^= 0x02;
                    }
                    println!("get feature {rnum:#04x}: {} B", out.len());
                    Some(out)
                }
                Err(e) => {
                    println!("get feature {rnum:#04x}: {e}");
                    None
                }
            },
            (0x09, 2) => { // SET_REPORT output
                self.output(data)?;
                Some(vec![])
            }
            (0x09, 3) => { // SET_REPORT feature
                // Like DS5Dongle only forward the test command (0x80) and profile blocks (0x60-0x62);
                // others are acked unsent: Stellar Blade's SET 0x08 (USB-only) makes the pad drop the BT link
                let head: String = data.iter().take(8).map(|b| format!("{b:02x}")).collect();
                println!("set feature {rnum:#04x}: {} B {head}", data.len());
                if (rnum == 0x80 || (0x60..=0x62).contains(&rnum)) && data.len() > 4 {
                    let mut d = data.to_vec();
                    let n = d.len() - 4;
                    let crc = bt_crc(0x53, &d[..n]); // USB leaves the CRC zero, BT checks it
                    d[n..].copy_from_slice(&crc);
                    if set_feature(&self.real, &mut d).is_err() {
                        return Ok(None);
                    }
                }
                Some(vec![])
            }
            _ => None,
        })
    }

    fn on_data(&mut self) -> io::Result<()> {
        let h = self.usb.recv(20)?;
        let (cmd, seq, direction, ep) = (be(&h, 0), be(&h, 1), be(&h, 3), be(&h, 4));
        if cmd == 2 {
            // CMD_UNLINK
            let target = be(&self.usb.recv(28)?, 0);
            let u = &mut self.usb;
            let found = u.int_in.iter().any(|(q, _)| *q == target) || u.timed.iter().any(|Reverse(t)| t.1 == target);
            u.int_in.retain(|(q, _)| *q != target);
            u.timed.retain(|Reverse(t)| t.1 != target);
            let mut msg = vec![];
            for v in [4, seq, 0, 0, 0, if found { ECONNRESET } else { 0 } as u32] {
                msg.extend(v.to_be_bytes());
            }
            msg.extend([0; 24]);
            return u.sock.write_all(&msg);
        }
        let b = self.usb.recv(28)?;
        let (length, start, npackets) = (be(&b, 1) as i32, be(&b, 2) as i32, (be(&b, 3) as i32).max(0));
        let setup: [u8; 8] = b[20..28].try_into().unwrap();
        let data = if direction == 0 && length > 0 { self.usb.recv(length as usize)? } else { vec![] };
        let iso = self.usb.recv(16 * npackets as usize)?;
        let iso: Vec<(usize, usize)> = (0..npackets as usize)
            .map(|i| (be(&iso, 4 * i) as usize, be(&iso, 4 * i + 1) as usize)).collect();
        if ep == 0 {
            self.control(seq, setup, &data)
        } else if ep == 4 && direction == 1 {
            // HID IN
            match self.usb.last_input.take() {
                Some(r) => self.usb.reply(seq, 0, &r[..(length.max(0) as usize).min(64)], None),
                None => {
                    self.usb.int_in.push_back((seq, length.max(0) as usize));
                    Ok(())
                }
            }
        } else if ep == 3 {
            // HID OUT
            self.output(&data)?;
            self.usb.reply(seq, 0, &[], Some(data.len()))
        } else if !iso.is_empty() {
            // audio: complete in real time, the host paces its stream by our completions
            let total: usize = iso.iter().map(|(_, l)| l).sum();
            let payload = if direction == 0 {
                let pcm: Vec<u8> = iso.iter().flat_map(|&(o, l)| data.get(o..o + l).unwrap_or(&[]).iter().copied())
                    .collect();
                self.pcm(&pcm)?;
                vec![]
            } else {
                vec![0; total] // mic: silence
            };
            let key = if direction == 0 { 0 } else { 1 };
            let due = now().max(self.usb.iso_due[key]) + npackets as f64 / 1000.0;
            self.usb.iso_due[key] = due;
            let mut msg = ret_submit(seq, 0, total, start, npackets);
            msg.extend(payload);
            for (o, l) in iso {
                for v in [o as u32, l as u32, l as u32, 0] {
                    msg.extend(v.to_be_bytes());
                }
            }
            self.usb.timed.push(Reverse(((due * 1e9) as u64, seq, msg)));
            Ok(())
        } else {
            self.usb.reply(seq, EPIPE, &[], None)
        }
    }

    fn control(&mut self, seq: u32, setup: [u8; 8], data: &[u8]) -> io::Result<()> {
        let (rtype, req) = (setup[0], setup[1]);
        let value = u16::from_le_bytes([setup[2], setup[3]]);
        let index = u16::from_le_bytes([setup[4], setup[5]]);
        let wlen = u16::from_le_bytes([setup[6], setup[7]]) as usize;
        let (hi, lo) = ((value >> 8) as u8, value as u8);
        // bytes to return (IN) or empty to ack; None stalls
        let out: Option<Vec<u8>> = match (rtype, req) {
            (0x80, 6) => match hi { // GET_DESCRIPTOR
                1 => Some(DEVICE.to_vec()),
                2 => Some(self.usb.config.clone()),
                3 => match lo {
                    0 => Some(vec![4, 3, 0x09, 0x04]),
                    1 => Some(usb_string("Sony Interactive Entertainment")),
                    2 => Some(usb_string("DualSense Wireless Controller")),
                    _ => None,
                },
                _ => None,
            },
            (0x81, 6) => match hi { // interface descriptors: HID report / HID
                0x22 => Some(RDESC.to_vec()),
                0x21 => Some(HID_DESC.to_vec()),
                _ => None,
            },
            (0x80, 0) => Some(vec![0, 0]), // GET_STATUS
            (0x80, 8) => Some(vec![1]),    // GET_CONFIGURATION
            (0x00..=0x02, 1 | 3 | 5 | 9 | 11) => Some(vec![]), // CLEAR/SET_FEATURE, SET_ADDRESS/CONFIG/INTERFACE
            (0x81, 10) => Some(vec![0]),   // GET_INTERFACE
            (0x21 | 0xA1, _) if index & 0xFF == 3 => self.hid_request(req, hi, lo, data, wlen)?, // HID class
            (0x21 | 0xA1 | 0x22 | 0xA2, _) => Some(if rtype & 0x80 == 0 { // audio class: volume/mute/sample rate
                vec![]
            } else if rtype == 0xA2 {
                48000u32.to_le_bytes()[..3].to_vec()
            } else if hi == 1 {
                vec![0] // mute off
            } else {
                let v: i16 = match req { 0x82 => -0x3C00, 0x84 => 0x100, _ => 0 }; // MIN, RES; CUR/MAX 0
                v.to_le_bytes().to_vec()
            }),
            _ => None,
        };
        match out {
            None => {
                let hex: String = setup.iter().map(|b| format!("{b:02x}")).collect();
                println!("stall control {hex}");
                self.usb.reply(seq, EPIPE, &[], None)
            }
            Some(out) if rtype & 0x80 != 0 => self.usb.reply(seq, 0, &out[..out.len().min(wlen)], None),
            Some(_) => self.usb.reply(seq, 0, &[], Some(data.len())),
        }
    }

    /// Short sample after a slider change in DS5Tee: speaker beep, haptics buzz or rumble.
    fn preview(&mut self, kind: &str) -> io::Result<()> {
        let t0 = now();
        if !self.timers.is_empty() {
            // a sample is still playing: cut it, interleaved packets would garble both
            self.timers.clear();
            let mut stop = vec![0u8; 48];
            (stop[0], stop[1]) = (0x02, 0x03);
            self.output(&stop)?; // and make sure no motor keeps running
        }
        if kind == "rumble" {
            // left motor alone, then right motor alone; the left motor coasts: 0.5 s gap
            for (at, left, right) in [(0.0, 0xC0, 0), (0.2, 0, 0), (0.7, 0, 0xC0), (0.9, 0, 0)] {
                let mut r = vec![0u8; 48];
                (r[0], r[1], r[3], r[4]) = (0x02, 0x03, right, left);
                self.timers.push((t0 + at, Step::Output(r)));
            }
        } else if kind == "speaker" || kind == "haptics" {
            let n = 512 * 2 * if kind == "speaker" { 16 } else { 56 }; // 0.34 s / 1.2 s, ends in silence
            let t = |i: usize| i as f64 / 48000.0;
            let (wave, ch): (Vec<f64>, usize) = if kind == "speaker" {
                // "lightsaber" swoosh the user liked: falling pitch, fast decay, soft attack
                let mut phase = 0.0;
                ((0..n).map(|i| {
                    phase += 450.0 + 550.0 * (-t(i) / 0.025).exp(); // 1000 -> 450 Hz (user pick)
                    (2.0 * PI * phase / 48000.0).sin() * (-t(i) / 0.045).exp() * (t(i) / 0.003).min(1.0) * 9000.0
                }).collect(), 0)
            } else {
                // game-like pattern: 0.5 s continuous, pause, then on / off / on pulses
                let on = |x: f64| [(0.0, 0.5), (0.6, 0.7), (0.8, 0.9), (1.0, 1.15)].iter().any(|&(a, b)| x >= a && x < b);
                let mut sum = vec![0.0; n + 1]; // prefix sums: 5 ms moving average = ramps, no clicks
                for i in 0..n {
                    sum[i + 1] = sum[i] + if on(t(i)) { 1.0 } else { 0.0 };
                }
                ((0..n).map(|i| {
                    let gate = (sum[(i + 120).min(n)] - sum[i.saturating_sub(120)]) / 240.0;
                    (2.0 * PI * 110.0 * t(i)).sin() * gate * 16000.0
                }).collect(), 2)
            };
            let mut pcm = vec![0i16; 2 * 2048]; // leading silent packet primes the pad's audio buffer
            for w in wave {
                let mut frame = [0i16; 4];
                frame[ch] = w as i16;
                frame[ch + 1] = w as i16;
                pcm.extend(frame);
            }
            // sending the trailing silence too (force) lets it drain cleanly - both avoid the crackle of an underrun
            for (i, pair) in pcm.chunks(2 * 2048).enumerate() { // one 0x39 report per 2 blocks, paced like real playback
                let at = t0 + (2 * i * 512) as f64 / 48000.0;
                self.timers.push((at, Step::Audio(pair[..2048].to_vec(), pair[2048..].to_vec())));
            }
        }
        self.timers.sort_by(|a, b| a.0.total_cmp(&b.0));
        Ok(())
    }

    /// Run due preview steps; returns seconds until the next one.
    fn tick(&mut self) -> io::Result<Option<f64>> {
        let t = now();
        while self.timers.first().is_some_and(|(due, _)| *due <= t) {
            match self.timers.remove(0).1 {
                Step::Output(r) => self.output(&r)?,
                Step::Audio(a, b) => self.audio(&[a, b], true)?,
            }
        }
        Ok(self.timers.first().map(|(due, _)| (due - t).max(0.0)))
    }

    /// Speaker volume + output path (what the PS5/Windows audio driver sets on a wired pad).
    fn audio_route(&mut self) -> io::Result<()> {
        let mut r = [0u8; 48];
        (r[0], r[1], r[2]) = (0x02, 0x10 | 0x20 | 0x80, 0x80); // allow headphone/speaker volume, audio control (1+2)
        (r[5], r[6]) = (0x7C, 0x64); // headphone, speaker volume (0x64 = max)
        r[8] = if self.headset { 0x00 } else { 0x30 }; // output path: headset jack / internal speaker
        r[2] |= 0x40; // motor power level: no global trigger reduction (strength is per side now)
        r[38] = SPEAKER_PREGAIN; // audio_control2 (common offset 37)
        self.output(&r)
    }

    /// Forced mode -> "game": clear the forced effect, games without trigger support never overwrite it.
    fn release_triggers(&mut self, old: &Cfg) -> io::Result<()> {
        let mut r = [0u8; 48];
        r[0] = 0x02;
        for (side, bit, lo) in [("r", 0x04, 11), ("l", 0x08, 22)] {
            let k = format!("trigger_mode_{side}");
            if text(old, &k) != "game" && text(&self.cfg, &k) == "game" {
                r[1] |= bit;
                r[lo] = 0x05; // off
            }
        }
        if r[1] != 0 { self.output(&r) } else { Ok(()) }
    }

    /// 4-ch s16 PCM from the USB audio stream; every 2 x 512 frames -> one BT 0x39 report.
    fn pcm(&mut self, data: &[u8]) -> io::Result<()> {
        self.pcm_buf.extend_from_slice(data);
        while self.pcm_buf.len() >= AFRAME {
            let frame = self.pcm_buf.drain(..AFRAME).collect::<Vec<u8>>()
                .chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
            self.frames.push(frame);
            if self.frames.len() == 2 {
                let frames = std::mem::take(&mut self.frames);
                self.audio(&frames, false)?;
            }
        }
        Ok(())
    }

    /// Two 512-frame PCM blocks -> BT report 0x39 (2 haptics frames + 2 Opus speaker frames).
    fn audio(&mut self, frames: &[Vec<i16>], force: bool) -> io::Result<()> {
        if !force && frames.iter().all(|f| f.iter().all(|&s| s == 0)) {
            return Ok(()); // silence: nothing to send
        }
        let mut r = [0u8; 547];
        let hgain = HAPTICS_GAIN * num(&self.cfg, "haptics") / 100.0;
        let sgain = SPEAKER_GAIN * (num(&self.cfg, "speaker") / 100.0).powi(3); // cubic: audible steps
        for (fi, f) in frames.iter().enumerate() {
            for blk in 0..32 {
                // 48 kHz -> 3 kHz, s16 -> s8
                for c in 0..2 {
                    let mean = (0..16).map(|i| f[(blk * 16 + i) * 4 + 2 + c] as f64).sum::<f64>() / 16.0;
                    r[12 + fi * 64 + blk * 2 + c] = (mean / 256.0 * hgain).clamp(-128.0, 127.0) as i8 as u8;
                }
            }
            // 512 -> 480 frames like DS5Dongle: the pad plays one 10 ms Opus frame per 10.67 ms block
            let mut sp = [0i16; 960];
            for k in 0..480 {
                let x = k as f64 * 511.0 / 479.0;
                let (i, frac) = (x as usize, x.fract());
                for c in 0..2 {
                    let a = f[i * 4 + c] as f64;
                    let b = f[(i + 1).min(511) * 4 + c] as f64;
                    let v = a + (b - a) * frac;
                    sp[k * 2 + c] = ((v / 32768.0 * sgain).tanh() * 32767.0) as i16; // soft clipping, no hard distortion
                }
            }
            let enc = self.opus.encode(&sp);
            r[142 + fi * 200..142 + (fi + 1) * 200].copy_from_slice(&enc);
        }
        (r[0], r[1], r[2], r[3], r[4]) = (0x39, self.seq << 4, 0x91, 6, 0b01111110);
        self.seq = (self.seq + 1) & 0x0F;
        self.packets = self.packets.wrapping_add(2);
        r[5..9].fill(48); // audio buffer length (DS5Dongle default)
        r[9] = self.packets;
        (r[10], r[11]) = (0xD2, 64); // haptics block, 2 frames of 64 bytes
        (r[140], r[141]) = ((if self.headset { 0x16 } else { 0x13 }) | 0xC0, 200); // headset jack or speaker
        let crc = bt_crc(0xA2, &r[..543]);
        r[543..].copy_from_slice(&crc);
        self.real.write_all(&r)
    }
}

/// Fork a watchdog; returns the pipe the main loop beats on. A hidraw write or ioctl can sleep
/// uninterruptibly on the kernel's global hidraw lock while a game holds it in open() of our virtual pad
/// and waits for USB replies only this loop sends: everything touching the pad freezes for good. Detaching
/// the vhci ports fails those waits, the game's open returns, the lock frees, and the main loop drops the
/// pads and re-creates them on its next scan. Its own process, since the stuck main thread can't act.
fn watchdog(vhci: &str, secs: i32) -> io::Result<OwnedFd> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    match unsafe { libc::fork() } {
        -1 => return Err(io::Error::last_os_error()),
        0 => {}
        _ => {
            drop(r);
            unsafe { libc::fcntl(w.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
            return Ok(w);
        }
    }
    unsafe { libc::signal(libc::SIGTERM, libc::SIG_DFL) }; // not the parent's handler and its cleanup
    drop(w);
    let mut buf = [0u8; 4096];
    loop {
        let mut p = libc::pollfd { fd: r.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut p, 1, secs * 1000) } > 0 {
            if unsafe { libc::read(r.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } <= 0 {
                break; // main process gone
            }
            continue;
        }
        let used: Vec<String> = read(&format!("{vhci}/status")).lines().skip(1)
            .filter_map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                (f.len() > 2 && f[2] != "004").then(|| f[1].parse::<u32>().ok().map(|p| p.to_string())).flatten()
            }).collect();
        if !used.is_empty() {
            println!("watchdog: main loop stuck for {secs} s, detaching vhci ports {used:?}");
        }
        for port in used {
            let _ = fs::write(format!("{vhci}/detach"), port);
        }
    }
    unsafe { libc::_exit(0) }
}

fn write_status(bridges: &BTreeMap<String, Bridge>, enabled: bool, last: &mut String) {
    let pads: Vec<Value> = bridges.values().map(|b| json!({"mac": b.mac, "battery": b.battery.0,
        "charging": b.battery.1, "headset": b.headset})).collect();
    let status = json!({"pads": pads, "bridge": enabled}).to_string();
    if status == *last && fs::exists(STATUS_PATH).unwrap_or(false) {
        return;
    }
    let _ = fs::create_dir_all("/run/ds5bridge");
    let tmp = format!("{STATUS_PATH}.tmp");
    if fs::write(&tmp, &status).and_then(|_| fs::rename(&tmp, STATUS_PATH)).is_ok() {
        *last = status;
    }
}

/// `bluetoothctl disconnect`, given up after 3 s when bluetoothd is busy (next idle check tries again).
fn disconnect(mac: &str) {
    let Ok(mut child) = Command::new("bluetoothctl").args(["disconnect", mac])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() else { return };
    for _ in 0..30 {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn main() {
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
    let mut bridges: BTreeMap<String, Bridge> = BTreeMap::new(); // hidraw path -> Bridge
    let (mut next_scan, mut next_cfg) = (0.0, 0.0);
    let mut config = Config::new();
    config.load();
    let mut last_preview = config.value["preview"]["n"].clone(); // don't replay an old preview at start
    let mut last_status = String::new();
    let beat = watchdog(VHCI, WATCHDOG_S).expect("watchdog");
    let mut beat = File::from(beat);
    while !STOP.load(Ordering::Relaxed) {
        let _ = beat.write(b".");
        let t = now();
        if t >= next_cfg {
            next_cfg = t + 0.3;
            if config.load() {
                let mut failed = vec![];
                for (path, b) in bridges.iter_mut() {
                    let new = settings_for(&config.value, &b.mac);
                    if new != b.cfg {
                        let old = std::mem::replace(&mut b.cfg, new);
                        println!("settings {} {}", b.mac, Value::Object(b.cfg.clone()));
                        // pushes triggers, lightbar, player LEDs right away
                        if b.audio_route().and_then(|_| b.release_triggers(&old)).is_err() {
                            failed.push(path.clone());
                        }
                    }
                }
                let p = &config.value["preview"];
                if p["n"] != last_preview {
                    last_preview = p["n"].clone();
                    let (mac, kind) = (p["mac"].as_str().unwrap_or(""), p["kind"].as_str().unwrap_or(""));
                    for (path, b) in bridges.iter_mut() {
                        if (mac.is_empty() || mac == b.mac) && b.preview(kind).is_err() { // "" = default section: all pads
                            failed.push(path.clone());
                        }
                    }
                }
                for path in failed {
                    if let Some(b) = bridges.remove(&path) {
                        b.close();
                    }
                }
            }
        }
        if t >= next_scan {
            let enabled = config.enabled();
            set_hiding(enabled);
            let current = if enabled { bt_dualsenses() } else { BTreeMap::new() };
            bridges.retain(|path, b| current.contains_key(path) || {
                b.close();
                false
            });
            for (path, (hiddir, mac)) in &current {
                if !bridges.contains_key(path) {
                    match Bridge::new(path, mac, &config.value) {
                        Ok(b) => {
                            bridges.insert(path.clone(), b);
                        }
                        Err(e) => {
                            println!("{path}: {e}");
                            continue;
                        }
                    }
                }
                hide(hiddir, path); // repeated: udev may restore permissions on change events
            }
            next_scan = t + 2.0;
            write_status(&bridges, enabled, &mut last_status);
            for b in bridges.values_mut() {
                let idle = num(&b.cfg, "idle_minutes");
                if idle > 0.0 && t - b.active_at > idle * 60.0 {
                    println!("{} idle for {idle} min, disconnecting", b.mac);
                    disconnect(&b.mac);
                    b.active_at = t;
                }
            }
        }
        let mut timeout: f64 = 2.0;
        let mut fds = vec![];
        let mut failed = vec![];
        for (path, b) in bridges.iter_mut() {
            match b.usb.tick().and_then(|a| Ok((a, b.tick()?))) {
                Ok((a, c)) => {
                    for due in [a, c].into_iter().flatten() {
                        timeout = timeout.min(due);
                    }
                }
                Err(e) => {
                    println!("{}: {e} in tick", b.mac);
                    failed.push(path.clone());
                    continue;
                }
            }
            fds.push((libc::pollfd { fd: b.real.as_raw_fd(), events: libc::POLLIN, revents: 0 }, path.clone(), false));
            fds.push((libc::pollfd { fd: b.usb.sock.as_raw_fd(), events: libc::POLLIN, revents: 0 }, path.clone(), true));
        }
        for path in failed {
            if let Some(b) = bridges.remove(&path) {
                b.close();
            }
            next_scan = 0.0;
        }
        let mut pfds: Vec<libc::pollfd> = fds.iter().map(|f| f.0).collect();
        let ms = (timeout * 1000.0).ceil() as i32;
        if unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as _, ms) } <= 0 {
            continue; // timeout or signal
        }
        for (p, (_, path, is_usb)) in pfds.iter().zip(&fds) {
            if p.revents == 0 {
                continue;
            }
            let Some(b) = bridges.get_mut(path) else { continue };
            let res = if *is_usb { b.on_data() } else { b.on_real() };
            if let Err(e) = res {
                // controller or vhci gone: drop it, the rescan re-creates it
                println!("{}: {e} in {}", b.mac, if *is_usb { "on_data" } else { "on_real" });
                bridges.remove(path).unwrap().close();
                next_scan = 0.0;
                break;
            }
        }
    }
    for b in bridges.values() {
        b.close();
    }
    set_hiding(false); // without the bridge a hidden pad would be unusable
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_zlib() {
        // zlib.crc32(b"\xa2" + b"123456789") & 0xFFFFFFFF == 0x63da9f12
        assert_eq!(bt_crc(0xA2, b"123456789"), 0x63da9f12u32.to_le_bytes());
    }

    #[test]
    fn effects_scale() {
        let mut e = [0x01, 0x40, 0xC0, 0, 0, 0, 0, 0, 0, 0, 0];
        scale_effect(&mut e, 50.0);
        assert_eq!(e[2], 0x60);
        let mut w = [0x25, 0, 0, 0b111, 0, 0, 0, 0, 0, 0, 0]; // weapon strength 8 (stored 7) -> 4 (stored 3)
        scale_effect(&mut w, 50.0);
        assert_eq!(w[3], 3);
        assert_eq!(battery_color(Some(100)), [0, 255, 0]);
        assert_eq!(battery_color(Some(0)), [255, 0, 0]);
    }

    /// The watchdog detaches used vhci ports only when the main loop stops beating.
    #[test]
    fn watchdog_detaches() {
        let dir = std::env::temp_dir().join(format!("ds5wd{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        fs::write(format!("{d}/status"), "hub port sta spd dev sockfd local_busid\n\
            hs 0000 006 002 00010002 000005 12-1\nhs 0001 004 000 0 0 0-0\n").unwrap();
        fs::write(format!("{d}/detach"), "").unwrap();
        let mut w = File::from(watchdog(d, 1).unwrap());
        for _ in 0..3 {
            w.write_all(b".").unwrap();
            std::thread::sleep(Duration::from_millis(500));
        }
        assert_eq!(read(&format!("{d}/detach")), "", "fired while the loop was beating");
        std::thread::sleep(Duration::from_millis(1600));
        assert_eq!(read(&format!("{d}/detach")), "0", "did not detach the used port");
        drop(w);
        std::thread::sleep(Duration::from_millis(300));
        assert!(unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) } > 0, "watchdog outlived the main process");
        fs::remove_dir_all(dir).unwrap();
    }
}
