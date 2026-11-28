import argparse
import http.client
import json
from pathlib import Path
import re
import time
import urllib.error
import urllib.request


READINESS_URL = "http://127.0.0.1:9944/inference/readiness"
RESPONSE_LIMIT = 64 * 1024


class RejectRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def validate_common_snapshot(body, profile):
    if not isinstance(body, dict):
        raise ValueError("worker readiness response must be a JSON object")
    if body.get("schema") != "arc.inference.readiness.v1":
        raise ValueError("liveness is not inference readiness: require the inference readiness schema")
    if body.get("required_community_execution_profile") != profile:
        raise ValueError("worker readiness does not bind the canonical execution profile")
    if body.get("mutation_free_observation") is not True:
        raise ValueError("readiness snapshot is not a mutation-free observation")
    for field in ("local_model_ready", "safe_to_dispatch", "community_dispatch_ready", "sharded_pipeline_ready"):
        if type(body.get(field)) is not bool:
            raise ValueError(f"readiness {field} must be a boolean")
    workers = body.get("live_community_workers")
    if type(workers) is not int or workers < 0:
        raise ValueError("readiness live_community_workers must be a non-negative integer")


def classify_readiness(status, body, model_id, profile):
    if status == 503:
        validate_common_snapshot(body, profile)
        if (
            body["safe_to_dispatch"]
            or body["local_model_ready"]
            or body["community_dispatch_ready"]
            or body["sharded_pipeline_ready"]
        ):
            raise ValueError("loading 503 readiness response must report all readiness flags false")
        if body.get("model_id") is not None:
            raise ValueError("loading 503 readiness response must report null model_id")
        if body["live_community_workers"] != 0:
            raise ValueError("loading 503 readiness response must report zero live_community_workers")
        return "loading"
    if status != 200:
        raise ValueError("worker readiness must return HTTP 503 while loading or HTTP 200 when serving")
    validate_common_snapshot(body, profile)
    observed_model = body.get("model_id")
    if observed_model is not None and observed_model != "0x" + model_id:
        raise ValueError("worker readiness reports a different model commitment")
    if body["local_model_ready"]:
        if observed_model != "0x" + model_id or not body["safe_to_dispatch"]:
            raise ValueError("local model readiness lacks its model commitment or dispatch readiness")
        return "ready"
    return "loading"


def read_snapshot(opener, timeout):
    try:
        response = opener.open(READINESS_URL, timeout=timeout)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        raw = response.read(RESPONSE_LIMIT + 1)
        if len(raw) > RESPONSE_LIMIT:
            raise ValueError("readiness response exceeds 64 KiB")
        status = response.code
        if status not in (200, 503):
            raise ValueError(
                f"worker readiness returned unexpected HTTP {status}; expected 503 while loading or 200 when serving"
            )
        try:
            body = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise ValueError(f"worker readiness HTTP {status} returned invalid JSON: {error}") from error
        return status, body


def wait_for_model(model_id, profile, seconds, output):
    if not isinstance(model_id, str) or not re.fullmatch(r"[0-9a-f]{64}", model_id):
        raise ValueError("expected model must be an exact lowercase 32-byte commitment")
    if not isinstance(profile, str) or not profile or type(seconds) is not int or not 1 <= seconds <= 600:
        raise ValueError("require an execution profile and a readiness deadline of 1..600 seconds")
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), RejectRedirects())
    start = time.monotonic()
    deadline = start + seconds
    with output.open("x", encoding="utf-8") as evidence:
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            try:
                status, body = read_snapshot(opener, min(2.0, max(0.01, remaining)))
            except (
                urllib.error.URLError,
                TimeoutError,
                ConnectionError,
                http.client.HTTPException,
            ) as error:
                record = {"state": "unreachable", "error": str(error)}
            else:
                state = classify_readiness(status, body, model_id, profile)
                record = {"state": state, "http_status": status, "snapshot": body}
            record["elapsed_seconds"] = round(time.monotonic() - start, 3)
            evidence.write(json.dumps(record, sort_keys=True) + "\n")
            evidence.flush()
            if record["state"] == "ready":
                return
            sleep_for = min(1.0, deadline - time.monotonic())
            if sleep_for > 0:
                time.sleep(sleep_for)
    raise TimeoutError("worker passed liveness but did not load the pinned inference model before the readiness deadline")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--model-id", required=True)
    parser.add_argument("--profile", required=True)
    parser.add_argument("--seconds", type=int, default=600)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    wait_for_model(args.model_id, args.profile, args.seconds, args.out)


if __name__ == "__main__":
    main()
