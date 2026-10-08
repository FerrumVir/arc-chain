#!/usr/bin/env python3
"""Recording HTTPS server that plays github.com for the Wave 0 desktop lab (THROWAWAY LAB FILE, never merged).

The released v0.7.11 desktop app updates through the Tauri updater plugin (tauri-plugin-updater 2.10.1). Its only endpoint
is https://github.com/FerrumVir/arc-chain/releases/latest/download/latest.json and its TLS goes through the operating
system trust store. On a CI runner the job maps the GitHub names to 127.0.0.1 in the hosts file, trusts a per-run CA
(lib/ca.py) and starts THIS server on port 443; it records every request and every refused handshake and never serves a
real bundle. Nothing here reaches the Internet.

What is recorded (one JSON object per line, fsynced; ``MitmServer.requests()`` returns the same objects):
  kind "request"      one served HTTP request: t, seq, scenario, sni, host (Host header, else SNI), method, path, query,
                      status, bytes_out (body bytes), user_agent, remote, role, payload, location (redirects only)
  kind "tls_failure"  a client that did not complete the TLS handshake (for example because it validates against bundled
                      webpki roots and rejected our CA, like the app's own banner and ensure_binary calls): t, seq,
                      scenario, sni (null if the client never sent one), host (= sni), remote, error
role is one of: manifest (GET .../releases/latest/download/latest.json), manifest_redirect (the redirect target of the
manifest URL while Latest is v0.7.12: .../releases/download/v0.7.12/latest.json), other_manifest (any other path that
ends in latest.json, for example the bait tag's), payload (see below), api_latest (api.github.com releases/latest), other.
payload is true for any path that looks like a bundle or installer (.sig .tar.gz .tgz .gz .zip .exe .msi .dmg .deb .rpm
.AppImage .pkg); such requests are answered 404 and are what the pass criteria call "a download".

Scenarios (a complete world per name; the API route answers in all of them):
  latest-404         GitHub with Latest = v0.7.12 which carries NO latest.json: the manifest URL answers 302 to
                     /FerrumVir/arc-chain/releases/download/v0.7.12/latest.json, which answers 404 (GitHub resolves "latest"
                     by redirect and answers 404 at the target when the asset does not exist).
  latest-404-direct  the same world, but the manifest URL answers 404 itself (no redirect hop)
  bait-0.8.11        the manifest URL answers 200 with a Tauri updater manifest for version 0.8.11 (all four platform
                     keys; URLs under releases/download/v0.8.11/; syntactically valid but WRONG signatures). The
                     bundles themselves are never served.
  api-latest         alias of latest-404, named for the API checks (api.github.com releases/latest answers a v0.7.12
                     release JSON with the five launchers and SHA256SUMS, no latest.json)
Any other path answers 404 and is recorded. ``serve_dir`` (optional) serves a non-payload file by the basename of the
path for fixtures; payload paths stay 404 even if such a file exists.

Every response carries the headers Server: wave0-lab-mitm and X-Wave0-Lab-Scenario so a job can assert, with one curl,
that the hosts mapping really lands on this server and not on the real GitHub.

Interfaces:
  MitmServer(cert, key, listen_host="127.0.0.1", port=443, scenario="latest-404", log_path=None, serve_dir=None)
      start() -> None   (binds; ``listen_host`` may list several addresses, e.g. "127.0.0.1,::1")
      stop() -> None
      requests() -> list of dict   (copies; request and tls_failure records in arrival order)
      port -> int       (the bound port; useful with port=0)
  CLI: python3 lib/mitm_server.py --scenario NAME --cert F --key F [--listen 127.0.0.1:443] --log requests.jsonl
                                  [--ready-file F] [--serve-dir DIR]
       The ready file (JSON with pid and port) is written after the sockets are bound; stop with SIGTERM or SIGINT.

UNVERIFIED ON CI: that GitHub answers the manifest URL with a 302 to the release download path before the 404 (the
latest-404-direct scenario covers the other possibility); that the plugin's reqwest negotiates http/1.1 from the single
ALPN protocol offered here; that binding ::1 works on every runner (a failure to bind a secondary address is logged and
tolerated, a failure to bind the first one is fatal).
"""
from __future__ import annotations

import argparse
import base64
import json
import os
import signal
import socket
import socketserver
import ssl
import sys
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

REPO_PATH = "/FerrumVir/arc-chain"
MANIFEST_PATH = REPO_PATH + "/releases/latest/download/latest.json"
REDIRECT_TARGET_PATH = REPO_PATH + "/releases/download/v0.7.12/latest.json"
API_LATEST_PATH = "/repos/FerrumVir/arc-chain/releases/latest"
SCENARIOS = ("latest-404", "latest-404-direct", "bait-0.8.11", "api-latest")
PAYLOAD_SUFFIXES = (".sig", ".tar.gz", ".tgz", ".gz", ".zip", ".exe", ".msi", ".dmg", ".deb", ".rpm", ".appimage", ".pkg")
BAIT_VERSION = "0.8.11"
BAIT_BASE = "https://github.com" + REPO_PATH + "/releases/download/v" + BAIT_VERSION + "/"
BAIT_ASSETS = {
    "darwin-aarch64": "ARC.Node_aarch64.app.tar.gz",
    "darwin-x86_64": "ARC.Node_x64.app.tar.gz",
    "windows-x86_64": "ARC.Node_0.8.11_x64-setup.exe",
    "linux-x86_64": "ARC.Node_0.8.11_amd64.AppImage",
}
LAUNCHERS = (
    ("arc-node-linux-aarch64", 2167024),
    ("arc-node-linux-x86_64", 2663184),
    ("arc-node-macos-arm64", 1945744),
    ("arc-node-macos-x86_64", 2299448),
    ("arc-node-windows-x86_64.exe", 2220544),
    ("SHA256SUMS", 446),
)
SERVER_HEADER = "wave0-lab-mitm"


def bait_signature() -> str:
    """A syntactically valid minisign signature file (base64 of its text) whose signature bytes are all zero: it parses,
    it can never verify."""
    sig_line = base64.b64encode(b"ED" + b"\x00" * 8 + b"\x00" * 64).decode("ascii")
    global_line = base64.b64encode(b"\x00" * 64).decode("ascii")
    text = "untrusted comment: wave0-lab bait signature, intentionally invalid\n%s\ntrusted comment: wave0-lab bait\n%s\n" % (sig_line, global_line)
    return base64.b64encode(text.encode("ascii")).decode("ascii")


def bait_manifest() -> Dict[str, Any]:
    return {
        "version": BAIT_VERSION,
        "notes": "wave0-lab bait manifest: not a real release, signatures are intentionally invalid",
        "pub_date": "2026-10-08T00:00:00Z",
        "platforms": {key: {"signature": bait_signature(), "url": BAIT_BASE + name} for key, name in BAIT_ASSETS.items()},
    }


def api_release(tag: str) -> Dict[str, Any]:
    base = "https://github.com" + REPO_PATH + "/releases/download/" + tag + "/"
    return {
        "url": "https://api.github.com" + API_LATEST_PATH,
        "tag_name": tag,
        "name": tag,
        "draft": False,
        "prerelease": False,
        "published_at": "2026-10-08T08:13:18Z",
        "html_url": "https://github.com" + REPO_PATH + "/releases/tag/" + tag,
        "body": "wave0-lab recording server: a stand-in for the GitHub API, no real release data",
        "assets": [{"name": name, "size": size, "browser_download_url": base + name} for name, size in LAUNCHERS],
    }


def is_payload_path(path: str) -> bool:
    lowered = path.lower()
    return any(lowered.endswith(suffix) for suffix in PAYLOAD_SUFFIXES)


def classify_role(host: str, path: str) -> str:
    if path == MANIFEST_PATH:
        return "manifest"
    if path == REDIRECT_TARGET_PATH:
        return "manifest_redirect"
    if path.lower().endswith("/latest.json"):
        return "other_manifest"
    if is_payload_path(path):
        return "payload"
    if path == API_LATEST_PATH:
        return "api_latest"
    return "other"


def route(scenario: str, method: str, host: str, path: str) -> Tuple[int, Dict[str, str], bytes]:
    """The response of the simulated GitHub: (status, extra headers, body). Pure; unit-tested."""
    if scenario not in SCENARIOS:
        raise ValueError("unknown scenario %r" % scenario)
    json_headers = {"Content-Type": "application/json; charset=utf-8"}
    if path == MANIFEST_PATH:
        if scenario == "bait-0.8.11":
            return 200, dict(json_headers), json.dumps(bait_manifest(), indent=2).encode("utf-8")
        if scenario == "latest-404-direct":
            return 404, {"Content-Type": "text/plain; charset=utf-8"}, b"Not Found"
        location = "https://github.com" + REDIRECT_TARGET_PATH
        body = ('<html><body>You are being <a href="%s">redirected</a>.</body></html>' % location).encode("utf-8")
        return 302, {"Content-Type": "text/html; charset=utf-8", "Location": location}, body
    if path == API_LATEST_PATH:
        tag = "v" + BAIT_VERSION if scenario == "bait-0.8.11" else "v0.7.12"
        return 200, dict(json_headers), json.dumps(api_release(tag), indent=2).encode("utf-8")
    return 404, {"Content-Type": "text/plain; charset=utf-8"}, b"Not Found"


class Recorder:
    """Thread-safe JSONL recorder with an in-memory copy; every line is flushed and fsynced."""

    def __init__(self, path: Optional[str], scenario: str):
        self.path = path
        self.scenario = scenario
        self.records: List[Dict[str, Any]] = []
        self.lock = threading.Lock()
        self.seq = 0
        self.handle = None
        if path:
            Path(path).parent.mkdir(parents=True, exist_ok=True)
            self.handle = open(path, "a", encoding="utf-8")

    def add(self, record: Dict[str, Any]) -> Dict[str, Any]:
        with self.lock:
            self.seq += 1
            record = dict(record)
            record["seq"] = self.seq
            record["scenario"] = self.scenario
            record.setdefault("t", round(time.time(), 6))
            self.records.append(record)
            if self.handle is not None:
                self.handle.write(json.dumps(record, sort_keys=True) + "\n")
                self.handle.flush()
                try:
                    os.fsync(self.handle.fileno())
                except OSError:
                    pass
            return record

    def snapshot(self) -> List[Dict[str, Any]]:
        with self.lock:
            return [dict(item) for item in self.records]

    def close(self) -> None:
        with self.lock:
            if self.handle is not None:
                self.handle.close()
                self.handle = None


class _State:
    def __init__(self, scenario: str, recorder: Recorder, serve_dir: Optional[str]):
        self.scenario = scenario
        self.recorder = recorder
        self.serve_dir = serve_dir


class _Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = SERVER_HEADER
    sys_version = ""

    def log_message(self, format: str, *args: Any) -> None:  # noqa: A002 - silence the default stderr access log
        return

    def version_string(self) -> str:
        return SERVER_HEADER

    def _serve(self) -> None:
        state: _State = self.server.state  # type: ignore[attr-defined]
        parsed = urllib.parse.urlsplit(self.path)
        path, query = parsed.path, parsed.query
        sni = getattr(self.connection, "lab_sni", None)
        host = (self.headers.get("Host") or sni or "").split(":")[0].lower()
        length = int(self.headers.get("Content-Length") or 0)
        if 0 < length <= (1 << 20):
            self.rfile.read(length)  # drain a request body so the reset does not hide the answer
        payload = is_payload_path(path)
        role = classify_role(host, path)
        status, headers, body = route(state.scenario, self.command, host, path)
        if payload:
            status, headers, body = 404, {"Content-Type": "text/plain; charset=utf-8"}, b"Not Found"
        elif state.serve_dir and status == 404:
            candidate = Path(state.serve_dir) / os.path.basename(path)
            if os.path.basename(path) and candidate.is_file():
                data = candidate.read_bytes()
                status, headers, body = 200, {"Content-Type": "application/octet-stream"}, data
        self.send_response(status)
        for key, value in headers.items():
            self.send_header(key, value)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.send_header("X-Wave0-Lab-Scenario", state.scenario)
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)
        self.close_connection = True
        record = {
            "kind": "request", "sni": sni, "host": host, "method": self.command, "path": path, "query": query,
            "status": status, "bytes_out": 0 if self.command == "HEAD" else len(body),
            "user_agent": self.headers.get("User-Agent"), "remote": self.client_address[0],
            "role": role, "payload": payload,
        }
        if "Location" in headers:
            record["location"] = headers["Location"]
        state.recorder.add(record)

    do_GET = do_HEAD = do_POST = do_PUT = do_DELETE = do_PATCH = do_OPTIONS = _serve


class _Server(socketserver.ThreadingMixIn, HTTPServer):
    daemon_threads = True
    allow_reuse_address = True
    request_queue_size = 64

    def __init__(self, address: Tuple[str, int], context: ssl.SSLContext, state: _State, family: int):
        self.address_family = family
        self.context = context
        self.state = state
        super().__init__(address, _Handler, bind_and_activate=False)

    def server_bind(self) -> None:
        # HTTPServer.server_bind() would call socket.getfqdn(), a reverse DNS lookup that can stall on a runner; none is needed here.
        socketserver.TCPServer.server_bind(self)
        host, port = self.server_address[:2]
        self.server_name = str(host)
        self.server_port = port

    def process_request_thread(self, request: socket.socket, client_address: Any) -> None:
        tls = None
        try:
            request.settimeout(15)
            tls = self.context.wrap_socket(request, server_side=True, do_handshake_on_connect=False)
            tls.do_handshake()
        except Exception as error:  # noqa: BLE001 - a refused or aborted handshake is evidence, not a crash
            sni = getattr(tls, "lab_sni", None) if tls is not None else None
            self.state.recorder.add({
                "kind": "tls_failure", "sni": sni, "host": sni, "remote": client_address[0] if client_address else None,
                "error": "%s: %s" % (type(error).__name__, str(error)[:200]),
            })
            for sock in (tls, request):  # wrap_socket detaches the raw socket, so the TLS object must be closed too
                try:
                    if sock is not None:
                        sock.close()
                except OSError:
                    pass
            return
        super().process_request_thread(tls, client_address)


def build_context(cert: str, key: str) -> ssl.SSLContext:
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert, key)
    try:
        context.minimum_version = ssl.TLSVersion.TLSv1_2
    except (AttributeError, ValueError):
        pass
    try:
        context.set_alpn_protocols(["http/1.1"])
    except (AttributeError, NotImplementedError):
        pass

    def remember_sni(sock: Any, name: Optional[str], _context: ssl.SSLContext) -> None:
        try:
            sock.lab_sni = name.lower() if name else None
        except AttributeError:
            pass
        return None

    context.sni_callback = remember_sni
    return context


class MitmServer:
    def __init__(self, cert: str, key: str, listen_host: str = "127.0.0.1", port: int = 443, scenario: str = "latest-404",
                 log_path: Optional[str] = None, serve_dir: Optional[str] = None):
        if scenario not in SCENARIOS:
            raise ValueError("unknown scenario %r (choose from %s)" % (scenario, ", ".join(SCENARIOS)))
        self.cert, self.key = cert, key
        self.listen_hosts = [item.strip() for item in listen_host.split(",") if item.strip()]
        self.requested_port = port
        self.scenario = scenario
        self.log_path = log_path
        self.serve_dir = serve_dir
        self.recorder = Recorder(log_path, scenario)
        self._servers: List[_Server] = []
        self._threads: List[threading.Thread] = []
        self.bind_warnings: List[str] = []
        self.port = 0

    def start(self) -> None:
        context = build_context(self.cert, self.key)
        state = _State(self.scenario, self.recorder, self.serve_dir)
        port = self.requested_port
        for index, host in enumerate(self.listen_hosts):
            family = socket.AF_INET6 if ":" in host else socket.AF_INET
            server = _Server((host, port), context, state, family)
            try:
                server.server_bind()
                server.server_activate()
            except OSError as error:
                server.server_close()
                if index == 0:
                    raise
                self.bind_warnings.append("could not listen on %s:%s: %s" % (host, port, error))
                continue
            if index == 0:
                port = server.server_address[1]
            self._servers.append(server)
        self.port = port
        for server in self._servers:
            thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.1}, daemon=True, name="mitm-%s" % server.server_address[0])
            thread.start()
            self._threads.append(thread)

    def stop(self) -> None:
        for server in self._servers:
            server.shutdown()
            server.server_close()
        for thread in self._threads:
            thread.join(timeout=5)
        self._servers, self._threads = [], []
        self.recorder.close()

    def requests(self) -> List[Dict[str, Any]]:
        return self.recorder.snapshot()


def parse_listen(value: str) -> Tuple[str, int]:
    host, _, port = value.rpartition(":")
    if not host or not port.isdigit():
        raise argparse.ArgumentTypeError("--listen must look like HOST:PORT (HOST may be a comma separated list)")
    return host.strip("[]"), int(port)


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--scenario", choices=SCENARIOS, default="latest-404")
    parser.add_argument("--cert", required=True)
    parser.add_argument("--key", required=True)
    parser.add_argument("--listen", type=parse_listen, default=("127.0.0.1", 443))
    parser.add_argument("--log", required=True)
    parser.add_argument("--ready-file")
    parser.add_argument("--serve-dir")
    args = parser.parse_args(argv)
    host, port = args.listen
    server = MitmServer(args.cert, args.key, listen_host=host, port=port, scenario=args.scenario, log_path=args.log, serve_dir=args.serve_dir)
    server.start()
    stop = threading.Event()

    def on_signal(_signo: int, _frame: Any) -> None:
        stop.set()

    signal.signal(signal.SIGINT, on_signal)
    if hasattr(signal, "SIGTERM"):
        signal.signal(signal.SIGTERM, on_signal)
    if hasattr(signal, "SIGBREAK"):
        signal.signal(signal.SIGBREAK, on_signal)  # Windows: CTRL_BREAK_EVENT
    if args.ready_file:
        Path(args.ready_file).write_text(json.dumps({"pid": os.getpid(), "port": server.port, "scenario": args.scenario, "warnings": server.bind_warnings}) + "\n", encoding="utf-8")
    print("mitm_server listening on %s:%d scenario=%s log=%s" % (",".join(server.listen_hosts), server.port, args.scenario, args.log), flush=True)
    while not stop.wait(0.2):
        pass
    server.stop()
    return 0


if __name__ == "__main__":
    sys.exit(main())
