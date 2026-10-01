# Bugs and new features

## Bugs

## New features

### Hardware
[] Power management: kernel loops, router and bridge now halt when idle (`wait_for_event` tape callback). Still spinning: telnet, ip-chat, net-chat, tape-net's internal waits (ARP/TCP), calc/ed and the audio tapes.
[] New RTL gigabit card: TP-link TG-3468 chip: RTL8168H. This is important as it's the only locally available new gigabit PCIe NIC.
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