#!/usr/bin/env python3
"""The `sipd` tape: a SIP registrar and stateless proxy on a user-mode NIC, spoken to from the host over a forwarded UDP port.

Two host sockets act as phones `alice` and `bob`. Checks: a REGISTER without credentials is challenged (401), a wrong password
is refused (403), the right one is accepted (200), alice's INVITE to
sip:bob@SERVER reaches bob with the proxy's Via and Record-Route and a rewritten request URI, bob's 200 comes back to alice
with the proxy's Via removed, and OPTIONS to the server is answered. The refusal of an unregistered address is a host unit test (every phone here shares slirp's one address). Run with `make test-sipd`."""
import hashlib
import re
import shutil
import socket
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from testslot import port  # noqa: E402
from test_boot import check  # noqa: E402
import test_httpd  # noqa: E402
import fosfs_image  # noqa: E402
from test_httpd import Guest  # noqa: E402

HOST_PORT = port(25060)
SERVER = "10.0.2.15"
USERS = {"alice": "wonderland", "bob": "builder"}


def md5(text):
    return hashlib.md5(text.encode()).hexdigest()


def phone():
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(("127.0.0.1", 0))
    sock.settimeout(1.0)
    return sock


def receive(sock, seconds=12):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        try:
            return sock.recvfrom(4096)[0].decode()
        except socket.timeout:
            pass
    return None


def exchange(sock, request, seconds=12):
    """Sends `request` (retrying: the guest's first ARP can swallow one datagram) and returns the first answer."""
    for _ in range(4):
        sock.sendto(request.encode(), ("127.0.0.1", HOST_PORT))
        answer = receive(sock, seconds / 4)
        if answer:
            return answer
    return None


def register(sock, user, password, call_id, cseq, auth=None):
    port_ = sock.getsockname()[1]
    headers = (f"REGISTER sip:{SERVER} SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:{port_};branch=z9hG4bK{call_id}{cseq}\r\n"
               f"Max-Forwards: 70\r\nFrom: <sip:{user}@{SERVER}>;tag=t{call_id}\r\nTo: <sip:{user}@{SERVER}>\r\n"
               f"Call-ID: {call_id}\r\nCSeq: {cseq} REGISTER\r\nContact: <sip:{user}@127.0.0.1:{port_}>\r\nExpires: 600\r\n")
    if auth:
        realm, nonce = auth
        uri = f"sip:{SERVER}"
        nc, cnonce = "00000001", "abcdef"
        digest = md5(f"{md5(f'{user}:{realm}:{password}')}:{nonce}:{nc}:{cnonce}:auth:{md5(f'REGISTER:{uri}')}")
        headers += (f'Authorization: Digest username="{user}", realm="{realm}", nonce="{nonce}", uri="{uri}", '
                    f'response="{digest}", qop=auth, nc={nc}, cnonce="{cnonce}"\r\n')
    return headers + "Content-Length: 0\r\n\r\n"


def log_in(sock, user, password, call_id):
    first = exchange(sock, register(sock, user, password, call_id, 1))
    check(first is not None and first.startswith("SIP/2.0 401"), f"{user}: REGISTER without credentials is challenged", str(first))
    realm = re.search(r'realm="([^"]*)"', first).group(1)
    nonce = re.search(r'nonce="([^"]*)"', first).group(1)
    return realm, nonce


def main():
    alice, bob = phone(), phone()
    seeded = Path(tempfile.mkdtemp()) / "seeded.img"
    shutil.copy(test_httpd.IMAGE, seeded)
    fosfs_image.add(seeded, {"sipusers": "# phones\nalice:wonderland\nbob:builder\n"})
    test_httpd.IMAGE = seeded
    with Guest(data_disk=False, guest_port=5060, user_extra=f",hostfwd=udp:127.0.0.1:{HOST_PORT}-10.0.2.15:5060") as g:
        g.line(f"run sipd users=sipusers log ip={SERVER}/24 gateway=10.0.2.2 &2", "fos> ", 40)
        time.sleep(2)

        realm, nonce = log_in(alice, "alice", "wrong", "a1")
        answer = exchange(alice, register(alice, "alice", "wrong", "a1", 2, (realm, nonce)))
        check(answer is not None and answer.startswith("SIP/2.0 403"), "a wrong password is refused", str(answer))
        answer = exchange(alice, register(alice, "alice", USERS["alice"], "a1", 3, (realm, nonce)))
        check(answer is not None and answer.startswith("SIP/2.0 200"), "the right password registers", str(answer))
        realm, nonce = log_in(bob, "bob", "builder", "b1")
        answer = exchange(bob, register(bob, "bob", USERS["bob"], "b1", 2, (realm, nonce)))
        check(answer is not None and answer.startswith("SIP/2.0 200"), "bob registers", str(answer))

        invite = (f"OPTIONS sip:{SERVER} SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:{alice.getsockname()[1]};branch=z9hG4bKopt\r\n"
                  f"Max-Forwards: 70\r\nFrom: <sip:alice@{SERVER}>;tag=o1\r\nTo: <sip:{SERVER}>\r\nCall-ID: opt1\r\nCSeq: 1 OPTIONS\r\n"
                  "Content-Length: 0\r\n\r\n")
        answer = exchange(alice, invite)
        check(answer is not None and answer.startswith("SIP/2.0 200"), "OPTIONS to the server is answered", str(answer))

        port_a = alice.getsockname()[1]
        invite = (f"INVITE sip:bob@{SERVER} SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:{port_a};branch=z9hG4bKinv1\r\nMax-Forwards: 70\r\n"
                  f"From: <sip:alice@{SERVER}>;tag=i1\r\nTo: <sip:bob@{SERVER}>\r\nCall-ID: call1\r\nCSeq: 1 INVITE\r\n"
                  f"Contact: <sip:alice@127.0.0.1:{port_a}>\r\nContent-Length: 0\r\n\r\n")
        alice.sendto(invite.encode(), ("127.0.0.1", HOST_PORT))
        seen = receive(bob)
        if seen is None:  # the first datagram can be lost to ARP: once more
            alice.sendto(invite.encode(), ("127.0.0.1", HOST_PORT))
            seen = receive(bob)
        check(seen is not None and seen.startswith("INVITE "), "bob receives the proxied INVITE", str(seen))
        vias = re.findall(r"^Via: .*$", seen, re.M)
        check(len(vias) == 2 and SERVER in vias[0], "the proxy put its own Via on top", seen)
        check("Record-Route:" in seen, "the proxy asked to stay in the path", seen)
        check(not seen.startswith(f"INVITE sip:bob@{SERVER}"), "the request URI was rewritten to bob's address", seen)

        reply = (f"SIP/2.0 200 OK\r\n{vias[0]}\r\n{vias[1]}\r\nFrom: <sip:alice@{SERVER}>;tag=i1\r\nTo: <sip:bob@{SERVER}>;tag=b9\r\n"
                 f"Call-ID: call1\r\nCSeq: 1 INVITE\r\nContact: <sip:bob@127.0.0.1:{bob.getsockname()[1]}>\r\nContent-Length: 0\r\n\r\n")
        bob.sendto(reply.encode(), ("127.0.0.1", HOST_PORT))
        back = receive(alice)
        check(back is not None and back.startswith("SIP/2.0 200"), "alice receives bob's 200", str(back))
        check(back is not None and len(re.findall(r"^Via: ", back, re.M)) == 1, "the proxy's Via was removed", str(back))

        screen = g.line("", "fos> ", 10)
        check("fos> " in screen, "the shell still answers while sipd runs", screen[-300:])
    print("PASS: sipd registrar and proxy")


if __name__ == "__main__":
    main()
