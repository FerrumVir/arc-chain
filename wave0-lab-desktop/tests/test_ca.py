"""Tests for lib/ca.py, the per-run throwaway CA of the desktop lab (THROWAWAY LAB FILE)."""
from __future__ import annotations

import hashlib
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

import _paths  # noqa: F401
import ca


def openssl(*args):
    done = subprocess.run([ca.openssl_binary()] + list(args), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=False)
    return done.returncode, done.stdout.decode("utf-8", "replace")


@unittest.skipUnless(shutil.which("openssl"), "needs the openssl command line")
class CaTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        cls.out = Path(cls.tmp.name) / "ca"
        cls.hosts = ["github.com", "api.github.com", "objects.githubusercontent.com"]
        cls.info = ca.make_ca(cls.out, cls.hosts)

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def test_the_server_certificate_chains_to_the_ca(self):
        code, text = openssl("verify", "-CAfile", self.info["ca_cert"], self.info["server_cert"])
        self.assertEqual(code, 0, text)
        self.assertIn("OK", text)

    def test_ca_sha256_is_the_sha256_of_the_der_certificate(self):
        done = subprocess.run([ca.openssl_binary(), "x509", "-in", self.info["ca_cert"], "-outform", "DER"], stdout=subprocess.PIPE, check=True)
        self.assertEqual(self.info["ca_sha256"], hashlib.sha256(done.stdout).hexdigest())
        self.assertEqual((self.out / "ca.sha256").read_text().strip(), self.info["ca_sha256"])
        self.assertRegex(self.info["ca_sha256"], r"^[0-9a-f]{64}$")

    def test_san_is_exactly_the_hostnames(self):
        code, text = openssl("x509", "-in", self.info["server_cert"], "-noout", "-text")
        self.assertEqual(code, 0, text)
        san = re.search(r"Subject Alternative Name:\s*\n\s*(.+)", text)
        self.assertIsNotNone(san, text)
        names = sorted(item.strip()[4:] for item in san.group(1).split(","))
        self.assertEqual(names, sorted(self.hosts))

    def test_certificates_are_ecdsa_p256_with_the_right_constraints(self):
        _, server = openssl("x509", "-in", self.info["server_cert"], "-noout", "-text")
        _, authority = openssl("x509", "-in", self.info["ca_cert"], "-noout", "-text")
        for text in (server, authority):
            self.assertRegex(text, r"prime256v1|P-256")
            self.assertRegex(text.lower(), r"ecdsa-with-sha256")
        self.assertIn("CA:FALSE", server)
        self.assertIn("TLS Web Server Authentication", server)
        self.assertIn("CA:TRUE", authority)
        self.assertRegex(authority, r"pathlen:\s*0")
        self.assertIn("Certificate Sign", authority)

    def test_validity_is_one_day(self):
        _, text = openssl("x509", "-in", self.info["ca_cert"], "-noout", "-dates")
        self.assertRegex(text, r"notBefore=")
        import datetime

        def parse(label):
            raw = re.search(label + r"=(.+)", text).group(1).strip()
            return datetime.datetime.strptime(raw.replace(" GMT", ""), "%b %d %H:%M:%S %Y")

        span = parse("notAfter") - parse("notBefore")
        self.assertLessEqual(span, datetime.timedelta(days=1, seconds=60))
        self.assertGreater(span, datetime.timedelta(hours=23))

    def test_private_keys_stay_under_private_and_are_not_public(self):
        self.assertTrue((self.out / "private" / "ca.key").is_file())
        self.assertTrue((self.out / "private" / "server.key").is_file())
        public = ca.public_files(self.out)
        self.assertEqual(sorted(path.name for path in public), ["ca.crt", "ca.sha256"])
        for path in public:
            self.assertNotRegex(path.read_text(), r"PRIVATE KEY")
        # the server certificate and key are used by the recording server, they are not evidence files
        self.assertNotIn("server.crt", [path.name for path in public])

    def test_copy_public_copies_only_the_two_public_files(self):
        with tempfile.TemporaryDirectory() as dest:
            copied = ca.copy_public(self.out, dest)
            self.assertEqual(sorted(path.name for path in copied), ["ca.crt", "ca.sha256"])
            self.assertEqual(sorted(path.name for path in Path(dest).iterdir()), ["ca.crt", "ca.sha256"])
            self.assertEqual(ca.scan_for_private_keys(dest), [])

    def test_scan_finds_every_private_key_and_nothing_else(self):
        leaks = ca.scan_for_private_keys(self.out)
        names = sorted(Path(item).name for item in leaks)
        self.assertIn("ca.key", names)
        self.assertIn("server.key", names)
        self.assertNotIn("ca.crt", names)
        self.assertNotIn("ca.sha256", names)

    def test_scan_is_relative_to_the_root_not_to_a_parent_called_private(self):
        with tempfile.TemporaryDirectory() as tmp:
            nested = Path(tmp) / "private" / "evidence"
            nested.mkdir(parents=True)
            (nested / "result.json").write_text("{}\n")
            self.assertEqual(ca.scan_for_private_keys(nested), [], "the root may live below a directory called private (macOS /private/tmp)")

    def test_scan_catches_a_key_copied_into_the_evidence_whatever_its_name(self):
        with tempfile.TemporaryDirectory() as tmp:
            Path(tmp, "innocent.txt").write_text(Path(self.info["server_key"]).read_text())
            Path(tmp, "other.txt").write_text("-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n")
            Path(tmp, "fine.txt").write_text("a public certificate would say BEGIN CERTIFICATE\n")
            found = sorted(Path(item).name for item in ca.scan_for_private_keys(tmp))
            self.assertEqual(found, ["innocent.txt", "other.txt"])

    def test_public_files_refuse_a_directory_whose_public_file_holds_a_key(self):
        with tempfile.TemporaryDirectory() as tmp:
            Path(tmp, "ca.crt").write_text("-----BEGIN EC PRIVATE KEY-----\nAAAA\n-----END EC PRIVATE KEY-----\n")
            Path(tmp, "ca.sha256").write_text("0" * 64 + "\n")
            with self.assertRaises(ca.CaError):
                ca.public_files(tmp)

    def test_hostname_validation(self):
        self.assertEqual(ca.validate_hostnames(["GitHub.com", "github.com", " api.github.com "]), ["github.com", "api.github.com"])
        for bad in ("", "a b", "*.github.com", "-x.com", "x..com", "ex_ample.com"):
            with self.subTest(bad):
                names = [bad] if bad else []
                with self.assertRaises(ca.CaError):
                    ca.validate_hostnames(names)

    def test_every_run_gets_its_own_authority(self):
        with tempfile.TemporaryDirectory() as tmp:
            other = ca.make_ca(Path(tmp) / "other", ["github.com"])
        self.assertNotEqual(other["ca_sha256"], self.info["ca_sha256"])

    def test_pem_to_der_rejects_text_without_a_certificate(self):
        with self.assertRaises(ca.CaError):
            ca.pem_to_der("nothing here")

    def test_cli_make_public_scan(self):
        import contextlib
        import io

        with tempfile.TemporaryDirectory() as tmp, contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            out, dest = Path(tmp) / "c", Path(tmp) / "pub"
            self.assertEqual(ca.main(["make", "--out", str(out), "--hosts", "a.example.com,b.example.com"]), 0)
            self.assertEqual(ca.main(["public", "--out", str(out), "--dest", str(dest)]), 0)
            self.assertEqual(ca.main(["scan", "--root", str(dest)]), 0)
            self.assertEqual(ca.main(["scan", "--root", str(out)]), 1, "the full directory holds private keys")


if __name__ == "__main__":
    unittest.main()
