"""R3: the release signing paths, exercised with clearly labelled FIXTURE keys.

Nothing here is, or may be described as, a signed production release. Each
test generates a throwaway key, signs fixture bytes, and runs the exact
verification the shipped code runs, so the path itself is proven to accept
the right signature and refuse everything else.

`install.sh` hard-codes the owner's key (deliberately: no override), so the
installer's check is replicated here with a fixture key, and a separate test
pins that `install.sh` still contains exactly this command.

    python3 -m unittest scripts/release/tests/test_signing_fixtures.py
"""

from __future__ import annotations

import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
NAMESPACE = "arc-release-manifest-v1"
IDENTITY = "arc-release"


def ssh_keygen(*args: str, stdin: bytes | None = None) -> subprocess.CompletedProcess:
    return subprocess.run(["ssh-keygen", *args], input=stdin, capture_output=True, check=False)


@unittest.skipUnless(shutil.which("ssh-keygen"), "ssh-keygen is required")
class InstallerChecksumSignature(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory(prefix="arc-FIXTURE-signing-")
        self.path = Path(self.dir.name)
        self.key = self.path / "FIXTURE-release-key"
        made = ssh_keygen("-q", "-t", "ed25519", "-N", "", "-C", "FIXTURE-not-a-release-key",
                          "-f", str(self.key))
        self.assertEqual(made.returncode, 0, made.stderr)
        public = (self.path / "FIXTURE-release-key.pub").read_text().split()
        self.allowed = self.path / "allowed-signers"
        # Same shape as install.sh: identity, namespace restriction, key.
        self.allowed.write_text(
            f'{IDENTITY} namespaces="{NAMESPACE}" {public[0]} {public[1]} {NAMESPACE}\n')
        self.sums = b"0" * 64 + b"  arc-node-linux-x86_64\n"

    def tearDown(self):
        self.dir.cleanup()

    def sign(self, payload: bytes, namespace: str = NAMESPACE, key: Path | None = None) -> Path:
        target = self.path / "SHA256SUMS"
        target.write_bytes(payload)
        signed = ssh_keygen("-Y", "sign", "-f", str(key or self.key), "-n", namespace, str(target))
        self.assertEqual(signed.returncode, 0, signed.stderr)
        return self.path / "SHA256SUMS.sig"

    def verify(self, payload: bytes, signature: Path) -> bool:
        # The exact command install.sh runs.
        result = ssh_keygen("-Y", "verify", "-f", str(self.allowed), "-I", IDENTITY,
                            "-n", NAMESPACE, "-s", str(signature), stdin=payload)
        return result.returncode == 0

    def test_the_owner_signature_over_the_exact_checksums_is_accepted(self):
        self.assertTrue(self.verify(self.sums, self.sign(self.sums)))

    def test_any_change_to_the_checksums_is_refused(self):
        signature = self.sign(self.sums)
        self.assertFalse(self.verify(self.sums.replace(b"0", b"1", 1), signature))
        self.assertFalse(self.verify(self.sums + b"\n", signature))

    def test_a_signature_for_another_namespace_is_refused(self):
        self.assertFalse(self.verify(self.sums, self.sign(self.sums, namespace="file")))

    def test_a_key_that_is_not_the_owner_is_refused(self):
        other = self.path / "FIXTURE-other-key"
        made = ssh_keygen("-q", "-t", "ed25519", "-N", "", "-f", str(other))
        self.assertEqual(made.returncode, 0, made.stderr)
        self.assertFalse(self.verify(self.sums, self.sign(self.sums, key=other)))

    def test_a_malformed_signature_is_refused(self):
        bad = self.path / "bad.sig"
        bad.write_text("-----BEGIN SSH SIGNATURE-----\nAAAA\n-----END SSH SIGNATURE-----\n")
        self.assertFalse(self.verify(self.sums, bad))


class InstallerStillRunsThisCheck(unittest.TestCase):
    def test_install_sh_verifies_checksums_with_the_owner_key_and_namespace(self):
        script = (REPO / "install.sh").read_text()
        self.assertRegex(script, re.escape(f'namespaces="{NAMESPACE}" ssh-ed25519 '))
        command = re.search(r"ssh-keygen -Y verify \\\s*\n(?:\s*-\S+ .*\\\s*\n)+", script)
        self.assertIsNotNone(command, "install.sh no longer runs ssh-keygen -Y verify")
        text = command.group(0)
        for flag in (f"-I {IDENTITY}", f"-n {NAMESPACE}", '-s "$TMP_DIR/SHA256SUMS.sig"'):
            self.assertIn(flag, text)
        self.assertIn('die "Release SHA256SUMS signature is invalid or not owner-authorized"', script)


if __name__ == "__main__":
    unittest.main()
