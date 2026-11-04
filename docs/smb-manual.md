# SMB on FOS: user manual

How to share a FOS volume with other machines (`smbd`), use another machine's share from FOS (`smb`, `smbmount`) and
mount a FOS share on Linux. Applies to the embedded kernel and the server kernel (FOS-mf); differences are in "FOS-mf".
Design, protocol scope and measurements: `docs/smb.md`. ext4 volumes: `docs/ext4.md`.

## 1. Pieces

| Tape | Role |
| --- | --- |
| `smbd` | File server, a service on one NIC (port 445). Shares one folder of any volume the kernel reads. |
| `smb` | Client as a shell command: `ls`, `cat`, `get`, `put`, `mkdir`, `rmdir`, `rm`, `mv`. |
| `smbmount` | Client as a service: makes `//NAME/path` work in the shell and in every tape (and so in `smbd`). |

Neither kernel has an IP stack; TCP is in the tapes. So each tape needs its own NIC (a service owns it), the address comes
from DHCP or `ip=`, and ABI 32 or newer is required (35 for `smbmount`, 36 for deferred commit). Redeploy tapes after a
kernel update (`tools/fosadm.py deploy`).

## 2. Sharing a volume with `smbd`

```
run smbd root=ext1 share=data &2
```

`&2` makes it a service on NIC 2; the shell stays free. `ps` lists services, `kill smbd` stops it. Settings are
`key=value` words or, one per line (`#` for comments), a file: `run smbd @SMBD.CFG &2`.

| Setting | Meaning |
| --- | --- |
| `root=` | What is shared. `disk2` (whole ext4 disk), `ext1` (the ext4 partition of a disk that also has FOSFS), `disk2/music`, `@LABEL/site`, a FOSFS or FAT folder, or `//NAME` (a mounted remote share). Default: the boot volume's root. |
| `share=` | Share name clients use: `//HOST/NAME` (default `share`). |
| `port=` | TCP port (445). |
| `ip=A.B.C.D/N`, `gateway=` | Fixed address; otherwise DHCP. |
| `nic=N` | NIC number from 1 (default: `nic use`). |
| `ro` | Refuse every change. |
| `log` | Print one line per connection and per request (command, status, detail, flags). Use it to find client problems. |

Which selector: on a disk that holds FOSFS, `diskN` always means the FOSFS volume. The ext volume is `extN` or `@LABEL`.
On a disk with only ext4 (or FAT), `diskN` works.

### Writes

Put, mkdir, rename and delete work on **ext2/3/4** roots only (ABI 34). FOSFS and FAT shares are read-only.
A new file belongs to the logged-in uid/gid and the folder's mode bits decide who may add or remove entries. The ext writer
keeps its journal open between calls; `smbd` commits after 20 ms without writes, so a power cut can lose the last
size/metadata update, never the consistency of the volume.

## 3. Logins and permissions

Without an `smbusers` file everyone is a guest (uid and gid 65534): use that only on a network you trust, and a guest
cannot write into a root-owned folder.

Make a login:

```
run smbd user alice 1000 1000 secretpw
```

It prints a line like `alice:1000:1000:a08576c2...` (the NT hash, no plaintext). Put the lines in the file `smbusers`
(`write smbusers LINE`, or `ebb smbusers`; at most 8 users). Restart `smbd` to read it.

- `guest:UID:GID:` (empty hash) lets anonymous and unknown logins in as that uid/gid. With the file present and no such
  line, guests are refused.
- Password logins are NTLMv2 and signed (HMAC-SHA-256 for SMB 2.x, AES-CMAC for 3.0). No Kerberos, encryption, SMB1 or 3.1.1.
- Mode bits are checked on every folder on the way (search bit) and the target (read/write), against owner, group or
  other for the session's uid/gid. uid 0 passes everything. No supplementary groups. To let a user write, `chown`/`chmod`
  on the volume from Linux, or log in with uid 0 (`smbd user NAME 0 0 PW`).

## 4. Using another machine's share from FOS

One-off, with the `smb` command (address from DHCP on the `nic use` NIC; add `user=NAME PASSWORD` for a login):

```
smb ls  //192.168.0.50/media
smb get //192.168.0.50/media/song.flac ext1/song.flac
smb put //192.168.0.50/media/up.txt notes.txt
```

As a volume, with `smbmount` (service on its own NIC):

```
run smbmount //nas/media as=nas user=alice secretpw &1
ls //nas/music
cp //nas/music/a.flac ext1/media/
```

Everything that takes a file reference takes `//nas/...`: shell commands, `fosplay`, other tapes and `smbd root=//nas`
(FOS as a gateway: smbmount on one NIC, smbd on another; the upstream server's permissions apply, smbd reports mode
777/666). Limits: one request in flight, 128 KiB per call (callers split larger files), 128 entries per folder listing,
`truncate` only to 0. A killed `smbmount` keeps its name until replaced; callers then fail after a timeout.

## 5. Mounting a FOS share on Linux

Use dialect 3.0. `mount.cifs` as a normal user needs an fstab entry; one entry per mount point:

```
//192.168.0.123/data /mnt/fos-rw cifs user,noauto,credentials=/home/you/.fos-smb,vers=3.0,uid=1000,gid=1000 0 0
//192.168.0.123/data /mnt/fos    cifs user,noauto,guest,vers=3.0,uid=1000,gid=1000 0 0
```

`~/.fos-smb` (mode 600) holds `username=alice` and `password=secretpw`. `guest` means no username, so credentials need
their own entry. Then `mount /mnt/fos-rw`. Other clients: `smbclient -m SMB3 //HOST/data -U alice`, gvfs
(`gio mount smb://HOST/data`), Windows and macOS (Connect to server), VLC/libsmb2 on iOS. If a mount fails, `journalctl -k`
shows the CIFS error and `smbd log` shows the last request the server answered.

## 6. Embedded kernel and FOS-mf

- **Embedded**: `run TAPE ... &N` starts a service on NIC N. Only one foreground tape runs, services run alongside it. A
  foreground tape blocks the shell, `foscopy` and KUP's reboot, a service does not. `ps`, `kill N|NAME|all`.
- **FOS-mf (server kernel)**: services are real threads (`threads` lists them); `tape smbmount ... &1` or `run ...` owns
  its NIC from the first instruction and `kill PID` stops it. Same tapes, same settings; use it for the box with the
  data disks (rack: `smbd root=ext1 share=data &2`, management on NIC 1).
- Both: the two kernels reach ext4 through the same `crates/ext4` and `tape_vfs`, so the selectors and permissions behave
  identically.

## 7. Performance and limits

- Reads of 4-8 MB files on rack (SATA SSD, e1000e/i350): 65-87 MB/s; puts about 28 MB/s with deferred commit. The
  limit is the peer's 64 KiB window (no window scaling) and 64 KiB per read/write message.
- Up to 128 clients at once (8 when the kernel has no memory for more; `smbd` prints which), 16 open files each. A client that hangs up frees its slot at once; a silent one is
  kept 10 minutes (a mount idles between requests; Linux sends an echo every 60 s), and a connection beyond the last slot evicts the
  quietest. A segment for a connection the server forgot gets an RST, so the client reconnects at once instead of
  retransmitting for minutes ("CIFS: VFS: ... has not responded in 180 seconds").
- Not implemented: change notify, locks and oplocks, named streams, extended attributes, symbolic-link following (links
  are listed, not followed), DFS, a `IPC$` share list (name the share explicitly).
- A tape ABI change needs `make tapes` and a redeploy of every tape.

## 8. Troubleshooting

| Symptom | Likely cause |
| --- | --- |
| `mount error(13)` / logon failure | No such user in `smbusers`, wrong password, or no `guest:` line while the file exists. |
| Write refused, "access denied" | Mode bits (guest into a root-owned folder), `ro`, or a FOSFS/FAT root. |
| `mount.cifs`: "get root inode failed" | A server bug of the kind `smbd log` shows; fixed in `query_info` (2026-11-04). |
| `diskN` finds FOSFS, not ext | By design on a disk with FOSFS: use `extN` or `@LABEL`. |
| Share list empty in a GUI | Name the share: `//HOST/data`. |
| Remote read/write times out | Provider (`smbmount`) killed or its NIC unplugged; restart it. |

## 9. Tests

`make test-smbd`, `test-smb-client`, `test-smbmount`, `test-smbd-reexport`, `test-vfs`, `test-vfs-write` (QEMU, against
`smbclient` and the Python server); `make test-mf-smbmount`, `test-mf-shell` for FOS-mf. What only hardware shows: window and
retransmission behaviour, NIC offload quirks, and Windows/macOS clients (not yet checked).
