#!/bin/sh
# Builds and installs the ds5bridge root service and the DS5Tee tray tool (root is needed for vhci_hcd and
# to hide the real Bluetooth nodes). Settings live in the invoking user's ~/.config/ds5bridge.json.
# Usage: sudo ./install.sh        Uninstall: sudo ./install.sh undo
set -e
cd "$(dirname "$0")"
user=${SUDO_USER:-${PKEXEC_UID:+$(id -nu "$PKEXEC_UID")}}  # sudo or pkexec
user=${user:-$(logname 2>/dev/null || echo root)}
if [ "$1" = undo ]; then
    pkill -f '/usr/local/bin/DS5Tee' || true
    systemctl disable --now ds5bridge.service || true
    rm -rf /usr/local/lib/ds5bridge /etc/systemd/system/ds5bridge.service /usr/local/bin/DS5Tee \
        /usr/local/bin/ds5bridge /usr/share/applications/DS5Tee.desktop /etc/xdg/autostart/DS5Tee.desktop \
        /etc/udev/rules.d/72-ds5bridge-hide.rules  # rule of an older version
    systemctl daemon-reload
    udevadm trigger --subsystem-match=hidraw --subsystem-match=input  # restore permissions of hidden nodes
    echo "DS5Tee removed."
    exit
fi
# build as the user, not as root (cargo cache and target dir stay theirs)
runuser -u "$user" -- cargo build --release --locked
install -Dm755 target/release/ds5bridge target/release/DS5Tee -t /usr/local/bin/
install -Dm644 ds5bridge.service -t /etc/systemd/system/
rm -rf /usr/local/lib/ds5bridge /etc/udev/rules.d/72-ds5bridge-hide.rules  # Python version / older rule
install -Dm644 DS5Tee.desktop /usr/share/applications/DS5Tee.desktop
install -Dm644 DS5Tee.desktop /etc/xdg/autostart/DS5Tee.desktop
systemctl daemon-reload
systemctl enable --now ds5bridge.service
systemctl restart ds5bridge.service
sleep 1
if systemctl is-active -q ds5bridge.service; then
    echo "DS5Tee installed, bridge running (log: journalctl -u ds5bridge -f)"
    # An open tray tool still runs the old code: restart it in the user's session
    if pkill -u "$user" -f '/usr/local/bin/DS5Tee'; then
        sleep 1
        systemd-run --user -M "$user@" --collect -q /usr/local/bin/DS5Tee && echo "DS5Tee restarted"
    else
        echo "Start the tray tool: DS5Tee (starts automatically at login from now on)"
    fi
else
    echo "ds5bridge is NOT running:"; journalctl -u ds5bridge -n 10 --no-pager; exit 1
fi
