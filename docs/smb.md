# SMB file server and client

Status: built (server, client, mount, writes to ext4). This file is the design and measurement log; the how-to is `docs/smb-manual.md`.

## Goal

Share the ext4 data volume of the server kernel (FOS-mf, `rack`) with Linux (`mount.cifs`, `smbclient`), VLC on an iPhone
(libsmb2) and FOS itself. First milestone: read-only, SMB 2.1 and 3.0 without signing or encryption. Then logins from a
users file, uid/gid mapping with mode-bit checks, and writes.

## Where the pieces live

- **No IP in the kernels.** Neither kernel has IP or TCP; both have raw Ethernet, and TCP lives in `crates/tape-ip` over the
  tape ABI's frame calls (that is how `httpd` and the SMB client of `fosplay` work). So the server is a tape, **`smbd`**, run
  as a service like `httpd` (`tape_ip::tcp::Server`, port 445), and the client is a standalone tape, **`smb`**
  (`smb //HOST/SHARE ls|cat|get PATH [NAME]`). Both x86-64 kernels run them; a kernel-level IP stack is a separate, larger job
  and not part of this.
- **`crates/smb`** (host tests, `no_std`) is split into modules that the client and the server share:
  `wire` (NetBIOS framing, the 64-byte SMB2 header, UTF-16 names, little-endian helpers), `ntlm` (NTLMSSP messages, SPNEGO
  wrapping, MD4, MD5, HMAC-MD5, NTLMv2), `client` (what exists today) and `server` (request handling against a `Share`
  trait, no I/O of its own: a message in, a message out). The server and the client are tested against each other, and both
  against Samba (`smbclient`, `mount.cifs` in the QEMU test).
- **`Share`** is what the server needs from storage: `stat(path)`, `read_dir(path, index)`, `read(path, offset, out)`, later
  `create`, `write_at`, `mkdir`, `remove`, `rename`. The tape implements it over new tape ABI calls.

## Tape ABI: files by path on any volume

Tapes reach only FOSFS and FAT today (`fs_read`, `fs_read_at`, `fs_list`: names and sizes), and ext4 does not fit: folders of
folders, modes, uid/gid, times, names of 255 bytes. So the ABI grows (append-only, version 32) a path-based set that resolves a
reference like `fs_read` does (`diskN/...`, `@LABEL/...`, the mount table) and answers for every volume type:

Done (ABI 32, `crates/kernel-core/src/tape_vfs.rs`, `tape_rt::vfs_stat/vfs_read_dir/vfs_read`, `make test-vfs`):

- `vfs_stat(path, len, *mut VfsStat)`: kind (file, folder, link), mode, size, mtime/ctime/atime, uid, gid, links, inode.
- `vfs_read_dir(path, len, index, *mut VfsEntry)`: 1, 0 past the end or a negative code; the `VfsEntry` is a `VfsStat` plus a
  name of up to 255 bytes UTF-8, so a listing needs no stat per entry. A one-folder cursor makes an in-order walk one pass.
- `vfs_read(path, len, offset, buffer, capacity)`: as `fs_read_at`.
- ext4 is reached as `diskN/path` on a disk with no FOSFS and no FAT volume; the mount is kept (`ext4vol::shared`) and dropped by
  any shell `ext4` command. `@LABEL` and the mount table are not resolved for ext4 yet. FOSFS and FAT answer through `tape_fs`
  (mode 0644/0755, uid/gid 0, no times).
- Symbolic links are reported as links, not followed. Calls are serialised by the file lock (mf) like the other fs calls.
- later: `vfs_create`, `vfs_write`, `vfs_mkdir`, `vfs_remove`, `vfs_rename`, `vfs_truncate`.

## Protocol scope

- Dialects 2.0.2, 2.1, 3.0 (no 3.1.1: its pre-authentication hash and negotiate contexts are only needed for signing and
  encryption). Credits: grant a fixed window; reads and writes up to 1 MiB (`MaxReadSize`) once large MTU is on.
- Commands: NEGOTIATE, SESSION_SETUP, LOGOFF, TREE_CONNECT, TREE_DISCONNECT, CREATE, CLOSE, READ, QUERY_DIRECTORY, QUERY_INFO
  (basic, standard, network-open, attribute-tag, file-system info), ECHO, IOCTL (VALIDATE_NEGOTIATE_INFO, and the answer a
  client wants for `FSCTL_QUERY_NETWORK_INTERFACE_INFO`), FLUSH. SET_INFO, WRITE, LOCK, CHANGE_NOTIFY, oplocks: later or refused.
  Compound requests (the Linux client sends CREATE+QUERY_INFO+CLOSE chains) must be handled from the first milestone.
- A pipe share `IPC$` with just enough of `srvsvc` (`NetShareEnumAll`) for share listings is optional; clients can mount a
  named share without it. Not in the first milestone.

## Authentication and permissions

1. **Guest**: SESSION_SETUP runs the NTLMSSP exchange (SPNEGO-wrapped for Linux, iOS and Windows; the existing client sends it
   raw). Without an `smbusers` file every login is a guest (uid and gid 65534, answered with the guest flag, not signed).
2. **Users file** (`smbusers`, per-machine, like `hostname`): `name:uid:gid:NTHASH` lines; the hash is MD4 of the UTF-16LE
   password, so no plaintext is stored. `smbd user NAME UID GID PASSWORD` prints a line to paste in (`write smbusers LINE`, or
   `ebb`). NTLMv2 is verified (HMAC-MD5 over the server challenge and the client blob, names compared without regard to case);
   LM and NTLMv1 are refused. A line `guest:uid:gid:` allows anonymous logins and unknown names; with the file present and no
   such line they get LOGON_FAILURE. At most 8 users.
3. **Mode bits**: `Stat` carries mode, uid and gid (`VfsStat` has them; FOSFS and FAT answer mode 0 for "none", nothing checked).
   Opening a path needs the search (x) bit on every folder on the way and the read (r) bit on the target, from the owner,
   group or other bits for the session's uid and gid. uid 0 passes everything. Supplementary groups are not supported.
   Directory listing checks the folder's r bit through the same open; entries are not filtered one by one.
4. **Signing**: a client that logged in with a password signs and insists on signed answers (Samba does), so the server signs
   (`crates/smb/src/sign.rs`: HMAC-SHA-256 for SMB 2.0.2/2.1, AES-128-CMAC with the SP 800-108 derived key for 3.0; the session
   key is the NTLMv2 session base key, no key exchange offered). Signed requests are verified. Guests are not signed. Not
   planned: Kerberos, encryption, SMB 3.1.1 (its pre-authentication hash), SMB1.
5. **The challenge** comes from the tape ABI `random` call (33: a SHA-256 pool fed by RDSEED/RDRAND, the TSC and a counter;
   the Pi 4 has no hardware source yet, so there it is clock and counter only). Without the call (older kernel) smbd falls back to MD5
   over the clock and a counter, good enough against replay on a lab network only.

## Order of work

1. (done) `crates/smb` restructure (wire, ntlm) with host tests: NTLMv2 against the MS-NLMP test vectors, MD4/MD5/HMAC vectors, SPNEGO
   encodings captured from `smbclient`.
2. (done) Server half in the crate (`crates/smb/src/server.rs`), read-only and guest, against a `Share` trait; `crates/smb/tests/server.rs`
   drives it from our own client and from `smbclient` (SMB 2.0.2 and 3.0, SPNEGO, compound requests, UTF-8 names). Not yet
   checked: Linux `mount.cifs`, Windows, VLC; (step 6 added password checks).
3. Tape ABI 32 (`vfs_*`, read side) with ext4 behind it; `make test-vfs` on QEMU. **Done.**
4. `smbd` tape (`tapes/smbd`: service on port 445, `root=`/`share=` settings, `run smbd @SMBD.CFG &2`); `make test-smbd` runs it in
   QEMU (user-mode networking) over an ext4 disk and uses `smbclient` (SMB2_02 and SMB3_00, a 3 MB file, a 300-file folder).
   **Done**; `mount.cifs` (needs root), Windows and VLC are unchecked. 128 connections at once, taken from kernel memory (8 if the kernel has no room; a new one evicts the quietest idle one when all are taken), one message at a time; idle ones are kept 10 minutes, and a segment for a forgotten connection is answered with RST.
5. `smb` client tape (`tapes/smb`: `smb ls|cat|get //HOST/SHARE/PATH [NAME]`, `get` streams to a local file); `make test-smb-client` runs it
   in QEMU against a host-side server (`crates/smb/examples/serve.rs`, through guestfwd). **Done**; Samba and Windows servers are
   unchecked (`make test-smb` still covers fosplay against a real Samba when one is reachable).
6. (done) Users file, NTLMv2 verification, signing, uid/gid and mode checks (`make test-smbd` second boot, `logins_are_checked` in
   `crates/smb/tests/server.rs`: right and wrong password, unknown user, anonymous, a 0600 file of another owner, SMB 2.0.2 and 3.0).
   The `smb` client tape logs in with `user=NAME password=SECRET` (NTLMv2, HMAC-SHA-256 signing, client nonce from `random`); `make test-smb-client`
   second section: password login, signed 3 MB `get`, wrong password and guest refused (the host `serve` example with `SMB_LOGIN`).
7. (done) Writes. Tape ABI 34 `vfs_create/write/truncate/remove/rename/set_owner` over the ext4 writer (every call opens the
   volume for writing, changes it, commits the journal and remounts the read side, `ext4vol::with_writer`; ext volumes only, other
   volumes answer `FS_READ_ONLY`; `make test-vfs-write` checks the result with e2fsck and debugfs). The server handles CREATE with
   all dispositions and the directory option, WRITE, SET_INFO (rename, delete-on-close, end-of-file; times and attributes are
   accepted and ignored) and FLUSH (a no-op: every call is already committed). Permissions: adding, removing or renaming needs
   write and search on the parent folder, writing an existing file the write bit, and a new file belongs to the session's uid/gid
   (mode 0644, folders 0755). smbd takes `ro` to refuse all changes. A message is up to `MAX_IO` (64 KiB), so the tape's receive
   buffer is that plus headers. The client tape has `put`, `mkdir`, `rmdir`, `rm` and `mv`. Tests: `smbclient_changes_files` and
   `our_client_changes_files` (`crates/smb/tests/server.rs`), `make test-smbd` (smbclient writes as a logged-in user into a
   world-writable folder, then e2fsck/debugfs of the guest's disk copy), `make test-smb-client` (put of 3 MB, mkdir, mv, rm, rmdir
   against the host `serve` example). Not checked: Windows, macOS, `mount.cifs` writes, and a power cut during a write on real
   disks; delete-on-close of a file that others hold open is not tracked (opens are not shared-mode checked).

Performance: `smbd ... log` prints, after a transfer, how long file reads and sending took (`smbd: read 32768 KB in 385 ms, sent
32811 KB in 2572 ms`). In QEMU (user networking, e1000) a 32 MB `smbclient get` runs at about 10 MB/s and the time is all in
sending (reads from ext4 run at about 85 MB/s); a bigger TCP flight made no difference, so QEMU's per-frame cost is the limit
and only real hardware can say more.

Open points for later: the ext4 writer does not keep hash-tree indexes (`DEFERRED.md`), which matters for big shared folders;
a tape runs one request at a time on the boot CPU (fine for a first server, `httpd` is the same); the tape ABI's file calls
read into the caller's buffer, so big reads are chunked at the 1 MiB credit limit.

## Mounting a share (smbmount)

`run smbmount //HOST/SHARE [as=NAME] [user=NAME PASSWORD] &N` is a service
tape that owns NIC N, connects with the `smb` client and registers `NAME`
(default: the share) as a *remote volume* (ABI 35, `remote_register`). The
kernel then routes any `//NAME/path` to it: shell commands (`ls`, `cat`,
`cp`, `mkdir`, `mv`, `rm`), `fs_*` and `vfs_*` calls, so tapes and `smbd`
(re-export) need no changes.

The layer (`crates/kernel-core/src/remote.rs`) is protocol-agnostic: one
request slot, posted by the caller and taken by the provider with
`remote_poll`, answered with `remote_reply`. The native layer-2 NETFS can
become a provider the same way. Callers yield to services while they wait
(10 s until taken, then 60 s).

Re-export: `run smbd root=//nas share=data &2` shares the mount again (FOS as a
gateway; smbmount on one NIC, smbd on another). smbmount reports every entry
as mode 777/666 so smbd's mode-bit checks let everything through; the real
permissions are the upstream server's, enforced by the login smbmount made.
`make test-smbd-reexport` (smbclient through both hops).

Limits: one request in flight, 128 KiB per call (larger files are split by
the callers), 128 entries per folder listing, `truncate` only to 0,
`set_owner` is a no-op, `fs_stream_*` is not remote. A killed provider keeps
its name until it is replaced; callers get errors after the timeout. On
FOS-mf the same works as a background process: `tape smbmount //HOST/SHARE
as=nas &1` (or `run ...`) owns NIC 1 from its first instruction, leaves the
shell free, and `kill PID` (the number it prints; `threads` lists it) stops
it. Verified in QEMU against the host `serve` example (`make test-smbmount`,
`make test-mf-smbmount`); real servers are not yet checked.

### Send path on real hardware (rack, SATA SSD, e1000e and i350)

A 4 MB `smbclient get`: 7.6 MB/s at first; 23.7 after AHCI 128 KiB commands and a directory-lookup fix; about 55 MB/s
once `Server::send_buffered` kept the flight going across messages (before, every 64 KiB message waited for all its
acknowledgements, a round trip each); 70-87 MB/s with `FLIGHT` at 44 segments (the peer's 64 KiB window was the limit; `tape-net` now scales
the windows it offers (RFC 7323) when a connection's buffer is over 64 KiB, smbd's buffer is 192 KiB, three requests, so a client keeps
sending while a write is on the disk; a window much wider than the NIC's 128-frame receive ring would drop frames during a
journal-commit stall. A put on rack averaged 23 MB/s with 40% of the time in file writes, which is what this addresses). Time went to waiting for acknowledgements, not to building, checksumming or sending frames: a wider
checksum and spinning instead of `idle()` changed nothing, and the I350's TCP checksum offload (`nic_tx_offload`) was slower
(235 against 176 ms for 8 MB; the context descriptor costs a ring slot per frame). The i350 and the e1000e perform the
same once pipelined. Reads of the volume are no longer the limit (180 MB/s).

### Writes to ext4, loss recovery, slots (rack, 8 MB files on the `rackdata` ext4 partition)

- **`extN` selector**: `smbd root=ext1` shares the first ext partition of any disk, also one that has a FOSFS boot
  partition (`diskN` finds ext only on a disk with no FOSFS or FAT volume). Writes need a login: with no `smbusers` file
  everyone is a guest, who cannot write into a root-owned ext root (`smbd user NAME 0 0 PW`, then `write smbusers LINE`).
- **Deferred commit (ABI 36 `vfs_sync`)**: `vfs_write` keeps the ext writer open between calls and the journal commits when
  its stage fills, on a read, or on `vfs_sync`; smbd calls `vfs_sync` after 20 ms without writes. Put: 5.7 -> about 28 MB/s.
  A crash before the commit loses the last metadata update (file size), never the consistency of the volume.
- **Cache flushes (AHCI)**: every `Drive::write` ended with FLUSH CACHE, so each 64 KiB of file data cost one flush (about
  1.2 ms per request on rack's M.2 SATA SSD, 12 of 14.5 s of a 680 MB put). File data now goes through `write_data` ->
  `block::write_deferred` without the flush; `Device::sync` (`block::flush`) runs once at the start of every journal commit,
  so data is on the drive before the metadata that refers to it. Metadata writes still flush each time.
- **TCP**: the receive window follows the free room and an overflowing segment is answered, not dropped; a retransmission
  is one segment (it used to be the whole flight, which the buffer refused silently); a duplicate ACK no longer pushes the
  retransmit timer out and the third one resends the oldest segment (fast retransmit).
- **Slots**: a client that hung up left its slot in CloseWait for the 10 s idle timeout, so the fifth quick connection in
  a row timed out in `smbclient` (5 s). smbd now answers the client's FIN (`Server::finish`, non-blocking).
- Result: 20 gets in a row about 120 ms each (65 MB/s), puts about 290 ms. The stalls and the slot leak only show on real
  hardware; the QEMU suites do not reproduce them.

### Real servers and a Linux client (2026-11-04)

- `smbmount //fraktal/media` (Samba on a NAS, guest) lists, reads and copies: `cp //media/sound-experiments/.../Caravan.flac
  ext1/media/` put 9 MB on rack's ext4 volume; `smbd root=ext1` shared it and the Linux desktop (gvfs, `gio mount -a
  smb://HOST/share`) read it back with the identical SHA-256 (0.24 s).
- Linux `mount.cifs` (`vers=3.0`, guest, fstab entry with `user,noauto`) failed with "get root inode failed": the client
  sends its QUERY_INFO as the last message of a compound without the one-byte buffer (104 bytes), and the server demanded
  105. Fixed (`query_info` needs 40); the mount lists, reads (SHA-256 identical) and unmounts. `smbd log` now prints every
  request (command, status, detail, flags) to find such things. Writes through `mount.cifs` are not tried yet.
- `4-bar-house.wav` on the NAS is access denied for the guest (`smbclient` gets NT_STATUS_ACCESS_DENIED too): a Samba
  ACL, not a fos fault. The shell now says "access denied" instead of "could not be read".

## Write speed on mf (measured on rack, 676 MB put over gigabit)

| Change | Active time | Rate |
| --- | --- | --- |
| 64 KiB TCP window | 29.3 s | ~190 Mbit/s |
| 192 KiB window with scaling | 14.5 s | 375 Mbit/s |
| file data written without a cache flush per request (one flush before each journal commit) | 38 s (the loop slept a tick in every gap) | 142 Mbit/s |
| `smbd` polls for 2 ms after the last frame before it sleeps | 6.5 s | ~830 Mbit/s |

On mf `idle` sleeps a whole timer tick, so a fast writer must not call it between frames. Disk writes were 1.6 s of the 6.5 s.

| 1 MiB requests (`maxio=`, SMB 2.1+ multi-credit; remount the client, its `wsize` is fixed at mount time) and a 1024-frame e1000 receive ring | 6.0 s | ~908 Mbit/s |

With 1 MiB requests and the old 128-frame ring the transfer fell to ~8 MB/s: the 2 MiB window outran the ring while a request was on the disk, and TCP retransmitted. The ring (about 12 ms at line rate) fixed it. One gigabit TCP stream carries about 941 Mbit/s.

## Worker threads (server kernel, ABI 37)

On mf `smbd` starts four worker threads (`WORKERS` in `tapes/smbd/src/main.rs`; the line "smbd: 4 worker threads." shows it). The
network thread keeps the NIC and every TCP slot and takes each complete request into an idle worker's buffer; the worker runs
`Server::handle` (all file I/O) and hands the answer back, and the network thread sends it. A slot has one request in flight, so
a session's requests stay in order and only one thread touches its `Server`. A slow disk no longer stops frames coming in, and
several clients no longer wait behind each other's I/O (the disk itself is still one lock). Embedded kernels have no threads
(`tape_rt::threads_supported()`) and keep the single loop. Tested by `make test-mf-smbd` (six clients at once); throughput with
several clients on rack is measured below.

**Read-ahead.** A client reading a file in 1 MiB requests waits for each answer before asking for the next, so the disk read sat
between two stretches of wire time. After a session's second sequential full read, a worker reads the next range into a shared
1 MiB cache (`Ahead` in `tapes/smbd/src/main.rs`) while the network thread is still sending the answer; the next request is a
copy. Only a request the cache covers entirely is served from it, and every create, write, truncate, remove or rename throws it away
(also a read in progress). On rack one client reading 675 MB went from 10.2 s to 7.9 s active (wire floor about 5.4 s). Two clients
at once finish together with the writes in about 17 s. Not covered by a test of its own beyond the file checks of `make test-smbd`.
