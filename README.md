# DS5Tee

**English** · [Deutsch](README.de.md)

**DualSense over Bluetooth – to games it looks like a wired controller.** On Linux, adaptive triggers,
HD haptics and the controller speaker normally only work over a USB cable. DS5Tee makes them work
wirelessly without any extra hardware: it is a software version of the Pico dongles
[DS5Dongle](https://github.com/awalol/DS5Dongle) and [DS5_Bridge](https://github.com/SundayMoments/DS5_Bridge).

Tested with Stellar Blade (GE-Proton 11): triggers, haptics and speaker work over Bluetooth, also with
several controllers at once.

## How it works

Games only enable the DualSense extras for a *wired* controller and find its audio device
(speaker + haptics) through the shared USB device. A virtual HID controller alone is therefore not enough.

For every DualSense connected over Bluetooth, `ds5bridge.py` presents a **complete USB DualSense** to the
kernel – via USB/IP (`vhci_hcd`), using the USB descriptors of DS5Dongle (HID + 4-channel USB audio).
Linux, PipeWire and Proton see a real USB device:

| Direction | Translation |
|---|---|
| Controller → PC | Bluetooth input `0x31` → USB input `0x01` |
| PC → Controller | USB output `0x02` → Bluetooth `0x31` with sequence number and CRC32 |
| USB audio (4 channels, 48 kHz) | channels 3/4 → haptics (3 kHz, int8), channels 1/2 → speaker (Opus), Bluetooth report `0x39` |
| Feature reports | read: passed through · write: only `0x80` and `0x60–0x62` (others drop the connection) |

The real Bluetooth controller is hidden from games and Steam so it doesn't show up twice: a udev rule
(`72-ds5bridge-hide.rules`) makes its nodes root-only the moment they appear, before anything can open them.
Other Bluetooth devices (keyboard, mouse, other gamepads) are left alone.

## DS5Tee – the tray tool

Settings **per controller** (or as the default for new controllers):

- Speaker, haptics, rumble – with a short test sound / test rumble when you release the slider
- Triggers L / R: strength and mode (*from game*, *off*, *resistance*, *weapon*, *vibration*)
- Gyro to right stick (off / while aiming with L2 / always) with sensitivity, horizontal/vertical invert –
  like on the Switch: coarse with the stick, fine by tilting; works in every game without Steam Input
- Light bar: custom color, by battery level (green → yellow → red) or from the game; controller ID (player LEDs 1–4)
- Auto-disconnect when idle, disconnect controller, reset
- Touchpad as desktop mouse on/off (KDE Plasma)
- Battery level in the tooltip

The interface follows the system language: German on German systems, English otherwise.

## Requirements

- Linux with `vhci_hcd` (part of the standard kernel, USB/IP) and BlueZ
- Python 3 with `numpy`, plus `libopus`, and `PySide6` for the tray tool
- For haptics and speaker in games: a Proton with DualSense audio support, e.g. GE-Proton 11
- In the game / in Steam: **turn off Steam Input for the game** so it talks to the controller directly

Arch / CachyOS:

```sh
sudo pacman -S python-numpy opus pyside6 bluez-utils
```

## Installation

```sh
git clone https://github.com/ProfessorTee/DS5Tee.git
cd DS5Tee
sudo ./install.sh
```

This installs the system service `ds5bridge` and the tray tool `DS5Tee` (starts at login).
Then connect the DualSense over Bluetooth as usual – even while a game is already running
(tested with Monster Hunter Wilds: haptics arrive in the running game after connecting).

- Log: `journalctl -u ds5bridge -f`
- Settings: `~/.config/ds5bridge.json` (written by DS5Tee)
- Uninstall: `sudo ./install.sh undo`

## Known limitations

- The controller microphone is not forwarded yet (the virtual controller delivers silence).
- The per-side trigger strength scales the effect parameters of the common modes; rare effects
  (bow, galloping, machine) are left unchanged.
- Tested on CachyOS with KDE Plasma; the touchpad switch uses KWin.

## Credits

- [awalol/DS5Dongle](https://github.com/awalol/DS5Dongle) (MIT) – USB descriptors, HID report descriptor
  (`usb_report_desc.bin`) and the format of the Bluetooth audio reports
- [SundayMoments/DS5_Bridge](https://github.com/SundayMoments/DS5_Bridge) – inspiration and speaker settings
- [GE-Proton](https://github.com/GloriousEggroll/proton-ge-custom) – DualSense haptics under Proton

## License

MIT, see [LICENSE](LICENSE).
