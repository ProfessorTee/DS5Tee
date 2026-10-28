#!/usr/bin/env python3
"""DualSense Bluetooth -> virtual wired USB DualSense (the DS5Dongle Pico, in software).

Games only enable adaptive triggers, HD haptics and the controller speaker for a wired
DualSense, and they pair its HID and audio functions through the shared USB device.
So for each DualSense connected over Bluetooth this plays a complete USB DualSense
(DS5Dongle's descriptors: UAC1 4-ch speaker/haptics + 2-ch mic + HID) to the kernel
through USB/IP (vhci_hcd, attached over a local socketpair). Linux, PipeWire and Proton
then see a real USB device with one container, exactly like a cable.

  BT input 0x31 (78 B)      -> HID interrupt IN 0x01 (64 B)
  HID output 0x02           -> BT output 0x31 with sequence tag + CRC32
  feature GET               -> forwarded to the controller
  feature SET               -> only 0x80 / 0x60-0x62 forwarded (CRC recomputed), like DS5Dongle
  iso OUT 4ch s16 48 kHz    -> BT 0x39: ch3/4 haptics at 3 kHz int8, ch1/2 speaker as Opus
Runs as root: needs vhci_hcd and hides the real Bluetooth nodes from users, so games and
Steam only see the USB pad. Other Bluetooth devices are untouched.
"""
import ctypes, fcntl, glob, json, heapq, os, select, socket, struct, subprocess, sys, time, zlib
import numpy as np

HERE = os.path.dirname(os.path.realpath(__file__))
RDESC = open(os.path.join(HERE, "usb_report_desc.bin"), "rb").read()  # HID report descriptor (DS5Dongle)
VHCI = "/sys/devices/platform/vhci_hcd.0"
AFRAME = 512 * 4 * 2  # 512 frames x 4 ch x s16 = 10.67 ms -> one haptics + one speaker frame
HAPTICS_GAIN = 2.0  # at haptics 100 %; DS5Dongle allows 1.0-2.0, 1.0 felt weak
SPEAKER_GAIN = 2.5  # at speaker 100 %: digital boost before Opus, soft-clipped; the BT speaker path is quieter than a PS5
SPEAKER_PREGAIN = 7  # speaker pre-amp gain 1-7 (DS5Dongle default 2); PS5 games sound louder than 2
EPIPE, ECONNRESET = -32, -104
CONFIG_PATH = os.environ.get("DS5BRIDGE_CONFIG", "/etc/ds5bridge.json")  # written by DS5Tee
STATUS_PATH = "/run/ds5bridge/status.json"  # read by DS5Tee
DEFAULTS = {"speaker": 50, "haptics": 100, "rumble": 100, "trigger_strength_l": 100, "trigger_strength_r": 100,
            "trigger_mode_l": "game", "trigger_mode_r": "game", "color": "", "player": 0, "idle_minutes": 15}
PLAYER_LEDS = {1: 0x04, 2: 0x0A, 3: 0x15, 4: 0x1B}
TRIGGER_EFFECTS = {  # raw DualSense trigger effects (mode, params...), forced per trigger
    "off": [0x05],
    "resistance": [0x01, 0x40, 0xC0],  # continuous: start position, force
    "weapon": [0x02, 0x40, 0xA0, 0xFF],  # section: start, end, force
    "vibration": [0x06, 0x28, 0xC0, 0x30],  # frequency, amplitude, start position
}
config = {}  # {"default": {...}, "pads": {MAC: {...}}} as written by DS5Tee


def load_config():
    """Re-read the tray tool's settings; returns True when they changed."""
    global config
    try:
        with open(CONFIG_PATH) as f:
            new = json.load(f)
    except (OSError, ValueError):
        new = {}
    changed, config = new != config, new
    return changed


def settings_for(mac):
    """Per-controller settings: defaults < "default" section < that pad's own section."""
    return {**DEFAULTS, **config.get("default", {}), **config.get("pads", {}).get(mac, {})}


def scale_effect(e, pct):
    """Scale the force of one trigger effect (11 bytes). The pad's own power reduction is shared by
    both triggers, so per-side strength rescales the effect parameters of the common modes instead."""
    f = pct / 100

    def packed(lo, hi, zones):  # 3-bit strengths (stored as strength-1) packed little-endian
        v = int.from_bytes(e[lo:hi], "little")
        for z in range(zones):
            st = (v >> 3 * z) & 7
            v = v & ~(7 << 3 * z) | max(0, round((st + 1) * f) - 1) << 3 * z
        e[lo:hi] = v.to_bytes(hi - lo, "little")

    mode = e[0]
    if mode == 0x01:  # continuous resistance: start, force
        e[2] = round(e[2] * f)
    elif mode == 0x02:  # section: start, end, force
        e[3] = round(e[3] * f)
    elif mode == 0x06:  # vibration: frequency, amplitude, start
        e[2] = round(e[2] * f)
    elif mode in (0x21, 0x26):  # feedback / vibration: zone mask, 10 packed strengths
        packed(3, 7, 10)
    elif mode == 0x25:  # weapon: zone mask, strength
        packed(3, 4, 1)
    # ponytail: bow (0x22), galloping (0x23), machine (0x27) unscaled; add them if a game uses them


def apply_settings(body, cfg):
    """User overrides on an outgoing SetStateData (common part, 47 bytes, report id stripped)."""
    if body[0] & 0x03:  # rumble motors: scale the values and the pad's own rumble power (high nibble)
        body[2] = body[2] * cfg["rumble"] // 100
        body[3] = body[3] * cfg["rumble"] // 100
        body[1] |= 0x40
        body[36] = (body[36] & 0x0F) | min(7, round((100 - cfg["rumble"]) / 12.5)) << 4
    for side, flag, lo in (("r", 0x04, 10), ("l", 0x08, 21)):
        if cfg["trigger_mode_" + side] != "game":
            body[0] |= flag
            body[lo:lo + 11] = bytes(TRIGGER_EFFECTS.get(cfg["trigger_mode_" + side], [0x05])).ljust(11, b"\0")
        if body[0] & flag and cfg["trigger_strength_" + side] < 100:
            e = bytearray(body[lo:lo + 11])
            scale_effect(e, cfg["trigger_strength_" + side])
            body[lo:lo + 11] = e
    if cfg["color"]:
        body[1] |= 0x04
        body[44:47] = bytes.fromhex(cfg["color"].lstrip("#"))
    if cfg["player"] in PLAYER_LEDS:
        body[1] |= 0x10
        body[43] = PLAYER_LEDS[cfg["player"]]


def s(text):  # USB string descriptor
    b = text.encode("utf-16-le")
    return bytes([2 + len(b), 3]) + b


DEVICE = bytes([0x12, 0x01, 0x00, 0x02, 0, 0, 0, 0x40, 0x4C, 0x05, 0xE6, 0x0C, 0x00, 0x01, 1, 2, 0, 1])
HID_DESC = bytes([0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22]) + struct.pack("<H", len(RDESC))
CONFIG = bytes([  # DS5Dongle usb_descriptors.cpp without CDC/wake interfaces
    0x09, 0x02, 0xE3, 0x00, 0x04, 0x01, 0x00, 0xC0, 0xFA,
    # interface 0: audio control
    0x09, 0x04, 0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00,
    0x0A, 0x24, 0x01, 0x00, 0x01, 0x49, 0x00, 0x02, 0x01, 0x02,
    0x0C, 0x24, 0x02, 0x01, 0x01, 0x01, 0x06, 0x04, 0x33, 0x00, 0x00, 0x00,  # IT 1: USB streaming, 4 ch
    0x0C, 0x24, 0x06, 0x02, 0x01, 0x01, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,  # FU 2: mute, volume
    0x09, 0x24, 0x03, 0x03, 0x01, 0x03, 0x04, 0x02, 0x00,  # OT 3: speaker
    0x0C, 0x24, 0x02, 0x04, 0x02, 0x04, 0x03, 0x02, 0x03, 0x00, 0x00, 0x00,  # IT 4: headset mic, 2 ch
    0x09, 0x24, 0x06, 0x05, 0x04, 0x01, 0x03, 0x00, 0x00,  # FU 5
    0x09, 0x24, 0x03, 0x06, 0x01, 0x01, 0x01, 0x05, 0x00,  # OT 6: USB streaming
    # interface 1: speaker/haptics stream, EP1 OUT iso 4 ch s16 48 kHz
    0x09, 0x04, 0x01, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00,
    0x09, 0x04, 0x01, 0x01, 0x01, 0x01, 0x02, 0x00, 0x00,
    0x07, 0x24, 0x01, 0x01, 0x01, 0x01, 0x00,
    0x0B, 0x24, 0x02, 0x01, 0x04, 0x02, 0x10, 0x01, 0x80, 0xBB, 0x00,
    0x09, 0x05, 0x01, 0x09, 0x88, 0x01, 0x01, 0x00, 0x00,
    0x07, 0x25, 0x01, 0x00, 0x00, 0x00, 0x00,
    # interface 2: mic stream, EP2 IN iso 2 ch s16 48 kHz (answered with silence)
    0x09, 0x04, 0x02, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00,
    0x09, 0x04, 0x02, 0x01, 0x01, 0x01, 0x02, 0x00, 0x00,
    0x07, 0x24, 0x01, 0x06, 0x01, 0x01, 0x00,
    0x0B, 0x24, 0x02, 0x01, 0x02, 0x02, 0x10, 0x01, 0x80, 0xBB, 0x00,
    0x09, 0x05, 0x82, 0x05, 0xC4, 0x00, 0x01, 0x00, 0x00,
    0x07, 0x25, 0x01, 0x00, 0x00, 0x00, 0x00,
    # interface 3: HID, EP4 IN / EP3 OUT interrupt 64 B
    0x09, 0x04, 0x03, 0x00, 0x02, 0x03, 0x00, 0x00, 0x00,
    *HID_DESC,
    0x07, 0x05, 0x84, 0x03, 0x40, 0x00, 0x01,
    0x07, 0x05, 0x03, 0x03, 0x40, 0x00, 0x01,
])
assert len(CONFIG) == 0xE3
STRINGS = {0: bytes([4, 3, 0x09, 0x04]), 1: s("Sony Interactive Entertainment"), 2: s("DualSense Wireless Controller")}


def ioc(dir_, nr, size):  # _IOC('H', nr, size)
    return (dir_ << 30) | (size << 16) | (ord("H") << 8) | nr


def get_feature(fd, rid, size):  # HIDIOCGFEATURE
    buf = bytearray(size)
    buf[0] = rid
    n = fcntl.ioctl(fd, ioc(3, 0x07, size), buf, True)
    return bytes(buf[:n])


def set_feature(fd, data):  # HIDIOCSFEATURE
    fcntl.ioctl(fd, ioc(3, 0x06, len(data)), bytearray(data), True)


def bt_crc(seed, data):  # DualSense BT CRC32 over a 1-byte HID header + report
    return struct.pack("<I", zlib.crc32(bytes([seed]) + data) & 0xFFFFFFFF)


def read(path):
    try:
        with open(path) as f:
            return f.read().strip()
    except OSError:
        return ""


def bt_dualsenses():
    """{hidraw path: (sysfs hid dir, MAC)} for DualSenses connected over Bluetooth."""
    found = {}
    for h in glob.glob("/sys/class/hidraw/hidraw*"):
        dev = os.path.realpath(h + "/device")
        if os.path.basename(dev).upper().startswith("0005:054C:0CE6"):
            uevent = dict(l.split("=", 1) for l in read(dev + "/uevent").splitlines() if "=" in l)
            found["/dev/" + os.path.basename(h)] = (dev, uevent.get("HID_UNIQ", "").upper())
    return found


def hide(hiddir, hidraw):
    """Make the real Bluetooth nodes root-only so games/Steam don't see a second pad."""
    nodes = [hidraw] + ["/dev/input/" + os.path.basename(p) for p in glob.glob(hiddir + "/input/input*/event*")
                        + glob.glob(hiddir + "/input/input*/js*")]
    for n in nodes:
        try:
            if os.stat(n).st_mode & 0o077:
                os.chown(n, 0, 0)
                os.chmod(n, 0o600)  # also masks uaccess ACL entries
        except OSError:
            pass


class Opus:
    """10 ms CBR Opus frames of 200 bytes, as the DualSense expects (DS5Dongle settings)."""
    def __init__(self):
        self.lib = ctypes.CDLL("libopus.so.0")
        self.lib.opus_encoder_create.restype = ctypes.c_void_p
        err = ctypes.c_int()
        self.enc = ctypes.c_void_p(self.lib.opus_encoder_create(48000, 2, 2051, ctypes.byref(err)))  # RESTRICTED_LOWDELAY = CELT
        if err.value:
            raise OSError(f"opus_encoder_create {err.value}")
        for req, val in ((4002, 160000), (4006, 0), (4010, 0), (4040, 5003)):  # bitrate, VBR off, complexity, 10 ms
            self.lib.opus_encoder_ctl(self.enc, ctypes.c_int(req), ctypes.c_int(val))
        self.out = ctypes.create_string_buffer(200)

    def encode(self, pcm):  # (480, 2) int16
        n = self.lib.opus_encode(self.enc, pcm.ctypes.data_as(ctypes.c_void_p), 480, self.out, 200)
        return self.out.raw[:max(n, 0)].ljust(200, b"\0")


class Usb:
    """The USB side: a USB/IP device served over a socketpair whose other end vhci_hcd owns."""
    def __init__(self, bridge):
        self.bridge = bridge
        self.sock, kernel_end = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
        self.int_in = []  # pending HID IN URBs (seqnum, length)
        self.last_input = None  # newest input report not yet delivered
        self.timed = []  # heap of (due, seqnum, reply) for paced iso completions
        self.iso_due = {1: 0.0, 0x82: 0.0}
        subprocess.run(["modprobe", "vhci-hcd"], check=False)
        self.port = next(int(l.split()[1]) for l in read(VHCI + "/status").splitlines()[1:]
                         if l.split()[0] == "hs" and l.split()[2] == "004")  # free USB2 port
        with open(VHCI + "/attach", "w") as f:  # port sockfd devid speed(full)
            f.write(f"{self.port} {kernel_end.fileno()} {(1 << 16) | 2} 2")
        kernel_end.close()  # vhci holds its own reference now

    def close(self):
        try:
            with open(VHCI + "/detach", "w") as f:
                f.write(str(self.port))
        except OSError:
            pass
        self.sock.close()

    def recv(self, n):
        buf = b""
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk:
                raise OSError("vhci closed the connection")
            buf += chunk
        return buf

    def reply(self, seq, status=0, data=b"", actual=None, start=0, iso=b"", npackets=0):
        self.sock.sendall(struct.pack(">IIIIIiiiii8x", 3, seq, 0, 0, 0, status,
                                      len(data) if actual is None else actual, start, npackets, 0) + data + iso)

    def input(self, report):
        """New HID input report: complete a waiting IN URB or keep it for the next one."""
        if self.int_in:
            seq, length = self.int_in.pop(0)
            self.reply(seq, data=report[:length])
        else:
            self.last_input = report

    def tick(self):
        """Send paced iso completions that are due; returns seconds until the next one."""
        now = time.monotonic()
        while self.timed and self.timed[0][0] <= now:
            self.sock.sendall(heapq.heappop(self.timed)[2])
        return max(0.0, self.timed[0][0] - now) if self.timed else None

    def on_data(self):
        cmd, seq, devid, direction, ep = struct.unpack(">5I", self.recv(20))
        if cmd == 2:  # CMD_UNLINK
            target = struct.unpack(">I24x", self.recv(28))[0]
            found = any(q == target for q, _ in self.int_in) or any(t[1] == target for t in self.timed)
            self.int_in = [(q, l) for q, l in self.int_in if q != target]
            self.timed = [t for t in self.timed if t[1] != target]
            heapq.heapify(self.timed)
            self.sock.sendall(struct.pack(">IIIIIi24x", 4, seq, 0, 0, 0, ECONNRESET if found else 0))
            return
        flags, length, start, npackets, interval, setup = struct.unpack(">IiiiI8s", self.recv(28))
        npackets = npackets if npackets > 0 else 0
        data = self.recv(length) if direction == 0 and length > 0 else b""
        iso = [struct.unpack(">4I", self.recv(16)) for _ in range(npackets)]
        if ep == 0:
            self.control(seq, setup, data)
        elif ep == 4 and direction == 1:  # HID IN
            if self.last_input is not None:
                self.reply(seq, data=self.last_input[:length])
                self.last_input = None
            else:
                self.int_in.append((seq, length))
        elif ep == 3:  # HID OUT
            self.bridge.output(data)
            self.reply(seq, actual=len(data))
        elif iso:  # audio: complete in real time, the host paces its stream by our completions
            key = 1 if direction == 0 else 0x82
            if direction == 0:
                self.bridge.pcm(b"".join(data[o:o + l] for o, l, _, _ in iso))
                payload, total = b"", sum(l for _, l, _, _ in iso)
            else:
                payload = bytes(sum(l for _, l, _, _ in iso))  # mic: silence
                total = len(payload)
            descs = b"".join(struct.pack(">4I", o, l, l, 0) for o, l, _, _ in iso)
            due = max(time.monotonic(), self.iso_due[key]) + npackets / 1000
            self.iso_due[key] = due
            msg = struct.pack(">IIIIIiiiii8x", 3, seq, 0, 0, 0, 0, total, start, npackets, 0) + payload + descs
            heapq.heappush(self.timed, (due, seq, msg))
        else:
            self.reply(seq, EPIPE)

    def control(self, seq, setup, data):
        rtype, req, value, index, wlen = struct.unpack("<BBHHH", setup)
        out = None  # bytes to return (IN) or b"" to ack; None stalls
        if rtype == 0x80 and req == 6:  # GET_DESCRIPTOR
            kind, idx = value >> 8, value & 0xFF
            out = {1: DEVICE, 2: CONFIG}.get(kind) if kind != 3 else STRINGS.get(idx)
        elif rtype == 0x81 and req == 6:  # interface descriptors: HID report / HID
            out = {0x22: RDESC, 0x21: HID_DESC}.get(value >> 8)
        elif rtype == 0x80 and req == 0:  # GET_STATUS
            out = b"\0\0"
        elif rtype == 0x80 and req == 8:  # GET_CONFIGURATION
            out = b"\x01"
        elif rtype in (0x00, 0x01, 0x02) and req in (1, 3, 5, 9, 11):  # CLEAR/SET_FEATURE, SET_ADDRESS/CONFIG/INTERFACE
            out = b""
        elif rtype == 0x81 and req == 10:  # GET_INTERFACE
            out = b"\0"
        elif rtype in (0x21, 0xA1) and index & 0xFF == 3:  # HID class
            out = self.bridge.hid_request(req, value >> 8, value & 0xFF, data, wlen)
        elif rtype in (0x21, 0xA1, 0x22, 0xA2):  # audio class: volume/mute/sample rate
            sel = value >> 8
            if rtype & 0x80:
                if rtype == 0xA2:
                    out = (48000).to_bytes(3, "little")
                elif sel == 1:
                    out = b"\0"  # mute off
                else:
                    out = struct.pack("<h", {0x81: 0, 0x82: -0x3C00, 0x83: 0, 0x84: 0x100}.get(req, 0))
            else:
                out = b""
        if out is None:
            print(f"stall control {setup.hex()}", flush=True)
            self.reply(seq, EPIPE)
        elif rtype & 0x80:
            self.reply(seq, data=out[:wlen])
        else:
            self.reply(seq, actual=len(data))


class Bridge:
    def __init__(self, hidraw, hiddir, mac):
        self.hidraw, self.hiddir, self.mac = hidraw, hiddir, mac
        self.real = os.open(hidraw, os.O_RDWR | os.O_NONBLOCK)
        self.seq, self.packets, self.headset, self.battery = 0, 0, False, (None, False)
        self.cfg = settings_for(mac)
        self.timers = []  # (due, callable) for previews
        self.controls, self.active_at = None, time.monotonic()  # for auto-disconnect when idle
        self.nout, self.tout = 0, time.monotonic()  # output reports per second, for the log
        self.pcm_buf, self.frames, self.opus = b"", [], Opus()
        self.usb = Usb(self)
        print(f"bridge {mac} {hidraw} -> vhci port {self.usb.port}", flush=True)
        self.audio_route()

    def close(self):
        self.usb.close()
        try:
            os.close(self.real)
        except OSError:
            pass
        print(f"closed {self.mac}", flush=True)

    def on_real(self):
        """BT input report -> USB input report."""
        while True:
            try:
                r = os.read(self.real, 128)
            except BlockingIOError:
                return
            if len(r) >= 65 and r[0] == 0x31 and not r[1] & 0x02:  # bit1 = mic audio frame
                if bool(r[55] & 1) != self.headset:
                    self.headset = not self.headset
                    self.audio_route()
                self.battery = min(100, (r[54] & 0x0F) * 10 + 5), r[54] >> 4 == 1
                controls = r[2:8] + r[9:12]  # sticks, analog triggers, buttons
                if self.controls is None or controls[6:] != self.controls[6:] or \
                        max(abs(a - b) for a, b in zip(controls[:6], self.controls[:6])) > 26:  # > ~10 %: no jitter
                    self.controls, self.active_at = controls, time.monotonic()
                self.usb.input(b"\x01" + r[2:65])

    def output(self, data):
        """USB output 0x02 -> BT output 0x31."""
        if not data or data[0] != 0x02:
            print(f"output ignored: id {data[:1].hex()} {len(data)} B", flush=True)
            return
        self.nout += 1
        if time.monotonic() - self.tout >= 5:
            print(f"outputs/s {self.nout / (time.monotonic() - self.tout):.0f}, last {bytes(data[:12]).hex()}", flush=True)
            self.nout, self.tout = 0, time.monotonic()
        r = bytearray(78)
        r[0], r[1], r[2] = 0x31, self.seq << 4, 0x10
        self.seq = (self.seq + 1) & 0x0F
        body = bytearray(data[1:48]).ljust(47, b"\0")  # SetStateData common part
        apply_settings(body, self.cfg)
        r[3:50] = body
        r[74:78] = bt_crc(0xA2, bytes(r[:74]))
        os.write(self.real, bytes(r))

    def hid_request(self, req, rtype, rnum, data, wlen):
        """HID class control request; returns the reply bytes, b"" to ack, None to stall."""
        if req == 0x0A:  # SET_IDLE
            return b""
        if req == 0x01 and rtype == 3:  # GET_REPORT feature
            try:
                out = bytearray(get_feature(self.real, rnum, max(wlen, 64)))
            except OSError as x:
                print(f"get feature {rnum:#04x}: {x}", flush=True)
                return None
            if rnum == 0x09 and len(out) >= 7:
                # hid-playstation refuses a second pad with the same MAC (the real BT one
                # stays bound): set the locally-administered bit of the first octet
                out[6] ^= 0x02
            print(f"get feature {rnum:#04x}: {len(out)} B", flush=True)
            return bytes(out)
        if req == 0x09 and rtype == 2:  # SET_REPORT output
            self.output(data)
            return b""
        if req == 0x09 and rtype == 3:  # SET_REPORT feature
            # Like DS5Dongle only forward the test command (0x80) and profile blocks (0x60-0x62);
            # others are acked unsent: Stellar Blade's SET 0x08 (USB-only) makes the pad drop the BT link
            print(f"set feature {rnum:#04x}: {len(data)} B {data[:8].hex()}", flush=True)
            if (rnum == 0x80 or 0x60 <= rnum <= 0x62) and len(data) > 4:
                d = bytearray(data)
                d[-4:] = bt_crc(0x53, bytes(d[:-4]))  # USB leaves the CRC zero, BT checks it
                try:
                    set_feature(self.real, d)
                except OSError:
                    return None
            return b""
        return None

    def preview(self, kind):
        """Short sample after a slider change in DS5Tee: speaker beep, haptics buzz or rumble."""
        now = time.monotonic()
        if self.timers:  # a sample is still playing: cut it, interleaved packets would garble both
            self.timers.clear()
            self.output(bytes([0x02, 0x03]) + bytes(46))  # and make sure no motor keeps running
        if kind == "rumble":  # left motor alone, then right motor alone
            steps = ((0, 0xC0, 0), (0.2, 0, 0), (0.7, 0, 0xC0), (0.9, 0, 0))  # left motor coasts: 0.5 s gap
            for at, left, right in steps:
                r = bytearray(48)
                r[0], r[1], r[3], r[4] = 0x02, 0x03, right, left
                self.timers.append((now + at, lambda r=bytes(r): self.output(r)))
        elif kind in ("speaker", "haptics"):
            t = np.arange(512 * 2 * (16 if kind == "speaker" else 56)) / 48000  # 0.34 s / 1.2 s, ends in silence
            pcm = np.zeros((len(t), 4), np.int16)
            if kind == "speaker":  # "lightsaber" swoosh the user liked: falling pitch, fast decay, soft attack
                freq = 450 + 550 * np.exp(-t / 0.025)  # 1000 -> 450 Hz (user pick)
                wave = np.sin(2 * np.pi * np.cumsum(freq) / 48000) * np.exp(-t / 0.045) * np.minimum(1, t / 0.003) * 9000
                ch = (0, 1)
            else:  # game-like pattern: 0.5 s continuous, pause, then on / off / on pulses
                gate = np.zeros(len(t))
                for start, end in ((0, 0.5), (0.6, 0.7), (0.8, 0.9), (1.0, 1.15)):
                    gate[(t >= start) & (t < end)] = 1
                gate = np.convolve(gate, np.ones(240) / 240, "same")  # 5 ms ramps, no clicks
                wave = np.sin(2 * np.pi * 110 * t) * gate * 16000
                ch = (2, 3)
            pcm[:, ch[0]] = pcm[:, ch[1]] = wave.astype(np.int16)
            # leading silent packet primes the pad's audio buffer; sending the trailing silence too
            # (force) lets it drain cleanly - both avoid the crackle of an underrun
            blocks = np.concatenate([np.zeros((2, 512, 4), np.int16), pcm.reshape(-1, 512, 4)])
            for i in range(0, len(blocks), 2):  # one 0x39 report per 2 blocks, paced like real playback
                self.timers.append((now + i * 512 / 48000,
                                    lambda f=[blocks[i], blocks[i + 1]]: self.audio(f, force=True)))
        self.timers.sort(key=lambda x: x[0])

    def tick(self):
        """Run due preview steps; returns seconds until the next one (or None)."""
        now = time.monotonic()
        while self.timers and self.timers[0][0] <= now:
            self.timers.pop(0)[1]()
        return max(0.0, self.timers[0][0] - now) if self.timers else None

    def audio_route(self):
        """Speaker volume + output path (what the PS5/Windows audio driver sets on a wired pad)."""
        r = bytearray(48)
        r[0], r[1], r[2] = 0x02, 0x10 | 0x20 | 0x80, 0x80  # allow headphone/speaker volume, audio control (1+2)
        r[5], r[6] = 0x7C, 0x64  # headphone, speaker volume (0x64 = max)
        r[8] = 0x00 if self.headset else 0x30  # output path: headset jack / internal speaker
        r[2] |= 0x40  # motor power level: no global trigger reduction (strength is per side now)
        r[38] = SPEAKER_PREGAIN  # audio_control2 (common offset 37)
        self.output(bytes(r))

    def pcm(self, data):
        """4-ch s16 PCM from the USB audio stream; every 2 x 512 frames -> one BT 0x39 report."""
        self.pcm_buf += data
        while len(self.pcm_buf) >= AFRAME:
            self.frames.append(np.frombuffer(self.pcm_buf[:AFRAME], "<i2").reshape(512, 4))
            self.pcm_buf = self.pcm_buf[AFRAME:]
            if len(self.frames) == 2:
                self.audio(self.frames)
                self.frames = []

    def audio(self, frames, force=False):
        """Two 512-frame PCM blocks -> BT report 0x39 (2 haptics frames + 2 Opus speaker frames)."""
        if not force and not any(f.any() for f in frames):
            return  # silence: nothing to send
        hap, spk, x = b"", b"", np.linspace(0, 511, 480)
        for f in frames:
            h = f[:, 2:4].reshape(32, 16, 2).mean(1) / 256 * HAPTICS_GAIN * self.cfg["haptics"] / 100  # 48 kHz -> 3 kHz, s16 -> s8
            hap += np.clip(h, -128, 127).astype(np.int8).tobytes()
            # 512 -> 480 frames like DS5Dongle: the pad plays one 10 ms Opus frame per 10.67 ms block
            sp = np.stack([np.interp(x, np.arange(512), f[:, c]) for c in (0, 1)], 1)
            sp = np.tanh(sp / 32768 * SPEAKER_GAIN * (self.cfg["speaker"] / 100) ** 3) * 32767  # cubic: audible steps  # boost with soft clipping instead of hard distortion
            spk += self.opus.encode(np.ascontiguousarray(sp.astype("<i2")))
        r = bytearray(547)
        r[0], r[1], r[2], r[3], r[4] = 0x39, self.seq << 4, 0x91, 6, 0b01111110
        self.seq = (self.seq + 1) & 0x0F
        self.packets = (self.packets + 2) & 0xFF
        r[5:9] = bytes([48] * 4)  # audio buffer length (DS5Dongle default)
        r[9] = self.packets
        r[10], r[11], r[12:140] = 0xD2, 64, hap  # haptics block, 2 frames of 64 bytes
        r[140], r[141], r[142:542] = (0x16 if self.headset else 0x13) | 0xC0, 200, spk  # headset jack or speaker
        r[543:547] = bt_crc(0xA2, bytes(r[:543]))
        os.write(self.real, bytes(r))


def write_status(bridges):
    pads = [{"mac": b.mac, "battery": b.battery[0], "charging": b.battery[1], "headset": b.headset}
            for b in bridges.values()]
    os.makedirs(os.path.dirname(STATUS_PATH), exist_ok=True)
    with open(STATUS_PATH + ".tmp", "w") as f:
        json.dump({"pads": pads}, f)
    os.replace(STATUS_PATH + ".tmp", STATUS_PATH)


def main():
    bridges = {}  # hidraw path -> Bridge
    next_scan = next_cfg = 0
    load_config()
    last_preview = config.get("preview", {}).get("n")  # don't replay an old preview at start
    while True:
        now = time.monotonic()
        if now >= next_cfg:
            next_cfg = now + 0.3
            if load_config():
                for b in bridges.values():
                    new = settings_for(b.mac)
                    if new != b.cfg:
                        b.cfg = new
                        print(f"settings {b.mac} {new}", flush=True)
                        b.audio_route()  # pushes triggers, lightbar, player LEDs right away
                p = config.get("preview", {})
                if p.get("n") != last_preview:
                    last_preview = p.get("n")
                    for b in bridges.values():
                        if p.get("mac") in ("", b.mac):  # "" = default section: all pads
                            b.preview(p.get("kind"))
        if now >= next_scan:
            current = bt_dualsenses()
            for path in list(bridges):
                if path not in current:
                    bridges.pop(path).close()
            for path, (hiddir, mac) in current.items():
                if path not in bridges:
                    try:
                        bridges[path] = Bridge(path, hiddir, mac)
                    except (OSError, StopIteration) as x:
                        print(f"{path}: {x!r}", flush=True)
                        continue
                hide(hiddir, path)  # repeated: udev may restore permissions on change events
            next_scan = now + 2
            write_status(bridges)
            for b in bridges.values():
                if b.cfg["idle_minutes"] and now - b.active_at > b.cfg["idle_minutes"] * 60:
                    print(f"{b.mac} idle for {b.cfg['idle_minutes']} min, disconnecting", flush=True)
                    subprocess.run(["bluetoothctl", "disconnect", b.mac], capture_output=True)
                    b.active_at = now
        fds, timeout = {}, 2.0
        for b in bridges.values():
            fds[b.real], fds[b.usb.sock.fileno()] = (b, b.on_real), (b, b.usb.on_data)
            for due in (b.usb.tick(), b.tick()):
                if due is not None:
                    timeout = min(timeout, due)
        ready, _, _ = select.select(list(fds), [], [], timeout)
        for fd in ready:
            b, fn = fds[fd]
            try:
                fn()
            except OSError as x:  # controller or vhci gone: drop it, the rescan re-creates it
                print(f"{b.mac}: {x} in {fn.__name__}", flush=True)
                b.close()
                bridges.pop(b.hidraw, None)
                next_scan = 0
                break


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(0)
