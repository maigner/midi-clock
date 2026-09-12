#!/bin/sh
# Install midi-clock as a system service on the Pi. Run as root from the
# repository root after `cargo build --release`.
set -eu
BIN_DIR=/usr/local/bin
CFG_DIR=/etc/midi-clock
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/.." && pwd)

if [ "$(id -u)" -ne 0 ]; then echo "run as root"; exit 1; fi
for b in midi-clock clockgen eval; do
  [ -x "$ROOT/target/release/$b" ] || { echo "missing $ROOT/target/release/$b, run: cargo build --release"; exit 1; }
  install -m 0755 "$ROOT/target/release/$b" "$BIN_DIR/$b"
done
id midiclock >/dev/null 2>&1 || useradd --system --home "$CFG_DIR" --shell /usr/sbin/nologin --groups audio,video,input,tty midiclock
usermod -a -G audio,video,input,tty midiclock
mkdir -p "$CFG_DIR"
[ -f "$CFG_DIR/config.toml" ] || install -m 0644 "$HERE/config.example.toml" "$CFG_DIR/config.toml"
chown -R midiclock:audio "$CFG_DIR"
install -m 0755 "$HERE/wait-for-audio.sh" "$BIN_DIR/midi-clock-wait-audio"
install -m 0644 "$HERE/midi-clock.service" /etc/systemd/system/midi-clock.service
cat > /etc/security/limits.d/midi-clock.conf <<LIM
@audio - rtprio 95
@audio - memlock unlimited
@audio - nice -19
LIM
systemctl daemon-reload
systemctl enable midi-clock.service
echo "installed. Edit $CFG_DIR/config.toml (device names!), then: systemctl start midi-clock"
echo "logs: journalctl -u midi-clock -f"
