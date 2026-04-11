#!/usr/bin/env python3
"""
ARC Chain — Community Worker Gateway

Lightweight HTTP server (port 3001) that runs ALONGSIDE the main arc-node.
Handles community worker registration, heartbeat, and listing without
touching the consensus binary. Deploys as a one-file sidecar.

Endpoints:
  POST /community/register   — worker registers (worker_id, name, platform, model)
  POST /community/heartbeat  — worker stays alive (worker_id)
  GET  /community/list        — returns all live workers (TTL 90s)
  GET  /community/stats       — aggregate stats

Workers are evicted after 90s without a heartbeat.

Usage:
  python3 community-gateway.py                    # port 3001
  python3 community-gateway.py --port 3001        # explicit port
  ARC_GATEWAY_PORT=3001 python3 community-gateway.py
"""

import json, time, sys, os
from http.server import HTTPServer, BaseHTTPRequestHandler
from threading import Lock

PORT = int(os.environ.get("ARC_GATEWAY_PORT", sys.argv[sys.argv.index("--port") + 1] if "--port" in sys.argv else 3001))
TTL_SECS = 90

workers = {}  # worker_id -> {info, last_seen, work_completed}
lock = Lock()

class Handler(BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        pass  # silent

    def _cors(self):
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "Content-Type")

    def _json(self, code, data):
        body = json.dumps(data).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self._cors()
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _read_body(self):
        length = int(self.headers.get("Content-Length", 0))
        return json.loads(self.rfile.read(length)) if length > 0 else {}

    def _prune(self):
        now = time.time()
        expired = [k for k, v in workers.items() if now - v["last_seen"] > TTL_SECS]
        for k in expired:
            del workers[k]

    def do_OPTIONS(self):
        self.send_response(200)
        self._cors()
        self.end_headers()

    def do_GET(self):
        if self.path == "/community/list":
            with lock:
                self._prune()
                live = [v["info"] | {"work_completed": v["work_completed"]} for v in workers.values()]
            self._json(200, {
                "workers": live,
                "count": len(live),
                "total_work_completed": sum(w["work_completed"] for w in workers.values()),
            })
        elif self.path == "/community/stats":
            with lock:
                self._prune()
            self._json(200, {
                "total_workers": len(workers),
                "total_work_completed": sum(w["work_completed"] for w in workers.values()),
                "uptime_secs": int(time.time() - START_TIME),
            })
        elif self.path == "/health":
            self._json(200, {"status": "ok", "service": "community-gateway", "workers": len(workers)})
        else:
            self._json(404, {"error": "not found"})

    def do_POST(self):
        try:
            body = self._read_body()
        except Exception:
            self._json(400, {"error": "invalid JSON"})
            return

        if self.path == "/community/register":
            wid = body.get("worker_id", "").strip()
            if not wid or len(wid) > 128:
                self._json(400, {"error": "worker_id required (1-128 chars)"})
                return
            with lock:
                existing = workers.get(wid)
                workers[wid] = {
                    "info": {
                        "worker_id": wid,
                        "name": body.get("name", ""),
                        "capabilities": body.get("capabilities", ["inference"]),
                        "model": body.get("model"),
                        "platform": body.get("platform", ""),
                        "registered_at": existing["info"]["registered_at"] if existing else int(time.time()),
                    },
                    "last_seen": time.time(),
                    "work_completed": existing["work_completed"] if existing else 0,
                }
            self._json(200, {"ok": True, "worker_id": wid, "registry_size": len(workers),
                             "welcome": "Your node is now visible on the ARC testnet."})

        elif self.path == "/community/heartbeat":
            wid = body.get("worker_id", "").strip()
            with lock:
                if wid in workers:
                    workers[wid]["last_seen"] = time.time()
                    if "work_completed" in body:
                        workers[wid]["work_completed"] = body["work_completed"]
                    self._json(200, {"ok": True})
                else:
                    self._json(404, {"error": "worker_id not registered — call /community/register first"})

        else:
            self._json(404, {"error": "not found"})

START_TIME = time.time()

if __name__ == "__main__":
    print(f"ARC Community Gateway listening on :{PORT}")
    server = HTTPServer(("0.0.0.0", PORT), Handler)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nShutting down.")
