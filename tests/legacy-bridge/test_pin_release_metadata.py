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


if __name__ == "__main__":
    unittest.main()
