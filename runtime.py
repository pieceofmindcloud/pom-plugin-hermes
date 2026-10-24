"""Verified upstream installer and loopback dashboard bridge. No Hermes source is vendored."""
import hashlib
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
import tarfile
import time
import urllib.request

REV = "5894e750c3dba980d43b25aa29e3958149ba2c61"
SHA256 = "c6a3a8abdcebe82bc634761186b0fc51438bf5da06254ce0f486a3d6167ed1be"
URL = f"https://codeload.github.com/NousResearch/hermes-agent/tar.gz/{REV}"
MAX_ARCHIVE = 512 * 1024 * 1024


def install(base, home):
    """Run the exact upstream installer with a pinned commit, no interactive setup.

    The official PM installs its own verified Python 3.14, uv, Node, web assets
    and dependencies. pip-installing Hermes on system Python is not supported.
    """
    base.mkdir(parents=True, exist_ok=True)
    home.mkdir(parents=True, exist_ok=True)
    source = base / "source"
    marker = base / ".installed"
    launcher = source / ".hermes/bin/hermes"
    if (marker.is_file() and marker.read_text() == SHA256
            and launcher.is_file() and (source / "hermes_cli/web_dist/index.html").is_file()):
        return
    archive = base / "source.tar.gz"
    if not archive.is_file() or hashlib.sha256(archive.read_bytes()).hexdigest() != SHA256:
        digest = hashlib.sha256()
        size = 0
        with urllib.request.urlopen(URL, timeout=60) as response, archive.open("wb") as output:
            while block := response.read(256 * 1024):
                size += len(block)
                if size > MAX_ARCHIVE:
                    raise ValueError("Hermes source archive exceeds limit")
                output.write(block)
                digest.update(block)
        if digest.hexdigest() != SHA256:
            archive.unlink(missing_ok=True)
            raise ValueError("Hermes source checksum mismatch")
    # Extract only the upstream install script, never an unverified full tree.
    script = base / "install.sh"
    with tarfile.open(archive, "r:gz") as tar:
        entry = tar.getmember(f"hermes-agent-{REV}/scripts/install.sh")
        with tar.extractfile(entry) as reader, script.open("wb") as output:
            import shutil
            shutil.copyfileobj(reader, output)
    env = os.environ.copy()
    env.update({"HERMES_HOME": str(home), "HOME": str(home), "HERMES_RUNTIME_DIR": str(base / "tools"),
                "CI": "1", "HERMES_INSTALL_VERBOSE": "1"})
    # --commit also checks that REV is on upstream's main branch. A retry
    # resumes the checkout and PM generations instead of deleting user data.
    subprocess.run(["bash", str(script), "--commit", REV, "--dir", str(source),
                    "--hermes-home", str(home), "--non-interactive",
                    "--skip-browser", "--skip-computer-use"], check=True,
                   cwd=base, env=env, stdin=subprocess.DEVNULL)
    if not launcher.is_file() or not (source / "hermes_cli/web_dist/index.html").is_file():
        raise ValueError("Hermes dashboard build is missing; see install.log")
    archive.unlink(missing_ok=True)
    marker.write_text(SHA256)


class Bridge(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def handle_request(self):
        token = self.headers.get("x-pom-plugin-token", "")
        if not secrets.compare_digest(token, self.server.secret):
            self.send_error(401)
            return
        # POM already authenticates the browser, strips its Authorization and
        # checks Origin before attaching its private x-pom-plugin-token.
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
        # POM streams request bodies using chunked transfer encoding. Decode
        # the framing and let HTTPConnection re-chunk to Hermes: never buffer
        # the whole upload in the plugin process.
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
    env.update({"HERMES_HOME": str(home), "HOME": str(home), "HERMES_RUNTIME_DIR": str(base / "tools"),
                "HERMES_DASHBOARD_SESSION_TOKEN": hermes_token})
    log = (base / "dashboard.log").open("ab", buffering=0)
    child = subprocess.Popen([str(base / "source/.hermes/bin/hermes"), "dashboard", "--no-open",
                              "--host", "127.0.0.1", "--port", str(port)],
                             cwd=base / "source", env=env, stdin=subprocess.DEVNULL,
                             stdout=log, stderr=log, start_new_session=True)
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
            os.killpg(child.pid, signal.SIGTERM)
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
        log.close()


if __name__ == "__main__":
    if sys.argv[1] == "install":
        install(Path(sys.argv[2]), Path(sys.argv[3]))
    elif sys.argv[1] == "serve":
        serve(Path(sys.argv[2]), Path(sys.argv[3]))
