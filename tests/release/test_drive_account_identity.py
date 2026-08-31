#!/usr/bin/env python3
import hashlib
import importlib.util
import pathlib
import ssl
import unittest


REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
HELPER_PATH = REPO_ROOT / "scripts" / "recovery" / "drive-account-identity.py"
SPEC = importlib.util.spec_from_file_location("drive_account_identity", HELPER_PATH)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


def digest(value):
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def success_body(email="Recovery@ARC.Example", permission_id="0123456789_arc"):
    return (
        '{"user":{"emailAddress":"%s","permissionId":"%s","me":true}}'
        % (email, permission_id)
    ).encode("utf-8")


class FakeResponse:
    def __init__(self, status=200, body=None, headers=None):
        self.status = status
        self.body = success_body() if body is None else body
        self.offset = 0
        self.headers = {"Content-Type": "application/json"}
        if headers:
            self.headers.update(headers)

    def getheader(self, name, default=None):
        return self.headers.get(name, default)

    def read(self, limit):
        chunk = self.body[self.offset : self.offset + limit]
        self.offset += len(chunk)
        return chunk


class FakeConnection:
    def __init__(self, outcome, requests):
        self.outcome = outcome
        self.requests = requests
        self.closed = False

    def request(self, method, path, headers):
        self.requests.append((method, path, dict(headers)))
        if isinstance(self.outcome, Exception):
            raise self.outcome

    def getresponse(self):
        return self.outcome

    def close(self):
        self.closed = True


def connection_factory(outcomes, requests, timeouts, contexts):
    queue = list(outcomes)

    def factory(timeout, context):
        timeouts.append(timeout)
        contexts.append(context)
        return FakeConnection(queue.pop(0), requests)

    return factory


class SelectedConfigTests(unittest.TestCase):
    def test_extracts_only_selected_drive_bearer_token(self):
        raw = bytearray(
            b"[arc-recovery-drive]\n"
            b"type = drive\n"
            b"client_id = custom.apps.googleusercontent.com\n"
            b"client_secret = private-value\n"
            b"token = {\"access_token\":\"fresh-access-token\","
            b"\"token_type\":\"Bearer\",\"refresh_token\":\"private-refresh\"}\n"
        )
        self.assertEqual(
            MODULE._selected_remote_token(raw, "arc-recovery-drive"),
            "fresh-access-token",
        )

    def test_rejects_extra_section_duplicate_token_and_service_account(self):
        fixtures = (
            b"[wanted]\ntype=drive\ntoken={\"access_token\":\"a\",\"token_type\":\"Bearer\"}\n[other]\n",
            b"[wanted]\ntype=drive\ntoken={\"access_token\":\"a\",\"access_token\":\"b\",\"token_type\":\"Bearer\"}\n",
            b"[wanted]\ntype=drive\nservice_account_file=/private/key\ntoken={\"access_token\":\"a\",\"token_type\":\"Bearer\"}\n",
        )
        for fixture in fixtures:
            with self.subTest(fixture=fixture):
                with self.assertRaises(MODULE.IdentityError):
                    MODULE._selected_remote_token(bytearray(fixture), "wanted")


class DriveAboutTests(unittest.TestCase):
    def query(self, outcomes):
        self.requests = []
        self.timeouts = []
        self.contexts = []
        self.sleeps = []
        return MODULE.query_drive_identity(
            "unit-test-access-token",
            connection_factory=connection_factory(
                outcomes, self.requests, self.timeouts, self.contexts
            ),
            sleep=self.sleeps.append,
        )

    def test_exact_about_query_returns_only_hashes(self):
        account_hash, permission_hash = self.query([FakeResponse()])
        self.assertEqual(account_hash, digest("recovery@arc.example"))
        self.assertEqual(permission_hash, digest("0123456789_arc"))
        self.assertEqual(len(self.requests), 1)
        method, path, headers = self.requests[0]
        self.assertEqual(method, "GET")
        self.assertEqual(path, MODULE.API_PATH)
        self.assertEqual(headers["Authorization"], "Bearer unit-test-access-token")
        self.assertEqual(headers["Accept"], "application/json")
        self.assertLessEqual(self.timeouts[0], MODULE.ATTEMPT_TIMEOUT_SECONDS)
        self.assertGreaterEqual(
            self.contexts[0].minimum_version, ssl.TLSVersion.TLSv1_2
        )

    def test_transient_http_retries_but_403_does_not(self):
        self.query([FakeResponse(status=503, body=b"{}"), FakeResponse()])
        self.assertEqual(len(self.requests), 2)
        self.assertEqual(self.sleeps, [0.25])

        with self.assertRaisesRegex(MODULE.IdentityError, "drive-about-http-403"):
            self.query([FakeResponse(status=403, body=b'{"error":"denied"}')])
        self.assertEqual(len(self.requests), 1)
        self.assertEqual(self.sleeps, [])

    def test_malformed_multiple_or_nonrequester_identity_fails_closed(self):
        invalid_bodies = (
            b"not-json",
            b'{"user":{"emailAddress":["a@arc.example","b@arc.example"],"permissionId":"1","me":true}}',
            b'{"user":{"emailAddress":"a@arc.example","emailAddress":"b@arc.example","permissionId":"1","me":true}}',
            b'{"user":{"emailAddress":"a@arc.example","permissionId":"1","me":false}}',
            b'{"user":{"emailAddress":"a@arc.example","permissionId":"bad value","me":true}}',
            b'{"user":{"emailAddress":"a@arc.example","permissionId":"1","me":true,"displayName":"extra"}}',
            b'{"user":{"emailAddress":"a@arc.example","permissionId":"1","me":true},"kind":"drive#about"}',
        )
        for body in invalid_bodies:
            with self.subTest(body=body):
                with self.assertRaises(MODULE.IdentityError):
                    self.query([FakeResponse(body=body)])

    def test_oversize_and_wrong_content_type_fail_closed(self):
        with self.assertRaisesRegex(MODULE.IdentityError, "response-too-large"):
            self.query(
                [
                    FakeResponse(
                        body=b"{}",
                        headers={"Content-Length": str(MODULE.RESPONSE_LIMIT_BYTES + 1)},
                    )
                ]
            )
        with self.assertRaisesRegex(MODULE.IdentityError, "response-too-large"):
            self.query([FakeResponse(body=b"x" * (MODULE.RESPONSE_LIMIT_BYTES + 1))])
        with self.assertRaisesRegex(MODULE.IdentityError, "content-type-invalid"):
            self.query([FakeResponse(headers={"Content-Type": "text/html"})])

    def test_transport_retries_are_bounded_and_errors_never_include_token(self):
        with self.assertRaises(MODULE.IdentityError) as raised:
            self.query([TimeoutError(), TimeoutError(), TimeoutError()])
        self.assertEqual(len(self.requests), 3)
        self.assertEqual(self.sleeps, [0.25, 0.5])
        self.assertNotIn("unit-test-access-token", str(raised.exception))


if __name__ == "__main__":
    unittest.main()
