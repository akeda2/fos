# Bugs and new features

## Bugs

## New features

### Hardware
[] We need power management, not only in shell. Even "term net":s waiting for connection prompt pins the cpu at 100%.
We need power management even in tapes. How? Do we need a scheduler for this? SMP? Multitasking is not needed right now.
[] New RTL gigabit card: TP-link TG-3468 chip: RTL8168H. This is important as it's the only locally available new gigabit PCIe NIC.

### Tape features
[] Network testing tape. Simulated frame/packet loss, low bandwidth etc. Start with bridge mode. "@nettest.cfg or interactive command prompt: nettest>". Start with frame_loss=NN% randomized.
[] ed tape - edit should edit the existing line, so we don't have to re-write the whole line again.