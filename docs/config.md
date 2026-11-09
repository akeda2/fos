# Per-machine configuration files

The kernel reads a few optional text files at boot. Each is looked up on the boot FOSFS volume first, then on any other FOSFS
volume, then in the root of a FAT32/exFAT disk. A missing file means the defaults. One option per line, `#` starts a comment, an
unknown line is reported on the console ("unknown line ... ignored") and skipped. Put a file on a machine with
`foscopy.py FILE :disk1/NAME`, or from the shell with `ed NAME`. The source of truth is `kernel/src/config.rs` (and
`kernel/src/gpio.rs` for the GPIO lines).

| File | What |
| --- | --- |
| `hostname` | First line: the base name this machine announces; the interface number is appended (`lab` becomes `lab1`, `lab2`); a virtual interface (server kernel) gets its vnic name instead, which does not change between boots (`lab` + vnic `web` = `labweb`). Without it the name is `fos-` plus the last three MAC bytes. |
| `settings` | Machine behaviour, below. |
| `debug` | Crash, watchdog and kernel-update behaviour, below. `tools/default-debug.txt` is installed on every built image. |
| `start` | A command script run once at boot (up to 4096 bytes; blank lines and `#` lines skipped). The shell command `start` reruns it. |
| `services` | Server kernel only: named services, one per line, `NAME [restart] TAPE ARGS... &NIC` (the line ends in `&N`, `&NAME` or `&+N`). `service start\|stop\|restart NAME\|all` acts on one or all without touching the others, `service` lists them, `service reload` rereads the file. A `start` file of `service start all` starts them at boot. `restart`: started again after 2, 4, 8 ... at most 60 s when the process ends without `service stop`. |
| `synckey` | First line: the shared secret (at most 128 bytes) that signs `tapesync`, the manifest check of `tools/fosadm.py`, and GPIO over Ethernet (`gpio @HOST`, `tools/gpio.py`, `gpiopanel host=`). The same on every machine that should trust each other. NETFS will not read or replace it. |
| `moxa-1110.fw` | Firmware for the Moxa UPort 1110 USB serial adapter (needs a `usb rescan`). |

## `settings`

| Line | Effect |
| --- | --- |
| `deny-term-net` | Close the network terminal (`term net`): nobody gets a shell over Ethernet. |
| `term-net NIC[,NIC...]` | Server kernel: `term net` is open on these interfaces only (numbered from 1, as in `nic list`). Without it every NIC accepts it. Up to four clients use the server at once, each with its own history, working folder, foreground tape and `top`. |
| `replace-in-place` | A rewrite of a FOSFS file with no room beside the old copy reuses the old copy's space (a power cut then loses the file). Without it the write fails and the old file stays. |
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
| `md auto`, `md wait SECONDS` | Server kernel only: assemble a Linux md RAID 0, 1, 5, 6 or 10 array (mdadm v1.x) in a background thread at boot; wait up to SECONDS (default 10) for all members before running degraded. Off by default. `docs/md.md`. |
| `vnic NAME on N [mac=02:..]` | Server kernel only: a virtual interface `NAME` on physical NIC N, created at boot (`vnic` command, `run TAPE &NAME`, `NETWORKING.md` "Virtual NICs"). |

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

## Where the files live

Every machine-specific file (start, services, settings, hostname, debug, smbusers, tape configs such as phono.cfg, smbd.cfg, httpd.cfg and @NAME files) belongs in `settings/`. A tape looks for `settings/NAME`, then `config/NAME`, then the bare name in the root (`tape_rt::config::locate`); a save goes where the file was found, a new file goes to `settings/` (the root on a volume without folders). `fstab` stays in the root because it builds the mount table. The mf shell command `settings migrate` moves the root *.cfg files, smbusers and services into `settings/`.

Step-by-step key handling for both directions: `docs/ssh-keys.md`.

## sshd (`settings/sshd.cfg`, `settings/authorized_keys`, `settings/ssh_host_key`)

`sshd.cfg` (or the command line) takes `root=DIR`, `export=NAME[=ROOT][,ro]` (up to 8; one export shows as `/`, several as folders), `port=22`, `nic=N`, `ip=A.B.C.D/N`, `gateway=`, `ro` and `log`. Without `ip=` the NIC asks DHCP.
`authorized_keys`: one line per key, `user=NAME[,uid=N][,gid=N][,ro] ssh-ed25519 BASE64 [comment]` (at most 16, ssh-ed25519 only). `uid`/`gid` default to 0, which sees everything: give real users real ids. `ro` refuses every change for that key.
`authorized_keys` options: `user=NAME`, `uid=N`, `gid=N`, `ro`, `shell` (opt-in: the account may open a shell channel, a full login with its own folder; `exit` or `logout` ends it; it shares the four `term net` session slots).

`ssh_host_key`: 64 hex digits, the seed of the host key; sshd makes it at the first start and prints the `SHA256:` fingerprint at every start. Delete the file to change the host key.

## sshmount and ssh (`settings/ssh_key`, `settings/known_hosts`)

`ssh_key`: 64 hex digits, the seed of the client's ed25519 key; `sshmount keygen` or `ssh keygen` makes it (`key=NAME` uses another file, so one machine can hold several identities) and prints the `ssh-ed25519 BASE64` line to put in the server's `authorized_keys`. `known_hosts`: one `HOST[:PORT] SHA256:FINGERPRINT` line per server; a host that is not there is added at the first login, a different key is refused until its line is removed.
