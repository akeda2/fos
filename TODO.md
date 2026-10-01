# Bugs and new features

## Bugs

## New features

### Hardware
[x] Power management: kernel loops and the router, bridge, net-chat, ip-chat, telnet, net-dhcp-test, calc and ed tapes halt when idle (`wait_for_event` tape callback). Still spinning: the audio tapes (USB isochronous has no IRQ) and beep/morse timing loops.
[x] New RTL gigabit card: TP-link TG-3468 chip: RTL8168H. Done: `rtl8168.rs`, working on the TP-Link (1000 Mb/s) and the onboard RTL8168 (100 Mb/s); see CLAUDE.md.
[x] 3C905 receive interrupts never fired on the AMD/nForce test machine: fixed (status-enable mask, command-register serialisation, 16-descriptor ring); see CLAUDE.md.
[] On the test machine, `nic use N` followed by `tape load net` in the *same* term net session on NIC N never receives the tape (works in QEMU, and works when the session is on a different NIC than N); unexplained, affects the 3C905 too.
[] NIC on MSI
[] Finish HDA work

### Shell: Scripts and tape execution
[x] scripts - like the "start" file we already have. ".scriptname" (without .sh) - alias for " scriptname.sh"
[x] ".tapename" (without .bin) - alias for "tape load tapename.bin; tape run"
.sh or .bin tells the shell how to handle - run shell script or load/run tape. Look for bin/sh-file as with "start" disk1->disk2 ...diskN

### Settings file
[x] Settings file "settings" on disk1->disk2...
[x] Settings file optional Deny-list for NIC term net: deny-term-net 1,2...N
(for when we don't want a router to expose term net on outbound NIC when in shell)

### Tape features
[] Network testing tape. Simulated frame/packet loss, low bandwidth etc. Start with bridge mode. "@nettest.cfg or interactive command prompt: nettest>". Start with frame_loss=NN% randomized.
[] ed tape - edit should edit the existing line, so we don't have to re-write the whole line again.
[] Rare `make test` flake (roughly 2-5% per run, present on 8b2c8f3 too): in `tools/qemu_smoke.py` the PS/2 step after `tape load disk router.bin` sends `sendkey a` then `sendkey ret`; the `a` is echoed but the Enter never reaches the shell ("did not observe b'Unknown command.'"), in the plain, `--second-disk` and `--nic e1000` variants. Ruled out: the kernel's scancode queue (0 dropped), pending PIC requests, the post-BIOS keyboard drain (the only discarded bytes were early-boot ones), and a harness monitor race (waiting for QEMU's ack changed nothing). Cause unknown. A passing rerun is not evidence of a regression.
