#!/bin/sh
# One-time OS tuning for a Raspberry Pi running midi-clock. Run as root.
# Safe to re-run. Reboot afterwards.
set -eu
if [ "$(id -u)" -ne 0 ]; then echo "run as root"; exit 1; fi

CMDLINE=/boot/firmware/cmdline.txt
[ -f "$CMDLINE" ] || CMDLINE=/boot/cmdline.txt
CONFIG=/boot/firmware/config.txt
[ -f "$CONFIG" ] || CONFIG=/boot/config.txt

add_cmdline() {
  grep -q "$1" "$CMDLINE" || sed -i "1 s/\$/ $1/" "$CMDLINE"
}

echo "== kernel command line ($CMDLINE)"
add_cmdline "isolcpus=3"              # audio + MIDI threads get CPU 3 (config: realtime.cpu = 3)
add_cmdline "nohz_full=3"
add_cmdline "rcu_nocbs=3"
add_cmdline "usbcore.autosuspend=-1"  # never suspend the USB interface
add_cmdline "threadirqs"              # harmless on RT kernels, helps on stock kernels
add_cmdline "consoleblank=0"          # never blank the touch screen
add_cmdline "vt.global_cursor_default=0"  # no console cursor under the UI

echo "== firmware config ($CONFIG)"
grep -q "^dtparam=audio=off" "$CONFIG" || echo "dtparam=audio=off" >> "$CONFIG"   # no onboard audio card
grep -q "^force_turbo=1" "$CONFIG" || echo "force_turbo=1" >> "$CONFIG"           # fixed CPU clock

echo "== CPU governor"
cat > /etc/systemd/system/cpu-performance.service <<UNIT
[Unit]
Description=Set CPU governor to performance
[Service]
Type=oneshot
ExecStart=/bin/sh -c 'for g in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do echo performance > \$g; done'
[Install]
WantedBy=multi-user.target
UNIT
systemctl enable cpu-performance.service

echo "== real-time limits"
cat > /etc/security/limits.d/audio.conf <<LIM
@audio - rtprio 95
@audio - memlock unlimited
@audio - nice -19
LIM

echo "== services not needed on a clock box"
for s in bluetooth.service triggerhappy.service; do   # avahi stays: midi-clock.local is how the Pi is found on a phone hotspot
  systemctl disable --now "$s" 2>/dev/null || true
done

echo "== PREEMPT_RT kernel"
if uname -v | grep -q PREEMPT_RT; then
  echo "already running a PREEMPT_RT kernel"
else
  echo "Not on an RT kernel. On Raspberry Pi OS Bookworm try:  apt install linux-image-rpi-v8-rt  (or the rpi-2712 variant on Pi 5)"
  echo "and select it in $CONFIG with:  kernel=<image name>   if it is not picked automatically."
fi
echo "done. Reboot now."
