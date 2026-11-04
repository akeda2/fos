# SMB file server and client

Status: design (2026-11-03). Nothing here is built yet except the client in `crates/smb` (guest only, used by `fosplay`).

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
   **Done**; `mount.cifs` (needs root), Windows and VLC are unchecked. Four connections at once, one message at a time.
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

Open points for later: the ext4 writer does not keep hash-tree indexes (`DEFERRED.md`), which matters for big shared folders;
a tape runs one request at a time on the boot CPU (fine for a first server, `httpd` is the same); the tape ABI's file calls
read into the caller's buffer, so big reads are chunked at the 1 MiB credit limit.
