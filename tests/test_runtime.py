"""Loopback bridge tests with a fake Hermes server; no upstream install or network."""
import http.client
import http.server
import importlib.util
from pathlib import Path
import socket
import sys
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

    def test_websocket_upgrade_sends_each_header_once(self):
        # POM (hyper) forwards header names in lowercase; uvicorn answers 426 to
        # a duplicated Upgrade, which broke every Hermes WebSocket behind POM.
        upstream = socket.socket()
        upstream.bind(("127.0.0.1", 0))
        upstream.listen(1)
        seen = {}
        def accept():
            conn, _ = upstream.accept()
            head = b""
            while b"\r\n\r\n" not in head: head += conn.recv(4096)
            seen["head"] = head.decode()
            conn.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
            conn.close()
        threading.Thread(target=accept, daemon=True).start()
        bridge = http.server.ThreadingHTTPServer(("127.0.0.1", 0), runtime.Bridge)
        bridge.secret = "p" * 32
        bridge.hermes_token = "hermes-only"
        bridge.target = upstream.getsockname()[1]
        threading.Thread(target=bridge.serve_forever, daemon=True).start()
        try:
            with socket.create_connection(("127.0.0.1", bridge.server_port)) as client:
                client.sendall(("GET /api/ws?token=t HTTP/1.1\r\nhost: localhost:8080\r\nupgrade: websocket\r\n"
                                "connection: Upgrade\r\nsec-websocket-key: a2V5a2V5a2V5a2V5a2V5aw==\r\n"
                                "sec-websocket-version: 13\r\norigin: http://localhost:8080\r\n"
                                f"x-pom-plugin-token: {bridge.secret}\r\n\r\n").encode())
                reply = client.recv(4096)
            self.assertTrue(reply.startswith(b"HTTP/1.1 101"), reply)
            names = [line.split(":", 1)[0].lower() for line in seen["head"].split("\r\n")[1:] if line]
            for name in ("host", "upgrade", "connection", "origin", "authorization"):
                self.assertEqual(names.count(name), 1, seen["head"])
            self.assertIn(f"Origin: http://127.0.0.1:{bridge.target}\r\n", seen["head"])
        finally:
            bridge.shutdown(); bridge.server_close(); upstream.close()

FAKE_DASHBOARD = r"""
import json, subprocess, sys, time
home = sys.argv[1]
sleep = "import time; time.sleep(60)"
stubborn = "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)"
tui = subprocess.Popen([sys.executable, "-c", sleep], start_new_session=True)
middle = subprocess.Popen([sys.executable, "-c", "import subprocess, sys; print(subprocess.Popen([sys.executable, '-c', "
                           + repr(stubborn) + "], start_new_session=True, stdout=subprocess.DEVNULL).pid)"], stdout=subprocess.PIPE, text=True)
orphan = int(middle.communicate()[0])
open(home + "/pids", "w").write(json.dumps([tui.pid, orphan]))
time.sleep(60)
"""

DRIVER = r"""
import importlib.util, json, os, subprocess, sys, time
spec = importlib.util.spec_from_file_location("runtime", sys.argv[1])
runtime = importlib.util.module_from_spec(spec); spec.loader.exec_module(runtime)
home = sys.argv[2]
runtime.adopt_orphans()
child = subprocess.Popen([sys.executable, "-c", sys.argv[3], home], start_new_session=True)
while not os.path.exists(home + "/pids"): time.sleep(0.05)
time.sleep(0.3)
started = time.monotonic()
runtime.stop_hermes(child, home)
zombies = []
for entry in filter(str.isdigit, os.listdir("/proc")):
    try:
        fields = open(f"/proc/{entry}/stat").read().rsplit(")", 1)[1].split()
    except OSError:
        continue
    if fields[0] == "Z" and int(fields[1]) == os.getpid():
        zombies.append(int(entry))
print(json.dumps({"dashboard": child.pid, "seconds": time.monotonic() - started, "zombies": zombies}))
"""

def alive(pid):
    try:
        with open(f"/proc/{pid}/stat") as stat:
            return stat.read().rsplit(")", 1)[1].split()[0] != "Z"
    except OSError:
        return False

WATCH_DRIVER = r"""
import importlib.util, signal, subprocess, sys
spec = importlib.util.spec_from_file_location("runtime", sys.argv[1])
runtime = importlib.util.module_from_spec(spec); spec.loader.exec_module(runtime)
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(0.3)"])
runtime.watch_dashboard(child, interval=0.1)
sys.stdin.buffer.read(1)
"""

class WatchTests(unittest.TestCase):
    @unittest.skipIf(sys.platform == "win32", "SIGTERM to self is POSIX")
    def test_the_bridge_exits_when_its_dashboard_exits(self):
        # The supervisor only notices a dead bridge: a dashboard that died
        # under a live bridge left POM showing a blank Hermes screen.
        import subprocess
        bridge = subprocess.Popen([sys.executable, "-c", WATCH_DRIVER, str(Path(runtime.__file__))],
                                  stdin=subprocess.PIPE)
        try:
            self.assertEqual(bridge.wait(timeout=5), 0)
        finally:
            if bridge.poll() is None: bridge.kill()

class ShutdownTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform.startswith("linux"), "subreaper adoption is Linux-only")
    def test_stop_ends_detached_descendants_and_the_recorded_gateway(self):
        import json, subprocess, tempfile
        with tempfile.TemporaryDirectory() as home:
            # A gateway outside the tree, found only through Hermes's own gateway.pid.
            gateway = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)", "hermes_cli"],
                                       start_new_session=True)
            with open(home + "/gateway.pid", "w") as record:
                json.dump({"pid": gateway.pid, "kind": "hermes-gateway", "hermes_home": home}, record)
            try:
                out = subprocess.run([sys.executable, "-c", DRIVER, str(Path(runtime.__file__)), home, FAKE_DASHBOARD],
                                     capture_output=True, text=True, timeout=30)
                self.assertEqual(out.returncode, 0, out.stderr)
                report = json.loads(out.stdout)
                gateway.wait(timeout=5)
                with open(home + "/pids") as pids:
                    for pid in [report["dashboard"], *json.load(pids)]:
                        self.assertFalse(alive(pid), pid)
                self.assertLess(report["seconds"], 5)
                self.assertEqual(report["zombies"], [])
            finally:
                if gateway.poll() is None: gateway.kill()

if __name__ == "__main__": unittest.main()
