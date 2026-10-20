#!/bin/sh
# Installs VibeFuse for the current user; the udev rule needs sudo once.
#   ./install.sh            install
#   ./install.sh --remove   remove again
set -eu
cd "$(dirname "$0")"

bin="$HOME/.local/bin"
apps="$HOME/.local/share/applications"
icons="$HOME/.local/share/icons/hicolor/256x256/apps"
rule="/etc/udev/rules.d/70-vibefuse.rules"

if [ "${1:-}" = "--remove" ]; then
	# The virtual outputs, if they were switched on.
	systemctl --user disable --now vibefuse-mixer.service 2>/dev/null || true
	rm -f "$HOME/.config/systemd/user/vibefuse-mixer.service"
	systemctl --user daemon-reload 2>/dev/null || true
	rm -f "$bin/vibefuse" "$apps/vibefuse.desktop" "$icons/vibefuse.png" \
		"$HOME/.config/autostart/vibefuse.desktop"
	if [ -e "$rule" ]; then
		sudo rm -f "$rule" && sudo udevadm control --reload-rules
	fi
	echo "VibeFuse removed. Settings stay in ~/.config/vibefuse."
	exit 0
fi

mkdir -p "$bin" "$apps" "$icons"
install -m 755 vibefuse "$bin/vibefuse"
install -m 644 vibefuse.desktop "$apps/vibefuse.desktop"
install -m 644 vibefuse.png "$icons/vibefuse.png"
if [ ! -e "$rule" ]; then
	echo "Access to the MiniFuse without root needs a udev rule (sudo):"
	sudo install -m 644 70-vibefuse.rules "$rule"
	sudo udevadm control --reload-rules
	sudo udevadm trigger --subsystem-match=usb
fi
echo "VibeFuse installed: start it from the applications menu or run 'vibefuse'."
