# Bugs and new features

## Bugs

## New features

### Hardware
[] We need power management, not only in shell. Even "term net":s waiting for connection prompt pins the cpu at 100%.
We need power management even in tapes. How? Do we need a scheduler for this? SMP? Multitasking is not needed right now.
[] New RTL gigabit card

### Shell: Scripts and tape execution
[] scripts - like the "start" file we already have. ".scriptname" (without .sh) - alias for " scriptname.sh"
[] ".tapename" (without .bin) - alias for "tape load tapename.bin; tape run"
.sh or .bin tells the shell how to handle - run shell script or load/run tape. Look for bin/sh-file as with "start" disk1->disk2 ...diskN

### Settings file
[] Settings file "settings" on disk1->disk2...
[] Settings file optional Deny-list for NIC term net: deny-term-net 1,2...N
(for when we don't want a router to expose term net on outbound NIC when in shell)

### Tape features
[] Network testing tape. Simulated frame/packet loss, low bandwidth etc. Start with bridge mode. "@nettest.cfg or interactive command prompt: nettest>". Start with frame_loss=NN% randomized.
[] ed tape - edit should edit the existing line, so we don't have to re-write the whole line again.