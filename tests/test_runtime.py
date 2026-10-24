"""Loopback bridge tests with a fake Hermes server; no upstream install or network."""
import http.client
import http.server
import importlib.util
from pathlib import Path
import socket
import threading
import unittest

spec = importlib.util.spec_from_file_location("runtime", Path(__file__).resolve().parents[1] / "runtime.py")
runtime = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runtime)

class FakeHermes(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        chunks = []
        if self.headers.get("Transfer-Encoding") == "chunked":
            while True:
                size = int(self.rfile.readline().strip(), 16)
                if not size:
                    self.rfile.readline()
                    break
                chunks.append(self.rfile.read(size))
                self.rfile.read(2)
        else:
            chunks.append(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        data = b"".join(chunks)
        self.send_response(200)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        content = (self.headers.get("Host", "") + "|" + self.headers.get("X-Forwarded-Prefix", "") + "|" + self.headers.get("Authorization", "")).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(content)))
        self.end_headers()
        self.wfile.write(content)
    def log_message(self, *_args): pass

class BridgeTests(unittest.TestCase):
    def test_proxy_requires_token_and_reauthenticates_upstream(self):
        upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FakeHermes)
        bridge = http.server.ThreadingHTTPServer(("127.0.0.1", 0), runtime.Bridge)
        bridge.secret = "p" * 32
        bridge.hermes_token = "hermes-only"
        bridge.target = upstream.server_port
        threads = [threading.Thread(target=s.serve_forever, daemon=True) for s in (upstream, bridge)]
        for thread in threads: thread.start()
        try:
            for token, expected in ((None, 401), ("bad", 401), (bridge.secret, 200)):
                client = http.client.HTTPConnection("127.0.0.1", bridge.server_port)
                headers = {"Authorization": "Bearer browser-private", "X-Forwarded-Prefix": "/evil"}
                if token: headers["x-pom-plugin-token"] = token
                client.request("GET", "/api/status", headers=headers)
                reply = client.getresponse()
                body = reply.read().decode()
                self.assertEqual(reply.status, expected)
                if expected == 200:
                    self.assertIn("/api/ui/plugins/hermes/proxy|Bearer hermes-only", body)
                    self.assertNotIn("browser-private", body)
                client.close()
            client = http.client.HTTPConnection("127.0.0.1", bridge.server_port)
            client.request("POST", "/api/upload", body=[b"a" * 32768, b"b" * 32768],
                           headers={"x-pom-plugin-token": bridge.secret}, encode_chunked=True)
            reply = client.getresponse()
            self.assertEqual(reply.status, 200)
            self.assertEqual(reply.read(), b"a" * 32768 + b"b" * 32768)
            client.close()
        finally:
            for server in (bridge, upstream): server.shutdown(); server.server_close()

if __name__ == "__main__": unittest.main()
