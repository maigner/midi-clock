#!/bin/sh
# Wait (up to 30 s) for the audio interface named in the config to enumerate.
# Used as ExecStartPre by midi-clock.service; returns immediately when
# [audio] enabled = false or no capture_device is set. Never fails the start.
CFG=${1:-/etc/midi-clock/config.toml}
audio=$(sed -n '/^\[audio\]/,/^\[/p' "$CFG" 2>/dev/null)
echo "$audio" | grep -q -E '^[[:space:]]*enabled[[:space:]]*=[[:space:]]*false' && exit 0
card=$(echo "$audio" | sed -n 's/^[[:space:]]*capture_device[[:space:]]*=[[:space:]]*"hw:CARD=\([^",]*\).*/\1/p' | head -1)
[ -n "$card" ] || exit 0
i=0
while [ $i -lt 30 ]; do
  grep -q -i "$card" /proc/asound/cards 2>/dev/null && exit 0
  i=$((i + 1)); sleep 1
done
echo "audio interface '$card' not found after 30 s, starting anyway"
exit 0
