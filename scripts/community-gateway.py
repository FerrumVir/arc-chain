#!/usr/bin/env python3
"""
ARC Chain — Community Worker Gateway v2

Lightweight HTTP server (port 3001) that runs ALONGSIDE the main arc-node.
Handles community worker registration, heartbeat, listing, AND inference
work distribution. Deploys as a one-file sidecar with zero dependencies.

Architecture:
  - Community workers register and long-poll /community/claim_work
  - External clients POST /inference/community for inference
  - Gateway picks an available worker, pushes the request, waits for result
  - Worker computes locally (full model), POSTs /community/submit_work
  - Gateway returns the result to the original requester

Endpoints:
  POST /community/register    — worker registers
  POST /community/heartbeat   — worker stays alive
  GET  /community/list        — returns all live workers
  GET  /community/stats       — aggregate stats
  POST /community/claim_work  — worker long-polls for inference jobs (30s timeout)
  POST /community/submit_work — worker submits inference result
  POST /inference/community   — external client submits inference request to be
                                 routed to a community worker
  GET  /health                — gateway health

Usage:
  python3 community-gateway.py [--port 3001]
"""

import json, time, sys, os, uuid, hashlib
from http.server import HTTPServer, BaseHTTPRequestHandler
from threading import Lock, Event, Thread
from collections import deque

PORT = int(os.environ.get("ARC_GATEWAY_PORT",
    sys.argv[sys.argv.index("--port") + 1] if "--port" in sys.argv else 3001))
TTL_SECS = 90
CLAIM_TIMEOUT_SECS = 30
LOCAL_ARC_NODE = os.environ.get("ARC_NODE_RPC", "http://localhost:9090")

# ─── Worker registry ──────────────────────────────────────────────────────────
workers = {}       # worker_id -> {info, last_seen, work_completed}
lock = Lock()

# ─── Work queue ───────────────────────────────────────────────────────────────
# Pending inference jobs waiting for a community worker to claim them.
# Each job is a dict with: job_id, input, max_tokens, submitted_at, result_event
pending_jobs = deque()          # FIFO queue of jobs waiting to be claimed
active_jobs = {}                # job_id -> job (currently being computed)
completed_jobs = {}             # job_id -> result (awaiting pickup by requester)
jobs_lock = Lock()

# Stats
stats = {"jobs_submitted": 0, "jobs_completed": 0, "jobs_failed": 0, "total_tokens": 0}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass

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

    def _body(self):
        n = int(self.headers.get("Content-Length", 0))
        return json.loads(self.rfile.read(n)) if n > 0 else {}

    def _prune(self):
        now = time.time()
        for k in [k for k, v in workers.items() if now - v["last_seen"] > TTL_SECS]:
            del workers[k]

    def do_OPTIONS(self):
        self.send_response(200)
        self._cors()
        self.end_headers()

    # ── GET ────────────────────────────────────────────────────────────────────

    def do_GET(self):
        if self.path == "/community/list":
            with lock:
                self._prune()
                live = [v["info"] | {"work_completed": v["work_completed"]} for v in workers.values()]
            self._json(200, {"workers": live, "count": len(live),
                             "total_work_completed": sum(w["work_completed"] for w in workers.values())})

        elif self.path == "/community/stats":
            with lock:
                self._prune()
            self._json(200, {
                "total_workers": len(workers),
                "total_work_completed": sum(w.get("work_completed", 0) for w in workers.values()),
                "jobs_submitted": stats["jobs_submitted"],
                "jobs_completed": stats["jobs_completed"],
                "jobs_failed": stats["jobs_failed"],
                "total_tokens_generated": stats["total_tokens"],
                "pending_jobs": len(pending_jobs),
                "active_jobs": len(active_jobs),
                "uptime_secs": int(time.time() - START_TIME),
            })

        elif self.path == "/health":
            with lock:
                self._prune()
            self._json(200, {"status": "ok", "service": "community-gateway",
                             "workers": len(workers), "pending_jobs": len(pending_jobs)})
        else:
            self._json(404, {"error": "not found"})

    # ── POST ───────────────────────────────────────────────────────────────────

    def do_POST(self):
        try:
            body = self._body()
        except Exception:
            self._json(400, {"error": "invalid JSON"})
            return

        # ── Worker registration ────────────────────────────────────────────
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

        # ── Heartbeat ──────────────────────────────────────────────────────
        elif self.path == "/community/heartbeat":
            wid = body.get("worker_id", "").strip()
            with lock:
                if wid in workers:
                    workers[wid]["last_seen"] = time.time()
                    if "work_completed" in body:
                        workers[wid]["work_completed"] = body["work_completed"]
                    self._json(200, {"ok": True})
                else:
                    self._json(404, {"error": "not registered"})

        # ── Claim work (long-poll) ─────────────────────────────────────────
        elif self.path == "/community/claim_work":
            wid = body.get("worker_id", "").strip()
            with lock:
                if wid not in workers:
                    self._json(404, {"error": "not registered"})
                    return
                workers[wid]["last_seen"] = time.time()

            # Poll the queue for up to CLAIM_TIMEOUT_SECS
            deadline = time.time() + CLAIM_TIMEOUT_SECS
            job = None
            while time.time() < deadline:
                with jobs_lock:
                    if pending_jobs:
                        job = pending_jobs.popleft()
                        job["worker_id"] = wid
                        job["claimed_at"] = time.time()
                        active_jobs[job["job_id"]] = job
                        break
                time.sleep(0.5)

            if job:
                self._json(200, {
                    "status": "work",
                    "job_id": job["job_id"],
                    "input": job["input"],
                    "max_tokens": job["max_tokens"],
                    "chat_template": job.get("chat_template", False),
                })
            else:
                self._json(200, {"status": "no_work"})

        # ── Submit work ────────────────────────────────────────────────────
        elif self.path == "/community/submit_work":
            job_id = body.get("job_id", "").strip()
            wid = body.get("worker_id", "").strip()
            with jobs_lock:
                if job_id not in active_jobs:
                    self._json(404, {"error": "unknown job_id"})
                    return
                job = active_jobs.pop(job_id)
                result = {
                    "success": body.get("success", True),
                    "output": body.get("output", ""),
                    "output_tokens": body.get("output_tokens", []),
                    "output_hash": body.get("output_hash", ""),
                    "tokens_generated": body.get("tokens_generated", 0),
                    "ms_per_token": body.get("ms_per_token", 0),
                    "total_ms": body.get("total_ms", 0),
                    "worker_id": wid,
                    "engine": body.get("engine", "community worker"),
                }
                completed_jobs[job_id] = result
                # Signal the waiting requester
                if "result_event" in job:
                    job["result_event"].set()
                stats["jobs_completed"] += 1
                stats["total_tokens"] += result.get("tokens_generated", 0)

            with lock:
                if wid in workers:
                    workers[wid]["work_completed"] = workers[wid].get("work_completed", 0) + 1
                    workers[wid]["last_seen"] = time.time()

            self._json(200, {"ok": True, "job_id": job_id})

        # ── Inference via community worker ─────────────────────────────────
        elif self.path == "/inference/community":
            input_text = body.get("input", "")
            max_tokens = min(body.get("max_tokens", 20), 256)
            if not input_text:
                self._json(400, {"error": "'input' required"})
                return

            # Check if any workers are available
            with lock:
                self._prune()
                available = len(workers)
            if available == 0:
                self._json(503, {"error": "no community workers available",
                                 "hint": "install a community node: curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/install-community-node.sh | bash"})
                return

            # Create job and wait for a worker to claim + complete it
            job_id = str(uuid.uuid4())[:12]
            event = Event()
            job = {
                "job_id": job_id,
                "input": input_text,
                "max_tokens": max_tokens,
                "chat_template": body.get("chat_template", False),
                "submitted_at": time.time(),
                "result_event": event,
            }
            with jobs_lock:
                pending_jobs.append(job)
                stats["jobs_submitted"] += 1

            # Wait for result (up to 5 min — inference can be slow)
            if event.wait(timeout=300):
                with jobs_lock:
                    result = completed_jobs.pop(job_id, None)
                if result:
                    self._json(200, {
                        "success": True,
                        "input": input_text,
                        "output": result.get("output", ""),
                        "output_hash": result.get("output_hash", ""),
                        "tokens_generated": result.get("tokens_generated", 0),
                        "total_ms": result.get("total_ms", 0),
                        "worker_id": result.get("worker_id", ""),
                        "engine": "community worker",
                        "community": True,
                    })
                else:
                    self._json(500, {"error": "result lost"})
            else:
                # Timeout — remove from queue if still pending
                with jobs_lock:
                    if job_id in active_jobs:
                        active_jobs.pop(job_id)
                    stats["jobs_failed"] += 1
                self._json(504, {"error": "worker timed out (300s)", "job_id": job_id})

        else:
            self._json(404, {"error": "not found"})


# ── Job cleanup thread ─────────────────────────────────────────────────────────
def cleanup_loop():
    """Evict stale active jobs (claimed but never completed) every 60s."""
    while True:
        time.sleep(60)
        now = time.time()
        with jobs_lock:
            stale = [jid for jid, j in active_jobs.items()
                     if now - j.get("claimed_at", now) > 600]  # 10 min stale
            for jid in stale:
                j = active_jobs.pop(jid)
                if "result_event" in j:
                    j["result_event"].set()  # unblock requester
                stats["jobs_failed"] += 1


START_TIME = time.time()

if __name__ == "__main__":
    # Start cleanup thread
    t = Thread(target=cleanup_loop, daemon=True)
    t.start()

    print(f"ARC Community Gateway v2 listening on :{PORT}")
    print(f"  /community/register    — worker registration")
    print(f"  /community/claim_work  — worker long-poll for jobs")
    print(f"  /community/submit_work — worker submits results")
    print(f"  /inference/community   — submit inference for community compute")
    print(f"  /community/list        — list live workers")
    print(f"  /community/stats       — job statistics")

    server = HTTPServer(("0.0.0.0", PORT), Handler)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nShutting down.")
