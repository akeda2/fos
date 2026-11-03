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
   raw), accepts any user and answers with the guest flag. The guest maps to the configured uid (default 65534).
2. **Users file** (`smbusers`, per-machine, like `hostname`): `name:uid:gid:NTHASH` lines; the hash is MD4 of the UTF-16LE
   password, so no plaintext is stored. NTLMv2 is verified (HMAC-MD5 over the server challenge and the client blob); LM and
   NTLMv1 are refused. A line `guest:uid:gid:` allows guests. A small shell or tape command writes the file (`smbpasswd`).
3. **Mode bits**: every request is checked against the owner/group/other bits of the file for the session's uid and gid
   (supplementary groups: the users file may list them). Read-only milestone: read and execute (search) bits only.
4. Not planned: Kerberos, signing, encryption, SMB1. A client that insists on signing is refused with a clear status, and the
   client half says why.

## Order of work

1. (done) `crates/smb` restructure (wire, ntlm) with host tests: NTLMv2 against the MS-NLMP test vectors, MD4/MD5/HMAC vectors, SPNEGO
   encodings captured from `smbclient`.
2. (done) Server half in the crate (`crates/smb/src/server.rs`), read-only and guest, against a `Share` trait; `crates/smb/tests/server.rs`
   drives it from our own client and from `smbclient` (SMB 2.0.2 and 3.0, SPNEGO, compound requests, UTF-8 names). Not yet
   checked: Linux `mount.cifs`, Windows, VLC; the NTLM response of a login is not verified (every client is a guest).
3. Tape ABI 32 (`vfs_*`, read side) with ext4 behind it; `make test-vfs` on QEMU.
4. `smbd` tape (service on port 445, `root=`/share name settings in `smbd.cfg`); `make test-smbd` mounts it from Linux in QEMU
   (user-mode networking) with `mount.cifs` or `smbclient`.
5. `smb` client tape over the same codecs; tested against our server and Samba.
6. Users file, NTLMv2 verification, uid/gid and mode checks.
7. Writes: the `vfs_*` write calls, the ext4 writer behind them (journal commit per request batch), CREATE for write, WRITE,
   SET_INFO (rename, delete-on-close, end-of-file), FLUSH.

Open points for later: the ext4 writer does not keep hash-tree indexes (`DEFERRED.md`), which matters for big shared folders;
a tape runs one request at a time on the boot CPU (fine for a first server, `httpd` is the same); the tape ABI's file calls
read into the caller's buffer, so big reads are chunked at the 1 MiB credit limit.
