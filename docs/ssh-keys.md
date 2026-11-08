# SSH keys on fos: making them and using them with Linux

fos logs in with **ed25519 keys only**, in both directions. This manual covers both:

* **Part 1-3: a fos machine logs in to a Linux host** (`ssh`, `sshmount`): make the key on fos, put its public line in the
  Linux user's `~/.ssh/authorized_keys`.
* **Part 4: a Linux machine logs in to fos** (`sftp`, `sshfs`, `ssh` to fos's `sshd`): make the key on Linux, put its
  public line in fos's `settings/authorized_keys`.
* Part 5: checking that two files really hold the same key, Part 6: when it does not work.

Words used here: the **private key** (a secret; whoever has it is you), the **public key** (one line, safe to hand around),
the **fingerprint** (a short `SHA256:...` summary of a public key, used to compare keys by eye) and the **host key** (the
server's own key, which proves to the client that it reached the right machine).

## 1. Make a key on fos

On the fos shell:

```
tape ssh keygen
```

The first time it prints `ssh: made settings/ssh_key.`, then the line you will need:

```
ssh: add this line to ~/.ssh/authorized_keys on the server:
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA...
```

* The private key is **`settings/ssh_key`**: 64 hex digits, the 32-byte seed of the key. Treat the file like a password.
  Never paste its contents anywhere and never copy it to a machine you do not trust.
* Running `ssh keygen` again does **not** make a new key. It prints the same line (the file is kept), so you can come back
  for the line whenever you need it. To replace the key, delete `settings/ssh_key` first (`rm settings/ssh_key`) and
  run `ssh keygen` again; every server must then be given the new line.
* `sshmount keygen` is the same command and the same file: `ssh` and `sshmount` share one identity.
* **More than one identity:** `tape ssh keygen key=work` makes and prints `settings/work` instead; use it with
  `tape ssh key=work user@host`. (Any file name works; `ssh_key` is the default.)
* The line printed has no comment. The Linux side does not need one; if you want to tell keys apart in
  `authorized_keys`, add a word after the key when you paste it, for example `... AAAAC3...xyz fos-box`.

Get the line to the Linux host by selecting it in a terminal that shows the fos console (a `term net` or serial session)
and pasting it as in part 2. Do not fetch `settings/ssh_key` to read the public line: that file is the secret, and
`ssh keygen` prints the line whenever you want it.

## 2. Import the key on the Linux host

On the Linux host, logged in as the account that fos should become (here `alice`):

```
mkdir -p ~/.ssh
chmod 700 ~/.ssh
echo 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA...paste-the-whole-line... fos-box' >> ~/.ssh/authorized_keys
chmod 600 ~/.ssh/authorized_keys
```

* Use single quotes and put the **whole line on one line**; a key broken over two lines is no key.
* `>>` adds to the file; a single `>` would **replace** it and lock out every other key. If the host has no
  `~/.ssh/authorized_keys` yet this is the same thing, but get in the habit of `>>`.
* sshd on Linux ignores the file if the permissions are loose: `~/.ssh` 700 (or tighter), `authorized_keys` 600 (or 644)
  and your home directory not group- or world-writable (`StrictModes yes`, the default).
* To limit what the key may do, put options in front of the key, for example
  `restrict,command="/usr/bin/uptime" ssh-ed25519 AAAA... fos-box` (see `man sshd`, section AUTHORIZED_KEYS FILE FORMAT).
  `from="192.168.0.0/24"` ties the key to an address range.
* Check the file has the line: `grep -c fos-box ~/.ssh/authorized_keys` (1) and
  `ssh-keygen -lf ~/.ssh/authorized_keys` (one fingerprint per key line).
* The Linux server must allow public keys: `PubkeyAuthentication yes` (the default) and, if `PubkeyAcceptedAlgorithms` is
  set in `/etc/ssh/sshd_config`, it has to include `ssh-ed25519`. Check with `sudo sshd -T | grep -i pubkey`.

Use `ssh-copy-id` instead if you can already log in to the Linux host with a password from another machine and the key
is in a file there: `ssh-copy-id -f -i fos_pub.txt user@host` (`-f` forces it without trying the key first). It does the
`mkdir`, the `>>` and the `chmod` for you.

## 3. Log in from fos

```
tape ssh alice@192.168.0.10 uptime         # run one command
tape ssh alice@192.168.0.10                # a login shell on the fos console; Esc leaves
tape ssh alice@192.168.0.10:2222           # another port
run sshmount alice@192.168.0.10 as=nas path=/srv/music &1   # a folder as the volume //nas
```

The first time fos meets a server it remembers the server's host key in `settings/known_hosts` (trust on first use) and
prints nothing about it. Later logins compare against that line:

```
ssh: THE KEY OF 192.168.0.10 CHANGED. Refusing to log in.
ssh: it was meant to be SHA256:...; if that is right, remove its line from settings/known_hosts.
```

A changed key means either the server was reinstalled (then edit `settings/known_hosts` with `ebb` and delete the
line of that host) or somebody sits between you and it (then do not). To compare with what Linux believes, on the server run
`ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`: the `SHA256:...` must be the one in fos's `known_hosts`. Servers
that have no ed25519 host key cannot be reached; `ssh-keyscan -t ed25519 host` tells you if there is one.

**No key on the Linux side yet?** `tape ssh pw alice@192.168.0.10` (or `password`) asks for the account's password (typed blind,
Esc cancels) and opens a shell on the server. That works only if the server has `PasswordAuthentication yes`. Use it
once to do part 2 from fos itself: in that shell run the commands of part 2 with the line `tape ssh keygen` printed.
(Do not give them as a command on the fos shell line: the fos shell would take the `>>` itself.)

## 4. The other way: Linux logs in to fos

fos's `sshd` (SFTP, and a shell for keys marked `shell`) accepts only keys listed in `settings/authorized_keys`.

**On the Linux workstation**, make a key if you have none for fos (a passphrase is a good idea on a laptop):

```
ssh-keygen -t ed25519 -f ~/.ssh/fos_ed25519 -C "$USER@$(hostname)"
cat ~/.ssh/fos_ed25519.pub
```

`ssh-keygen` refuses to overwrite an existing file unless you say `y`: if one is there, it may be an older key
that the fos machines already trust, so look before you replace it (`ssh-keygen -lf ~/.ssh/fos_ed25519.pub`).

**On fos**, add the public line to `settings/authorized_keys` (one line per key, at most 16):

```
echo 'user=alice,uid=0,gid=0,shell ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA...' >> settings/authorized_keys
```

* `user=NAME` is the name you type in `ssh NAME@host`; `uid=`/`gid=` decide which files the account may see (`0` sees
  everything, so give real users real numbers); `ro` makes the account read-only; `shell` lets it open a login shell,
  not only SFTP. All options go before the key, separated by commas without spaces.
* Use `>>`. With a single `>` the file would contain only this key and every other account would be locked out.
* `cat settings/authorized_keys` shows what is in there. Over Ethernet from the workstation:
  `tools/foscopy.py --iface IF HOST:disk1/settings/authorized_keys .`, edit, and `tools/foscopy.py ... FILE :disk1/settings/authorized_keys`
  (`fosadm put` cannot write into folders).
* sshd reads the file when it starts: **restart the service** after a change. On a machine that runs it as a service
  (`settings/services`) that is `service restart NAME`, NAME being the first word of its line (`ssh` on one machine, from
  `ssh restart sshd @sshd.cfg &ssh`); otherwise stop it with `kill PID` and run `run sshd @sshd.cfg &N` again.

**Log in**, naming the key so the client does not offer others first:

```
ssh -i ~/.ssh/fos_ed25519 -o IdentitiesOnly=yes alice@fosbox
sftp -i ~/.ssh/fos_ed25519 -o IdentitiesOnly=yes alice@fosbox
sshfs -o IdentityFile=~/.ssh/fos_ed25519,IdentitiesOnly=yes alice@fosbox:/ /mnt/fos
```

or once and for all in `~/.ssh/config`:

```
Host fosbox
    User alice
    IdentityFile ~/.ssh/fos_ed25519
    IdentitiesOnly yes
```

Without `IdentityFile`, OpenSSH tries `id_ed25519`, `id_rsa` and so on; fos's sshd accepts ed25519 only and
`Permission denied (publickey)` is what you see when none of the keys offered is listed.

The warning `connection is not using a post-quantum key exchange algorithm` from a new OpenSSH client is only that: fos's
sshd (and its `ssh`) offer `curve25519-sha256` and no post-quantum exchange yet. The connection is still encrypted.

The first time you connect, the client asks whether to trust the host key. The fingerprint sshd prints in its start
message (and `settings/ssh_host_key` keeps) is the one to compare with; after the question OpenSSH stores it in
`~/.ssh/known_hosts`. If fos's host key file is deleted, sshd makes a new key and every client then warns about a changed
key (`ssh-keygen -R fosbox` removes the old line on the Linux side).

## 5. Are these the same key?

Two keys that start alike may not be the same. Compare fingerprints, not the first characters:

```
ssh-keygen -lf ~/.ssh/fos_ed25519.pub    # a public key file: its fingerprint
ssh-keygen -y -f ~/.ssh/fos_ed25519      # the public key a private key file makes (proves the pair belongs together)
```

To get the fingerprint of a line you have on fos (`cat settings/authorized_keys`, without the options in front of the
key, or the line `ssh keygen` printed), paste just `ssh-ed25519 AAAA...` into a file on Linux and run `ssh-keygen -lf FILE`.
The fingerprints must read the same `SHA256:...`.

## 6. When it does not work

| What you see | Likely cause |
| --- | --- |
| `ssh: no settings/ssh_key` (fos) | no key yet: `tape ssh keygen` |
| `ssh: ... the server did not accept the login` (fos) | the line is not in the server's `authorized_keys`, wrong user name, or the Linux permissions of `~/.ssh` are too loose (look in `journalctl -u ssh` or `/var/log/auth.log` on the server: it says why) |
| `THE KEY OF ... CHANGED` (fos) | the server's host key is not the remembered one: see part 3 |
| `Permission denied (publickey)` (Linux to fos) | the offered key is not in fos's `authorized_keys`, `sshd` was not restarted after the edit, or the user name differs from `user=` |
| `Load key ...: error in libcrypto` (Linux) | that private key *file* is damaged or not a valid OpenSSH key on this machine (a partial copy, CRLF line endings, a missing last line) or an odd OpenSSL is in use; the public line on fos is not at fault. Compare `sha256sum` of the file on both machines, or make a new key (part 4) |
| `Load key ...: invalid format` | the file lost its first or last line, or got a different line ending in a copy; the file starts with `-----BEGIN OPENSSH PRIVATE KEY-----` and ends with `-----END OPENSSH PRIVATE KEY-----` |
| connects, then no shell | the key line has no `shell` option (SFTP only), or the four `term net` sessions are in use |
| the password prompt is refused | the server has `PasswordAuthentication no` (many do) or the account has none |

Debug from the Linux side with `ssh -vvv ...`: it lists which keys were offered and why they were turned down. fos's
`sshd.cfg` takes `log`, which makes `sshd` print its connections and logins on its console.

Other files, for reference: `docs/config.md` ("sshd", "sshmount and ssh") lists every option; `README.md` lists the tapes;
`make test-mf-ssh`, `make test-mf-sshd` and `make test-mf-sshmount` show each flow working against real OpenSSH or the host
copy of the server.
