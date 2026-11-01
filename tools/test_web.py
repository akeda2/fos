#!/usr/bin/env python3
"""The `web` tape: the curl-like command line mode and the full-screen text mode against a small web server on the host.

The guest's NIC 2 is QEMU's user-mode network; the host (127.0.0.1) is 10.0.2.2 there, so a `http.server` on the host plays the web.
Command line mode (`web -c`, `-ci`, `-I`, `-cL`, `-d`): body, head, head only, redirects followed, the text rendering with its list
of links, a 404, a refused connection. Text mode: the page and its title appear, Down selects a link (its address shows on the
status line), Right follows it, Left goes back, `g` takes a typed address, `q` returns to the shell. Run with `make test-web`."""
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from testslot import port  # noqa: E402
from test_boot import TAG_DATA, check  # noqa: E402
from test_httpd import Guest  # noqa: E402

WEB_PORT = port(28180)
NET = "ip=10.0.2.15/24 gateway=10.0.2.2"

PAGES = {
    "/": ("text/html", b"<html><head><title>Test Page</title></head><body><h1>Welcome</h1><p>Plain words and a "
                       b"<a href='/second.html'>second page</a> plus <a href='other/third.html'>third</a> caf&eacute; &aring;&auml;&ouml;.</p></body></html>"),
    "/second.html": ("text/html", b"<html><head><title>Second</title></head><body><p>This is the second page. <a href='/'>home</a></p></body></html>"),
    "/a.txt": ("text/plain", b"just text from the host\n"),
    "/final": ("text/plain", b"arrived after the redirect\n"),
}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def do_GET(self):
        if self.path == "/old":
            self.send_response(301)
            self.send_header("Location", "/final")
            self.send_header("Content-Length", "9")
            self.end_headers()
            self.wfile.write(b"moved now")
            return
        kind, body = PAGES.get(self.path, (None, None))
        if body is None:
            self.send_response(404)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            self.wfile.write(b"no such page here")
            return
        self.send_response(200)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_HEAD(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("X-Head-Only", "yes")
        self.end_headers()

    def log_message(self, *args):
        pass


def main():
    server = HTTPServer(("127.0.0.1", WEB_PORT), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f"10.0.2.2:{WEB_PORT}"
    with Guest() as g:
        g.line("nic use 2")

        def web(arguments, marker="fos> "):
            return g.line(f"run web {arguments} {NET}", marker, 60)

        out = web(f"-c {base}/a.txt")
        check("just text from the host" in out, "web -c prints the page", out)
        out = web(f"-ci {base}/a.txt")
        check("200 OK" in out and "Content-Type: text/plain" in out and "just text from the host" in out, "-ci prints the head and the page", out)
        out = web(f"-I {base}/a.txt")
        check("X-Head-Only: yes" in out and "just text" not in out, "-I sends HEAD and prints the head only", out)
        out = web(f"-ci {base}/old")
        check("301" in out and "moved now" in out and "arrived" not in out, "a redirect is shown as it is without -L", out)
        out = web(f"-cL {base}/old")
        check("arrived after the redirect" in out and "moved now" not in out, "-L follows the redirect", out)
        out = web(f"-d {base}/")
        check("Welcome" in out and "second page" in out and "café" in out and "åäö" in out, "-d prints the page as text (entities decoded)", out)
        check(f"http://{base}/second.html" in out and f"http://{base}/other/third.html" in out, "and lists the links with absolute addresses", out)
        out = web(f"-ci {base}/missing")
        check("404" in out and "no such page here" in out, "a 404 is shown with its body", out)
        out = web("-c 10.0.2.2:1/")
        check("refused" in out, "a closed port says the connection was refused", out)
        out = web("-c https://10.0.2.2/")
        check("https is not supported" in out, "https is refused with a reason", out)

        # Text mode. Sequences go in one frame so the console sees ESC [ B as one key, not a lone Escape.
        screen = web(f"{base}/", "Welcome")
        check("Test" in screen and "Welcome" in screen, "text mode shows the page and its title", screen)
        g.wire.send(g.guest, TAG_DATA, b"\x1b[B")
        screen = g.wire.until("third.html", 15)
        check("third.html" in screen, "the first link starts selected; Down selects the next and the status line shows where it goes", screen)
        g.wire.send(g.guest, TAG_DATA, b"\x1b[A")
        screen = g.wire.until("/second.html", 15)
        check("/second.html" in screen, "Up selects the previous link", screen)
        g.wire.send(g.guest, TAG_DATA, b"\r")
        screen = g.wire.until("second", 20)
        check("This" in screen, "Enter follows the link", screen)
        g.wire.send(g.guest, TAG_DATA, b"\x1b[D")
        screen = g.wire.until("Welcome", 20)
        check("Welcome" in screen, "Left goes back to the first page", screen)
        g.type(b"g")
        g.wire.send(g.guest, TAG_DATA, b"\x15")
        g.type(f"{base}/a.txt\r".encode())
        screen = g.wire.until("just", 20)
        check("just" in screen and "host" in screen, "g takes a typed address (Ctrl-U clears the old one)", screen)
        g.type(b"q")
        check("fos> " in g.wire.until("fos> ", 20), "q returns to the shell", "")
        check("ticks at 100 Hz" in g.line("uptime", "ticks at 100 Hz"), "and the shell is still usable", "")
    server.shutdown()
    print("web: all checks passed")


if __name__ == "__main__":
    main()
