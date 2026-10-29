#!/bin/sh
# Installs the ds5bridge root service and the DS5Tee tray tool (root is needed for vhci_hcd and
# to hide the real Bluetooth nodes). Settings live in the invoking user's ~/.config/ds5bridge.json.
# Usage: sudo ./install.sh        Uninstall: sudo ./install.sh undo
set -e
cd "$(dirname "$0")"
if [ "$1" = undo ]; then
    systemctl disable --now ds5bridge.service || true
    rm -rf /usr/local/lib/ds5bridge /etc/systemd/system/ds5bridge.service /usr/local/bin/DS5Tee \
        /usr/share/applications/DS5Tee.desktop /etc/xdg/autostart/DS5Tee.desktop
    systemctl daemon-reload
    udevadm trigger --subsystem-match=hidraw --subsystem-match=input  # restore permissions of hidden nodes
    echo "DS5Tee entfernt."
    exit
fi
user=${SUDO_USER:-${PKEXEC_UID:+$(id -nu "$PKEXEC_UID")}}  # sudo or pkexec
user=${user:-$(logname 2>/dev/null || echo root)}
home=$(getent passwd "$user" | cut -d: -f6)
install -Dm644 ds5bridge.py usb_report_desc.bin -t /usr/local/lib/ds5bridge/
sed "s|@CONFIG@|$home/.config/ds5bridge.json|" ds5bridge.service > /etc/systemd/system/ds5bridge.service
install -Dm755 DS5Tee /usr/local/bin/DS5Tee
install -Dm644 DS5Tee.desktop /usr/share/applications/DS5Tee.desktop
install -Dm644 DS5Tee.desktop /etc/xdg/autostart/DS5Tee.desktop
systemctl daemon-reload
systemctl enable --now ds5bridge.service
systemctl restart ds5bridge.service
sleep 1
if systemctl is-active -q ds5bridge.service; then
    echo "DS5Tee installiert, Brücke läuft (Log: journalctl -u ds5bridge -f)"
    echo "Tray-Tool starten: DS5Tee (startet künftig automatisch bei der Anmeldung)"
else
    echo "ds5bridge läuft NICHT:"; journalctl -u ds5bridge -n 10 --no-pager; exit 1
fi
