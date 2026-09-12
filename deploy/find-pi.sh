#!/bin/sh
# Find the midi-clock Pi from a laptop on the same network (e.g. an iPhone hotspot)
# and print its address. Tries mDNS first, then looks at every host on the subnet.
#   deploy/find-pi.sh            prints the address
#   deploy/find-pi.sh ssh        opens an SSH session
set -u
NAME=midi-clock.local
USER=${PI_USER:-its}

ip=""
if ping -c1 -W2 "$NAME" >/dev/null 2>&1; then
  ip=$(ping -c1 "$NAME" 2>/dev/null | sed -n 's/.*(\([0-9.]*\)).*/\1/p' | head -1)
fi

if [ -z "$ip" ]; then
  # No mDNS answer: ping every address of the local /24 (an iPhone hotspot is
  # 172.20.10.0/28) and pick the one whose MAC starts with the Pi's OUI.
  net=$(ipconfig getifaddr en0 2>/dev/null | sed 's/\.[0-9]*$//')
  [ -n "$net" ] || { echo "no network on en0" >&2; exit 1; }
  for i in $(seq 1 254); do ping -c1 -W1 "$net.$i" >/dev/null 2>&1 & done; wait
  # Raspberry Pi 4 Ethernet/Wi-Fi OUIs: dc:a6:32, e4:5f:01, d8:3a:dd, 2c:cf:67, b8:27:eb
  ip=$(arp -an | grep -iE 'dc:a6:32|e4:5f:1|d8:3a:dd|2c:cf:67|b8:27:eb' | sed -n 's/.*(\([0-9.]*\)).*/\1/p' | head -1)
fi

[ -n "$ip" ] || { echo "Pi not found. Is the hotspot on and the Pi booted (about 30 s)?" >&2; exit 1; }
echo "$ip"
[ "${1:-}" = "ssh" ] && exec ssh "$USER@$ip"
exit 0
