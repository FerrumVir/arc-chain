from collections.abc import Sequence
import datetime, json, math, re

PHASE_TARGET, PHASE_MESSAGE = "arc::community_shard_phase", "community shard phase timing"
PHASES = ("shard_execution", "verifier_permit_wait", "quorum_verification")
SCHEMA = "arc.community_shard.phase_evidence.v1"
MAX_JOURNAL_BYTES, MAX_JOURNAL_LINES, MAX_LINE_BYTES = 16 * 1024 * 1024, 50_000, 32 * 1024
MAX_MESSAGE_CHARS, MAX_JSON_FIELDS, MAX_JSON_DEPTH, MAX_SESSIONS = 4_096, 128, 8, 4_096
MIN_I64, MAX_U64 = -(1 << 63), (1 << 64) - 1
_HEX, _DIGITS = frozenset("0123456789abcdef"), frozenset("0123456789")
_REQUIRED_FIELDS = frozenset(("boot_nonce", "session_seq", "phase", "duration_ms", "returned_ok"))
_ALLOWED_FIELDS = _REQUIRED_FIELDS | frozenset(("error_status",))
_ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")
_HEADER_RE = re.compile(
    r"^([!-~]+)[ ]{1,2}(TRACE|DEBUG|INFO|WARN|ERROR)[ ](?:[A-Za-z_][A-Za-z0-9_]*\{[^}\r\n]*\}: )*"
    r"([A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*):(.*)\Z"
)
_TS_RE = re.compile(r"^([0-9]{4})-([0-9]{2})-([0-9]{2})T([0-9]{2}):([0-9]{2}):([0-9]{2})(?:\.[0-9]{1,9})?Z\Z")
_MALFORMED_TARGET_RE = re.compile(
    r"^(?:\S+[ \t]+)?(?:TRACE|DEBUG|INFO|WARN|ERROR)[ \t]+"
    r"(?:[A-Za-z_][A-Za-z0-9_]*\{[^}\r\n]*\}:[ \t]+)*"
    + re.escape(PHASE_TARGET) + r"(?=[: \t]|$)"
)

class PhaseEvidenceError(ValueError):
    pass

def _strict_int(value: object, lo: int, hi: int, label: str) -> int:
    if type(value) is not int or value < lo or value > hi:
        raise PhaseEvidenceError(f"{label} must be int in [{lo}, {hi}]")
    return value

def _hex64(value: object, label: str) -> str:
    if not isinstance(value, str) or len(value) != 64 or any(c not in _HEX for c in value):
        raise PhaseEvidenceError(f"{label} must be 64 lowercase hex chars")
    return value

def _parse_uint(raw: str, lo: int, hi: int, label: str) -> int:
    if not raw or len(raw) > 20 or any(c not in _DIGITS for c in raw) or (len(raw) > 1 and raw[0] == "0"):
        raise PhaseEvidenceError(f"{label} must be canonical unsigned digits")
    return _strict_int(int(raw), lo, hi, label)

def _parse_quoted(raw: str, label: str) -> str:
    if len(raw) < 2 or raw[0] != '"' or raw[-1] != '"' or '"' in raw[1:-1] or "\\" in raw[1:-1]:
        raise PhaseEvidenceError(f"{label} must be an unescaped double-quoted string")
    return raw[1:-1]

def _validate_timestamp(raw: str) -> None:
    match = _TS_RE.match(raw)
    if match is None:
        raise PhaseEvidenceError("invalid tracing UTC timestamp")
    y, mo, d, hh, mm, ss = (int(part) for part in match.groups())
    if y < 1970 or hh > 23 or mm > 59 or ss > 59:
        raise PhaseEvidenceError("tracing UTC timestamp out of range")
    try:
        datetime.date(y, mo, d)
    except ValueError as exc:
        raise PhaseEvidenceError("invalid calendar date in tracing timestamp") from exc

def _strict_json_int(token: str) -> int:
    return _strict_int(int(token) if len(token) <= 21 else MAX_U64 + 1, MIN_I64, MAX_U64, "json_int")

def _strict_json_float(token: str) -> float:
    if not math.isfinite(val := float(token)):
        raise PhaseEvidenceError("non-finite JSON float")
    return val

def _strict_json_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    if len(pairs) > MAX_JSON_FIELDS:
        raise PhaseEvidenceError("JSON object exceeds field limit")
    out: dict[str, object] = {}
    for key, value in pairs:
        if key in out:
            raise PhaseEvidenceError(f"duplicate JSON key: {key}")
        out[key] = value
    return out

def _validate_json_tree(value: object, depth: int = 0) -> None:
    if depth > MAX_JSON_DEPTH:
        raise PhaseEvidenceError("JSON nesting exceeds depth limit")
    if isinstance(value, list):
        if len(value) > MAX_JSON_FIELDS:
            raise PhaseEvidenceError("JSON array exceeds element limit")
        for item in value:
            _validate_json_tree(item, depth + 1)
    elif isinstance(value, dict):
        for key, item in value.items():
            _validate_json_tree(key, depth + 1)
            _validate_json_tree(item, depth + 1)
    elif isinstance(value, str):
        try:
            value.encode("utf-8")
        except UnicodeEncodeError as exc:
            raise PhaseEvidenceError("JSON string contains invalid UTF-8") from exc

def _check_json_depth(line: str) -> None:
    depth, quoted, escaped = 0, False, False
    for char in line:
        if quoted:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                quoted = False
        elif char == '"':
            quoted = True
        elif char in "[{":
            depth += 1
            if depth > MAX_JSON_DEPTH:
                raise PhaseEvidenceError("JSON nesting exceeds depth limit before decoding")
        elif char in "]}":
            depth -= 1

def _parse_phase_body(body: str) -> dict[str, object]:
    prefix = PHASE_MESSAGE + " "
    if not body.startswith(prefix):
        raise PhaseEvidenceError("unexpected phase event message")
    raw_fields = body[len(prefix):]
    if not raw_fields or any(ord(ch) < 0x20 or ord(ch) > 0x7E for ch in raw_fields):
        raise PhaseEvidenceError("invalid characters in phase event fields")
    seen: dict[str, str] = {}
    for token in raw_fields.split(" "):
        if not token or "=" not in token:
            raise PhaseEvidenceError("malformed phase field token")
        key, raw_val = token.split("=", 1)
        if key not in _ALLOWED_FIELDS or key in seen:
            raise PhaseEvidenceError(f"invalid or duplicate phase field: {key}")
        seen[key] = raw_val
    if not _REQUIRED_FIELDS.issubset(seen):
        raise PhaseEvidenceError("missing required phase fields")
    boot_nonce = _hex64(_parse_quoted(seen["boot_nonce"], "boot_nonce"), "boot_nonce")
    session_seq = _parse_uint(seen["session_seq"], 0, MAX_U64, "session_seq")
    phase = _parse_quoted(seen["phase"], "phase")
    if phase not in PHASES:
        raise PhaseEvidenceError(f"unsupported phase name: {phase}")
    duration_ms = _parse_uint(seen["duration_ms"], 0, MAX_U64, "duration_ms")
    if seen["returned_ok"] not in ("true", "false"):
        raise PhaseEvidenceError("returned_ok must be lowercase boolean")
    returned_ok = seen["returned_ok"] == "true"
    error_status = _parse_uint(seen["error_status"], 100, 599, "error_status") if "error_status" in seen else None
    if returned_ok and error_status is not None:
        raise PhaseEvidenceError("returned_ok=true cannot carry error_status")
    return {"boot_nonce": boot_nonce, "session_seq": session_seq, "phase": phase,
            "duration_ms": duration_ms, "returned_ok": returned_ok, "error_status": error_status}

def parse_phase_message(message: str) -> dict[str, object] | None:
    if type(message) is not str or len(message) > MAX_MESSAGE_CHARS:
        raise PhaseEvidenceError("MESSAGE must be bounded string")
    first_line = message.split("\n", 1)[0].rstrip("\r")
    match = _HEADER_RE.match(first_line)
    if match is not None:
        ts_raw, level, target, rest = match.groups()
        if target != PHASE_TARGET:
            return None
        if "\n" in message or "\r" in message or "\x1b" in message or not rest.startswith(" "):
            raise PhaseEvidenceError("malformed phase target log formatting")
        _validate_timestamp(ts_raw)
        if level != "INFO":
            raise PhaseEvidenceError("unexpected log level for phase target")
        return _parse_phase_body(rest[1:])
    stripped_match = _HEADER_RE.match(_ANSI_RE.sub("", first_line))
    if stripped_match is not None and stripped_match.group(3) != PHASE_TARGET:
        return None
    if _MALFORMED_TARGET_RE.match(_ANSI_RE.sub("", first_line).lstrip()):
        raise PhaseEvidenceError("malformed phase target log line")
    return None

def parse_journal_phase_events(raw_journal: str) -> list[dict[str, object]]:
    if type(raw_journal) is not str:
        raise PhaseEvidenceError("raw_journal must be string")
    try:
        if len(raw_journal.encode("utf-8")) > MAX_JOURNAL_BYTES:
            raise PhaseEvidenceError("raw_journal exceeds byte limit")
    except UnicodeEncodeError as exc:
        raise PhaseEvidenceError("raw_journal contains invalid UTF-8") from exc
    lines = raw_journal.split("\n")
    if lines[-1] == "":
        lines.pop()
    if len(lines) > MAX_JOURNAL_LINES:
        raise PhaseEvidenceError("raw_journal exceeds line limit")
    events: list[dict[str, object]] = []
    for line in lines:
        if "\r" in line or len(line.encode("utf-8")) > MAX_LINE_BYTES:
            raise PhaseEvidenceError("journal line malformed or exceeds byte limit")
        _check_json_depth(line)
        try:
            entry = json.loads(line, object_pairs_hook=_strict_json_object, parse_int=_strict_json_int,
                               parse_float=_strict_json_float, parse_constant=_strict_json_float)
        except json.JSONDecodeError as exc:
            raise PhaseEvidenceError("malformed journal JSON line") from exc
        if not isinstance(entry, dict) or "MESSAGE" not in entry or type(entry["MESSAGE"]) is not str:
            raise PhaseEvidenceError("journal JSON entry requires string MESSAGE")
        _validate_json_tree(entry)
        if (parsed := parse_phase_message(entry["MESSAGE"])) is not None:
            events.append(parsed)
    return events

def correlate_phase_evidence(raw_journal: str, validated_session_keys: Sequence[tuple[str, int]]) -> dict[str, object]:
    if isinstance(validated_session_keys, (str, bytes)) or not isinstance(validated_session_keys, Sequence):
        raise PhaseEvidenceError("validated_session_keys must be a sequence")
    if len(validated_session_keys) > MAX_SESSIONS:
        raise PhaseEvidenceError("validated_session_keys exceeds session limit")
    ordered_keys: list[tuple[str, int]] = []
    seen_keys: set[tuple[str, int]] = set()
    for item in validated_session_keys:
        if not isinstance(item, tuple) or len(item) != 2:
            raise PhaseEvidenceError("session key must be (boot_nonce, session_seq) tuple")
        key = (_hex64(item[0], "boot_nonce"), _strict_int(item[1], 0, MAX_U64, "session_seq"))
        if key in seen_keys:
            raise PhaseEvidenceError("duplicate validated response session key")
        seen_keys.add(key)
        ordered_keys.append(key)
    events = parse_journal_phase_events(raw_journal)
    matched: dict[tuple[str, int], dict[str, dict[str, object] | None]] = {k: {p: None for p in PHASES} for k in ordered_keys}
    unknown_sessions: set[tuple[str, int]] = set()
    seen_phases: set[tuple[tuple[str, int], str]] = set()
    unknown_events = 0
    for ev in events:
        key, phase = (str(ev["boot_nonce"]), int(ev["session_seq"])), str(ev["phase"])
        if (key, phase) in seen_phases:
            raise PhaseEvidenceError(f"duplicate phase {phase} for session")
        seen_phases.add((key, phase))
        if key in matched:
            matched[key][phase] = {k: ev[k] for k in ("duration_ms", "returned_ok", "error_status")}
        else:
            if key not in unknown_sessions and len(unknown_sessions) >= MAX_SESSIONS:
                raise PhaseEvidenceError("unknown session count exceeds limit")
            unknown_sessions.add(key)
            unknown_events += 1
    sessions = [
        {"boot_nonce": k[0], "session_seq": k[1], "phases": matched[k],
         "missing_phases": [p for p in PHASES if matched[k][p] is None],
         "aborted_phases": [p for p in PHASES if matched[k][p] and not matched[k][p]["returned_ok"]],
         "complete_ok": all(matched[k][p] is not None and bool(matched[k][p]["returned_ok"]) for p in PHASES)}
        for k in ordered_keys
    ]
    distributions = {
        p: {"ok_durations_ms": [int(matched[k][p]["duration_ms"]) for k in ordered_keys if matched[k][p] and matched[k][p]["returned_ok"]],
            "missing_count": sum(1 for k in ordered_keys if matched[k][p] is None),
            "aborted_count": sum(1 for k in ordered_keys if matched[k][p] and not matched[k][p]["returned_ok"])}
        for p in PHASES
    }
    return {
        "schema": SCHEMA, "proves_request_success": False, "infers_server_concurrency": False,
        "requested_session_count": len(ordered_keys), "unknown_session_count": len(unknown_sessions),
        "unknown_phase_event_count": unknown_events,
        "complete_ok_session_count": sum(1 for s in sessions if s["complete_ok"]),
        "incomplete_session_count": sum(1 for s in sessions if s["missing_phases"]),
        "aborted_session_count": sum(1 for s in sessions if s["aborted_phases"]),
        "sessions": sessions, "phase_distributions": distributions,
    }
