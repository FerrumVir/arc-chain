#!/usr/bin/env python3
"""Hash the selected OAuth user's Drive identity without persisting credentials."""

from __future__ import annotations

import gc
import hashlib
import http.client
import json
import os
import re
import signal
import ssl
import sys
import time
from contextlib import contextmanager


API_HOST = "www.googleapis.com"
API_PATH = "/drive/v3/about?fields=user(emailAddress,permissionId,me)"
CONFIG_LIMIT_BYTES = 1024 * 1024
RESPONSE_LIMIT_BYTES = 64 * 1024
ATTEMPT_TIMEOUT_SECONDS = 8.0
TOTAL_TIMEOUT_SECONDS = 25.0
MAX_ATTEMPTS = 3
TRANSIENT_HTTP_STATUSES = {408, 429, 500, 502, 503, 504}
EMAIL_RE = re.compile(
    r"[A-Za-z0-9.!#$%&'*+/=?^_`{|}~-]+@"
    r"[A-Za-z0-9](?:[A-Za-z0-9.-]{0,251}[A-Za-z0-9])?"
)
PERMISSION_ID_RE = re.compile(r"[A-Za-z0-9_-]{1,256}")


class IdentityError(Exception):
    """A deliberately sanitized, operator-safe failure."""

    def __init__(self, code: str):
        super().__init__(code)
        self.code = code


class _AttemptTimeout(Exception):
    pass


def _reject_duplicate_keys(pairs):
    value = {}
    for key, child in pairs:
        if key in value:
            raise IdentityError("duplicate-json-key")
        value[key] = child
    return value


def _read_stdin_bounded() -> bytearray:
    raw = bytearray(CONFIG_LIMIT_BYTES + 1)
    view = memoryview(raw)
    used = 0
    try:
        while used < len(raw):
            count = sys.stdin.buffer.readinto(view[used:])
            if not count:
                break
            used += count
    finally:
        view.release()
    if used > CONFIG_LIMIT_BYTES:
        for index in range(len(raw)):
            raw[index] = 0
        raise IdentityError("selected-config-too-large")
    del raw[used:]
    return raw


def _trim_ascii(raw: bytearray, start: int, end: int) -> tuple[int, int]:
    while start < end and raw[start] in b" \t\r":
        start += 1
    while end > start and raw[end - 1] in b" \t\r":
        end -= 1
    return start, end


def _selected_remote_credentials(raw: bytearray, remote_name: str) -> tuple[str, str]:
    """Hash the custom client and extract the bearer from one selected config."""
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]{0,63}", remote_name):
        raise IdentityError("unsafe-remote-name")

    section_count = 0
    selected = False
    keys = set()
    backend_type = None
    client_id_range = None
    client_secret_range = None
    token_range = None
    service_account_present = False
    cursor = 0
    length = len(raw)
    while cursor <= length:
        newline = raw.find(b"\n", cursor)
        if newline < 0:
            newline = length
        start, end = _trim_ascii(raw, cursor, newline)
        cursor = newline + 1
        if start == end or raw[start] in (ord("#"), ord(";")):
            if newline == length:
                break
            continue
        if raw[start] == ord("[") and raw[end - 1] == ord("]"):
            section_count += 1
            try:
                section = bytes(raw[start + 1 : end - 1]).decode("ascii")
            except UnicodeError as error:
                raise IdentityError("invalid-selected-config") from error
            selected = section == remote_name
            if newline == length:
                break
            continue
        if section_count != 1 or not selected:
            raise IdentityError("selected-config-section-mismatch")
        equals = raw.find(b"=", start, end)
        if equals < 0:
            raise IdentityError("invalid-selected-config")
        key_start, key_end = _trim_ascii(raw, start, equals)
        value_start, value_end = _trim_ascii(raw, equals + 1, end)
        try:
            key = bytes(raw[key_start:key_end]).decode("ascii").lower()
        except UnicodeError as error:
            raise IdentityError("invalid-selected-config") from error
        if not key or key in keys:
            raise IdentityError("duplicate-selected-config-key")
        keys.add(key)
        if key == "type":
            try:
                backend_type = bytes(raw[value_start:value_end]).decode("ascii")
            except UnicodeError as error:
                raise IdentityError("invalid-selected-config") from error
        elif key == "client_id":
            client_id_range = (value_start, value_end)
        elif key == "client_secret":
            client_secret_range = (value_start, value_end)
        elif key == "token":
            token_range = (value_start, value_end)
        elif key in ("service_account_file", "service_account_credentials"):
            service_account_present = value_start != value_end
        if newline == length:
            break

    if section_count != 1 or not selected:
        raise IdentityError("selected-config-section-mismatch")
    if backend_type != "drive":
        raise IdentityError("selected-config-not-drive")
    if service_account_present:
        raise IdentityError("service-account-config-forbidden")
    if client_id_range is None or client_id_range[0] == client_id_range[1]:
        raise IdentityError("selected-config-client-id-missing")
    if client_secret_range is None or client_secret_range[0] == client_secret_range[1]:
        raise IdentityError("selected-config-client-secret-missing")
    if token_range is None or token_range[0] == token_range[1]:
        raise IdentityError("selected-config-token-missing")

    client_id_bytes = bytearray(raw[client_id_range[0] : client_id_range[1]])
    client_secret_bytes = bytearray(
        raw[client_secret_range[0] : client_secret_range[1]]
    )
    token_bytes = bytearray(raw[token_range[0] : token_range[1]])
    client_hash = None
    token = None
    access_token = None
    try:
        if (
            not 1 <= len(client_id_bytes) <= 16 * 1024
            or client_id_bytes == b"XXX"
            or b"\x00" in client_id_bytes
        ):
            raise IdentityError("selected-config-client-id-invalid")
        try:
            client_id_bytes.decode("utf-8")
        except UnicodeDecodeError as error:
            raise IdentityError("selected-config-client-id-invalid") from error
        if (
            not 1 <= len(client_secret_bytes) <= 16 * 1024
            or client_secret_bytes == b"XXX"
            or b"\x00" in client_secret_bytes
        ):
            raise IdentityError("selected-config-client-secret-invalid")
        client_hash = hashlib.sha256(client_id_bytes).hexdigest()
        try:
            token = json.loads(token_bytes, object_pairs_hook=_reject_duplicate_keys)
        except (UnicodeError, json.JSONDecodeError) as error:
            raise IdentityError("selected-config-token-invalid") from error
        if not isinstance(token, dict):
            raise IdentityError("selected-config-token-invalid")
        access_token = token.get("access_token")
        token_type = token.get("token_type")
        if (
            not isinstance(access_token, str)
            or not (1 <= len(access_token) <= 16 * 1024)
            or not access_token.isascii()
            or any(character.isspace() or ord(character) < 0x21 for character in access_token)
        ):
            raise IdentityError("selected-config-access-token-invalid")
        if not isinstance(token_type, str) or token_type.casefold() != "bearer":
            raise IdentityError("selected-config-token-type-invalid")
        return client_hash, access_token
    finally:
        if isinstance(token, dict):
            token.clear()
        for sensitive in (client_id_bytes, client_secret_bytes, token_bytes):
            for index in range(len(sensitive)):
                sensitive[index] = 0
        client_hash = None
        token = None
        access_token = None


def read_selected_remote_credentials(remote_name: str) -> tuple[str, str]:
    raw = _read_stdin_bounded()
    try:
        return _selected_remote_credentials(raw, remote_name)
    finally:
        for index in range(len(raw)):
            raw[index] = 0


def _alarm_handler(_signum, _frame):
    raise _AttemptTimeout()


@contextmanager
def _wall_clock_timeout(seconds: float):
    if not hasattr(signal, "setitimer") or not hasattr(signal, "ITIMER_REAL"):
        raise IdentityError("wall-clock-timeout-unavailable")
    previous_handler = signal.getsignal(signal.SIGALRM)
    signal.signal(signal.SIGALRM, _alarm_handler)
    signal.setitimer(signal.ITIMER_REAL, seconds)
    try:
        yield
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous_handler)


def _default_connection(timeout: float, context: ssl.SSLContext):
    return http.client.HTTPSConnection(
        API_HOST,
        443,
        timeout=timeout,
        context=context,
    )


def _parse_identity(body: bytearray) -> tuple[str, str]:
    value = None
    user = None
    email = None
    permission_id = None
    try:
        try:
            value = json.loads(body, object_pairs_hook=_reject_duplicate_keys)
        except (UnicodeError, json.JSONDecodeError) as error:
            raise IdentityError("drive-about-json-invalid") from error
        if not isinstance(value, dict) or set(value) != {"user"}:
            raise IdentityError("drive-about-shape-invalid")
        user = value["user"]
        if not isinstance(user, dict) or set(user) != {
            "emailAddress",
            "permissionId",
            "me",
        }:
            raise IdentityError("drive-about-user-shape-invalid")
        if user["me"] is not True:
            raise IdentityError("drive-about-user-is-not-requester")
        if not isinstance(user["emailAddress"], str):
            raise IdentityError("drive-about-email-invalid")
        email = user["emailAddress"].strip().lower()
        if len(email) > 320 or EMAIL_RE.fullmatch(email) is None:
            raise IdentityError("drive-about-email-invalid")
        permission_id = user["permissionId"]
        if not isinstance(permission_id, str) or PERMISSION_ID_RE.fullmatch(permission_id) is None:
            raise IdentityError("drive-about-permission-id-invalid")
        return (
            hashlib.sha256(email.encode("utf-8")).hexdigest(),
            hashlib.sha256(permission_id.encode("utf-8")).hexdigest(),
        )
    finally:
        if isinstance(user, dict):
            user.clear()
        if isinstance(value, dict):
            value.clear()
        email = None
        permission_id = None


def query_drive_identity(
    access_token: str,
    *,
    connection_factory=None,
    sleep=time.sleep,
    monotonic=time.monotonic,
) -> tuple[str, str]:
    if connection_factory is None:
        connection_factory = _default_connection
    context = ssl.create_default_context()
    context.minimum_version = ssl.TLSVersion.TLSv1_2
    deadline = monotonic() + TOTAL_TIMEOUT_SECONDS
    retry_delays = (0.25, 0.5)

    for attempt in range(MAX_ATTEMPTS):
        remaining = deadline - monotonic()
        if remaining <= 0:
            raise IdentityError("drive-about-total-timeout")
        attempt_timeout = min(ATTEMPT_TIMEOUT_SECONDS, remaining)
        connection = None
        response_body = None
        headers = {
            "Accept": "application/json",
            "Authorization": "Bearer " + access_token,
            "User-Agent": "arc-recovery-drive-prefreeze/1",
        }
        try:
            with _wall_clock_timeout(attempt_timeout):
                connection = connection_factory(attempt_timeout, context)
                connection.request("GET", API_PATH, headers=headers)
                response = connection.getresponse()
                content_length = response.getheader("Content-Length")
                if content_length is not None:
                    try:
                        parsed_length = int(content_length)
                    except ValueError as error:
                        raise IdentityError("drive-about-content-length-invalid") from error
                    if parsed_length < 0 or parsed_length > RESPONSE_LIMIT_BYTES:
                        raise IdentityError("drive-about-response-too-large")
                response_body = bytearray()
                while len(response_body) <= RESPONSE_LIMIT_BYTES:
                    chunk = response.read(
                        min(16 * 1024, RESPONSE_LIMIT_BYTES + 1 - len(response_body))
                    )
                    if not chunk:
                        break
                    response_body.extend(chunk)
                    chunk = None
                if len(response_body) > RESPONSE_LIMIT_BYTES:
                    raise IdentityError("drive-about-response-too-large")
                if content_length is not None and len(response_body) != parsed_length:
                    raise IdentityError("drive-about-content-length-mismatch")
            if response.status == 200:
                content_type = response.getheader("Content-Type", "")
                if not content_type.lower().startswith("application/json"):
                    raise IdentityError("drive-about-content-type-invalid")
                return _parse_identity(response_body)
            if response.status not in TRANSIENT_HTTP_STATUSES:
                raise IdentityError(f"drive-about-http-{response.status}")
        except IdentityError:
            raise
        except (_AttemptTimeout, TimeoutError, OSError, http.client.HTTPException, ssl.SSLError):
            if attempt + 1 >= MAX_ATTEMPTS:
                raise IdentityError("drive-about-transport-failed") from None
        finally:
            headers["Authorization"] = ""
            headers.clear()
            if response_body is not None:
                for index in range(len(response_body)):
                    response_body[index] = 0
            if connection is not None:
                try:
                    connection.close()
                except Exception:
                    pass

        if attempt + 1 >= MAX_ATTEMPTS:
            raise IdentityError("drive-about-transient-http-exhausted")
        delay = retry_delays[attempt]
        if monotonic() + delay >= deadline:
            raise IdentityError("drive-about-total-timeout")
        sleep(delay)
    raise IdentityError("drive-about-retry-state-invalid")


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("drive-account-identity: expected one selected remote name", file=sys.stderr)
        return 2
    client_hash = None
    access_token = None
    account_hash = None
    permission_hash = None
    try:
        client_hash, access_token = read_selected_remote_credentials(argv[1])
        account_hash, permission_hash = query_drive_identity(access_token)
        print(client_hash, account_hash, permission_hash)
        return 0
    except IdentityError as error:
        print(f"drive-account-identity: {error.code}", file=sys.stderr)
        return 1
    except Exception:
        print("drive-account-identity: internal-error", file=sys.stderr)
        return 1
    finally:
        client_hash = None
        access_token = None
        account_hash = None
        permission_hash = None
        gc.collect()


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
