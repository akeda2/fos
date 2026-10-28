# Per-machine configuration files

The kernel reads a few optional text files at boot. Each is looked up on the boot FOSFS volume first, then on any other FOSFS
volume, then in the root of a FAT32/exFAT disk. A missing file means the defaults. One option per line, `#` starts a comment, an
unknown line is reported on the console ("unknown line ... ignored") and skipped. Put a file on a machine with
`foscopy.py FILE :disk1/NAME`, or from the shell with `ed NAME`. The source of truth is `kernel/src/config.rs` (and
`kernel/src/gpio.rs` for the GPIO lines).

| File | What |
| --- | --- |
| `hostname` | First line: the base name this machine announces; the interface number is appended (`lab` becomes `lab1`, `lab2`). Without it the name is `fos-` plus the last three MAC bytes. |
| `settings` | Machine behaviour, below. |
| `debug` | Crash, watchdog and kernel-update behaviour, below. `tools/default-debug.txt` is installed on every built image. |
| `start` | A command script run once at boot (up to 4096 bytes; blank lines and `#` lines skipped). The shell command `start` reruns it. |
| `synckey` | First line: the shared secret (at most 128 bytes) that signs `tapesync`, the manifest check of `tools/fosadm.py`, and GPIO over Ethernet (`gpio @HOST`, `tools/gpio.py`, `gpiopanel host=`). The same on every machine that should trust each other. NETFS will not read or replace it. |
| `moxa-1110.fw` | Firmware for the Moxa UPort 1110 USB serial adapter (needs a `usb rescan`). |

## `settings`

| Line | Effect |
| --- | --- |
| `deny-term-net` | Close the network terminal (`term net`): nobody gets a shell over Ethernet. |
| `no-msi` | NICs stay on polling instead of MSI interrupts. |
| `prefer-msix` | Try MSI-X before MSI on devices that offer both (a test aid; `docs/design.md`, interrupt delivery). |
| `park-idle` | Park every NIC whose link has been down for a minute. |
| `hwp performance\|balanced\|power` | Let an Intel CPU with hardware P-states pick its own speed, with that preference. Use `performance` on machines doing line-rate network or audio work; `balanced` has cost an i350 receive overruns. |
| `keymap us\|sv` | Local keyboard layout: US (default) or Swedish/Finnish. |
| `mouse-speed PERCENT` | Pointer gain of a PS/2 or USB mouse or touchpad as a percentage of the default (25-1000, default 100). Touchpads differ a lot: piano's and rack's mouse are right at 100, laptop1's pad needs less. `docs/design.md`, "Mouse and touchpad". |
| `foreground NAME\|#rrggbb`, `background NAME\|#rrggbb` | Pixel console colours (foreground gray, green, amber, black; background black, blue, gray). Not in BIOS text mode. The Pi kernel reads them too. |
| `rotate [all\|0\|1] r\|l\|flip\|off` | Turn what tapes draw, for a monitor mounted in portrait. The text console stays upright. `run A \| B rotate=r` does it for one run. |
| `serial-shell [PORT] [BAUD]` | The shell also on a serial port (COM2-COM4, numbered as `fterm list`; default port 1, 115200 baud, 8n1). No password, so off unless asked; see `docs/design.md`. |
| `gpio-remote` | Other machines may change this machine's GPIO pins (with the `synckey`); reads need only the key. |
| `gpio-deny N..` | Forbid GPIO pins (numbers 0-57), locally and remotely. |
| `gpio-sim` | A RAM-backed GPIO bank, for tests (`make test-gpio`). |

## `debug`

| Line | Effect |
| --- | --- |
| `reboot-on-crash` | Reset after a fatal CPU exception instead of halting. The crash record is printed by the next boot. Five crashes in a row before the boot reaches its milestone stop the loop. |
| `reboot-delay N` | Seconds the crash details stay on screen before the reset (default 3, at most 60). |
| `watchdog [N]` | Reset if the foreground makes no progress for N seconds (default 60). Armed only once boot reaches a working shell, and off after three unclean boots in a row. |
| `kernel-update` | Enable KUP, so `tools/send_kernel_update.py` / `devloop.py push` can push a kernel over Ethernet. |
| `crash-on-boot` | Test only (`tools/test_debug_mode_e2e.py`): crash every boot. Never in a real `debug` file. |

The shell command `debug` (not a file option) inspects and changes things at run time: `debug slots` (A/B slot metadata and why it
cannot be read), `debug video skip|auto|size WxH` (BIOS boots: skip the video-mode switch, or give a monitor size when the card has
no EDID).

## Tape settings files

A tape that keeps settings reads a text file of the same words it takes on its command line; `run TAPE @NAME` loads another file.
Examples: `fterm.cfg` (the panel's W, repeated by `run fterm @fterm.cfg`), `gonio.cfg`, `phono.cfg` (also read by `phono service`),
`fosplay.cfg`, `audio.cfg`, `bench.cfg`, and `ROUTER.CFG` for the router (at most 4096 bytes, `NETWORKING.md`). Each tape's `help NAME`
lists its words.
