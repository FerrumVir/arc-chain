import importlib.util
import io
import os
from pathlib import Path
import unittest
from unittest.mock import patch
import urllib.error
import urllib.request


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("pin_release", ROOT / "scripts/legacy-bridge/pin-release.py")
PIN_RELEASE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PIN_RELEASE)


class ReleaseMetadataTests(unittest.TestCase):
    def test_authenticated_metadata_never_passes_credentials_to_asset_fetches(self):
        with patch.dict(os.environ, {"GH_TOKEN": "fixture-metadata-credential"}, clear=True):
            with patch.object(urllib.request, "build_opener") as build:
                build.return_value.open.return_value = io.BytesIO(b'{"immutable": true}')
                self.assertEqual(PIN_RELEASE.release_metadata("v0.8.27"), {"immutable": True})
                request = build.return_value.open.call_args.args[0]
                self.assertEqual(request.full_url, "https://api.github.com/repos/FerrumVir/arc-chain/releases/tags/v0.8.27")
                self.assertEqual(request.get_header("Authorization"), "Bearer fixture-metadata-credential")
                self.assertIsInstance(build.call_args.args[0], PIN_RELEASE.RejectMetadataRedirects)
            with patch.object(urllib.request, "urlopen", return_value=io.BytesIO(b"manifest")) as fetch:
                self.assertEqual(PIN_RELEASE.fetch("https://github.com/FerrumVir/arc-chain/releases/download/v0.8.27/SHA256SUMS"), b"manifest")
                self.assertIsNone(fetch.call_args.args[0].get_header("Authorization"))

    def test_anonymous_metadata_and_api_failure_remain_explicit(self):
        with patch.dict(os.environ, {}, clear=True):
            with patch.object(urllib.request, "build_opener") as build:
                build.return_value.open.return_value = io.BytesIO(b'{}')
                self.assertEqual(PIN_RELEASE.release_metadata("v0.8.27"), {})
                self.assertIsNone(build.return_value.open.call_args.args[0].get_header("Authorization"))
                failure = urllib.error.HTTPError("https://api.github.com", 403, "rate limit exceeded", {}, None)
                build.return_value.open.side_effect = failure
                with self.assertRaises(urllib.error.HTTPError) as raised:
                    PIN_RELEASE.release_metadata("v0.8.27")
                self.assertIs(raised.exception, failure)

    def test_metadata_redirects_are_refused_for_both_origins(self):
        handler = PIN_RELEASE.RejectMetadataRedirects()
        request = urllib.request.Request("https://api.github.com/repos/FerrumVir/arc-chain/releases/tags/v0.8.27")
        for url in ("https://api.github.com/redirect", "https://example.invalid/redirect"):
            self.assertIsNone(handler.redirect_request(request, None, 302, "Found", {}, url))

    def test_oversized_metadata_is_rejected(self):
        with patch.object(urllib.request, "build_opener") as build:
            build.return_value.open.return_value = io.BytesIO(b" " * (4 * 1024 * 1024 + 1))
            with self.assertRaisesRegex(ValueError, "exceeds 4 MiB"):
                PIN_RELEASE.release_metadata("v0.8.27")


class ShardWorkerPinExtractionTests(unittest.TestCase):
    SAMPLE_INSTALL_SH = "\n".join(
        [
            "SHARD_COORDINATOR_URL=https://149.28.153.31",
            "SHARD_COORDINATOR_ID=f521309b041da7aefc742548bdc002c31b47183aacfbbbf245ded09845d0415b",
            "SHARD_TRANSACTION_DOMAIN=3b9147e44d878646caf2d0729d06bd2e082b265e0525760595bd10605f37a10f",
            "SHARD_EXPECTED_MODEL_ID=934efc12a2ed8372a944e5aaedf059a8a0f42c0906f6b2f1fb3626bdeb1ffa67",
            "SHARD_REQUIRED_ADMITTED_LAYERS=6",
        ]
    ) + "\n"

    def test_extracts_valid_shard_pin_from_sample_and_repo_install_sh(self):
        parsed = PIN_RELEASE.parse_shard_worker_pin(self.SAMPLE_INSTALL_SH, "v0.8.27")
        self.assertEqual(
            parsed,
            {
                "coordinator_url": "https://149.28.153.31",
                "coordinator_id": "f521309b041da7aefc742548bdc002c31b47183aacfbbbf245ded09845d0415b",
                "transaction_domain": "3b9147e44d878646caf2d0729d06bd2e082b265e0525760595bd10605f37a10f",
                "expected_model_id": "934efc12a2ed8372a944e5aaedf059a8a0f42c0906f6b2f1fb3626bdeb1ffa67",
                "required_admitted_layers": 6,
            },
        )
        repo_install = (ROOT / "install.sh").read_text(encoding="utf-8")
        repo_parsed = PIN_RELEASE.parse_shard_worker_pin(repo_install, "worktree")
        self.assertEqual(repo_parsed, parsed)

    def test_rejects_missing_or_duplicate_shard_assignments(self):
        for var in (
            "SHARD_COORDINATOR_URL",
            "SHARD_COORDINATOR_ID",
            "SHARD_TRANSACTION_DOMAIN",
            "SHARD_EXPECTED_MODEL_ID",
            "SHARD_REQUIRED_ADMITTED_LAYERS",
        ):
            missing = "\n".join(
                line for line in self.SAMPLE_INSTALL_SH.splitlines() if not line.startswith(f"{var}=")
            ) + "\n"
            with self.assertRaisesRegex(ValueError, var):
                PIN_RELEASE.parse_shard_worker_pin(missing, "v0.8.27")

            duplicate = self.SAMPLE_INSTALL_SH + f"{var}=extra\n"
            with self.assertRaisesRegex(ValueError, var):
                PIN_RELEASE.parse_shard_worker_pin(duplicate, "v0.8.27")

    def test_rejects_invalid_coordinator_urls_digests_and_layer_floors(self):
        for bad_url in (
            "http://149.28.153.31",
            "https://149.28.153.31/",
            "https://149.28.153.31/api",
            "https://149.28.153.31?x=1",
            "https://149.28.153.31#frag",
            "https://user@149.28.153.31",
            "https://user:pass@149.28.153.31",
            "https://149.28.153.31:0",
            "https://149.28.153.31:0443",
            "https://999.28.153.31",
        ):
            tampered = self.SAMPLE_INSTALL_SH.replace("https://149.28.153.31", bad_url)
            with self.assertRaisesRegex(ValueError, "SHARD_COORDINATOR_URL", msg=bad_url):
                PIN_RELEASE.parse_shard_worker_pin(tampered, "v0.8.27")

        for field, original in (
            ("SHARD_COORDINATOR_ID", "f521309b041da7aefc742548bdc002c31b47183aacfbbbf245ded09845d0415b"),
            ("SHARD_TRANSACTION_DOMAIN", "3b9147e44d878646caf2d0729d06bd2e082b265e0525760595bd10605f37a10f"),
            ("SHARD_EXPECTED_MODEL_ID", "934efc12a2ed8372a944e5aaedf059a8a0f42c0906f6b2f1fb3626bdeb1ffa67"),
        ):
            for bad_digest in ("0" * 64, "a" * 63, "A" * 64):
                tampered = self.SAMPLE_INSTALL_SH.replace(original, bad_digest)
                with self.assertRaisesRegex(ValueError, field, msg=f"{field}={bad_digest}"):
                    PIN_RELEASE.parse_shard_worker_pin(tampered, "v0.8.27")

        for bad_layers in ("0", "1", "5", "06", "six"):
            tampered = self.SAMPLE_INSTALL_SH.replace(
                "SHARD_REQUIRED_ADMITTED_LAYERS=6",
                f"SHARD_REQUIRED_ADMITTED_LAYERS={bad_layers}",
            )
            with self.assertRaisesRegex(ValueError, "SHARD_REQUIRED_ADMITTED_LAYERS", msg=bad_layers):
                PIN_RELEASE.parse_shard_worker_pin(tampered, "v0.8.27")


if __name__ == "__main__":
    unittest.main()
