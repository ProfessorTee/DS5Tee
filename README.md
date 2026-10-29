# DS5Tee

**DualSense über Bluetooth – für Spiele wie per Kabel.** Adaptive Trigger, HD-Haptik und der
Controller-Lautsprecher funktionieren unter Linux sonst nur mit USB-Kabel. DS5Tee macht das
kabellos, ganz ohne zusätzliche Hardware: Es ist eine Software-Version der Pico-Dongles
[DS5Dongle](https://github.com/awalol/DS5Dongle) und [DS5_Bridge](https://github.com/SundayMoments/DS5_Bridge).

Getestet mit Stellar Blade (GE-Proton 11): Trigger, Haptik und Lautsprecher funktionieren über Bluetooth,
auch mit mehreren Controllern gleichzeitig.

## Wie es funktioniert

Spiele schalten die DualSense-Extras nur für einen *kabelgebundenen* Controller ein und finden dessen
Audiogerät (Lautsprecher + Haptik) über das gemeinsame USB-Gerät. Ein virtueller HID-Controller allein
reicht deshalb nicht.

`ds5bridge.py` spielt für jeden per Bluetooth verbundenen DualSense dem Kernel einen **kompletten
USB-DualSense** vor – über USB/IP (`vhci_hcd`), mit den USB-Deskriptoren des DS5Dongle
(HID + USB-Audio mit 4 Kanälen). Linux, PipeWire und Proton sehen damit ein echtes USB-Gerät:

| Richtung | Übersetzung |
|---|---|
| Controller → PC | Bluetooth-Eingabe `0x31` → USB-Eingabe `0x01` |
| PC → Controller | USB-Ausgabe `0x02` → Bluetooth `0x31` mit Sequenz und CRC32 |
| USB-Audio (4 Kanäle, 48 kHz) | Kanal 3/4 → Haptik (3 kHz, int8), Kanal 1/2 → Lautsprecher (Opus), Bluetooth-Report `0x39` |
| Feature-Reports | lesen: durchgereicht · schreiben: nur `0x80` und `0x60–0x62` (andere trennen sonst die Verbindung) |

Der echte Bluetooth-Controller wird vor Spielen und Steam versteckt, damit er nicht doppelt auftaucht.
Andere Bluetooth-Geräte (Tastatur, Maus, andere Gamepads) bleiben unberührt.

## DS5Tee – das Tray-Tool

Einstellungen **pro Controller** (oder als Standard für neue Controller):

- Lautsprecher, Haptik, Vibration – mit kurzem Probeton bzw. Probevibration beim Loslassen des Reglers
- Trigger L / R: Stärke und Modus (*vom Spiel*, *aus*, *Widerstand*, *Waffe*, *Vibration*)
- Gyro auf den rechten Stick (aus / beim Zielen mit L2 / immer) mit Empfindlichkeit – wie bei der Switch:
  grob mit dem Stick, fein durch Kippen; funktioniert ohne Steam Input in jedem Spiel
- Lichtleiste: eigene Farbe, nach Akkustand (grün → gelb → rot) oder vom Spiel; Controller-ID (Spieler-LEDs 1–4)
- Auto-Trennen nach Leerlauf, Controller trennen, Zurücksetzen
- Touchpad als Maus am Desktop an/aus (KDE Plasma)
- Akkustand im Tooltip

## Voraussetzungen

- Linux mit `vhci_hcd` (Teil des Standard-Kernels, USB/IP) und BlueZ
- Python 3 mit `numpy`, dazu `libopus` und für das Tray-Tool `PySide6`
- Für Haptik und Lautsprecher in Spielen: ein Proton mit DualSense-Audio-Unterstützung, z. B. GE-Proton 11
- Im Spiel bzw. in Steam: **Steam Input für das Spiel ausschalten**, damit es den Controller direkt anspricht

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

Das installiert den Systemdienst `ds5bridge` und das Tray-Tool `DS5Tee` (startet bei der Anmeldung).
Danach den DualSense ganz normal per Bluetooth verbinden – auch während ein Spiel schon läuft
(getestet mit Monster Hunter Wilds: Haptik kommt nach dem Verbinden im laufenden Spiel an).

- Log: `journalctl -u ds5bridge -f`
- Einstellungen: `~/.config/ds5bridge.json` (schreibt DS5Tee)
- Deinstallieren: `sudo ./install.sh undo`

## Bekannte Grenzen

- Das Controller-Mikrofon wird noch nicht übertragen (der virtuelle Controller liefert Stille).
- Die Trigger-Stärke pro Seite skaliert die Effekt-Parameter der gängigen Modi; seltene Effekte
  (Bogen, Galopp, Maschine) bleiben unverändert.
- Getestet unter CachyOS mit KDE Plasma; der Touchpad-Schalter nutzt KWin.

## Danksagung

- [awalol/DS5Dongle](https://github.com/awalol/DS5Dongle) (MIT) – USB-Deskriptoren, HID-Report-Deskriptor
  (`usb_report_desc.bin`) und das Format der Bluetooth-Audio-Reports
- [SundayMoments/DS5_Bridge](https://github.com/SundayMoments/DS5_Bridge) – Inspiration und Lautsprecher-Einstellungen
- [GE-Proton](https://github.com/GloriousEggroll/proton-ge-custom) – DualSense-Haptik unter Proton

## Lizenz

MIT, siehe [LICENSE](LICENSE).
