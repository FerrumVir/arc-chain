import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import signal
import socket
import ssl
import stat
import subprocess
import sys
import tempfile
import threading
import time
from typing import Any
import urllib.error
import urllib.parse
import urllib.request
import zipfile

EXPECTED_REPO = "FerrumVir/arc-chain"
EXPECTED_CANDIDATE_WORKFLOW_PATH = ".github/workflows/linux-candidate.yml"
ALLOWED_CANDIDATE_EVENTS = frozenset({"workflow_dispatch", "push"})
COORDINATOR_URL = "https://149.28.153.31"
COORDINATOR_NETLOC = "149.28.153.31"
GITHUB_API_ORIGIN = "https://api.github.com"
GITHUB_API_NETLOC = "api.github.com"
COORDINATOR_ID = "f521309b041da7aefc742548bdc002c31b47183aacfbbbf245ded09845d0415b"
TRANSACTION_DOMAIN = "3b9147e44d878646caf2d0729d06bd2e082b265e0525760595bd10605f37a10f"
EXPECTED_MODEL_ID = "934efc12a2ed8372a944e5aaedf059a8a0f42c0906f6b2f1fb3626bdeb1ffa67"
CANONICAL_EXECUTION_PROFILE = "INT8 integer (per-row, cross-platform deterministic)"
EXPECTED_PROTOCOL_VERSION = 1
EXPECTED_TOTAL_LAYERS = 32
EXPECTED_MAX_TILE_LAYERS = 6
EXPECTED_SIX_RANGES = ((0, 6), (6, 12), (12, 17), (17, 22), (22, 27), (27, 32))
EXPECTED_SLOTS = (1, 2, 3, 4, 5, 6)
SCALING_SLOTS = tuple(range(1, 13))
EXPECTED_READINESS_SCHEMA = "arc.community_shard.dispatch_readiness.v1"
EXPECTED_PARTICIPANT_IDENTITY_BASIS = "node_key_not_proof_of_independent_operator"
GGUF_SIZE_BYTES = 4081004224
GGUF_SHA256 = "08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa"
WORKER_MAX_WORKING_SET_BYTES = 7516192768
WORKER_MAX_KV_CACHE_BYTES = 268435456
WORKER_MAX_RESIDENT_LAYERS = 6
WORKER_THREADS = 2
WORKER_NAME_MAX_BYTES = 96
DISPATCH_PAYLOAD = {"input": "Hello", "max_tokens": 2, "chat_template": False}
DISPATCH_TIMEOUT_SECONDS = 420
READINESS_PATH = "/inference/community_shards/dispatch_readiness"
SHARD_STATUS_PATH = "/community/shards/status"
DISPATCH_PATH = "/inference/run_community_sharded"
COHORT_CLASS = "ephemeral_ci_qualification"
VERIFICATION_BASIS = "server_reported_validator_quorum_only_no_client_independent_crypto"
MAX_HTTP_BODY_BYTES = 1 * 1024 * 1024
MAX_GH_API_BODY_BYTES = 4 * 1024 * 1024
MAX_LOG_BYTES = 8 * 1024 * 1024
MAX_JSON_FILE_BYTES = 2 * 1024 * 1024
MAX_IDENTITY_ARTIFACT_BYTES = 256 * 1024
MAX_CANDIDATE_ARTIFACT_BYTES = 512 * 1024 * 1024
MAX_EVIDENCE_DIR_BYTES = 24 * 1024 * 1024
MAX_GH_ARTIFACT_PAGES = 5
MIN_MEM_AVAILABLE_BYTES = 12 * 1024 * 1024 * 1024
MIN_DISK_FREE_BYTES = 12 * 1024 * 1024 * 1024
MODEL_DOWNLOAD_ATTEMPT_SECONDS = 600
MODEL_DOWNLOAD_RETRY_WINDOW_SECONDS = 300
MODEL_DOWNLOAD_RETRIES = 2
MODEL_DOWNLOAD_TOTAL_SECONDS = 645
MODEL_DOWNLOAD_KILL_GRACE_SECONDS = 15
PREPARATION_GRACE_SECONDS = 240
ARTIFACT_AND_CLEANUP_GRACE_SECONDS = 180
WORKER_JOB_TIMEOUT_MINUTES = 60
CONTROLLER_JOB_TIMEOUT_MINUTES = 45
DEFAULT_WORKER_SUPERVISOR_MAX_SECONDS = 2400
MAX_WORKER_SUPERVISOR_SECONDS = 3600
RUNTIME_MAX_GRACE_SECONDS = 60
DEFAULT_CLEANUP_WAIT_SECONDS = 65
WORKER_POLL_INTERVAL_SECONDS = 30
MAX_POLL_BACKOFF_SECONDS = 120
CONTROLLER_IDENTITY_TIMEOUT_SECONDS = 900
CONTROLLER_READINESS_TIMEOUT_SECONDS = 300
CONTROLLER_POLL_INTERVAL_SECONDS = 5
REQUIRED_PROVENANCE_PIN_KEYS = frozenset(
    {"repository", "commit", "workflow_run_id", "workflow_run_attempt"}
)
REPEATABLE_PROVENANCE_KEYS = frozenset({"build", "tests", "check"})
FORBIDDEN_SECRET_MARKERS = (
    b"secret_key",
    b"private_key",
    b"authorization: bearer",
    b"ghs_",
    b"ghp_",
    b"github_pat_",
    b"begin private key",
)
READINESS_REQUIRED_KEYS = frozenset(
    {
        "schema",
        "mutation_free_observation",
        "enabled",
        "protocol_version",
        "verified_dispatch_enabled",
        "sharded_pipeline_ready",
        "safe_to_dispatch",
        "rewards_enabled",
        "chat_template_supported",
        "coordinator_id",
        "transaction_domain",
        "model_id",
        "execution_profile",
        "max_prompt_bytes",
        "chat_template_overhead_bytes",
        "max_tokens_cap",
    }
)
EXIT_REPORT_TOP_KEYS = frozenset(
    {
        "worker_id",
        "worker_name",
        "coordinator_url",
        "coordinator_id",
        "transaction_domain",
        "model_id",
        "model_name",
        "admitted_max_resident_layers",
        "threads",
        "summary",
    }
)
EXIT_REPORT_SUMMARY_KEYS = frozenset(
    {
        "registrations",
        "leases_acquired",
        "models_loaded",
        "heartbeats_sent",
        "steps_executed",
        "steps_submitted",
        "submit_retries",
        "submit_stale_conflicts",
        "cancellations_applied",
        "rebalances_completed",
        "clean_release_sent",
    }
)
DISPATCH_RESPONSE_KEYS = frozenset(
    {
        "protocol_version",
        "session_id",
        "model_name",
        "model_id",
        "execution_profile",
        "generated_tokens",
        "tokens_generated",
        "output",
        "output_hash",
        "stopped_by_eos",
        "total_ms",
        "participants",
        "participant_identity_basis",
        "verification",
        "rewards_enabled",
        "reward_eligible",
    }
)
PARTICIPANT_KEYS = frozenset({"worker_id", "lease_id", "lease_epoch", "range"})
VERIFICATION_REQUIRED_KEYS = frozenset(
    {
        "method",
        "profile_bound",
        "quorum_verified",
        "execution_profile",
        "output_hash",
        "tokens_generated",
        "ranges",
        "range_position_quorums",
        "signatures_required_per_quorum",
        "replicas_contacted_per_quorum",
    }
)
VERIFICATION_OPTIONAL_KEYS = frozenset({"ranges_verified_2_of_2"})
VERIFICATION_METHOD_2_OF_3 = "authenticated_shard_quorum_2_of_3_per_range"
VERIFICATION_METHOD_2_OF_2 = "authenticated_shard_quorum_2_of_2_per_range"
VERIFICATION_METHOD_MIXED = "authenticated_shard_quorum_2_of_3_or_2_of_2_per_range"
RUN_ID_RE = re.compile(r"^[1-9][0-9]{0,18}$")
SHA40_RE = re.compile(r"^[0-9a-f]{40}$")
HEX64_RE = re.compile(r"^[0-9a-f]{64}$")
SAFE_UNIT_RE = re.compile(r"^[a-zA-Z0-9._-]+$")
SAFE_ARTIFACT_NAME_RE = re.compile(r"^[a-zA-Z0-9._-]+$")


class QualificationError(RuntimeError):
    pass


class GitHubApiHttpError(QualificationError):
    def __init__(self, status_code: int, message: str, retry_after_seconds: int | None = None) -> None:
        super().__init__(message)
        self.status_code = status_code
        self.retry_after_seconds = retry_after_seconds


def minimal_host_env(extra: dict[str, str] | None = None) -> dict[str, str]:
    env = {"PATH": "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"}
    inherited_home = os.environ.get("HOME")
    if inherited_home:
        env["HOME"] = inherited_home
    if extra:
        env.update(extra)
    return env


def redact_secret(text: str, secret: str) -> str:
    if secret:
        return text.replace(secret, "<redacted>")
    return text


def read_error_preview(exc: urllib.error.HTTPError, max_bytes: int, secret: str = "") -> str:
    try:
        raw = exc.read(max_bytes + 1)
    except OSError:
        return "<unreadable error body>"
    finally:
        exc.close()
    truncated = len(raw) > max_bytes
    text = redact_secret(raw[:max_bytes].decode("utf-8", errors="replace"), secret)
    return f"{text}...(truncated)" if truncated else text


def parse_retry_after_seconds(headers: Any) -> int | None:
    if headers is None:
        return None
    raw = headers.get("Retry-After")
    if isinstance(raw, str) and raw.strip().isdigit():
        parsed = int(raw.strip())
        if parsed > 0:
            return min(parsed, MAX_POLL_BACKOFF_SECONDS)
    return None


class NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise QualificationError(f"HTTP redirect {code} is forbidden for {req.full_url}")


def utc_now_iso() -> str:
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def is_strict_int(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def is_strict_positive_int(value: Any) -> bool:
    return is_strict_int(value) and value > 0


def is_strict_nonneg_int(value: Any) -> bool:
    return is_strict_int(value) and value >= 0


def validate_positive_id(value: str, label: str) -> str:
    if not isinstance(value, str) or not RUN_ID_RE.fullmatch(value):
        raise QualificationError(f"{label} must be a positive integer string")
    return value


def normalize_git_sha(value: str, label: str = "candidate_sha") -> str:
    if not isinstance(value, str):
        raise QualificationError(f"{label} must be a string")
    lowered = value.lower()
    if not SHA40_RE.fullmatch(lowered):
        raise QualificationError(f"{label} must be 40 hexadecimal characters")
    return lowered


def normalize_hex64(value: Any, label: str, allow_zero: bool = False) -> str:
    if not isinstance(value, str):
        raise QualificationError(f"{label} must be a string")
    raw = value[2:] if value.startswith(("0x", "0X")) else value
    lowered = raw.lower()
    if not HEX64_RE.fullmatch(lowered):
        raise QualificationError(f"{label} must be exact 64 hex characters (optional 0x prefix)")
    if not allow_zero and lowered == "0" * 64:
        raise QualificationError(f"{label} must be nonzero")
    return lowered


def validate_slot(slot: int) -> int:
    if not is_strict_int(slot) or slot not in SCALING_SLOTS:
        raise QualificationError(f"slot must be in {SCALING_SLOTS}, got {slot!r}")
    return slot


def make_worker_name(run_id: str, attempt: str, slot: int) -> str:
    validate_positive_id(run_id, "workflow_run_id")
    validate_positive_id(attempt, "workflow_run_attempt")
    validate_slot(slot)
    name = f"qual-ci-{run_id}-{attempt}-{slot}"
    encoded = name.encode("utf-8")
    if not encoded or len(encoded) > WORKER_NAME_MAX_BYTES:
        raise QualificationError("worker_name exceeds protocol length limit")
    if any(ord(ch) < 32 or ord(ch) == 127 for ch in name):
        raise QualificationError("worker_name contains control characters")
    return name


def make_identity_artifact_name(run_id: str, attempt: str, slot: int) -> str:
    validate_positive_id(run_id, "workflow_run_id")
    validate_positive_id(attempt, "workflow_run_attempt")
    validate_slot(slot)
    return f"qual-worker-identity-r{run_id}-a{attempt}-slot-{slot}"


def make_worker_evidence_artifact_name(run_id: str, attempt: str, slot: int) -> str:
    validate_positive_id(run_id, "workflow_run_id")
    validate_positive_id(attempt, "workflow_run_attempt")
    validate_slot(slot)
    return f"qual-worker-evidence-r{run_id}-a{attempt}-slot-{slot}"


def make_controller_finished_artifact_name(run_id: str, attempt: str) -> str:
    validate_positive_id(run_id, "workflow_run_id")
    validate_positive_id(attempt, "workflow_run_attempt")
    return f"qual-controller-finished-r{run_id}-a{attempt}"


def make_controller_evidence_artifact_name(run_id: str, attempt: str) -> str:
    validate_positive_id(run_id, "workflow_run_id")
    validate_positive_id(attempt, "workflow_run_attempt")
    return f"qual-controller-evidence-r{run_id}-a{attempt}"


def make_candidate_artifact_name(sha: str, run_id: str, attempt: str) -> str:
    norm_sha = normalize_git_sha(sha)
    validate_positive_id(run_id, "candidate_run_id")
    validate_positive_id(attempt, "candidate_run_attempt")
    return f"arc-linux-candidate-{norm_sha}-{run_id}-{attempt}"


def make_strict_ssl_context() -> ssl.SSLContext:
    ctx = ssl.create_default_context()
    ctx.minimum_version = ssl.TLSVersion.TLSv1_2
    ctx.check_hostname = True
    ctx.verify_mode = ssl.CERT_REQUIRED
    return ctx


def build_strict_opener() -> urllib.request.OpenerDirector:
    return urllib.request.build_opener(
        urllib.request.ProxyHandler({}),
        urllib.request.HTTPSHandler(context=make_strict_ssl_context()),
        NoRedirectHandler(),
    )


def assert_pinned_https_url(url: str, expected_netloc: str) -> urllib.parse.ParseResult:
    parsed = urllib.parse.urlsplit(url)
    if parsed.scheme != "https":
        raise QualificationError(f"non-HTTPS URL rejected: {url}")
    if parsed.netloc != expected_netloc:
        raise QualificationError(f"unexpected host {parsed.netloc!r}; expected {expected_netloc!r}")
    if parsed.username is not None or parsed.password is not None:
        raise QualificationError("URL userinfo is forbidden")
    if not parsed.path.startswith("/"):
        raise QualificationError("URL path must start with '/'")
    return parsed


def read_bounded_bytes(stream: Any, max_bytes: int, label: str) -> bytes:
    data = stream.read(max_bytes + 1)
    if len(data) > max_bytes:
        raise QualificationError(f"{label} exceeded {max_bytes} byte limit")
    return data


def read_bounded_json_file(path: Path, max_bytes: int = MAX_JSON_FILE_BYTES) -> Any:
    if path.is_symlink() or not path.is_file():
        raise QualificationError(f"expected regular non-symlink file: {path}")
    size = path.stat().st_size
    if size > max_bytes:
        raise QualificationError(f"file {path} exceeded {max_bytes} bytes")
    with path.open("rb") as handle:
        raw = read_bounded_bytes(handle, max_bytes, str(path))
    try:
        return json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise QualificationError(f"invalid JSON in {path}: {exc}") from exc


def write_json_file(path: Path, payload: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    rendered = json.dumps(payload, indent=2, sort_keys=True) + "\n"
    path.write_text(rendered, encoding="utf-8")


def coordinator_request_once(
    method: str,
    path: str,
    timeout_seconds: float,
    json_body: dict[str, Any] | None = None,
    opener: urllib.request.OpenerDirector | None = None,
) -> tuple[int, Any, int]:
    if not path.startswith("/"):
        raise QualificationError(f"invalid coordinator path: {path!r}")
    url = f"{COORDINATOR_URL}{path}"
    assert_pinned_https_url(url, COORDINATOR_NETLOC)
    headers = {"Accept": "application/json"}
    body_bytes: bytes | None = None
    if json_body is not None:
        body_bytes = json.dumps(json_body, separators=(",", ":")).encode("utf-8")
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url=url, data=body_bytes, headers=headers, method=method)
    active_opener = opener if opener is not None else build_strict_opener()
    started = time.monotonic()
    try:
        with active_opener.open(req, timeout=timeout_seconds) as resp:
            status = int(resp.status)
            raw = read_bounded_bytes(resp, MAX_HTTP_BODY_BYTES, f"coordinator {method} {path}")
    except urllib.error.HTTPError as exc:
        elapsed_ms = int((time.monotonic() - started) * 1000)
        err_preview = read_error_preview(exc, 4096)
        raise QualificationError(
            f"coordinator {method} {path} returned HTTP {exc.code} after {elapsed_ms}ms: {err_preview}"
        ) from exc
    except (urllib.error.URLError, ssl.SSLError, TimeoutError, OSError) as exc:
        elapsed_ms = int((time.monotonic() - started) * 1000)
        raise QualificationError(
            f"coordinator {method} {path} transport error after {elapsed_ms}ms: {exc}"
        ) from exc
    elapsed_ms = int((time.monotonic() - started) * 1000)
    if status != 200:
        raise QualificationError(f"coordinator {method} {path} returned unexpected status {status}")
    try:
        parsed = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise QualificationError(f"coordinator {method} {path} returned invalid JSON: {exc}") from exc
    return status, parsed, elapsed_ms


def coordinator_get_json_with_retries(
    path: str,
    deadline_monotonic: float,
    max_attempts: int = 3,
    per_attempt_timeout: float = 15.0,
) -> Any:
    last_error: QualificationError | None = None
    for attempt in range(1, max_attempts + 1):
        remaining = deadline_monotonic - time.monotonic()
        if remaining <= 0:
            break
        timeout_now = min(per_attempt_timeout, remaining)
        try:
            _, payload, _ = coordinator_request_once("GET", path, timeout_now)
            return payload
        except QualificationError as exc:
            last_error = exc
            if attempt < max_attempts:
                sleep_for = min(1.5 * attempt, max(0.0, deadline_monotonic - time.monotonic()))
                if sleep_for > 0:
                    time.sleep(sleep_for)
    if last_error is not None:
        raise last_error
    raise QualificationError(f"deadline exceeded before GET {path}")


def github_api_get_json_conditional(
    path_and_query: str,
    token: str,
    etag: str | None = None,
    timeout_seconds: float = 15.0,
    opener: urllib.request.OpenerDirector | None = None,
) -> tuple[Any, str | None, bool]:
    if not token:
        raise QualificationError("GH_TOKEN is required for GitHub API calls")
    if not path_and_query.startswith("/"):
        raise QualificationError("GitHub API path must start with '/'")
    url = f"{GITHUB_API_ORIGIN}{path_and_query}"
    assert_pinned_https_url(url, GITHUB_API_NETLOC)
    headers = {
        "Accept": "application/vnd.github+json",
        "Authorization": f"Bearer {token}",
        "X-GitHub-Api-Version": "2022-11-28",
        "User-Agent": "arc-community-shard-qual",
    }
    if etag:
        headers["If-None-Match"] = etag
    req = urllib.request.Request(url=url, headers=headers, method="GET")
    active_opener = opener if opener is not None else build_strict_opener()
    try:
        with active_opener.open(req, timeout=timeout_seconds) as resp:
            status = int(resp.status)
            new_etag = resp.headers.get("ETag") if resp.headers is not None else None
            raw = read_bounded_bytes(resp, MAX_GH_API_BODY_BYTES, f"GitHub API {path_and_query}")
    except urllib.error.HTTPError as exc:
        if exc.code == 304:
            new_etag = exc.headers.get("ETag") if exc.headers is not None else None
            exc.close()
            return None, new_etag or etag, True
        retry_after = parse_retry_after_seconds(exc.headers)
        err_preview = read_error_preview(exc, 2048, secret=token)
        raise GitHubApiHttpError(
            int(exc.code),
            f"GitHub API GET {path_and_query} HTTP {exc.code}: {err_preview}",
            retry_after_seconds=retry_after,
        ) from exc
    except (urllib.error.URLError, ssl.SSLError, TimeoutError, OSError) as exc:
        raise QualificationError(
            f"GitHub API GET {path_and_query} error: {redact_secret(str(exc), token)}"
        ) from exc
    if status != 200:
        raise QualificationError(f"GitHub API GET {path_and_query} unexpected status {status}")
    try:
        return json.loads(raw.decode("utf-8")), new_etag, False
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise QualificationError(f"GitHub API GET {path_and_query} invalid JSON: {exc}") from exc


def github_api_get_json(
    path_and_query: str,
    token: str,
    timeout_seconds: float = 15.0,
    opener: urllib.request.OpenerDirector | None = None,
) -> Any:
    payload, _, _ = github_api_get_json_conditional(
        path_and_query,
        token,
        etag=None,
        timeout_seconds=timeout_seconds,
        opener=opener,
    )
    return payload


def list_run_artifacts_conditional(
    run_id: str,
    token: str,
    etag: str | None = None,
) -> tuple[dict[str, dict[str, Any]] | None, str | None, bool]:
    validate_positive_id(run_id, "run_id")
    found: dict[str, dict[str, Any]] = {}
    first_etag: str | None = etag
    single_page = True
    for page in range(1, MAX_GH_ARTIFACT_PAGES + 1):
        page_etag = etag if page == 1 else None
        payload, new_etag, not_modified = github_api_get_json_conditional(
            f"/repos/{EXPECTED_REPO}/actions/runs/{run_id}/artifacts?per_page=100&page={page}",
            token,
            etag=page_etag,
        )
        if page == 1:
            first_etag = new_etag
            if not_modified:
                return None, first_etag, True
        if not isinstance(payload, dict) or not isinstance(payload.get("artifacts"), list):
            raise QualificationError("malformed GitHub artifacts listing response")
        items = payload["artifacts"]
        for item in items:
            if not isinstance(item, dict) or item.get("expired") is not False:
                continue
            name = item.get("name")
            if not isinstance(name, str):
                continue
            if name in found:
                raise QualificationError(f"ambiguous duplicate active artifacts for {name}")
            found[name] = item
        if len(items) < 100:
            break
        single_page = False
    return found, (first_etag if single_page else None), False


def list_run_artifacts(run_id: str, token: str) -> dict[str, dict[str, Any]]:
    found, _, _ = list_run_artifacts_conditional(run_id, token, etag=None)
    return found if found is not None else {}


def find_unexpired_run_artifact(
    run_id: str,
    artifact_name: str,
    token: str,
    etag: str | None = None,
) -> tuple[dict[str, Any] | None, str | None, bool]:
    validate_positive_id(run_id, "run_id")
    if not SAFE_ARTIFACT_NAME_RE.fullmatch(artifact_name):
        raise QualificationError(f"unsafe artifact name: {artifact_name!r}")
    query = urllib.parse.urlencode({"name": artifact_name, "per_page": 100})
    payload, new_etag, not_modified = github_api_get_json_conditional(
        f"/repos/{EXPECTED_REPO}/actions/runs/{run_id}/artifacts?{query}",
        token,
        etag=etag,
    )
    if not_modified:
        return None, new_etag, True
    if not isinstance(payload, dict) or not isinstance(payload.get("artifacts"), list):
        raise QualificationError("malformed GitHub artifacts listing response")
    matches = [
        item
        for item in payload["artifacts"]
        if isinstance(item, dict) and item.get("name") == artifact_name and item.get("expired") is False
    ]
    if len(matches) > 1:
        raise QualificationError(f"ambiguous duplicate active artifacts for {artifact_name}")
    return (matches[0] if matches else None), new_etag, False


def inspect_tree_bounds(root_dir: Path, max_total_bytes: int, max_files: int = 64) -> int:
    if not root_dir.is_dir() or root_dir.is_symlink():
        raise QualificationError(f"invalid directory: {root_dir}")
    resolved_root = root_dir.resolve()
    total_bytes = 0
    file_count = 0
    for dirpath, dirnames, filenames in os.walk(root_dir, followlinks=False):
        current_dir = Path(dirpath)
        for dirname in dirnames:
            sub = current_dir / dirname
            if sub.is_symlink():
                raise QualificationError(f"symlink directory rejected: {sub}")
        for filename in filenames:
            file_path = current_dir / filename
            if file_path.is_symlink() or not file_path.is_file():
                raise QualificationError(f"non-regular file or symlink rejected: {file_path}")
            resolved = file_path.resolve()
            if not resolved.is_relative_to(resolved_root):
                raise QualificationError(f"path escape rejected: {file_path}")
            file_count += 1
            if file_count > max_files:
                raise QualificationError(f"extracted file count exceeded {max_files}")
            total_bytes += file_path.stat().st_size
            if total_bytes > max_total_bytes:
                raise QualificationError(f"extracted size exceeded {max_total_bytes} bytes")
    return total_bytes


def parse_artifact_sha256_digest(artifact_meta: dict[str, Any], label: str) -> str:
    raw_digest = artifact_meta.get("digest")
    if not isinstance(raw_digest, str) or not raw_digest.startswith("sha256:"):
        raise QualificationError(f"{label} missing required sha256 digest")
    return normalize_hex64(raw_digest[len("sha256:"):], f"{label}.digest")


def extract_validated_zip_archive(
    zip_path: Path,
    dest_dir: Path,
    max_uncompressed_bytes: int,
    max_files: int = 64,
) -> int:
    if dest_dir.exists():
        shutil.rmtree(dest_dir)
    dest_dir.mkdir(parents=True, exist_ok=True)
    resolved_dest = dest_dir.resolve()
    try:
        with zipfile.ZipFile(zip_path, "r") as zf:
            infos = zf.infolist()
            if not infos:
                raise QualificationError("artifact zip archive is empty")
            if len(infos) > max_files * 2:
                raise QualificationError(f"artifact zip entry count exceeded {max_files * 2}")
            file_entries: list[tuple[zipfile.ZipInfo, Path]] = []
            declared_total = 0
            for info in infos:
                raw_name = info.filename
                if not raw_name or "\x00" in raw_name or "\\" in raw_name or raw_name.startswith("/"):
                    raise QualificationError(f"unsafe zip entry path: {raw_name!r}")
                if info.flag_bits & 0x1:
                    raise QualificationError(f"encrypted zip entry rejected: {raw_name!r}")
                mode = (info.external_attr >> 16) & 0xFFFF
                file_type = stat.S_IFMT(mode)
                stripped = raw_name.rstrip("/")
                if not stripped:
                    raise QualificationError(f"unsafe empty zip entry path: {raw_name!r}")
                posix_path = PurePosixPath(stripped)
                if posix_path.is_absolute() or any(part in {"", ".", ".."} for part in posix_path.parts):
                    raise QualificationError(f"path traversal zip entry rejected: {raw_name!r}")
                if info.is_dir():
                    if file_type not in (0, stat.S_IFDIR):
                        raise QualificationError(f"non-directory mode on dir zip entry: {raw_name!r}")
                    continue
                if file_type not in (0, stat.S_IFREG):
                    raise QualificationError(f"non-regular zip entry rejected: {raw_name!r}")
                if info.file_size < 0:
                    raise QualificationError(f"negative zip entry size: {raw_name!r}")
                declared_total += info.file_size
                if declared_total > max_uncompressed_bytes:
                    raise QualificationError(
                        f"zip declared uncompressed size exceeded {max_uncompressed_bytes} bytes"
                    )
                target_path = dest_dir.joinpath(*posix_path.parts)
                resolved_target = target_path.resolve()
                if not resolved_target.is_relative_to(resolved_dest) or resolved_target == resolved_dest:
                    raise QualificationError(f"zip entry escapes destination: {raw_name!r}")
                file_entries.append((info, target_path))
            if not file_entries:
                raise QualificationError("artifact zip archive contains no regular files")
            if len(file_entries) > max_files:
                raise QualificationError(f"zip file count exceeded {max_files}")
            actual_total = 0
            seen_targets: set[Path] = set()
            for info, target_path in file_entries:
                if target_path in seen_targets:
                    raise QualificationError(f"duplicate zip entry path: {info.filename!r}")
                seen_targets.add(target_path)
                target_path.parent.mkdir(parents=True, exist_ok=True)
                with zf.open(info, "r") as src, target_path.open("wb") as dst:
                    while True:
                        chunk = src.read(1024 * 1024)
                        if not chunk:
                            break
                        actual_total += len(chunk)
                        if actual_total > max_uncompressed_bytes:
                            raise QualificationError(
                                f"zip extracted size exceeded {max_uncompressed_bytes} bytes"
                            )
                        dst.write(chunk)
    except (zipfile.BadZipFile, OSError) as exc:
        shutil.rmtree(dest_dir, ignore_errors=True)
        raise QualificationError(f"invalid artifact zip archive: {exc}") from exc
    except QualificationError:
        shutil.rmtree(dest_dir, ignore_errors=True)
        raise
    return inspect_tree_bounds(dest_dir, max_uncompressed_bytes, max_files=max_files)


def download_validated_artifact_by_id(
    run_id: str,
    artifact_name: str,
    artifact_meta: dict[str, Any],
    dest_dir: Path,
    max_bytes: int,
    token: str,
    timeout_seconds: int = 120,
    max_files: int = 64,
) -> None:
    validate_positive_id(run_id, "run_id")
    if not token:
        raise QualificationError("GH_TOKEN is required for artifact download")
    if not SAFE_ARTIFACT_NAME_RE.fullmatch(artifact_name):
        raise QualificationError(f"unsafe artifact name: {artifact_name!r}")
    if artifact_meta.get("name") != artifact_name:
        raise QualificationError("artifact metadata name mismatch")
    if artifact_meta.get("expired") is not False:
        raise QualificationError(f"artifact {artifact_name} is expired")
    artifact_id = artifact_meta.get("id")
    if not is_strict_positive_int(artifact_id):
        raise QualificationError(f"artifact {artifact_name} missing valid positive id")
    wf_run = artifact_meta.get("workflow_run")
    if isinstance(wf_run, dict) and "id" in wf_run and wf_run.get("id") != int(run_id):
        raise QualificationError(f"artifact {artifact_name} workflow_run.id mismatch")
    size_bytes = artifact_meta.get("size_in_bytes")
    if not is_strict_positive_int(size_bytes) or size_bytes > max_bytes:
        raise QualificationError(
            f"artifact {artifact_name} size {size_bytes!r} exceeds bound {max_bytes}"
        )
    expected_zip_sha256 = parse_artifact_sha256_digest(artifact_meta, artifact_name)
    dest_dir.parent.mkdir(parents=True, exist_ok=True)
    gh_env = minimal_host_env({"GH_TOKEN": token})
    endpoint = f"/repos/{EXPECTED_REPO}/actions/artifacts/{artifact_id}/zip"
    with tempfile.TemporaryDirectory(prefix=f"arc-art-{artifact_id}-", dir=str(dest_dir.parent)) as tmp_dir:
        zip_path = Path(tmp_dir) / f"artifact-{artifact_id}.zip"
        timed_out = threading.Event()
        proc: subprocess.Popen[bytes] | None = None
        stderr_thread: threading.Thread | None = None
        stderr_state: dict[str, Any] = {"buf": bytearray(), "truncated": False, "total_bytes": 0}
        timer: threading.Timer | None = None
        try:
            proc = subprocess.Popen(
                [
                    "gh",
                    "api",
                    "-H",
                    "Accept: application/vnd.github+json",
                    "-H",
                    "X-GitHub-Api-Version: 2022-11-28",
                    endpoint,
                ],
                env=gh_env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )

            def kill_on_timeout() -> None:
                timed_out.set()
                if proc is not None and proc.poll() is None:
                    try:
                        proc.kill()
                    except OSError:
                        return

            timer = threading.Timer(float(timeout_seconds), kill_on_timeout)
            timer.daemon = True
            timer.start()
            stderr_thread, stderr_state = start_bounded_pipe_reader(proc.stderr, 4096)
            if proc.stdout is None:
                raise QualificationError(f"gh api stdout missing for {artifact_name}")
            digest = hashlib.sha256()
            total_zip_bytes = 0
            with zip_path.open("wb") as out_handle:
                while True:
                    chunk = proc.stdout.read(1024 * 1024)
                    if not chunk:
                        break
                    total_zip_bytes += len(chunk)
                    if total_zip_bytes > max_bytes:
                        proc.kill()
                        raise QualificationError(
                            f"artifact {artifact_name} raw zip exceeded {max_bytes} byte limit"
                        )
                    digest.update(chunk)
                    out_handle.write(chunk)
            rc = proc.wait(timeout=15)
        except (subprocess.SubprocessError, OSError) as exc:
            raise QualificationError(
                f"gh api download failed for {artifact_name} (id={artifact_id}): {redact_secret(str(exc), token)}"
            ) from exc
        finally:
            if timer is not None:
                timer.cancel()
            if proc is not None and proc.poll() is None:
                try:
                    proc.kill()
                    proc.wait(timeout=5)
                except (subprocess.SubprocessError, OSError):
                    pass
            if stderr_thread is not None:
                stderr_thread.join(timeout=5)
        if timed_out.is_set():
            raise QualificationError(f"gh api download timed out for {artifact_name} (id={artifact_id})")
        if rc != 0:
            err_text = redact_secret(bytes(stderr_state["buf"]).decode("utf-8", errors="replace"), token)
            raise QualificationError(
                f"gh api exited {rc} for {artifact_name} (id={artifact_id}): {err_text[:512]}"
            )
        if total_zip_bytes != size_bytes:
            raise QualificationError(
                f"artifact {artifact_name} zip size mismatch: got {total_zip_bytes}, expected {size_bytes}"
            )
        actual_sha256 = digest.hexdigest()
        if actual_sha256 != expected_zip_sha256:
            raise QualificationError(
                f"artifact {artifact_name} zip SHA-256 mismatch: got {actual_sha256}, expected {expected_zip_sha256}"
            )
        extract_validated_zip_archive(zip_path, dest_dir, max_bytes, max_files=max_files)


def validate_readiness_payload(
    payload: Any,
    expect_ready: bool,
) -> dict[str, Any]:
    if not isinstance(payload, dict):
        raise QualificationError("dispatch_readiness response must be a JSON object")
    actual_keys = set(payload.keys())
    if actual_keys != READINESS_REQUIRED_KEYS:
        missing = sorted(READINESS_REQUIRED_KEYS - actual_keys)
        extra = sorted(actual_keys - READINESS_REQUIRED_KEYS)
        raise QualificationError(f"dispatch_readiness keys mismatch: missing={missing} extra={extra}")
    if payload["schema"] != EXPECTED_READINESS_SCHEMA:
        raise QualificationError(
            f"dispatch_readiness schema={payload['schema']!r}, expected {EXPECTED_READINESS_SCHEMA!r}"
        )
    if payload["mutation_free_observation"] is not True:
        raise QualificationError("dispatch_readiness mutation_free_observation must be true")
    if payload["enabled"] is not True:
        raise QualificationError("dispatch_readiness enabled must be true")
    if not is_strict_int(payload["protocol_version"]) or payload["protocol_version"] != EXPECTED_PROTOCOL_VERSION:
        raise QualificationError("dispatch_readiness protocol_version mismatch")
    if payload["verified_dispatch_enabled"] is not True:
        raise QualificationError("dispatch_readiness verified_dispatch_enabled must be true")
    if payload["sharded_pipeline_ready"] is not expect_ready:
        raise QualificationError(
            f"dispatch_readiness sharded_pipeline_ready={payload['sharded_pipeline_ready']!r}, expected {expect_ready!r}"
        )
    if payload["safe_to_dispatch"] is not expect_ready:
        raise QualificationError(
            f"dispatch_readiness safe_to_dispatch={payload['safe_to_dispatch']!r}, expected {expect_ready!r}"
        )
    if payload["rewards_enabled"] is not False:
        raise QualificationError("dispatch_readiness rewards_enabled must be false")
    if not isinstance(payload["chat_template_supported"], bool):
        raise QualificationError("dispatch_readiness chat_template_supported must be boolean")
    if normalize_hex64(payload["coordinator_id"], "coordinator_id") != COORDINATOR_ID:
        raise QualificationError("dispatch_readiness coordinator_id pin mismatch")
    if normalize_hex64(payload["transaction_domain"], "transaction_domain") != TRANSACTION_DOMAIN:
        raise QualificationError("dispatch_readiness transaction_domain pin mismatch")
    if normalize_hex64(payload["model_id"], "model_id") != EXPECTED_MODEL_ID:
        raise QualificationError("dispatch_readiness model_id pin mismatch")
    if payload["execution_profile"] != CANONICAL_EXECUTION_PROFILE:
        raise QualificationError("dispatch_readiness execution_profile pin mismatch")
    min_prompt = len(DISPATCH_PAYLOAD["input"].encode("utf-8"))
    if not is_strict_positive_int(payload["max_prompt_bytes"]) or payload["max_prompt_bytes"] < min_prompt:
        raise QualificationError("dispatch_readiness max_prompt_bytes too small")
    if not is_strict_nonneg_int(payload["chat_template_overhead_bytes"]):
        raise QualificationError("dispatch_readiness chat_template_overhead_bytes invalid")
    if not is_strict_positive_int(payload["max_tokens_cap"]) or payload["max_tokens_cap"] < DISPATCH_PAYLOAD["max_tokens"]:
        raise QualificationError("dispatch_readiness max_tokens_cap too small")
    return payload


def validate_shard_status_payload(
    payload: Any,
    expected_ready_workers: int,
    expected_provisioning_workers: int,
    expected_active_sessions: int,
    expected_tile_ready: int,
    expected_tile_provisioning: int,
) -> dict[str, Any]:
    if not isinstance(payload, dict):
        raise QualificationError("shards/status response must be a JSON object")
    if payload.get("enabled") is not True:
        raise QualificationError("shards/status enabled must be true")
    if not is_strict_int(payload.get("protocol_version")) or payload["protocol_version"] != EXPECTED_PROTOCOL_VERSION:
        raise QualificationError("shards/status protocol_version mismatch")
    if payload.get("verified_dispatch_enabled") is not True:
        raise QualificationError("shards/status verified_dispatch_enabled must be true")
    if payload.get("rewards_enabled") is not False:
        raise QualificationError("shards/status rewards_enabled must be false")
    status = payload.get("status")
    if not isinstance(status, dict):
        raise QualificationError("shards/status missing status object")
    ready_workers = status.get("ready_workers")
    provisioning_workers = status.get("provisioning_workers")
    active_sessions = status.get("active_sessions")
    if not is_strict_int(ready_workers) or ready_workers != expected_ready_workers:
        raise QualificationError(
            f"shards/status ready_workers={ready_workers!r}, expected {expected_ready_workers}"
        )
    if not is_strict_int(provisioning_workers) or provisioning_workers != expected_provisioning_workers:
        raise QualificationError(
            f"shards/status provisioning_workers={provisioning_workers!r}, expected {expected_provisioning_workers}"
        )
    if not is_strict_int(active_sessions) or active_sessions != expected_active_sessions:
        raise QualificationError(
            f"shards/status active_sessions={active_sessions!r}, expected {expected_active_sessions}"
        )
    tiles = status.get("tiles")
    if not isinstance(tiles, list) or len(tiles) != len(EXPECTED_SIX_RANGES):
        raise QualificationError("shards/status tiles must contain exact 6 tile entries")
    parsed_tiles: list[tuple[int, int, int, int]] = []
    for tile in tiles:
        if not isinstance(tile, dict):
            raise QualificationError("shards/status tile entry must be object")
        tile_range = tile.get("range")
        if not isinstance(tile_range, dict):
            raise QualificationError("shards/status tile missing range")
        start = tile_range.get("start_layer")
        end = tile_range.get("end_layer")
        t_ready = tile.get("ready_workers")
        t_prov = tile.get("provisioning_workers")
        if not (is_strict_nonneg_int(start) and is_strict_positive_int(end) and start < end):
            raise QualificationError(f"shards/status invalid tile range: {tile_range!r}")
        if not (is_strict_nonneg_int(t_ready) and is_strict_nonneg_int(t_prov)):
            raise QualificationError("shards/status invalid tile worker counts")
        parsed_tiles.append((start, end, t_ready, t_prov))
    parsed_tiles.sort(key=lambda item: (item[0], item[1]))
    ranges = tuple((item[0], item[1]) for item in parsed_tiles)
    if ranges != EXPECTED_SIX_RANGES:
        raise QualificationError(f"shards/status tile ranges {ranges} != expected {EXPECTED_SIX_RANGES}")
    for start, end, t_ready, t_prov in parsed_tiles:
        if t_ready != expected_tile_ready or t_prov != expected_tile_provisioning:
            raise QualificationError(
                f"tile [{start},{end}) ready={t_ready} provisioning={t_prov}, "
                f"expected ({expected_tile_ready}, {expected_tile_provisioning})"
            )
    return payload


def assert_status_no_external_interference(payload: Any, max_workers: int = 6) -> tuple[int, int, int]:
    if not isinstance(payload, dict) or not isinstance(payload.get("status"), dict):
        raise QualificationError("shards/status malformed during readiness poll")
    if payload.get("enabled") is not True or payload.get("verified_dispatch_enabled") is not True:
        raise QualificationError("coordinator enabled/verified_dispatch_enabled drifted false")
    if payload.get("rewards_enabled") is not False:
        raise QualificationError("coordinator rewards_enabled drifted true")
    if not is_strict_int(payload.get("protocol_version")) or payload["protocol_version"] != EXPECTED_PROTOCOL_VERSION:
        raise QualificationError("coordinator protocol_version drifted")
    status = payload["status"]
    ready = status.get("ready_workers")
    prov = status.get("provisioning_workers")
    sessions = status.get("active_sessions")
    if not (is_strict_nonneg_int(ready) and is_strict_nonneg_int(prov) and is_strict_nonneg_int(sessions)):
        raise QualificationError("shards/status worker/session counts invalid")
    if ready + prov > max_workers:
        raise QualificationError(
            f"unexpected extra workers on coordinator: ready={ready} provisioning={prov} > {max_workers}"
        )
    if sessions != 0:
        raise QualificationError(f"unexpected active sessions on coordinator: {sessions}")
    return ready, prov, sessions


def extract_final_json_object(stdout_text: str) -> dict[str, Any]:
    trimmed = stdout_text.rstrip()
    if not trimmed.endswith("}"):
        raise QualificationError("stdout does not terminate with a JSON object")
    candidate_starts: list[int] = []
    offset = 0
    for line in trimmed.splitlines(keepends=True):
        if line.lstrip().startswith("{"):
            brace_pos = offset + line.find("{")
            candidate_starts.append(brace_pos)
        offset += len(line)
    if not candidate_starts:
        raise QualificationError("no JSON object start found in stdout")
    decoder = json.JSONDecoder()
    for start_idx in reversed(candidate_starts[-128:]):
        try:
            obj, end_idx = decoder.raw_decode(trimmed, start_idx)
        except json.JSONDecodeError:
            continue
        if end_idx == len(trimmed) and isinstance(obj, dict):
            return obj
    raise QualificationError("failed to parse exact final valid JSON object from stdout")


def validate_exit_report(
    report: Any,
    expected_worker_id: str,
    expected_worker_name: str,
) -> dict[str, Any]:
    if not isinstance(report, dict):
        raise QualificationError("exit report must be a JSON object")
    if set(report.keys()) != EXIT_REPORT_TOP_KEYS:
        missing = sorted(EXIT_REPORT_TOP_KEYS - set(report.keys()))
        extra = sorted(set(report.keys()) - EXIT_REPORT_TOP_KEYS)
        raise QualificationError(f"exit report keys mismatch: missing={missing} extra={extra}")
    if normalize_hex64(report["worker_id"], "exit.worker_id") != normalize_hex64(expected_worker_id, "expected_worker_id"):
        raise QualificationError("exit report worker_id mismatch")
    if report["worker_name"] != expected_worker_name:
        raise QualificationError("exit report worker_name mismatch")
    if report["coordinator_url"] != COORDINATOR_URL:
        raise QualificationError("exit report coordinator_url mismatch")
    if normalize_hex64(report["coordinator_id"], "exit.coordinator_id") != COORDINATOR_ID:
        raise QualificationError("exit report coordinator_id mismatch")
    if normalize_hex64(report["transaction_domain"], "exit.transaction_domain") != TRANSACTION_DOMAIN:
        raise QualificationError("exit report transaction_domain mismatch")
    if normalize_hex64(report["model_id"], "exit.model_id") != EXPECTED_MODEL_ID:
        raise QualificationError("exit report model_id mismatch")
    if not isinstance(report["model_name"], str) or not report["model_name"].strip():
        raise QualificationError("exit report model_name must be non-empty")
    if not is_strict_int(report["admitted_max_resident_layers"]) or report["admitted_max_resident_layers"] != WORKER_MAX_RESIDENT_LAYERS:
        raise QualificationError("exit report admitted_max_resident_layers mismatch")
    if not is_strict_int(report["threads"]) or report["threads"] != WORKER_THREADS:
        raise QualificationError("exit report threads mismatch")
    summary = report["summary"]
    if not isinstance(summary, dict) or set(summary.keys()) != EXIT_REPORT_SUMMARY_KEYS:
        raise QualificationError("exit report summary keys mismatch")
    for field in EXIT_REPORT_SUMMARY_KEYS - {"clean_release_sent"}:
        if not is_strict_nonneg_int(summary[field]):
            raise QualificationError(f"exit report summary.{field} must be non-negative int")
    if not isinstance(summary["clean_release_sent"], bool):
        raise QualificationError("exit report summary.clean_release_sent must be bool")
    return report


def extract_and_validate_exit_report(
    stdout_text: str,
    expected_worker_id: str,
    expected_worker_name: str,
) -> dict[str, Any]:
    obj = extract_final_json_object(stdout_text)
    return validate_exit_report(obj, expected_worker_id, expected_worker_name)


def validate_check_only_report(report: Any, expected_worker_name: str) -> dict[str, Any]:
    if not isinstance(report, dict):
        raise QualificationError("check-only report must be JSON object")
    if report.get("worker_name") != expected_worker_name:
        raise QualificationError("check-only worker_name mismatch")
    if report.get("coordinator_url") != COORDINATOR_URL:
        raise QualificationError("check-only coordinator_url mismatch")
    if normalize_hex64(report.get("coordinator_id"), "check_only.coordinator_id") != COORDINATOR_ID:
        raise QualificationError("check-only coordinator_id mismatch")
    if normalize_hex64(report.get("transaction_domain"), "check_only.transaction_domain") != TRANSACTION_DOMAIN:
        raise QualificationError("check-only transaction_domain mismatch")
    if normalize_hex64(report.get("model_id"), "check_only.model_id") != EXPECTED_MODEL_ID:
        raise QualificationError("check-only model_id mismatch")
    if report.get("execution_profile") != CANONICAL_EXECUTION_PROFILE:
        raise QualificationError("check-only execution_profile mismatch")
    if not is_strict_int(report.get("n_layers")) or report["n_layers"] != EXPECTED_TOTAL_LAYERS:
        raise QualificationError("check-only n_layers mismatch")
    if not is_strict_int(report.get("threads")) or report["threads"] != WORKER_THREADS:
        raise QualificationError("check-only threads mismatch")
    admission = report.get("admission")
    if not isinstance(admission, dict):
        raise QualificationError("check-only missing admission object")
    admitted = admission.get("admitted_max_resident_layers")
    if not is_strict_int(admitted) or admitted != WORKER_MAX_RESIDENT_LAYERS:
        raise QualificationError(
            f"check-only admitted_max_resident_layers={admitted!r}, expected {WORKER_MAX_RESIDENT_LAYERS}"
        )
    return report


def validate_contiguous_ranges(
    ranges: list[tuple[int, int]],
    total_layers: int,
    max_tile_layers: int | None = None,
) -> list[tuple[int, int]]:
    if not ranges:
        raise QualificationError("ranges list must be non-empty")
    ordered = sorted(ranges, key=lambda pair: (pair[0], pair[1]))
    cursor = 0
    for start, end in ordered:
        if start != cursor or end <= start or end > total_layers:
            raise QualificationError(f"ranges {ordered} do not contiguously cover 0..{total_layers}")
        if max_tile_layers is not None and (end - start) > max_tile_layers:
            raise QualificationError(f"range [{start},{end}) exceeds max tile size {max_tile_layers}")
        cursor = end
    if cursor != total_layers:
        raise QualificationError(f"ranges end at {cursor}, expected {total_layers}")
    return ordered


def validate_dispatch_response(
    response: Any,
    expected_worker_ids: set[str],
) -> dict[str, Any]:
    if not isinstance(response, dict):
        raise QualificationError("dispatch response must be JSON object")
    if set(response.keys()) != DISPATCH_RESPONSE_KEYS:
        missing = sorted(DISPATCH_RESPONSE_KEYS - set(response.keys()))
        extra = sorted(set(response.keys()) - DISPATCH_RESPONSE_KEYS)
        raise QualificationError(f"dispatch response keys mismatch: missing={missing} extra={extra}")
    if not is_strict_int(response["protocol_version"]) or response["protocol_version"] != EXPECTED_PROTOCOL_VERSION:
        raise QualificationError("dispatch response protocol_version mismatch")
    session_id = response["session_id"]
    if not isinstance(session_id, dict) or set(session_id.keys()) != {"boot_nonce", "seq"}:
        raise QualificationError("dispatch response invalid session_id")
    session_boot_nonce = normalize_hex64(session_id["boot_nonce"], "session_id.boot_nonce")
    if not is_strict_nonneg_int(session_id["seq"]):
        raise QualificationError("dispatch response session_id.seq invalid")
    if not isinstance(response["model_name"], str) or not response["model_name"].strip():
        raise QualificationError("dispatch response model_name invalid")
    if normalize_hex64(response["model_id"], "response.model_id") != EXPECTED_MODEL_ID:
        raise QualificationError("dispatch response model_id mismatch")
    if response["execution_profile"] != CANONICAL_EXECUTION_PROFILE:
        raise QualificationError("dispatch response execution_profile mismatch")
    tokens = response["generated_tokens"]
    if not isinstance(tokens, list) or not (1 <= len(tokens) <= DISPATCH_PAYLOAD["max_tokens"]):
        raise QualificationError("dispatch response generated_tokens must have 1..=2 items")
    if not all(is_strict_nonneg_int(tok) for tok in tokens):
        raise QualificationError("dispatch response generated_tokens must contain non-negative ints")
    tokens_generated = response["tokens_generated"]
    if not is_strict_int(tokens_generated) or tokens_generated != len(tokens):
        raise QualificationError("dispatch response tokens_generated mismatch")
    stopped_by_eos = response["stopped_by_eos"]
    if not isinstance(stopped_by_eos, bool):
        raise QualificationError("dispatch response stopped_by_eos must be bool")
    if not stopped_by_eos and tokens_generated != DISPATCH_PAYLOAD["max_tokens"]:
        raise QualificationError("dispatch response stopped early without EOS")
    if not isinstance(response["output"], str):
        raise QualificationError("dispatch response output must be string")
    output_hash = normalize_hex64(response["output_hash"], "response.output_hash")
    if not is_strict_nonneg_int(response["total_ms"]):
        raise QualificationError("dispatch response total_ms invalid")
    if response["participant_identity_basis"] != EXPECTED_PARTICIPANT_IDENTITY_BASIS:
        raise QualificationError(
            f"dispatch response participant_identity_basis={response['participant_identity_basis']!r}, "
            f"expected {EXPECTED_PARTICIPANT_IDENTITY_BASIS!r}"
        )
    if response["rewards_enabled"] is not False or response["reward_eligible"] is not False:
        raise QualificationError("dispatch response must have rewards_enabled=false and reward_eligible=false")
    participants = response["participants"]
    if not isinstance(participants, list) or len(participants) != len(EXPECTED_SLOTS):
        raise QualificationError("dispatch response must have exactly 6 participants")
    seen_ids: set[str] = set()
    seen_lease_seqs: set[int] = set()
    part_ranges: list[tuple[int, int]] = []
    for item in participants:
        if not isinstance(item, dict) or set(item.keys()) != PARTICIPANT_KEYS:
            raise QualificationError("dispatch response malformed participant entry")
        wid = normalize_hex64(item["worker_id"], "participant.worker_id")
        if wid in seen_ids:
            raise QualificationError(f"duplicate participant worker_id: {wid}")
        seen_ids.add(wid)
        lease_id = item["lease_id"]
        if not isinstance(lease_id, dict) or set(lease_id.keys()) != {"boot_nonce", "seq"}:
            raise QualificationError("participant lease_id malformed")
        lease_boot_nonce = normalize_hex64(lease_id["boot_nonce"], "participant.lease_id.boot_nonce")
        if lease_boot_nonce != session_boot_nonce:
            raise QualificationError("participant lease_id.boot_nonce mismatch with session_id")
        if not is_strict_positive_int(lease_id["seq"]):
            raise QualificationError("participant lease_id.seq invalid")
        if lease_id["seq"] in seen_lease_seqs:
            raise QualificationError(f"duplicate participant lease_id.seq: {lease_id['seq']}")
        seen_lease_seqs.add(lease_id["seq"])
        if not is_strict_positive_int(item["lease_epoch"]):
            raise QualificationError("participant lease_epoch invalid")
        rng = item["range"]
        if not isinstance(rng, dict) or set(rng.keys()) != {"start_layer", "end_layer"}:
            raise QualificationError("participant range malformed")
        start = rng["start_layer"]
        end = rng["end_layer"]
        if not (is_strict_nonneg_int(start) and is_strict_positive_int(end)):
            raise QualificationError("participant range bounds invalid")
        part_ranges.append((start, end))
    normalized_expected = {normalize_hex64(wid, "expected_worker_id") for wid in expected_worker_ids}
    if seen_ids != normalized_expected:
        raise QualificationError(
            f"participant worker_ids mismatch: got={sorted(seen_ids)} expected={sorted(normalized_expected)}"
        )
    ordered_part_ranges = validate_contiguous_ranges(
        part_ranges,
        EXPECTED_TOTAL_LAYERS,
        EXPECTED_MAX_TILE_LAYERS,
    )
    if tuple(ordered_part_ranges) != EXPECTED_SIX_RANGES:
        raise QualificationError(
            f"participant ranges {tuple(ordered_part_ranges)} != expected {EXPECTED_SIX_RANGES}"
        )
    verification = response["verification"]
    if not isinstance(verification, dict):
        raise QualificationError("dispatch response verification must be object")
    if not VERIFICATION_REQUIRED_KEYS <= set(verification) or set(verification) - VERIFICATION_REQUIRED_KEYS - VERIFICATION_OPTIONAL_KEYS:
        missing = sorted(VERIFICATION_REQUIRED_KEYS - set(verification.keys()))
        extra = sorted(set(verification.keys()) - VERIFICATION_REQUIRED_KEYS - VERIFICATION_OPTIONAL_KEYS)
        raise QualificationError(f"verification keys mismatch: missing={missing} extra={extra}")
    method = verification["method"]
    if method not in (VERIFICATION_METHOD_2_OF_3, VERIFICATION_METHOD_2_OF_2, VERIFICATION_METHOD_MIXED):
        raise QualificationError("verification method is not an authenticated two-signature quorum")
    if verification["profile_bound"] is not True:
        raise QualificationError("verification profile_bound must be true")
    if verification["quorum_verified"] is not True:
        raise QualificationError("verification quorum_verified must be true")
    if verification["execution_profile"] != CANONICAL_EXECUTION_PROFILE:
        raise QualificationError("verification execution_profile mismatch")
    if normalize_hex64(verification["output_hash"], "verification.output_hash") != output_hash:
        raise QualificationError("verification output_hash mismatch")
    if not is_strict_int(verification["tokens_generated"]) or verification["tokens_generated"] != tokens_generated:
        raise QualificationError("verification tokens_generated mismatch")
    if not is_strict_positive_int(verification["ranges"]) or verification["ranges"] > EXPECTED_TOTAL_LAYERS:
        raise QualificationError("verification ranges count invalid")
    if not is_strict_positive_int(verification["range_position_quorums"]) or verification["range_position_quorums"] % verification["ranges"] != 0 or verification["range_position_quorums"] < verification["ranges"] * tokens_generated:
        raise QualificationError("verification range_position_quorums invalid")
    if not is_strict_int(verification["signatures_required_per_quorum"]) or verification["signatures_required_per_quorum"] != 2:
        raise QualificationError("verification signatures_required_per_quorum must be 2")
    expected_replicas = 3 if method == VERIFICATION_METHOD_2_OF_3 else 2
    if not is_strict_int(verification["replicas_contacted_per_quorum"]) or verification["replicas_contacted_per_quorum"] != expected_replicas:
        raise QualificationError("verification replica count does not match its quorum method")
    if method == VERIFICATION_METHOD_2_OF_3:
        if "ranges_verified_2_of_2" in verification:
            raise QualificationError("all-three-replica verification must omit the two-replica range field")
        return response
    verified_ranges_raw = verification.get("ranges_verified_2_of_2")
    if not isinstance(verified_ranges_raw, list) or not verified_ranges_raw or len(verified_ranges_raw) > verification["ranges"]:
        raise QualificationError("verification two-replica ranges must be a nonempty bounded subset")
    if (len(verified_ranges_raw) == verification["ranges"]) != (method == VERIFICATION_METHOD_2_OF_2):
        raise QualificationError("verification two-replica range count does not match its quorum method")
    verified_ranges: list[tuple[int, int]] = []
    for pair in verified_ranges_raw:
        if not isinstance(pair, list) or len(pair) != 2:
            raise QualificationError("verification range entry must be [start, end]")
        if not (is_strict_nonneg_int(pair[0]) and is_strict_positive_int(pair[1])):
            raise QualificationError("verification range bounds invalid")
        verified_ranges.append((pair[0], pair[1]))
    ordered = sorted(verified_ranges)
    previous_end = 0
    for start, end in ordered:
        if start < previous_end or end <= start or end > EXPECTED_TOTAL_LAYERS:
            raise QualificationError("verification two-replica ranges overlap or exceed model bounds")
        previous_end = end
    if method == VERIFICATION_METHOD_2_OF_2:
        validate_contiguous_ranges(verified_ranges, EXPECTED_TOTAL_LAYERS, max_tile_layers=None)
    else:
        remaining_ranges = verification["ranges"] - len(ordered)
        uncovered_layers = EXPECTED_TOTAL_LAYERS - sum(end - start for start, end in ordered)
        previous_end = 0
        uncovered_segments = 0
        for start, end in ordered:
            uncovered_segments += int(start > previous_end)
            previous_end = end
        uncovered_segments += int(previous_end < EXPECTED_TOTAL_LAYERS)
        if uncovered_layers < remaining_ranges or not (0 < uncovered_segments <= remaining_ranges):
            raise QualificationError("mixed verification complement cannot fit the remaining three-replica ranges")
    return response


def validate_two_dispatch_responses(
    first: Any,
    second: Any,
    expected_worker_ids: set[str],
) -> None:
    resp1 = validate_dispatch_response(first, expected_worker_ids)
    resp2 = validate_dispatch_response(second, expected_worker_ids)
    if resp1["session_id"] == resp2["session_id"]:
        raise QualificationError("two sequential requests returned identical session_id")
    if resp1["generated_tokens"] != resp2["generated_tokens"]:
        raise QualificationError("generated_tokens differed across two identical requests")
    if resp1["tokens_generated"] != resp2["tokens_generated"]:
        raise QualificationError("tokens_generated differed across two identical requests")
    if resp1["output"] != resp2["output"]:
        raise QualificationError("decoded output differed across two identical requests")
    if normalize_hex64(resp1["output_hash"], "resp1.output_hash") != normalize_hex64(
        resp2["output_hash"], "resp2.output_hash"
    ):
        raise QualificationError("output_hash differed across two identical requests")
    if resp1["stopped_by_eos"] != resp2["stopped_by_eos"]:
        raise QualificationError("stopped_by_eos differed across two identical requests")


def validate_boot_id(value: Any) -> str:
    if not isinstance(value, str) or not re.fullmatch(
        r"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}", value
    ):
        raise QualificationError("machine boot_id must be a canonical nonzero Linux boot UUID")
    return value


def read_kernel_boot_id(path: Path = Path("/proc/sys/kernel/random/boot_id")) -> str:
    try:
        with path.open("rb") as stream:
            raw = stream.read(65)
    except OSError as exc:
        raise QualificationError(f"failed to read machine boot_id: {exc}") from exc
    if len(raw) > 64:
        raise QualificationError("machine boot_id exceeds the bounded UUID format")
    try:
        value = raw.decode("ascii")
    except UnicodeDecodeError as exc:
        raise QualificationError("machine boot_id must be ASCII") from exc
    return validate_boot_id(value.strip())


def validate_worker_identities(
    identities: list[dict[str, Any]],
    expected_run_id: str,
    expected_attempt: str,
    expected_candidate_run_id: str,
    expected_candidate_attempt: str,
    expected_candidate_sha: str,
    expected_slots: tuple[int, ...] = EXPECTED_SLOTS,
) -> dict[int, dict[str, Any]]:
    norm_sha = normalize_git_sha(expected_candidate_sha)
    if type(expected_slots) is not tuple or expected_slots not in (EXPECTED_SLOTS, SCALING_SLOTS):
        raise QualificationError("select exactly the six-worker or twelve-worker qualification cohort")
    if len(identities) != len(expected_slots):
        raise QualificationError(f"expected {len(expected_slots)} worker identities, got {len(identities)}")
    by_slot: dict[int, dict[str, Any]] = {}
    seen_worker_ids: set[str] = set()
    seen_runner_names: set[str] = set()
    seen_boot_ids: set[str] = set()
    for record in identities:
        if not isinstance(record, dict):
            raise QualificationError("worker identity record must be JSON object")
        if "secret_key" in record:
            raise QualificationError("secret_key field forbidden in public identity record")
        slot = validate_slot(record.get("slot"))
        if slot in by_slot:
            raise QualificationError(f"duplicate worker identity slot {slot}")
        if record.get("cohort_class") != COHORT_CLASS:
            raise QualificationError("worker identity cohort_class mismatch")
        if record.get("counts_toward_operator_target") is not False:
            raise QualificationError("worker identity counts_toward_operator_target must be false")
        if record.get("workflow_run_id") != expected_run_id:
            raise QualificationError("worker identity workflow_run_id mismatch")
        if record.get("workflow_run_attempt") != expected_attempt:
            raise QualificationError("worker identity workflow_run_attempt mismatch")
        if record.get("candidate_run_id") != expected_candidate_run_id:
            raise QualificationError("worker identity candidate_run_id mismatch")
        if record.get("candidate_run_attempt") != expected_candidate_attempt:
            raise QualificationError("worker identity candidate_run_attempt mismatch")
        if normalize_git_sha(record.get("candidate_sha", "")) != norm_sha:
            raise QualificationError("worker identity candidate_sha mismatch")
        expected_name = make_worker_name(expected_run_id, expected_attempt, slot)
        if record.get("worker_name") != expected_name:
            raise QualificationError(f"worker identity worker_name mismatch for slot {slot}")
        wid = normalize_hex64(record.get("worker_id"), f"slot{slot}.worker_id")
        if wid in seen_worker_ids:
            raise QualificationError(f"duplicate worker_id across slots: {wid}")
        seen_worker_ids.add(wid)
        runner_name = record.get("runner_name")
        hostname = record.get("hostname")
        if not isinstance(runner_name, str) or not runner_name.strip():
            raise QualificationError(f"missing runner_name for slot {slot}")
        if not isinstance(hostname, str) or not hostname.strip():
            raise QualificationError(f"missing hostname for slot {slot}")
        if runner_name in seen_runner_names:
            raise QualificationError(f"duplicate runner_name across slots: {runner_name}")
        boot_id = validate_boot_id(record.get("boot_id"))
        if boot_id in seen_boot_ids:
            raise QualificationError(f"duplicate machine boot_id across slots: {boot_id}")
        seen_runner_names.add(runner_name)
        seen_boot_ids.add(boot_id)
        normalized_copy = dict(record)
        normalized_copy["worker_id"] = wid
        normalized_copy["candidate_sha"] = norm_sha
        by_slot[slot] = normalized_copy
    if tuple(sorted(by_slot.keys())) != expected_slots:
        raise QualificationError(f"worker identities do not cover exact slots {expected_slots}")
    return by_slot


def verify_sha256_file(path: Path, expected_sha256: str) -> None:
    if path.is_symlink() or not path.is_file():
        raise QualificationError(f"expected regular file for checksum: {path}")
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
    actual = digest.hexdigest()
    if actual != expected_sha256.lower():
        raise QualificationError(f"SHA-256 mismatch for {path.name}: got {actual}, expected {expected_sha256}")


def purge_directory_contents(directory: Path) -> None:
    if not directory.exists() or directory.is_symlink() or not directory.is_dir():
        return
    for child in directory.iterdir():
        if child.is_dir() and not child.is_symlink():
            shutil.rmtree(child, ignore_errors=True)
        else:
            try:
                child.unlink()
            except OSError:
                continue


def extract_private_key_secrets(key_path: Path) -> tuple[bytes, ...]:
    if key_path.is_symlink() or not key_path.is_file():
        return ()
    try:
        with key_path.open("rb") as handle:
            raw = read_bounded_bytes(handle, 64 * 1024, str(key_path))
    except (OSError, QualificationError):
        return ()
    secrets: list[bytes] = []
    try:
        parsed = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        parsed = None
    if isinstance(parsed, dict):
        for key, value in parsed.items():
            if isinstance(key, str) and ("secret" in key.lower() or "private" in key.lower()):
                if isinstance(value, str) and len(value) >= 16:
                    secrets.append(value.encode("utf-8"))
    return tuple(secrets)


def verify_evidence_dir_no_secrets(
    evidence_dir: Path,
    extra_secret_values: tuple[bytes, ...] = (),
) -> None:
    try:
        inspect_tree_bounds(evidence_dir, MAX_EVIDENCE_DIR_BYTES, max_files=64)
        env_token = os.environ.get("GH_TOKEN", "").encode("utf-8")
        exact_secrets = tuple(val for val in (*extra_secret_values, env_token) if val)
        for dirpath, _, filenames in os.walk(evidence_dir, followlinks=False):
            current = Path(dirpath)
            for filename in filenames:
                lowered = filename.lower()
                if (
                    lowered == "worker-key.json"
                    or lowered.endswith(".gguf")
                    or lowered.endswith(".key")
                    or lowered.endswith(".log")
                ):
                    raise QualificationError(f"forbidden file in public evidence dir: {filename}")
                fpath = current / filename
                with fpath.open("rb") as handle:
                    content = read_bounded_bytes(handle, MAX_LOG_BYTES, str(fpath))
                content_lower = content.lower()
                for marker in FORBIDDEN_SECRET_MARKERS:
                    if marker in content_lower:
                        raise QualificationError(f"secret marker detected in public evidence file: {filename}")
                for secret_val in exact_secrets:
                    if secret_val in content:
                        raise QualificationError(f"live secret value detected in public evidence file: {filename}")
    except QualificationError:
        purge_directory_contents(evidence_dir)
        raise


EXPECTED_PROVENANCE_HOST = "x86_64-unknown-linux-gnu"
RUSTC_VERBOSE_LINE_RE = re.compile(r"^([A-Za-z][A-Za-z0-9 _-]*):\s*(.+)$")


def validate_provenance_text(
    text: str,
    candidate_sha: str,
    candidate_run_id: str,
    candidate_run_attempt: str,
) -> dict[str, str]:
    norm_sha = normalize_git_sha(candidate_sha)
    validate_positive_id(candidate_run_id, "candidate_run_id")
    validate_positive_id(candidate_run_attempt, "candidate_run_attempt")
    seen_pins: dict[str, str] = {}
    seen_unique_meta: set[str] = set()
    seen_toolchain_meta: dict[str, str] = {}
    in_toolchain_footer = False
    saw_cargo_version = False
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if not line:
            continue
        if saw_cargo_version:
            raise QualificationError(f"unexpected trailing PROVENANCE.txt line: {line!r}")
        if not in_toolchain_footer:
            if line.startswith("rustc "):
                in_toolchain_footer = True
                continue
            if "=" not in line:
                raise QualificationError(f"malformed PROVENANCE.txt line: {line!r}")
            key, val = line.split("=", 1)
            key = key.strip()
            val = val.strip()
            if not key:
                raise QualificationError(f"empty key in PROVENANCE.txt line: {line!r}")
            if key in REQUIRED_PROVENANCE_PIN_KEYS:
                if key in seen_pins:
                    raise QualificationError(f"duplicate provenance pin key: {key}")
                seen_pins[key] = val
            elif key in REPEATABLE_PROVENANCE_KEYS:
                continue
            else:
                if key in seen_unique_meta:
                    raise QualificationError(f"duplicate provenance metadata key: {key}")
                seen_unique_meta.add(key)
        else:
            if line.startswith("cargo "):
                saw_cargo_version = True
                continue
            if "=" in line:
                raise QualificationError(f"unexpected key=value line in toolchain footer: {line!r}")
            match = RUSTC_VERBOSE_LINE_RE.fullmatch(line)
            if match is None:
                raise QualificationError(f"malformed toolchain line in PROVENANCE.txt: {line!r}")
            meta_key = match.group(1).strip()
            meta_val = match.group(2).strip()
            if meta_key in seen_toolchain_meta:
                raise QualificationError(f"duplicate toolchain metadata key: {meta_key}")
            seen_toolchain_meta[meta_key] = meta_val
    missing = sorted(REQUIRED_PROVENANCE_PIN_KEYS - set(seen_pins.keys()))
    if missing:
        raise QualificationError(f"PROVENANCE.txt missing expected pins: {missing}")
    if seen_pins["repository"] != EXPECTED_REPO:
        raise QualificationError("PROVENANCE.txt repository pin mismatch")
    if normalize_git_sha(seen_pins["commit"], "provenance.commit") != norm_sha:
        raise QualificationError("PROVENANCE.txt commit pin mismatch")
    if seen_pins["workflow_run_id"] != candidate_run_id:
        raise QualificationError("PROVENANCE.txt workflow_run_id pin mismatch")
    if seen_pins["workflow_run_attempt"] != candidate_run_attempt:
        raise QualificationError("PROVENANCE.txt workflow_run_attempt pin mismatch")
    if not in_toolchain_footer or not saw_cargo_version:
        raise QualificationError("PROVENANCE.txt missing rustc/cargo toolchain footer")
    if seen_toolchain_meta.get("host") != EXPECTED_PROVENANCE_HOST:
        raise QualificationError(
            f"PROVENANCE.txt host={seen_toolchain_meta.get('host')!r}, expected {EXPECTED_PROVENANCE_HOST!r}"
        )
    return seen_pins


def validate_candidate_run_api(
    candidate_run_id: str,
    candidate_run_attempt: str,
    candidate_sha: str,
    token: str,
) -> tuple[dict[str, Any], str, dict[str, Any], str]:
    run_payload = github_api_get_json(
        f"/repos/{EXPECTED_REPO}/actions/runs/{candidate_run_id}/attempts/{candidate_run_attempt}",
        token,
    )
    if not isinstance(run_payload, dict):
        raise QualificationError("candidate run API response is not an object")
    if not is_strict_int(run_payload.get("id")) or run_payload.get("id") != int(candidate_run_id):
        raise QualificationError("candidate run id mismatch")
    if not is_strict_int(run_payload.get("run_attempt")) or run_payload.get("run_attempt") != int(candidate_run_attempt):
        raise QualificationError("candidate run_attempt mismatch")
    repo_info = run_payload.get("repository")
    if not isinstance(repo_info, dict) or repo_info.get("full_name") != EXPECTED_REPO:
        raise QualificationError("candidate run repository mismatch")
    head_repo = run_payload.get("head_repository")
    if not isinstance(head_repo, dict) or head_repo.get("full_name") != EXPECTED_REPO:
        raise QualificationError("candidate run head_repository mismatch")
    if run_payload.get("path") != EXPECTED_CANDIDATE_WORKFLOW_PATH:
        raise QualificationError(
            f"candidate workflow path {run_payload.get('path')!r} != {EXPECTED_CANDIDATE_WORKFLOW_PATH!r}"
        )
    if run_payload.get("event") not in ALLOWED_CANDIDATE_EVENTS:
        raise QualificationError(f"candidate run event {run_payload.get('event')!r} not allowed")
    if normalize_git_sha(run_payload.get("head_sha", "")) != candidate_sha:
        raise QualificationError("candidate run head_sha mismatch")
    if run_payload.get("status") != "completed" or run_payload.get("conclusion") != "success":
        raise QualificationError(
            f"candidate run not completed/success: status={run_payload.get('status')!r} "
            f"conclusion={run_payload.get('conclusion')!r}"
        )
    expected_artifact = make_candidate_artifact_name(candidate_sha, candidate_run_id, candidate_run_attempt)
    meta, _, _ = find_unexpired_run_artifact(candidate_run_id, expected_artifact, token)
    if not isinstance(meta, dict):
        raise QualificationError(f"candidate artifact {expected_artifact} not found")
    if not is_strict_positive_int(meta.get("id")):
        raise QualificationError(f"candidate artifact {expected_artifact} missing valid id")
    if not is_strict_positive_int(meta.get("size_in_bytes")) or meta["size_in_bytes"] > MAX_CANDIDATE_ARTIFACT_BYTES:
        raise QualificationError("candidate artifact size out of bounds")
    digest_hex = parse_artifact_sha256_digest(meta, expected_artifact)
    return run_payload, expected_artifact, meta, digest_hex


def validate_lifecycle_time_budgets() -> None:
    if not 0 < MODEL_DOWNLOAD_RETRY_WINDOW_SECONDS < MODEL_DOWNLOAD_ATTEMPT_SECONDS:
        raise QualificationError("keep the retry window positive and below a full download attempt")
    if MODEL_DOWNLOAD_ATTEMPT_SECONDS > MODEL_DOWNLOAD_TOTAL_SECONDS:
        raise QualificationError("increase the outer download deadline to allow one full attempt")
    if ARTIFACT_AND_CLEANUP_GRACE_SECONDS < 75:
        raise QualificationError("increase cleanup grace to cover both bounded systemd cleanup commands")
    download_bound = MODEL_DOWNLOAD_TOTAL_SECONDS + MODEL_DOWNLOAD_KILL_GRACE_SECONDS
    preparation_bound = download_bound + PREPARATION_GRACE_SECONDS
    if CONTROLLER_IDENTITY_TIMEOUT_SECONDS < preparation_bound:
        raise QualificationError("increase identity wait to cover model download, retries, and preparation")
    dispatch_bound = CONTROLLER_IDENTITY_TIMEOUT_SECONDS + CONTROLLER_READINESS_TIMEOUT_SECONDS + 2 * DISPATCH_TIMEOUT_SECONDS
    completion_bound = dispatch_bound + ARTIFACT_AND_CLEANUP_GRACE_SECONDS
    if DEFAULT_WORKER_SUPERVISOR_MAX_SECONDS < completion_bound + MAX_POLL_BACKOFF_SECONDS:
        raise QualificationError("increase worker lifetime to cover cohort readiness, both dispatches, and cleanup")
    if DEFAULT_WORKER_SUPERVISOR_MAX_SECONDS > MAX_WORKER_SUPERVISOR_SECONDS:
        raise QualificationError("worker lifetime exceeds the hard supervisor bound")
    worker_job_bound = preparation_bound + DEFAULT_WORKER_SUPERVISOR_MAX_SECONDS + RUNTIME_MAX_GRACE_SECONDS + ARTIFACT_AND_CLEANUP_GRACE_SECONDS
    if 60 * WORKER_JOB_TIMEOUT_MINUTES < worker_job_bound:
        raise QualificationError("increase worker job timeout to include download, systemd lifetime, and cleanup")
    if 60 * CONTROLLER_JOB_TIMEOUT_MINUTES < completion_bound:
        raise QualificationError("increase controller job timeout to include startup, dispatches, and cleanup")


def cmd_validate(args: argparse.Namespace) -> int:
    validate_lifecycle_time_budgets()
    token = os.environ.get("GH_TOKEN", "")
    candidate_run_id = validate_positive_id(args.candidate_run_id, "candidate_run_id")
    candidate_run_attempt = validate_positive_id(args.candidate_run_attempt, "candidate_run_attempt")
    candidate_sha = normalize_git_sha(args.candidate_sha)
    run_payload, expected_artifact, meta, digest_hex = validate_candidate_run_api(
        candidate_run_id,
        candidate_run_attempt,
        candidate_sha,
        token,
    )
    if args.candidate_source_dir:
        src_dir = Path(args.candidate_source_dir)
        proc = subprocess.run(
            ["git", "-C", str(src_dir), "rev-parse", "HEAD"],
            env=minimal_host_env(),
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        if proc.returncode != 0 or normalize_git_sha(proc.stdout.strip()) != candidate_sha:
            raise QualificationError("candidate source checkout HEAD does not match candidate_sha")
    deadline = time.monotonic() + 30.0
    readiness = validate_readiness_payload(
        coordinator_get_json_with_retries(READINESS_PATH, deadline),
        expect_ready=False,
    )
    shard_status = validate_shard_status_payload(
        coordinator_get_json_with_retries(SHARD_STATUS_PATH, deadline),
        expected_ready_workers=0,
        expected_provisioning_workers=0,
        expected_active_sessions=0,
        expected_tile_ready=0,
        expected_tile_provisioning=0,
    )
    report = {
        "schema": "arc.community_shard_qual.validate.v1",
        "validated_at_utc": utc_now_iso(),
        "repository": EXPECTED_REPO,
        "candidate_run_id": candidate_run_id,
        "candidate_run_attempt": candidate_run_attempt,
        "candidate_sha": candidate_sha,
        "candidate_workflow_path": EXPECTED_CANDIDATE_WORKFLOW_PATH,
        "candidate_event": run_payload["event"],
        "candidate_artifact_id": meta["id"],
        "candidate_artifact_name": expected_artifact,
        "candidate_artifact_digest": f"sha256:{digest_hex}",
        "candidate_artifact_size_bytes": meta["size_in_bytes"],
        "baseline_dispatch_readiness": readiness,
        "baseline_shard_status": shard_status,
    }
    out_dir = Path(args.output_dir)
    write_json_file(out_dir / "validate-report.json", report)
    verify_evidence_dir_no_secrets(out_dir)
    return 0


def cmd_check_resources(args: argparse.Namespace) -> int:
    mem_path = Path("/proc/meminfo")
    with mem_path.open("rb") as handle:
        raw_mem = read_bounded_bytes(handle, 64 * 1024, "/proc/meminfo").decode("utf-8")
    mem: dict[str, int] = {}
    for line in raw_mem.splitlines():
        if line.startswith(("MemTotal:", "MemAvailable:")):
            parts = line.split()
            mem[parts[0][:-1]] = int(parts[1]) * 1024
    disk = shutil.disk_usage(args.runner_temp)
    report = {
        "checked_at_utc": utc_now_iso(),
        "runner_name": os.environ.get("RUNNER_NAME", ""),
        "hostname": socket.gethostname(),
        "boot_id": read_kernel_boot_id(),
        "mem_bytes": mem,
        "disk_total_bytes": disk.total,
        "disk_free_bytes": disk.free,
    }
    out_dir = Path(args.output_dir)
    write_json_file(out_dir / "runner-resources.json", report)
    verify_evidence_dir_no_secrets(out_dir)
    if mem.get("MemAvailable", 0) < MIN_MEM_AVAILABLE_BYTES:
        raise QualificationError("runner MemAvailable below 12 GiB floor")
    if disk.free < MIN_DISK_FREE_BYTES:
        raise QualificationError("runner disk free space below 12 GiB floor")
    return 0


def cmd_download_and_verify_candidate(args: argparse.Namespace) -> int:
    token = os.environ.get("GH_TOKEN", "")
    candidate_run_id = validate_positive_id(args.candidate_run_id, "candidate_run_id")
    candidate_run_attempt = validate_positive_id(args.candidate_run_attempt, "candidate_run_attempt")
    candidate_sha = normalize_git_sha(args.candidate_sha)
    _, artifact_name, meta, _ = validate_candidate_run_api(
        candidate_run_id,
        candidate_run_attempt,
        candidate_sha,
        token,
    )
    dest_dir = Path(args.dest_dir)
    download_validated_artifact_by_id(
        candidate_run_id,
        artifact_name,
        meta,
        dest_dir,
        MAX_CANDIDATE_ARTIFACT_BYTES,
        token,
        timeout_seconds=180,
    )
    provenance_path = dest_dir / "PROVENANCE.txt"
    sums_path = dest_dir / "SHA256SUMS"
    for req_file in (provenance_path, sums_path):
        if req_file.is_symlink() or not req_file.is_file():
            raise QualificationError(f"missing candidate metadata file: {req_file.name}")
    with provenance_path.open("rb") as handle:
        prov_text = read_bounded_bytes(handle, 256 * 1024, "PROVENANCE.txt").decode("utf-8")
    validate_provenance_text(prov_text, candidate_sha, candidate_run_id, candidate_run_attempt)
    with sums_path.open("rb") as handle:
        sums_lines = read_bounded_bytes(handle, 256 * 1024, "SHA256SUMS").decode("utf-8").splitlines()
    verified_names: set[str] = set()
    for line in sums_lines:
        stripped = line.strip()
        if not stripped:
            continue
        parts = stripped.split(maxsplit=1)
        if len(parts) != 2:
            raise QualificationError(f"malformed SHA256SUMS line: {stripped!r}")
        expected_hex, rel_name = parts[0], parts[1].lstrip("*")
        if "/" in rel_name or "\\" in rel_name or rel_name in {".", ".."}:
            raise QualificationError(f"unsafe filename in SHA256SUMS: {rel_name!r}")
        if rel_name in verified_names:
            raise QualificationError(f"duplicate filename in SHA256SUMS: {rel_name!r}")
        if not HEX64_RE.fullmatch(expected_hex.lower()):
            raise QualificationError(f"invalid SHA-256 hex in SHA256SUMS for {rel_name}")
        target = dest_dir / rel_name
        verify_sha256_file(target, expected_hex)
        verified_names.add(rel_name)
    for binary_name in ("arc-node", "arc"):
        if binary_name not in verified_names:
            raise QualificationError(f"candidate SHA256SUMS missing required binary {binary_name}")
        bin_path = dest_dir / binary_name
        if bin_path.is_symlink() or not bin_path.is_file():
            raise QualificationError(f"candidate binary invalid: {binary_name}")
        os.chmod(bin_path, 0o755)
    return 0


def cmd_verify_gguf(args: argparse.Namespace) -> int:
    model_path = Path(args.model_path)
    if model_path.is_symlink() or not model_path.is_file():
        raise QualificationError("model_path must be a regular non-symlink file")
    actual_size = model_path.stat().st_size
    if actual_size != GGUF_SIZE_BYTES:
        raise QualificationError(f"GGUF size mismatch: got {actual_size}, expected {GGUF_SIZE_BYTES}")
    verify_sha256_file(model_path, GGUF_SHA256)
    return 0


def cmd_worker_preflight(args: argparse.Namespace) -> int:
    worker_name = make_worker_name(args.workflow_run_id, args.workflow_run_attempt, args.slot)
    arc_node_bin = Path(args.candidate_dir) / "arc-node"
    model_path = Path(args.model_path)
    if arc_node_bin.is_symlink() or not arc_node_bin.is_file():
        raise QualificationError("arc-node binary missing")
    clean_env = minimal_host_env({"RUST_LOG": "info"})
    try:
        proc = subprocess.run(
            [
                str(arc_node_bin),
                "community-shard-worker",
                "--coordinator-url",
                COORDINATOR_URL,
                "--coordinator-id",
                COORDINATOR_ID,
                "--transaction-domain",
                TRANSACTION_DOMAIN,
                "--expected-model-id",
                EXPECTED_MODEL_ID,
                "--model-path",
                str(model_path),
                "--worker-name",
                worker_name,
                "--max-working-set-bytes",
                str(WORKER_MAX_WORKING_SET_BYTES),
                "--max-kv-cache-bytes",
                str(WORKER_MAX_KV_CACHE_BYTES),
                "--max-resident-layers",
                str(WORKER_MAX_RESIDENT_LAYERS),
                "--threads",
                str(WORKER_THREADS),
                "--check-only",
            ],
            env=clean_env,
            capture_output=True,
            timeout=300,
            check=False,
        )
    except (subprocess.SubprocessError, OSError) as exc:
        raise QualificationError(f"check-only invocation failed: {exc}") from exc
    if len(proc.stdout) > MAX_LOG_BYTES or len(proc.stderr) > MAX_LOG_BYTES:
        raise QualificationError("check-only output exceeded 8 MiB bound")
    if proc.returncode != 0:
        err_text = proc.stderr.decode("utf-8", errors="replace")[:1024]
        raise QualificationError(f"check-only exited {proc.returncode}: {err_text}")
    report = validate_check_only_report(
        extract_final_json_object(proc.stdout.decode("utf-8")),
        worker_name,
    )
    evidence_dir = Path(args.evidence_dir)
    write_json_file(evidence_dir / "check-only-report.json", report)
    verify_evidence_dir_no_secrets(evidence_dir)
    return 0


def cmd_worker_create_identity(args: argparse.Namespace) -> int:
    slot = validate_slot(args.slot)
    run_id = validate_positive_id(args.workflow_run_id, "workflow_run_id")
    attempt = validate_positive_id(args.workflow_run_attempt, "workflow_run_attempt")
    cand_run_id = validate_positive_id(args.candidate_run_id, "candidate_run_id")
    cand_attempt = validate_positive_id(args.candidate_run_attempt, "candidate_run_attempt")
    cand_sha = normalize_git_sha(args.candidate_sha)
    worker_name = make_worker_name(run_id, attempt, slot)
    arc_bin = Path(args.candidate_dir) / "arc"
    if arc_bin.is_symlink() or not arc_bin.is_file():
        raise QualificationError("arc CLI binary missing")
    runner_name = os.environ.get("RUNNER_NAME", "")
    if not runner_name.strip():
        raise QualificationError("RUNNER_NAME environment variable must be non-empty")
    hostname = socket.gethostname()
    if not hostname.strip():
        raise QualificationError("hostname must be non-empty")
    boot_id = read_kernel_boot_id()
    key_path = Path(args.private_key_path)
    key_path.parent.mkdir(parents=True, exist_ok=True)
    os.chmod(key_path.parent, 0o700)
    if key_path.exists() or key_path.is_symlink():
        key_path.unlink()
    clean_env = minimal_host_env()
    try:
        gen_proc = subprocess.run(
            [str(arc_bin), "keygen", "--scheme", "ed25519", "--output", str(key_path)],
            env=clean_env,
            capture_output=True,
            timeout=30,
            check=False,
        )
    except (subprocess.SubprocessError, OSError) as exc:
        raise QualificationError(f"arc keygen failed: {exc}") from exc
    if gen_proc.returncode != 0:
        raise QualificationError(f"arc keygen exited {gen_proc.returncode}")
    if key_path.is_symlink() or not key_path.is_file():
        raise QualificationError("generated keyfile is not a regular file")
    os.chmod(key_path, 0o600)
    if stat.S_IMODE(key_path.stat().st_mode) != 0o600:
        raise QualificationError("private keyfile mode is not 0600")
    try:
        ver_proc = subprocess.run(
            [str(arc_bin), "keygen", "--verify-keyfile", str(key_path)],
            env=clean_env,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
    except (subprocess.SubprocessError, OSError) as exc:
        raise QualificationError(f"arc keygen --verify-keyfile failed: {exc}") from exc
    if ver_proc.returncode != 0:
        raise QualificationError(f"arc keygen --verify-keyfile exited {ver_proc.returncode}")
    raw_addr = ver_proc.stdout.strip()
    if not HEX64_RE.fullmatch(raw_addr) or raw_addr == "0" * 64:
        raise QualificationError("arc keygen --verify-keyfile did not print bare 64-hex address")
    identity = {
        "schema": "arc.community_shard_qual.worker_identity.v1",
        "cohort_class": COHORT_CLASS,
        "counts_toward_operator_target": False,
        "slot": slot,
        "worker_name": worker_name,
        "worker_id": raw_addr,
        "runner_name": runner_name,
        "hostname": hostname,
        "boot_id": boot_id,
        "workflow_run_id": run_id,
        "workflow_run_attempt": attempt,
        "candidate_run_id": cand_run_id,
        "candidate_run_attempt": cand_attempt,
        "candidate_sha": cand_sha,
        "created_at_utc": utc_now_iso(),
    }
    identity_dir = Path(args.identity_dir)
    write_json_file(identity_dir / "worker-identity.json", identity)
    verify_evidence_dir_no_secrets(identity_dir, extra_secret_values=extract_private_key_secrets(key_path))
    return 0


def query_systemd_memory_bytes(unit_name: str) -> tuple[int | None, int | None, str]:
    try:
        proc = subprocess.run(
            [
                "sudo",
                "-n",
                "systemctl",
                "show",
                "--no-pager",
                "--property=ActiveState,MemoryCurrent,MemoryPeak",
                "--",
                unit_name,
            ],
            env=minimal_host_env(),
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
    except (subprocess.SubprocessError, OSError):
        return None, None, "unknown"
    if proc.returncode != 0:
        return None, None, "unknown"
    mem_current: int | None = None
    mem_peak: int | None = None
    active_state = "unknown"
    for line in proc.stdout.splitlines():
        if "=" not in line:
            continue
        key, val = line.split("=", 1)
        if key == "ActiveState":
            active_state = val.strip()
        elif key == "MemoryCurrent" and val.isdigit():
            parsed = int(val)
            if 0 < parsed < (1 << 62):
                mem_current = parsed
        elif key == "MemoryPeak" and val.isdigit():
            parsed = int(val)
            if 0 < parsed < (1 << 62):
                mem_peak = parsed
    return mem_current, mem_peak, active_state


def start_bounded_pipe_reader(pipe: Any, limit: int) -> tuple[threading.Thread, dict[str, Any]]:
    state: dict[str, Any] = {"buf": bytearray(), "truncated": False, "total_bytes": 0}

    def reader() -> None:
        try:
            while True:
                chunk = pipe.read(16384)
                if not chunk:
                    break
                state["total_bytes"] += len(chunk)
                remaining = limit - len(state["buf"])
                if remaining > 0:
                    state["buf"].extend(chunk[:remaining])
                if len(chunk) > remaining:
                    state["truncated"] = True
        except OSError:
            return
        finally:
            try:
                pipe.close()
            except OSError:
                return

    thread = threading.Thread(target=reader)
    thread.start()
    return thread, state


def stop_owned_systemd_unit(unit_name: str) -> bool:
    try:
        proc = subprocess.run(
            ["sudo", "-n", "systemctl", "stop", "--", unit_name],
            env=minimal_host_env(),
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
        return proc.returncode == 0
    except (subprocess.SubprocessError, OSError):
        return False


def kill_owned_systemd_unit(unit_name: str) -> None:
    try:
        subprocess.run(
            ["sudo", "-n", "systemctl", "kill", "--signal=SIGKILL", "--", unit_name],
            env=minimal_host_env(),
            capture_output=True,
            text=True,
            timeout=15,
            check=False,
        )
    except (subprocess.SubprocessError, OSError):
        return


def terminate_unit_gracefully(
    unit_name: str,
    proc: subprocess.Popen[bytes],
    wait_seconds: float = 60.0,
) -> int:
    stop_deadline = time.monotonic() + wait_seconds
    stopped_ack = False
    while time.monotonic() < stop_deadline:
        polled = proc.poll()
        if polled is not None:
            return polled
        if not stopped_ack:
            stopped_ack = stop_owned_systemd_unit(unit_name)
        remaining = stop_deadline - time.monotonic()
        if remaining <= 0:
            break
        try:
            return proc.wait(timeout=min(1.0, remaining))
        except subprocess.TimeoutExpired:
            continue
    polled = proc.poll()
    if polled is not None:
        return polled
    if stopped_ack:
        try:
            return proc.wait(timeout=5.0)
        except subprocess.TimeoutExpired:
            pass
    kill_owned_systemd_unit(unit_name)
    proc.kill()
    try:
        return proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        return -9


def delete_private_key(key_path: Path) -> bool:
    try:
        if key_path.exists() or key_path.is_symlink():
            key_path.unlink()
        parent = key_path.parent
        if parent.is_dir() and parent.name == "private-worker-key":
            shutil.rmtree(parent, ignore_errors=True)
    except OSError:
        return False
    return not key_path.exists() and not key_path.is_symlink()


class SignalStopFlag:
    def __init__(self):
        self.stopped = False

    def set(self):
        self.stopped = True

    def is_set(self):
        return self.stopped

    def wait(self, timeout):
        deadline = time.monotonic() + timeout
        while not self.stopped:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            time.sleep(min(0.05, remaining))
        return self.stopped


def cmd_worker_run(args: argparse.Namespace) -> int:
    token = os.environ.get("GH_TOKEN", "")
    slot = validate_slot(args.slot)
    run_id = validate_positive_id(args.workflow_run_id, "workflow_run_id")
    attempt = validate_positive_id(args.workflow_run_attempt, "workflow_run_attempt")
    max_seconds = int(args.max_seconds)
    if not (60 <= max_seconds <= MAX_WORKER_SUPERVISOR_SECONDS):
        raise QualificationError(f"max_seconds must be within 60..={MAX_WORKER_SUPERVISOR_SECONDS}")
    runtime_max_sec = max_seconds + RUNTIME_MAX_GRACE_SECONDS
    identity_path = Path(args.identity_dir) / "worker-identity.json"
    identity = read_bounded_json_file(identity_path)
    if validate_boot_id(identity.get("boot_id")) != read_kernel_boot_id():
        raise QualificationError("worker identity belongs to a different machine boot")
    expected_worker_id = normalize_hex64(identity["worker_id"], "identity.worker_id")
    worker_name = make_worker_name(run_id, attempt, slot)
    if identity.get("worker_name") != worker_name:
        raise QualificationError("identity worker_name mismatch")
    key_path = Path(args.private_key_path)
    if key_path.is_symlink() or not key_path.is_file():
        raise QualificationError("private keyfile missing")
    if stat.S_IMODE(key_path.stat().st_mode) != 0o600:
        raise QualificationError("private keyfile must have mode 0600")
    key_secret_values = extract_private_key_secrets(key_path)
    arc_node_bin = Path(args.candidate_dir) / "arc-node"
    model_path = Path(args.model_path)
    evidence_dir = Path(args.evidence_dir)
    evidence_dir.mkdir(parents=True, exist_ok=True)
    work_dir = Path(args.work_dir)
    work_dir.mkdir(parents=True, exist_ok=True)
    unit_name = f"arc-qual-worker-{run_id}-{attempt}-s{slot}.service"
    if not SAFE_UNIT_RE.fullmatch(unit_name):
        raise QualificationError("invalid systemd unit name")
    finished_artifact = make_controller_finished_artifact_name(run_id, attempt)
    scrubbed_env = minimal_host_env()
    cmd = [
        "sudo",
        "-n",
        "systemd-run",
        "--unit",
        unit_name,
        "--wait",
        "--pipe",
        "--collect",
        "--quiet",
        "--property=MemoryMax=8G",
        "--property=MemorySwapMax=0",
        "--property=TasksMax=128",
        f"--property=RuntimeMaxSec={runtime_max_sec}s",
        "--property=CPUQuota=200%",
        "--property=TimeoutStopSec=45s",
        "--property=KillMode=control-group",
        f"--uid={os.getuid()}",
        f"--gid={os.getgid()}",
        f"--working-directory={work_dir.resolve()}",
        "--setenv=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        "--setenv=RUST_LOG=info",
        "--",
        str(arc_node_bin.resolve()),
        "community-shard-worker",
        "--coordinator-url",
        COORDINATOR_URL,
        "--coordinator-id",
        COORDINATOR_ID,
        "--transaction-domain",
        TRANSACTION_DOMAIN,
        "--expected-model-id",
        EXPECTED_MODEL_ID,
        "--model-path",
        str(model_path.resolve()),
        "--worker-key-file",
        str(key_path.resolve()),
        "--worker-name",
        worker_name,
        "--max-working-set-bytes",
        str(WORKER_MAX_WORKING_SET_BYTES),
        "--max-kv-cache-bytes",
        str(WORKER_MAX_KV_CACHE_BYTES),
        "--max-resident-layers",
        str(WORKER_MAX_RESIDENT_LAYERS),
        "--threads",
        str(WORKER_THREADS),
    ]
    started_at_utc = utc_now_iso()
    started_mono = time.monotonic()
    deadline = started_mono + max_seconds
    max_mem_current: int | None = None
    max_mem_peak: int | None = None
    stop_reason = "exited_early"
    exit_code: int | None = None
    proc: subprocess.Popen[bytes] | None = None
    stdout_thread: threading.Thread | None = None
    stderr_thread: threading.Thread | None = None
    stdout_state: dict[str, Any] = {"buf": bytearray(), "truncated": False, "total_bytes": 0}
    stderr_state: dict[str, Any] = {"buf": bytearray(), "truncated": False, "total_bytes": 0}
    supervisor_error: str | None = None
    poll_etag: str | None = None
    poll_attempts = 0
    poll_not_modified_count = 0
    poll_error_count = 0
    consecutive_poll_errors = 0
    last_poll_error: str | None = None
    stop_event = SignalStopFlag()
    received_signal: dict[str, int | None] = {"signum": None}
    install_signals = threading.current_thread() is threading.main_thread()
    prev_sigterm: Any = None
    prev_sigint: Any = None

    def handle_signal(signum: int, _frame: Any) -> None:
        if received_signal["signum"] is None:
            received_signal["signum"] = signum
        stop_event.set()

    try:
        if install_signals:
            prev_sigterm = signal.signal(signal.SIGTERM, handle_signal)
            prev_sigint = signal.signal(signal.SIGINT, handle_signal)
        pre_spawn_finished = False
        try:
            poll_attempts += 1
            pre_meta, poll_etag, pre_not_modified = find_unexpired_run_artifact(
                run_id,
                finished_artifact,
                token,
                etag=poll_etag,
            )
            if pre_not_modified:
                poll_not_modified_count += 1
            elif pre_meta is not None:
                pre_spawn_finished = True
                stop_reason = "controller_already_finished"
        except GitHubApiHttpError as exc:
            poll_error_count += 1
            consecutive_poll_errors += 1
            last_poll_error = redact_secret(str(exc), token)
        except QualificationError as exc:
            poll_error_count += 1
            consecutive_poll_errors += 1
            last_poll_error = redact_secret(str(exc), token)
        if stop_event.is_set() and not pre_spawn_finished:
            stop_reason = f"signal_{received_signal['signum']}"
        if not pre_spawn_finished and not stop_event.is_set():
            proc = subprocess.Popen(
                cmd,
                env=scrubbed_env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            stdout_thread, stdout_state = start_bounded_pipe_reader(proc.stdout, MAX_LOG_BYTES)
            stderr_thread, stderr_state = start_bounded_pipe_reader(proc.stderr, MAX_LOG_BYTES)
            next_poll_mono = time.monotonic() + WORKER_POLL_INTERVAL_SECONDS
            while True:
                polled = proc.poll()
                if polled is not None:
                    exit_code = polled
                    break
                if stop_event.is_set():
                    stop_reason = f"signal_{received_signal['signum']}"
                    cur_mem, peak_mem, _ = query_systemd_memory_bytes(unit_name)
                    if cur_mem is not None:
                        max_mem_current = cur_mem if max_mem_current is None else max(max_mem_current, cur_mem)
                    if peak_mem is not None:
                        max_mem_peak = peak_mem if max_mem_peak is None else max(max_mem_peak, peak_mem)
                    exit_code = terminate_unit_gracefully(unit_name, proc, wait_seconds=60.0)
                    break
                now_mono = time.monotonic()
                if now_mono >= deadline:
                    stop_reason = "deadline_reached"
                    cur_mem, peak_mem, _ = query_systemd_memory_bytes(unit_name)
                    if cur_mem is not None:
                        max_mem_current = cur_mem if max_mem_current is None else max(max_mem_current, cur_mem)
                    if peak_mem is not None:
                        max_mem_peak = peak_mem if max_mem_peak is None else max(max_mem_peak, peak_mem)
                    exit_code = terminate_unit_gracefully(unit_name, proc, wait_seconds=60.0)
                    break
                if now_mono >= next_poll_mono:
                    cur_mem, peak_mem, _ = query_systemd_memory_bytes(unit_name)
                    if cur_mem is not None:
                        max_mem_current = cur_mem if max_mem_current is None else max(max_mem_current, cur_mem)
                    if peak_mem is not None:
                        max_mem_peak = peak_mem if max_mem_peak is None else max(max_mem_peak, peak_mem)
                    controller_done = False
                    try:
                        poll_attempts += 1
                        meta, poll_etag, not_modified = find_unexpired_run_artifact(
                            run_id,
                            finished_artifact,
                            token,
                            etag=poll_etag,
                        )
                        if not_modified:
                            poll_not_modified_count += 1
                        elif meta is not None:
                            controller_done = True
                        consecutive_poll_errors = 0
                        next_poll_mono = time.monotonic() + WORKER_POLL_INTERVAL_SECONDS
                    except GitHubApiHttpError as exc:
                        poll_error_count += 1
                        consecutive_poll_errors += 1
                        last_poll_error = redact_secret(str(exc), token)
                        backoff = min(
                            float(MAX_POLL_BACKOFF_SECONDS),
                            float(WORKER_POLL_INTERVAL_SECONDS) * (2 ** min(consecutive_poll_errors, 2)),
                        )
                        if exc.retry_after_seconds is not None:
                            backoff = max(backoff, float(exc.retry_after_seconds))
                        next_poll_mono = time.monotonic() + min(float(MAX_POLL_BACKOFF_SECONDS), backoff)
                    except QualificationError as exc:
                        poll_error_count += 1
                        consecutive_poll_errors += 1
                        last_poll_error = redact_secret(str(exc), token)
                        backoff = min(
                            float(MAX_POLL_BACKOFF_SECONDS),
                            float(WORKER_POLL_INTERVAL_SECONDS) * (2 ** min(consecutive_poll_errors, 2)),
                        )
                        next_poll_mono = time.monotonic() + backoff
                    if controller_done:
                        stop_reason = "controller_finished"
                        cur_mem, peak_mem, _ = query_systemd_memory_bytes(unit_name)
                        if cur_mem is not None:
                            max_mem_current = cur_mem if max_mem_current is None else max(max_mem_current, cur_mem)
                        if peak_mem is not None:
                            max_mem_peak = peak_mem if max_mem_peak is None else max(max_mem_peak, peak_mem)
                        exit_code = terminate_unit_gracefully(unit_name, proc, wait_seconds=60.0)
                        break
                next_wake = min(deadline, next_poll_mono)
                sleep_for = min(1.0, max(0.05, next_wake - time.monotonic()))
                stop_event.wait(timeout=sleep_for)
    except (subprocess.SubprocessError, OSError, QualificationError) as exc:
        supervisor_error = redact_secret(str(exc), token)
    finally:
        if install_signals:
            if prev_sigterm is not None:
                signal.signal(signal.SIGTERM, prev_sigterm)
            if prev_sigint is not None:
                signal.signal(signal.SIGINT, prev_sigint)
        if proc is not None and proc.poll() is None:
            exit_code = terminate_unit_gracefully(unit_name, proc, wait_seconds=45.0)
        if stdout_thread is not None:
            stdout_thread.join(timeout=10)
        if stderr_thread is not None:
            stderr_thread.join(timeout=10)
        key_deleted = delete_private_key(key_path)
    stdout_bytes = bytes(stdout_state["buf"])
    stderr_bytes = bytes(stderr_state["buf"])
    (work_dir / "worker-stdout.log").write_bytes(stdout_bytes)
    (work_dir / "worker-stderr.log").write_bytes(stderr_bytes)
    exit_report: dict[str, Any] | None = None
    parse_error: str | None = None
    if proc is None:
        parse_error = f"worker not started: {stop_reason}"
    else:
        try:
            exit_report = extract_and_validate_exit_report(
                stdout_bytes.decode("utf-8", errors="replace"),
                expected_worker_id,
                worker_name,
            )
        except QualificationError as exc:
            parse_error = str(exc)
    passed = (
        supervisor_error is None
        and parse_error is None
        and is_strict_int(exit_code)
        and exit_code == 0
        and key_deleted
        and not stdout_state["truncated"]
        and not stderr_state["truncated"]
        and exit_report is not None
        and exit_report["summary"]["clean_release_sent"] is True
        and stop_reason == "controller_finished"
    )
    evidence = {
        "schema": "arc.community_shard_qual.worker_evidence.v1",
        "cohort_class": COHORT_CLASS,
        "counts_toward_operator_target": False,
        "slot": slot,
        "unit_name": unit_name,
        "worker_name": worker_name,
        "worker_id": expected_worker_id,
        "runner_name": identity["runner_name"],
        "hostname": identity["hostname"],
        "boot_id": identity["boot_id"],
        "workflow_run_id": run_id,
        "workflow_run_attempt": attempt,
        "candidate_run_id": identity["candidate_run_id"],
        "candidate_run_attempt": identity["candidate_run_attempt"],
        "candidate_sha": identity["candidate_sha"],
        "started_at_utc": started_at_utc,
        "finished_at_utc": utc_now_iso(),
        "elapsed_ms": int((time.monotonic() - started_mono) * 1000),
        "runtime_max_seconds": runtime_max_sec,
        "stop_reason": stop_reason,
        "exit_code": exit_code,
        "key_deleted": key_deleted,
        "systemd_memory_peak_bytes": max_mem_peak,
        "systemd_memory_current_max_bytes": max_mem_current,
        "poll_attempts": poll_attempts,
        "poll_not_modified_count": poll_not_modified_count,
        "poll_error_count": poll_error_count,
        "last_poll_error": last_poll_error,
        "stdout_total_bytes": stdout_state["total_bytes"],
        "stderr_total_bytes": stderr_state["total_bytes"],
        "stdout_truncated": stdout_state["truncated"],
        "stderr_truncated": stderr_state["truncated"],
        "supervisor_error": supervisor_error,
        "parse_error": parse_error,
        "exit_report": exit_report,
        "passed": passed,
    }
    write_json_file(evidence_dir / "worker-evidence.json", evidence)
    verify_evidence_dir_no_secrets(evidence_dir, extra_secret_values=key_secret_values)
    if not passed:
        raise QualificationError(
            f"worker slot {slot} failed: exit_code={exit_code} stop_reason={stop_reason} "
            f"supervisor_error={supervisor_error} parse_error={parse_error}"
        )
    return 0


def perform_and_record_dispatch_once(record: dict[str, Any], index: int, expected_worker_ids: set[str]) -> dict[str, Any]:
    attempt = {
        "request_index": index,
        "sent_at_utc": utc_now_iso(),
        "request_payload": dict(DISPATCH_PAYLOAD),
        "monotonic_latency_ms": None,
        "response": None,
    }
    record.setdefault("dispatches", []).append(attempt)
    _, response, monotonic_ms = coordinator_request_once(
        "POST", DISPATCH_PATH, float(DISPATCH_TIMEOUT_SECONDS), json_body=DISPATCH_PAYLOAD,
    )
    attempt["monotonic_latency_ms"] = monotonic_ms
    attempt["response"] = response
    validate_dispatch_response(response, expected_worker_ids)
    return attempt


def cmd_controller(args: argparse.Namespace) -> int:
    token = os.environ.get("GH_TOKEN", "")
    run_id = validate_positive_id(args.workflow_run_id, "workflow_run_id")
    attempt = validate_positive_id(args.workflow_run_attempt, "workflow_run_attempt")
    cand_run_id = validate_positive_id(args.candidate_run_id, "candidate_run_id")
    cand_attempt = validate_positive_id(args.candidate_run_attempt, "candidate_run_attempt")
    cand_sha = normalize_git_sha(args.candidate_sha)
    evidence_dir = Path(args.evidence_dir)
    evidence_dir.mkdir(parents=True, exist_ok=True)
    downloads_dir = Path(args.downloads_dir)
    downloads_dir.mkdir(parents=True, exist_ok=True)
    started_at_utc = utc_now_iso()
    record: dict[str, Any] = {
        "schema": "arc.community_shard_qual.controller_evidence.v1",
        "cohort_class": COHORT_CLASS,
        "counts_toward_operator_target": False,
        "verification_basis": VERIFICATION_BASIS,
        "workflow_run_id": run_id,
        "workflow_run_attempt": attempt,
        "candidate_run_id": cand_run_id,
        "candidate_run_attempt": cand_attempt,
        "candidate_sha": cand_sha,
        "started_at_utc": started_at_utc,
        "status": "failed",
        "error": None,
        "identity_poll_error_count": 0,
        "last_identity_poll_error": None,
        "identities": [],
        "ready_dispatch_readiness": None,
        "ready_shard_status": None,
        "dispatches": [],
    }
    try:
        expected_names = {
            slot: make_identity_artifact_name(run_id, attempt, slot) for slot in EXPECTED_SLOTS
        }
        id_deadline = time.monotonic() + CONTROLLER_IDENTITY_TIMEOUT_SECONDS
        found_meta: dict[int, dict[str, Any]] = {}
        id_etag: str | None = None
        consecutive_id_errors = 0
        while time.monotonic() < id_deadline:
            poll_delay = 15.0
            try:
                artifacts, id_etag, not_modified = list_run_artifacts_conditional(run_id, token, etag=id_etag)
                consecutive_id_errors = 0
                if not not_modified and artifacts is not None:
                    for slot, name in expected_names.items():
                        meta = artifacts.get(name)
                        if isinstance(meta, dict) and meta.get("expired") is False:
                            found_meta[slot] = meta
            except GitHubApiHttpError as exc:
                record["identity_poll_error_count"] += 1
                record["last_identity_poll_error"] = redact_secret(str(exc), token)
                consecutive_id_errors += 1
                poll_delay = min(60.0, 15.0 * (2 ** min(consecutive_id_errors, 2)))
                if exc.retry_after_seconds is not None:
                    poll_delay = min(float(MAX_POLL_BACKOFF_SECONDS), max(poll_delay, float(exc.retry_after_seconds)))
            except QualificationError as exc:
                record["identity_poll_error_count"] += 1
                record["last_identity_poll_error"] = redact_secret(str(exc), token)
                consecutive_id_errors += 1
                poll_delay = min(60.0, 15.0 * (2 ** min(consecutive_id_errors, 2)))
            if len(found_meta) == len(EXPECTED_SLOTS):
                break
            sleep_for = min(poll_delay, max(0.5, id_deadline - time.monotonic()))
            time.sleep(sleep_for)
        if len(found_meta) != len(EXPECTED_SLOTS):
            missing_slots = sorted(set(EXPECTED_SLOTS) - set(found_meta.keys()))
            raise QualificationError(f"timed out awaiting worker identity artifacts for slots {missing_slots}")
        loaded_identities: list[dict[str, Any]] = []
        for slot in EXPECTED_SLOTS:
            slot_dir = downloads_dir / f"slot-{slot}"
            download_validated_artifact_by_id(
                run_id,
                expected_names[slot],
                found_meta[slot],
                slot_dir,
                MAX_IDENTITY_ARTIFACT_BYTES,
                token,
                timeout_seconds=60,
            )
            loaded_identities.append(read_bounded_json_file(slot_dir / "worker-identity.json", MAX_IDENTITY_ARTIFACT_BYTES))
        validated_by_slot = validate_worker_identities(
            loaded_identities,
            run_id,
            attempt,
            cand_run_id,
            cand_attempt,
            cand_sha,
        )
        record["identities"] = [validated_by_slot[slot] for slot in EXPECTED_SLOTS]
        expected_worker_ids = {validated_by_slot[slot]["worker_id"] for slot in EXPECTED_SLOTS}
        ready_deadline = time.monotonic() + CONTROLLER_READINESS_TIMEOUT_SECONDS
        ready_readiness: dict[str, Any] | None = None
        ready_status: dict[str, Any] | None = None
        while time.monotonic() < ready_deadline:
            raw_readiness = coordinator_get_json_with_retries(READINESS_PATH, ready_deadline)
            raw_status = coordinator_get_json_with_retries(SHARD_STATUS_PATH, ready_deadline)
            ready_cnt, prov_cnt, _ = assert_status_no_external_interference(raw_status, max_workers=len(EXPECTED_SLOTS))
            if ready_cnt == len(EXPECTED_SLOTS) and prov_cnt == 0:
                try:
                    ready_readiness = validate_readiness_payload(raw_readiness, expect_ready=True)
                    ready_status = validate_shard_status_payload(
                        raw_status,
                        expected_ready_workers=len(EXPECTED_SLOTS),
                        expected_provisioning_workers=0,
                        expected_active_sessions=0,
                        expected_tile_ready=1,
                        expected_tile_provisioning=0,
                    )
                    break
                except QualificationError:
                    ready_readiness = None
                    ready_status = None
            sleep_for = min(CONTROLLER_POLL_INTERVAL_SECONDS, max(0.5, ready_deadline - time.monotonic()))
            time.sleep(sleep_for)
        if ready_readiness is None or ready_status is None:
            raise QualificationError("timed out waiting for 6 ready workers and all 6 tiles ready on coordinator")
        record["ready_dispatch_readiness"] = ready_readiness
        record["ready_shard_status"] = ready_status
        dispatches: list[dict[str, Any]] = []
        record["dispatches"] = dispatches
        for idx in (1, 2):
            perform_and_record_dispatch_once(record, idx, expected_worker_ids)
        validate_two_dispatch_responses(
            dispatches[0]["response"],
            dispatches[1]["response"],
            expected_worker_ids,
        )
        record["status"] = "passed"
    except QualificationError as exc:
        record["error"] = redact_secret(str(exc), token)
        raise
    finally:
        record["finished_at_utc"] = utc_now_iso()
        write_json_file(evidence_dir / "controller-evidence.json", record)
        verify_evidence_dir_no_secrets(evidence_dir)
    return 0


def cmd_write_controller_finished(args: argparse.Namespace) -> int:
    run_id = validate_positive_id(args.workflow_run_id, "workflow_run_id")
    attempt = validate_positive_id(args.workflow_run_attempt, "workflow_run_attempt")
    payload = {
        "schema": "arc.community_shard_qual.controller_finished.v1",
        "workflow_run_id": run_id,
        "workflow_run_attempt": attempt,
        "controller_outcome": args.controller_outcome,
        "finished_at_utc": utc_now_iso(),
    }
    out_dir = Path(args.output_dir)
    write_json_file(out_dir / "controller-finished.json", payload)
    verify_evidence_dir_no_secrets(out_dir)
    return 0


def evaluate_aggregate(
    identities: list[dict[str, Any]],
    worker_evidences: list[dict[str, Any]],
    controller_evidence: dict[str, Any],
    cleanup_readiness: dict[str, Any] | None,
    cleanup_status: dict[str, Any] | None,
    cleanup_error: str | None,
    expected_run_id: str,
    expected_attempt: str,
    expected_candidate_run_id: str,
    expected_candidate_attempt: str,
    expected_candidate_sha: str,
) -> dict[str, Any]:
    failures: list[str] = []
    by_slot: dict[int, dict[str, Any]] = {}
    try:
        by_slot = validate_worker_identities(
            identities,
            expected_run_id,
            expected_attempt,
            expected_candidate_run_id,
            expected_candidate_attempt,
            expected_candidate_sha,
        )
    except QualificationError as exc:
        failures.append(f"identities: {exc}")
    expected_ids = {rec["worker_id"] for rec in by_slot.values()}
    ev_by_slot: dict[int, dict[str, Any]] = {}
    for ev in worker_evidences:
        if not isinstance(ev, dict):
            failures.append("worker evidence entry is not object")
            continue
        try:
            slot = validate_slot(ev.get("slot"))
        except QualificationError as exc:
            failures.append(f"worker evidence slot: {exc}")
            continue
        if slot in ev_by_slot:
            failures.append(f"duplicate worker evidence for slot {slot}")
            continue
        ev_by_slot[slot] = ev
    worker_summaries: list[dict[str, Any]] = []
    seen_ev_boot_ids: set[str] = set()
    seen_ev_runners: set[str] = set()
    for slot in EXPECTED_SLOTS:
        ev = ev_by_slot.get(slot)
        ident = by_slot.get(slot)
        if ev is None:
            failures.append(f"missing worker evidence for slot {slot}")
            continue
        if ident is None:
            failures.append(f"missing worker identity for slot {slot}")
            continue
        if ev.get("passed") is not True or not is_strict_int(ev.get("exit_code")) or ev.get("exit_code") != 0:
            failures.append(f"slot {slot} did not pass cleanly (exit_code={ev.get('exit_code')})")
        if ev.get("stop_reason") != "controller_finished":
            failures.append(f"slot {slot} stop_reason={ev.get('stop_reason')!r}, expected 'controller_finished'")
        if ev.get("cohort_class") != COHORT_CLASS or ev.get("counts_toward_operator_target") is not False:
            failures.append(f"slot {slot} cohort_class or operator target flag invalid")
        if ev.get("workflow_run_id") != expected_run_id or ev.get("workflow_run_attempt") != expected_attempt:
            failures.append(f"slot {slot} workflow run/attempt mismatch in evidence")
        if (
            ev.get("candidate_run_id") != expected_candidate_run_id
            or ev.get("candidate_run_attempt") != expected_candidate_attempt
        ):
            failures.append(f"slot {slot} candidate run/attempt mismatch in evidence")
        if ev.get("worker_id") != ident["worker_id"] or ev.get("worker_name") != ident["worker_name"]:
            failures.append(f"slot {slot} worker identity mismatch in evidence")
        if ev.get("key_deleted") is not True:
            failures.append(f"slot {slot} key_deleted is not true")
        if ev.get("stdout_truncated") is not False or ev.get("stderr_truncated") is not False:
            failures.append(f"slot {slot} logs were truncated")
        if ev.get("candidate_sha") != ident["candidate_sha"]:
            failures.append(f"slot {slot} candidate_sha mismatch")
        if ev.get("runner_name") != ident["runner_name"] or ev.get("hostname") != ident["hostname"]:
            failures.append(f"slot {slot} host/runner mismatch between identity and evidence")
        if ev.get("boot_id") != ident["boot_id"]:
            failures.append(f"slot {slot} machine boot_id mismatch between identity and evidence")
        else:
            seen_ev_boot_ids.add(ident["boot_id"])
        seen_ev_runners.add(str(ev.get("runner_name", "")))
        exit_rep = ev.get("exit_report")
        try:
            validated_exit = validate_exit_report(exit_rep, ident["worker_id"], ident["worker_name"])
            summary = validated_exit["summary"]
            if summary["registrations"] < 1:
                failures.append(f"slot {slot} registrations < 1")
            if summary["leases_acquired"] < 1:
                failures.append(f"slot {slot} leases_acquired < 1")
            if summary["models_loaded"] < 1:
                failures.append(f"slot {slot} models_loaded < 1")
            if summary["heartbeats_sent"] < 1:
                failures.append(f"slot {slot} heartbeats_sent < 1")
            if summary["steps_executed"] <= 0:
                failures.append(f"slot {slot} steps_executed <= 0")
            if summary["steps_submitted"] <= 0:
                failures.append(f"slot {slot} steps_submitted <= 0")
            if summary["submit_stale_conflicts"] != 0:
                failures.append(f"slot {slot} submit_stale_conflicts != 0")
            if summary["clean_release_sent"] is not True:
                failures.append(f"slot {slot} clean_release_sent is not true")
            worker_summaries.append(
                {
                    "slot": slot,
                    "worker_id": ident["worker_id"],
                    "worker_name": ident["worker_name"],
                    "runner_name": ident["runner_name"],
                    "hostname": ident["hostname"],
                    "boot_id": ident["boot_id"],
                    "systemd_memory_peak_bytes": ev.get("systemd_memory_peak_bytes"),
                    "steps_executed": summary["steps_executed"],
                    "steps_submitted": summary["steps_submitted"],
                    "clean_release_sent": summary["clean_release_sent"],
                }
            )
        except QualificationError as exc:
            failures.append(f"slot {slot} exit_report invalid: {exc}")
    if len(seen_ev_boot_ids) != len(EXPECTED_SLOTS) or len(seen_ev_runners) != len(EXPECTED_SLOTS):
        failures.append("worker evidences do not come from 6 distinct machine boots and runners")
    dispatch_summaries: list[dict[str, Any]] = []
    norm_expected_sha = normalize_git_sha(expected_candidate_sha)
    if not isinstance(controller_evidence, dict):
        failures.append("controller evidence missing or invalid")
    else:
        if (
            controller_evidence.get("cohort_class") != COHORT_CLASS
            or controller_evidence.get("counts_toward_operator_target") is not False
        ):
            failures.append("controller evidence cohort_class or operator target flag invalid")
        if controller_evidence.get("verification_basis") != VERIFICATION_BASIS:
            failures.append("controller evidence verification_basis mismatch")
        if (
            controller_evidence.get("workflow_run_id") != expected_run_id
            or controller_evidence.get("workflow_run_attempt") != expected_attempt
        ):
            failures.append("controller evidence workflow run/attempt mismatch")
        if (
            controller_evidence.get("candidate_run_id") != expected_candidate_run_id
            or controller_evidence.get("candidate_run_attempt") != expected_candidate_attempt
        ):
            failures.append("controller evidence candidate run/attempt mismatch")
        try:
            if normalize_git_sha(controller_evidence.get("candidate_sha", ""), "controller.candidate_sha") != norm_expected_sha:
                failures.append("controller evidence candidate_sha mismatch")
        except QualificationError:
            failures.append("controller evidence candidate_sha invalid")
        if controller_evidence.get("status") != "passed":
            failures.append(f"controller did not pass: {controller_evidence.get('error')}")
        dispatches = controller_evidence.get("dispatches")
        if not isinstance(dispatches, list) or len(dispatches) != 2:
            failures.append("controller did not record exactly 2 dispatches")
        elif len(expected_ids) == len(EXPECTED_SLOTS):
            try:
                validate_two_dispatch_responses(
                    dispatches[0].get("response"),
                    dispatches[1].get("response"),
                    expected_ids,
                )
                for item in dispatches:
                    resp = item["response"]
                    dispatch_summaries.append(
                        {
                            "request_index": item.get("request_index"),
                            "session_id": resp["session_id"],
                            "monotonic_latency_ms": item.get("monotonic_latency_ms"),
                            "coordinator_total_ms": resp["total_ms"],
                            "tokens_generated": resp["tokens_generated"],
                            "generated_tokens": resp["generated_tokens"],
                            "output_hash": normalize_hex64(resp["output_hash"], "output_hash"),
                            "stopped_by_eos": resp["stopped_by_eos"],
                            "verification": resp["verification"],
                        }
                    )
            except QualificationError as exc:
                failures.append(f"dispatch proof validation failed: {exc}")
    cleanup_verified = False
    if cleanup_error is not None:
        failures.append(f"cleanup: {cleanup_error}")
    else:
        try:
            validate_readiness_payload(cleanup_readiness, expect_ready=False)
            validate_shard_status_payload(
                cleanup_status,
                expected_ready_workers=0,
                expected_provisioning_workers=0,
                expected_active_sessions=0,
                expected_tile_ready=0,
                expected_tile_provisioning=0,
            )
            cleanup_verified = True
        except QualificationError as exc:
            failures.append(f"cleanup verification failed: {exc}")
    overall_passed = len(failures) == 0
    return {
        "schema": "arc.community_shard_qual.summary.v1",
        "generated_at_utc": utc_now_iso(),
        "overall_status": "passed" if overall_passed else "failed",
        "cohort_class": COHORT_CLASS,
        "ephemeral_qualification_workers": len(EXPECTED_SLOTS),
        "counts_toward_operator_target": False,
        "real_operators_proven": 0,
        "machine_identity_basis": "distinct_linux_kernel_boot_ids_and_github_runner_names",
        "verification_basis": VERIFICATION_BASIS,
        "repository": EXPECTED_REPO,
        "coordinator_url": COORDINATOR_URL,
        "coordinator_id": COORDINATOR_ID,
        "transaction_domain": TRANSACTION_DOMAIN,
        "model_id": EXPECTED_MODEL_ID,
        "execution_profile": CANONICAL_EXECUTION_PROFILE,
        "workflow_run_id": expected_run_id,
        "workflow_run_attempt": expected_attempt,
        "candidate_run_id": expected_candidate_run_id,
        "candidate_run_attempt": expected_candidate_attempt,
        "candidate_sha": normalize_git_sha(expected_candidate_sha),
        "cleanup_verified_zero_workers": cleanup_verified,
        "workers": worker_summaries,
        "dispatches": dispatch_summaries,
        "failures": failures,
    }


def poll_coordinator_zero_after_cleanup(max_wait_seconds: int) -> tuple[dict[str, Any] | None, dict[str, Any] | None, str | None]:
    deadline = time.monotonic() + max_wait_seconds
    last_readiness: dict[str, Any] | None = None
    last_status: dict[str, Any] | None = None
    last_err: str | None = None
    while True:
        try:
            raw_r = coordinator_get_json_with_retries(READINESS_PATH, deadline, max_attempts=2)
            raw_s = coordinator_get_json_with_retries(SHARD_STATUS_PATH, deadline, max_attempts=2)
            last_readiness = raw_r
            last_status = raw_s
            validate_readiness_payload(raw_r, expect_ready=False)
            validate_shard_status_payload(
                raw_s,
                expected_ready_workers=0,
                expected_provisioning_workers=0,
                expected_active_sessions=0,
                expected_tile_ready=0,
                expected_tile_provisioning=0,
            )
            return raw_r, raw_s, None
        except QualificationError as exc:
            last_err = str(exc)
        if time.monotonic() >= deadline:
            break
        sleep_for = min(5.0, max(0.5, deadline - time.monotonic()))
        time.sleep(sleep_for)
    if isinstance(last_status, dict) and isinstance(last_status.get("status"), dict):
        st = last_status["status"]
        r_cnt = st.get("ready_workers")
        p_cnt = st.get("provisioning_workers")
        if (is_strict_int(r_cnt) and r_cnt > 0) or (is_strict_int(p_cnt) and p_cnt > 0):
            return (
                last_readiness,
                last_status,
                f"coordinator still reports ready_workers={r_cnt} provisioning_workers={p_cnt} "
                f"after {max_wait_seconds}s; cannot assert zero qualification leases without interfering with other workers",
            )
    return last_readiness, last_status, last_err or "cleanup zero-state check timed out"


def cmd_aggregate(args: argparse.Namespace) -> int:
    run_id = validate_positive_id(args.workflow_run_id, "workflow_run_id")
    attempt = validate_positive_id(args.workflow_run_attempt, "workflow_run_attempt")
    cand_run_id = validate_positive_id(args.candidate_run_id, "candidate_run_id")
    cand_attempt = validate_positive_id(args.candidate_run_attempt, "candidate_run_attempt")
    cand_sha = normalize_git_sha(args.candidate_sha)
    artifacts_root = Path(args.artifacts_dir)
    output_dir = Path(args.output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    load_errors: list[str] = []
    identities: list[dict[str, Any]] = []
    worker_evidences: list[dict[str, Any]] = []
    for slot in EXPECTED_SLOTS:
        id_dir = artifacts_root / make_identity_artifact_name(run_id, attempt, slot)
        id_file = id_dir / "worker-identity.json"
        if id_dir.is_symlink():
            load_errors.append(f"slot {slot} identity dir is symlink")
        elif id_file.is_file() and not id_file.is_symlink():
            try:
                loaded_id = read_bounded_json_file(id_file, MAX_IDENTITY_ARTIFACT_BYTES)
                if isinstance(loaded_id, dict):
                    identities.append(loaded_id)
                else:
                    load_errors.append(f"slot {slot} worker-identity.json is not a JSON object")
            except QualificationError as exc:
                load_errors.append(f"slot {slot} worker-identity.json load failed: {exc}")
        ev_dir = artifacts_root / make_worker_evidence_artifact_name(run_id, attempt, slot)
        ev_file = ev_dir / "worker-evidence.json"
        if ev_dir.is_symlink():
            load_errors.append(f"slot {slot} worker evidence dir is symlink")
        elif ev_file.is_file() and not ev_file.is_symlink():
            try:
                loaded_ev = read_bounded_json_file(ev_file, MAX_JSON_FILE_BYTES)
                if isinstance(loaded_ev, dict):
                    worker_evidences.append(loaded_ev)
                else:
                    load_errors.append(f"slot {slot} worker-evidence.json is not a JSON object")
            except QualificationError as exc:
                load_errors.append(f"slot {slot} worker-evidence.json load failed: {exc}")
    ctrl_dir = artifacts_root / make_controller_evidence_artifact_name(run_id, attempt)
    ctrl_file = ctrl_dir / "controller-evidence.json"
    controller_evidence: dict[str, Any] = {}
    if ctrl_dir.is_symlink():
        load_errors.append("controller evidence dir is symlink")
    elif ctrl_file.is_file() and not ctrl_file.is_symlink():
        try:
            loaded_ctrl = read_bounded_json_file(ctrl_file, MAX_JSON_FILE_BYTES)
            if isinstance(loaded_ctrl, dict):
                controller_evidence = loaded_ctrl
            else:
                load_errors.append("controller-evidence.json is not a JSON object")
        except QualificationError as exc:
            load_errors.append(f"controller-evidence.json load failed: {exc}")
    cleanup_wait = int(args.cleanup_wait_seconds)
    if not (5 <= cleanup_wait <= DEFAULT_CLEANUP_WAIT_SECONDS):
        raise QualificationError(f"cleanup_wait_seconds must be in 5..={DEFAULT_CLEANUP_WAIT_SECONDS}")
    cleanup_r, cleanup_s, cleanup_err = poll_coordinator_zero_after_cleanup(cleanup_wait)
    summary = evaluate_aggregate(
        identities=identities,
        worker_evidences=worker_evidences,
        controller_evidence=controller_evidence,
        cleanup_readiness=cleanup_r,
        cleanup_status=cleanup_s,
        cleanup_error=cleanup_err,
        expected_run_id=run_id,
        expected_attempt=attempt,
        expected_candidate_run_id=cand_run_id,
        expected_candidate_attempt=cand_attempt,
        expected_candidate_sha=cand_sha,
    )
    if load_errors:
        summary["failures"] = load_errors + summary["failures"]
        summary["overall_status"] = "failed"
    write_json_file(output_dir / "qualification-summary.json", summary)
    verify_evidence_dir_no_secrets(output_dir)
    if summary["overall_status"] != "passed":
        raise QualificationError(f"qualification failed: {summary['failures']}")
    return 0


def cmd_verify_public_evidence(args: argparse.Namespace) -> int:
    directory = Path(args.evidence_dir)
    if directory.is_symlink() or not directory.is_dir():
        raise QualificationError("public evidence directory must exist and cannot be a symlink")
    key_path = Path(args.private_key_path) if args.private_key_path else None
    secrets = extract_private_key_secrets(key_path) if key_path else ()
    verify_evidence_dir_no_secrets(directory, extra_secret_values=secrets)
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="community_shard_six_machine")
    sub = parser.add_subparsers(dest="command", required=True)

    p_public = sub.add_parser("verify-public-evidence")
    p_public.add_argument("--evidence-dir", required=True)
    p_public.add_argument("--private-key-path", default="")
    p_public.set_defaults(func=cmd_verify_public_evidence)

    p_val = sub.add_parser("validate")
    p_val.add_argument("--candidate-run-id", required=True)
    p_val.add_argument("--candidate-run-attempt", required=True)
    p_val.add_argument("--candidate-sha", required=True)
    p_val.add_argument("--candidate-source-dir", default="")
    p_val.add_argument("--output-dir", required=True)
    p_val.set_defaults(func=cmd_validate)

    p_res = sub.add_parser("check-resources")
    p_res.add_argument("--runner-temp", required=True)
    p_res.add_argument("--output-dir", required=True)
    p_res.set_defaults(func=cmd_check_resources)

    p_dl = sub.add_parser("download-candidate")
    p_dl.add_argument("--candidate-run-id", required=True)
    p_dl.add_argument("--candidate-run-attempt", required=True)
    p_dl.add_argument("--candidate-sha", required=True)
    p_dl.add_argument("--dest-dir", required=True)
    p_dl.set_defaults(func=cmd_download_and_verify_candidate)

    p_gguf = sub.add_parser("verify-gguf")
    p_gguf.add_argument("--model-path", required=True)
    p_gguf.set_defaults(func=cmd_verify_gguf)

    p_pre = sub.add_parser("worker-preflight")
    p_pre.add_argument("--workflow-run-id", required=True)
    p_pre.add_argument("--workflow-run-attempt", required=True)
    p_pre.add_argument("--slot", type=int, required=True)
    p_pre.add_argument("--candidate-dir", required=True)
    p_pre.add_argument("--model-path", required=True)
    p_pre.add_argument("--evidence-dir", required=True)
    p_pre.set_defaults(func=cmd_worker_preflight)

    p_id = sub.add_parser("worker-create-identity")
    p_id.add_argument("--workflow-run-id", required=True)
    p_id.add_argument("--workflow-run-attempt", required=True)
    p_id.add_argument("--candidate-run-id", required=True)
    p_id.add_argument("--candidate-run-attempt", required=True)
    p_id.add_argument("--candidate-sha", required=True)
    p_id.add_argument("--slot", type=int, required=True)
    p_id.add_argument("--candidate-dir", required=True)
    p_id.add_argument("--private-key-path", required=True)
    p_id.add_argument("--identity-dir", required=True)
    p_id.set_defaults(func=cmd_worker_create_identity)

    p_run = sub.add_parser("worker-run")
    p_run.add_argument("--workflow-run-id", required=True)
    p_run.add_argument("--workflow-run-attempt", required=True)
    p_run.add_argument("--slot", type=int, required=True)
    p_run.add_argument("--candidate-dir", required=True)
    p_run.add_argument("--model-path", required=True)
    p_run.add_argument("--private-key-path", required=True)
    p_run.add_argument("--identity-dir", required=True)
    p_run.add_argument("--work-dir", required=True)
    p_run.add_argument("--evidence-dir", required=True)
    p_run.add_argument("--max-seconds", type=int, default=DEFAULT_WORKER_SUPERVISOR_MAX_SECONDS)
    p_run.set_defaults(func=cmd_worker_run)

    p_ctrl = sub.add_parser("controller")
    p_ctrl.add_argument("--workflow-run-id", required=True)
    p_ctrl.add_argument("--workflow-run-attempt", required=True)
    p_ctrl.add_argument("--candidate-run-id", required=True)
    p_ctrl.add_argument("--candidate-run-attempt", required=True)
    p_ctrl.add_argument("--candidate-sha", required=True)
    p_ctrl.add_argument("--downloads-dir", required=True)
    p_ctrl.add_argument("--evidence-dir", required=True)
    p_ctrl.set_defaults(func=cmd_controller)

    p_fin = sub.add_parser("write-controller-finished")
    p_fin.add_argument("--workflow-run-id", required=True)
    p_fin.add_argument("--workflow-run-attempt", required=True)
    p_fin.add_argument("--controller-outcome", required=True)
    p_fin.add_argument("--output-dir", required=True)
    p_fin.set_defaults(func=cmd_write_controller_finished)

    p_agg = sub.add_parser("aggregate")
    p_agg.add_argument("--workflow-run-id", required=True)
    p_agg.add_argument("--workflow-run-attempt", required=True)
    p_agg.add_argument("--candidate-run-id", required=True)
    p_agg.add_argument("--candidate-run-attempt", required=True)
    p_agg.add_argument("--candidate-sha", required=True)
    p_agg.add_argument("--artifacts-dir", required=True)
    p_agg.add_argument("--output-dir", required=True)
    p_agg.add_argument("--cleanup-wait-seconds", type=int, default=DEFAULT_CLEANUP_WAIT_SECONDS)
    p_agg.set_defaults(func=cmd_aggregate)

    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        return int(args.func(args))
    except QualificationError as exc:
        sys.stderr.write(f"ERROR: {exc}\n")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
