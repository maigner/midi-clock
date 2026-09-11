# EDID override for the Joy-it RB-LCD-10-2 on a Raspberry Pi 4

The panel's EDID advertises its native 1280x800 mode with a horizontal total
of 1403 pixels. The BCM2711 HDMI block cannot do odd horizontal timings, so
the kernel logs `User-defined mode not supported` and uses 1920x1080, which
the panel scales down. `rb-lcd10-2.bin` is the panel's EDID with the
horizontal blanking widened by one pixel (total 1404) and the checksum fixed;
the panel accepts it and runs at native resolution (60.12 Hz).

Install (as root):

```sh
mkdir -p /lib/firmware/edid
cp rb-lcd10-2.bin /lib/firmware/edid/
install -m 0755 initramfs-hook /etc/initramfs-tools/hooks/edid   # ships the file in the initramfs
update-initramfs -u
# append to the single line in /boot/firmware/cmdline.txt:
#   drm.edid_firmware=HDMI-A-1:edid/rb-lcd10-2.bin video=HDMI-A-1:1280x800@60D consoleblank=0
reboot
```

Check with `cat /sys/class/graphics/fb0/virtual_size` (wants `1280,800`).
For another panel with the same problem: dump its EDID from
`/sys/class/drm/card1-HDMI-A-1/edid`, bump byte 57 (horizontal blanking, low
byte of the first detailed timing) by one and recompute byte 127 so the first
128 bytes sum to 0 mod 256.
