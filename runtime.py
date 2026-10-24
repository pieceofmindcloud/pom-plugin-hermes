"""Loopback dashboard bridge using Hermes PM's managed Python on every platform."""
import http.client
import http.server
import json
import os
from pathlib import Path
import secrets
import select
import signal
import socket
import subprocess
import sys
import time


class Bridge(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def handle_request(self):
        token = self.headers.get("x-pom-plugin-token", "")
        if not secrets.compare_digest(token, self.server.secret):
            self.send_error(401)
            return
        # POM authenticates the browser, strips its Authorization and checks
        # Origin before attaching the private x-pom-plugin-token.
        target = self.server.target
        headers = {key: value for key, value in self.headers.items() if key.lower() not in
                   ("host", "connection", "x-pom-plugin-token", "authorization", "cookie", "x-forwarded-prefix", "content-length", "transfer-encoding")}
        headers["Host"] = f"127.0.0.1:{target}"
        headers["X-Forwarded-Prefix"] = "/api/ui/plugins/hermes/proxy"
        headers["Authorization"] = f"Bearer {self.server.hermes_token}"
        if self.headers.get("Upgrade", "").lower() == "websocket":
            self.websocket(headers, target)
            return
        chunked = self.headers.get("Transfer-Encoding", "").lower() == "chunked"
        if self.headers.get("Transfer-Encoding") and not chunked:
            self.send_error(400)
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            self.send_error(400)
            return
        if length < 0 or length > 512 * 1024 * 1024:
            self.send_error(413)
            return
        if chunked and self.headers.get("Content-Length"):
            self.send_error(400)
            return
        body = self.request_body(chunked, length) if chunked or length else None
        connection = http.client.HTTPConnection("127.0.0.1", target, timeout=120)
        try:
            connection.request(self.command, self.path, body, headers)
            response = connection.getresponse()
            self.send_response_only(response.status, response.reason)
            for key, value in response.getheaders():
                if key.lower() not in ("transfer-encoding", "connection", "content-length", "set-cookie", "x-frame-options"):
                    self.send_header(key, value)
            self.send_header("Transfer-Encoding", "chunked")
            self.send_header("Connection", "close")
            self.end_headers()
            while block := response.read1(64 * 1024):
                self.wfile.write(f"{len(block):x}\r\n".encode() + block + b"\r\n")
                self.wfile.flush()
            self.wfile.write(b"0\r\n\r\n")
        except (OSError, ConnectionError):
            self.close_connection = True
        finally:
            connection.close()
            self.close_connection = True

    def request_body(self, chunked, length):
        # POM streams request bodies with chunked encoding. Re-chunk to Hermes;
        # never buffer a whole upload in the plugin process.
        total = 0
        if chunked:
            while True:
                line = self.rfile.readline(100)
                size = int(line.split(b";", 1)[0].strip(), 16)
                if size < 0 or total + size > 512 * 1024 * 1024:
                    raise ValueError("Hermes upload too large")
                if size == 0:
                    while self.rfile.readline(8192) not in (b"\r\n", b"\n", b""):
                        pass
                    return
                remaining = size
                while remaining:
                    block = self.rfile.read(min(remaining, 64 * 1024))
                    if not block:
                        raise ValueError("Truncated Hermes upload")
                    remaining -= len(block)
                    total += len(block)
                    yield block
                if self.rfile.read(2) != b"\r\n":
                    raise ValueError("Invalid Hermes upload framing")
        else:
            while total < length:
                block = self.rfile.read(min(length - total, 64 * 1024))
                if not block:
                    raise ValueError("Truncated Hermes upload")
                total += len(block)
                yield block

    def websocket(self, headers, target):
        upstream = socket.create_connection(("127.0.0.1", target), timeout=10)
        try:
            headers["Upgrade"] = "websocket"
            headers["Connection"] = "Upgrade"
            if "Origin" in headers:
                headers["Origin"] = f"http://127.0.0.1:{target}"
            request = f"{self.command} {self.path} HTTP/1.1\r\n" + "".join(f"{k}: {v}\r\n" for k, v in headers.items()) + "\r\n"
            upstream.sendall(request.encode())
            response = b""
            while b"\r\n\r\n" not in response and len(response) < 65536:
                block = upstream.recv(4096)
                if not block:
                    return
                response += block
            self.connection.sendall(response)
            if not response.startswith(b"HTTP/1.1 101"):
                return
            self.close_connection = True
            upstream.settimeout(None)
            for sock in (self.connection, upstream):
                sock.setblocking(False)
            while True:
                ready, _, _ = select.select((self.connection, upstream), (), (), 30)
                for sock in ready:
                    try:
                        block = sock.recv(65536)
                        if not block:
                            return
                        (upstream if sock is self.connection else self.connection).sendall(block)
                    except (OSError, ConnectionError):
                        return
        finally:
            upstream.close()

    do_GET = do_POST = do_PUT = do_PATCH = do_DELETE = do_OPTIONS = handle_request


def serve(base, home):
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    hermes_token = secrets.token_urlsafe(32)
    env = os.environ.copy()
    env.update({"HERMES_HOME": str(home), "HERMES_RUNTIME_DIR": str(base / "tools"),
                "HERMES_DASHBOARD_SESSION_TOKEN": hermes_token, "PYTHONPATH": str(base / "source")})
    if os.name != "nt":
        env["HOME"] = str(home)
    else:
        env["USERPROFILE"] = str(home)
    log = (base / "dashboard.log").open("ab", buffering=0)
    child = subprocess.Popen([sys.executable, "-m", "hermes_cli.main", "dashboard", "--no-open",
                              "--host", "127.0.0.1", "--port", str(port)],
                             cwd=base / "source", env=env, stdin=subprocess.DEVNULL,
                             stdout=log, stderr=log, start_new_session=(os.name != "nt"),
                             creationflags=(subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0))
    try:
        for _ in range(120):
            if child.poll() is not None:
                raise RuntimeError("Hermes dashboard exited; see dashboard.log")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                    break
            except OSError:
                time.sleep(1)
        else:
            raise RuntimeError("Hermes dashboard did not start; see dashboard.log")
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Bridge)
        server.daemon_threads = True
        server.secret = secrets.token_hex(32)
        server.hermes_token = hermes_token
        server.target = port
        print(json.dumps({"status": "ready", "port": server.server_port, "token": server.secret}), flush=True)
        import threading
        threading.Thread(target=server.serve_forever, daemon=True).start()
        while sys.stdin.buffer.read(1):
            pass
        server.shutdown()
    finally:
        if child.poll() is None:
            if os.name == "nt":
                child.terminate()
            else:
                os.killpg(child.pid, signal.SIGTERM)
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            if os.name == "nt":
                child.kill()
            else:
                os.killpg(child.pid, signal.SIGKILL)
        log.close()


if __name__ == "__main__":
    if sys.argv[1] == "serve":
        serve(Path(sys.argv[2]), Path(sys.argv[3]))
