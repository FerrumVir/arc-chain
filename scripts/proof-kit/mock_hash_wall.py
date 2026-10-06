#!/usr/bin/env python3
"""A local, in-memory stand-in for the Hash Wall API, for tests only.

Serves the two calls the Proof Kit makes (docs/proof-kit.md, "What the Hash
Wall endpoint must accept") on 127.0.0.1 only:

  GET  /challenge   a challenge signed with HMAC-SHA256 under a key made up
                    at start-up: {challenge_id, seed, expires_at, signature}
  POST /            one arc.proof-result.v1 body (at most 8192 bytes): checked
                    with arc_conformance.proof_kit_reference.validate_result,
                    then the signature, expiry and nonce. Accepted bodies are
                    written to --store; the reply says "pending" until a
                    second submission with a different nonce reports the same
                    challenge digest for the same challenge, then "verified".

It shows the server-side rules; it is not the production service (no rate
limits, no persistence beyond --store, no IP handling at all).

    python3 scripts/proof-kit/mock_hash_wall.py --port 8787 --store DIR
"""

from __future__ import annotations

import argparse
import hashlib
import hmac
import json
import secrets
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Dict, List

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from arc_conformance import proof_kit_reference as ref  # noqa: E402

KEY = secrets.token_bytes(32)
LIFETIME_SECONDS = 30 * 60
ISSUED: Dict[str, Dict[str, str]] = {}
ACCEPTED: List[Dict[str, Any]] = []
NONCES: set = set()


def sign(challenge_id: str, seed: str, expires_at: str) -> str:
    message = f"{challenge_id}\n{seed}\n{expires_at}".encode("ascii")
    return hmac.new(KEY, message, hashlib.sha256).hexdigest()


def open_challenge() -> Dict[str, str]:
    """Hand out a challenge that already has exactly one submission (so a
    second, independent run can confirm it), otherwise a new one."""
    now = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() + 300))
    for challenge_id, challenge in ISSUED.items():
        count = sum(1 for a in ACCEPTED if a["challenge_id"] == challenge_id)
        if count == 1 and challenge["expires_at"] > now:
            return challenge
    return new_challenge()


def new_challenge() -> Dict[str, str]:
    challenge_id = "mock-" + secrets.token_hex(8)
    seed = secrets.token_hex(32)
    expires_at = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() + LIFETIME_SECONDS))
    challenge = {"challenge_id": challenge_id, "seed": seed, "expires_at": expires_at,
                 "signature": sign(challenge_id, seed, expires_at)}
    ISSUED[challenge_id] = challenge
    return challenge


class Handler(BaseHTTPRequestHandler):
    store: Path

    def reply(self, status: int, body: Dict[str, Any]) -> None:
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self) -> None:  # noqa: N802 - http.server naming
        if self.path.rstrip("/") != "/challenge":
            self.reply(404, {"error": "not found"})
            return
        self.reply(200, open_challenge())

    def do_POST(self) -> None:  # noqa: N802 - http.server naming
        length = int(self.headers.get("Content-Length") or 0)
        if self.path.rstrip("/") not in ("", "/"):
            self.reply(404, {"error": "not found"})
            return
        if length > ref.MAX_RESULT_BYTES:
            self.reply(413, {"error": f"body over {ref.MAX_RESULT_BYTES} bytes"})
            return
        raw = self.rfile.read(length)
        try:
            payload = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            self.reply(400, {"error": "not JSON"})
            return
        errors = ref.validate_result(payload, raw_bytes=len(raw))
        if errors:
            self.reply(400, {"errors": errors})
            return
        challenge = payload["challenge"]
        issued = ISSUED.get(challenge["challenge_id"])
        expected = sign(challenge["challenge_id"], challenge["seed"], challenge["expires_at"])
        if issued is None or not hmac.compare_digest(expected, challenge["signature"]):
            self.reply(400, {"error": "unknown challenge or bad signature"})
            return
        if time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()) > challenge["expires_at"]:
            self.reply(400, {"error": "challenge expired"})
            return
        nonce_hash = hashlib.sha256(payload["nonce"].encode()).hexdigest()
        if nonce_hash in NONCES:
            self.reply(409, {"error": "nonce already used"})
            return
        NONCES.add(nonce_hash)
        agreeing = [a for a in ACCEPTED
                    if a["challenge_id"] == challenge["challenge_id"]
                    and a["challenge_digest"] == challenge["digest"]]
        status = "verified" if agreeing else "pending"
        ACCEPTED.append({"challenge_id": challenge["challenge_id"],
                         "challenge_digest": challenge["digest"]})
        self.store.mkdir(parents=True, exist_ok=True)
        index = len(list(self.store.glob("submission-*.json")))
        (self.store / f"submission-{index}.json").write_bytes(raw)
        self.reply(202, {"status": status, "verdict": payload["verdict"]})

    def log_message(self, format: str, *args: Any) -> None:  # noqa: A002
        sys.stderr.write("mock hash wall: " + (format % args) + "\n")


def main(argv: List[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--port", type=int, default=8787)
    parser.add_argument("--store", required=True)
    args = parser.parse_args(argv)
    Handler.store = Path(args.store)
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    sys.stderr.write(f"mock hash wall: http://127.0.0.1:{args.port}\n")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
