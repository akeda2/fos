# Legacy v1 tests: what was dropped, what is covered, what is worth porting

Written 2026-10-08 when the shared 64-bit tree replaced the 32-bit one on `main`. The v1 edition
(tag `legacy-v1`, branch `legacy`) had 99 `test_*.py` scripts; this tree has 44 (`tools/` and
`tapes/`). The plan (`docs/shared-codebase.md`, section 6) asked for each v1 script to be either
dropped because a current test covers it, or ported. This is that list. Names are the v1 file
names; check them out with `git show legacy-v1:PATH`.

## Covered by a current test (dropped)

| v1 scripts | Covered now by |
| --- | --- |
| `test_ahci_e2e`, `test_ehci_e2e`, `test_xhci_e2e`, `test_usb_hosts_e2e`, `test_usb_storage_e2e`, `test_usb_storage_disk_e2e`, `test_usb_boot`, `test_usb_boot_promote_e2e` | `test_disks`, `test_usb_boot`, `test_usb_hub`, `test_usb_replug` (AHCI, NVMe, xHCI and EHCI sticks, boot from a stick, hubs, replug) |
| `test_debug_mode`, `test_debug_mode_e2e`, `test_kernel_update_e2e`, `test_stack_guard`, `test_page_protection`, `test_reset_e2e` | `test_debug_mode` (crash, loop guard, KUP, watchdog), `test_faults` (deliberate faults), `test_bios_*` (slot logic on BIOS) |
| `test_msi`, `test_msi_e2e`, `test_nic`, `test_nic_map_e2e`, `test_hwaccess_e2e` | `test_msi`, `test_nic`, `test_rtl8139`, `test_diag` (`pci read`, `io`, `mmio`) |
| `test_ed`, `test_ed_e2e`, `test_ed_keys_e2e` | `test_ed`, `test_lineedit`, `test_keyboard` |
| `test_hda_*` (four), `test_audio_capture_e2e`, `test_audio_multi_e2e`, `test_audio_net*` (four), `test_audio_rtp_*` (five) | `test_hda`, `test_usb_audio`, `test_record`, `test_recorder*`, `test_audio_net` (raw Ethernet and RTP/Opus), `test_audio_serve` |
| `test_netfs`, `test_netfs_e2e`, `test_foscopy_e2e`, `test_hostname_discovery_e2e`, `test_scripts_settings_e2e` | `test_netfs`, `test_config` (hostname, settings), `test_scripts` |
| `test_calc_e2e`, `test_hello_rs_e2e`, `test_memtest_e2e`, `test_term_net_e2e`, `test_term_net_tape_e2e`, `test_tape_elf`, `test_gpt` | `test_tapes` (load, run, term net), `test_boot`, `test_stick_guids` |
| `test_vga_modes` | Not needed: the 64-bit console is a framebuffer, not VGA text modes (see `test_boot`, `test_bios_video`, `test_bios_edid`). |

## Not applicable: the thing they test is not in this tree

- The 32-bit teaching tapes and what exercised them: `test_primes_e2e`, `test_primes_vga_e2e`,
  `test_vga_colors_e2e`, `test_beep_e2e`, `test_morse_e2e`, `test_bigsieve_e2e`,
  `test_longmode_e2e`, `test_longmode_rs_e2e`, `test_cpu64_lab_e2e`, `test_cpu64_show_e2e`.
  They need the 32-bit kernel (VGA text memory, a PC speaker, a protected-mode CPU to leave).
- Drivers not carried over (UHCI, OHCI, NE2000, AC'97, ES1370, the parallel port): `test_uhci_e2e`,
  `test_uhci_play_e2e`, `test_uhci_audio_facade_e2e`. The machines they were written for are
  listed under "Driver parity" in `docs/shared-codebase.md`.
- `test_realnet_e2e` needs a host tap interface and root-equivalent set-up. It was never part of
  the suite and proves the host tools, not the kernel.

## Gaps worth porting (nothing covers them now)

1. **`test_net_chat_e2e`** and **`test_telnet_e2e`**: the `net-chat` and `telnet` tapes have no
   dedicated test here (`test_ip_chat_e2e` only touches telnet's library).
2. **`test_nettest_e2e`, `test_nettest_retarget_e2e`, `test_nettest_router_e2e`,
   `test_nettest_selftest_e2e`**: `test_nettest` covers the engine and the basic run; the
   retarget, router and four-NIC self-test scenarios are not repeated.
3. **`test_router_arp_queue_e2e`**: a frame that misses the ARP cache is held and sent when the
   reply arrives. `test_router_e2e` does not check that.
4. **`test_net_dhcp_e2e`**: DHCP client behaviour on its own; the router and ip-chat tests only
   use it on the way.
5. **`test_release_loops_e2e`**: ending blocking commands (`tape load net`, `nic sniff`,
   `nic discover`) from a `term net` session. Re-check that behaviour first, since the loops
   were rewritten for this kernel.
6. **`test_idle_cpu_e2e`**: that an idle machine halts instead of spinning (CPU use of the QEMU
   process). Cheap to port and a good guard for the idle loops.
7. **`reboot soft`** (v1's `test_reset_e2e` covered it): this tree has no soft reboot, only the
   firmware reset; nothing to port until it exists.

Also noticed while triaging: the host unit tests of the tape libraries (`cargo test` in
`tapes/router`, `nettest`, `ed`, `ip-chat`, `telnet`) do not build, with a duplicate `panic_impl`
because the crates depend on `tape-rt`, whose panic handler is unconditional. That was already
true before the `crates/` move. The fix is probably a `cfg(not(test))` around the handler.

## Ported since (2026-10-09)

- `test_idle_cpu_e2e` -> `tools/test_idle_cpu.py` (`make test-idle-cpu`): the idle prompt, `calc`, `ed`, `net-chat` and an open
  `term net` session use 1-2% of a host core; `memtest` is the 100% control.
- `test_net_chat_e2e` -> `tools/test_net_chat.py` (`make test-net-chat`): the test plays the chat peer itself on the multicast segment
  (this kernel has no shell `chat` command to run a second guest in).
- `test_telnet_e2e` -> `tools/test_telnet.py` (`make test-telnet`): a Telnet server on the host, reached through QEMU's user-mode
  network on the guest's first NIC (a tape's network calls use interface 0; the `term net` session rides the second NIC). The legacy
  test implemented ARP/IPv4/TCP by hand. Found on the way: the console prints each byte above 0x7f as `?` (it does not decode UTF-8).
- `test_router_arp_queue_e2e` -> a scenario at the end of the forwarding checks in `tapes/test_router_e2e.py` (`make test-router`).

Still open from the list above: `test_net_dhcp_e2e` (DHCP is exercised by the router and ip-chat tests but not on its own),
`test_release_loops_e2e` and the nettest retarget/router/self-test scenarios.
