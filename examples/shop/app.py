"""A local demo shop. Python 3 standard library only; no real accounts or payments."""
import argparse
from http.cookies import SimpleCookie
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

ROOT = Path(__file__).parent


class Shop(BaseHTTPRequestHandler):
    def send(self, status, body=b"", content_type="text/html; charset=utf-8", **headers):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        for key, value in headers.items():
            self.send_header(key.replace("_", "-"), value)
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = urlsplit(self.path).path
        if path == "/health":
            self.send(200, b"ready", "text/plain")
        elif path in ("/", "/login"):
            self.send(200, (ROOT / "login.html").read_bytes())
        elif path in ("/checkout", "/preview"):
            cookies = SimpleCookie(self.headers.get("Cookie", ""))
            session = cookies.get("demo_session")
            if session is None or session.value != "sample-user":
                self.send(303, Location="/login")
            else:
                name = "checkout.html" if path == "/checkout" else "preview.html"
                self.send(200, (ROOT / name).read_bytes())
        else:
            self.send(404, b"Not found", "text/plain")

    def do_POST(self):
        if urlsplit(self.path).path != "/login":
            self.send(404)
            return
        size = int(self.headers.get("Content-Length", "0"))
        if size > 4096:
            self.send(413)
            return
        fields = parse_qs(self.rfile.read(size).decode())
        if fields.get("email") == ["demo@example.com"] and fields.get("password") == ["demo-password"]:
            self.send(303, Location="/checkout", Set_Cookie="demo_session=sample-user; Path=/; HttpOnly; SameSite=Lax")
        else:
            self.send(401, b'<h1>Sign in failed</h1><a href="/login">Try again</a>')


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=4173)
    args = parser.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Shop)
    print(f"http://127.0.0.1:{server.server_port}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
