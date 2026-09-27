"""Tests for the build-local-artifacts verifier. Synthetic OUTPUT_DIRs, nothing built."""

import hashlib
import importlib.util
import io
import json
import os
import shutil
import tarfile
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

SPEC = importlib.util.spec_from_file_location(
    "verify_local_artifacts", Path(__file__).resolve().parents[1] / "verify_local_artifacts.py")
v = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(v)

NODE = b"\x7fELF arc-node release binary"
CLI = b"\x7fELF arc cli"
DMG = b"koly synthetic dmg"
NOT_A_RELEASE = ("UNSIGNED desktop bundle built locally by scripts/release/build-local-artifacts.sh\n"
                 "on 2026-09-22T03:00:00Z from revision 757935ed.\n\n"
                 "This is NOT a signed production release. bundle.createUpdaterArtifacts was\n"
                 "force-disabled for this build.\n")


def sha(data):
    return hashlib.sha256(data).hexdigest()


def write(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb" if isinstance(data, bytes) else "w") as fh:
        fh.write(data)


def app_tarball(path, stapled=False, extra=None):
    """A .app archived as the script does (tar -czf of the bundle directory)."""
    work = tempfile.mkdtemp()
    try:
        app = os.path.join(work, "ARC.app", "Contents")
        write(os.path.join(app, "MacOS", "ARC"), b"mach-o")
        write(os.path.join(app, "_CodeSignature", "CodeResources"), b"ad-hoc signature")
        if stapled:
            write(os.path.join(app, "CodeResources"), b"stapled notarization ticket")
        if extra:
            write(os.path.join(work, "ARC.app", extra), b"x")
        with tarfile.open(path, "w:gz") as tar:
            tar.add(os.path.join(work, "ARC.app"), arcname="ARC.app")
    finally:
        shutil.rmtree(work)


def write_sums(out):
    collected = os.path.join(out, "collected")
    lines = []
    for rel in sorted(v._files(collected)):
        with open(os.path.join(collected, rel), "rb") as fh:
            lines.append(f"{sha(fh.read())}  {rel}")
    write(os.path.join(out, "SHA256SUMS"), "\n".join(lines) + "\n")


def make_output(root, cli=True):
    out = os.path.join(root, "out")
    collected = os.path.join(out, "collected")
    desktop = os.path.join(out, "desktop-unsigned")
    prov = os.path.join(out, "arc-node-provenance")
    os.makedirs(desktop)
    app_tarball(os.path.join(desktop, "UNSIGNED-ARC.app.tar.gz"))
    write(os.path.join(desktop, "UNSIGNED-ARC_0.8.0_aarch64.dmg"), DMG)
    write(os.path.join(desktop, "NOT-A-RELEASE.txt"), NOT_A_RELEASE)
    os.makedirs(collected)
    for name in ("UNSIGNED-ARC.app.tar.gz", "UNSIGNED-ARC_0.8.0_aarch64.dmg"):
        shutil.copyfile(os.path.join(desktop, name), os.path.join(collected, name))
    digest = sha(NODE)
    immutable = os.path.join(prov, f"arc-node-{digest[:16]}")
    write(immutable, NODE)
    write(os.path.join(collected, "arc-node"), NODE)
    record = os.path.join(prov, "provenance-20260922T030000Z.txt")
    write(record, "\n".join([
        "=== ARC build provenance ===",
        "recorded_utc:     20260922T030000Z",
        "command:          cargo build -p arc-node --locked --release",
        "source_revision:  757935ed",
        "input_digest:     " + "cd" * 32 + "   (content of crates/**, scripts/**, Cargo.toml, Cargo.lock)",
        "profile:          release",
        "--- building ---",
        "build_exit:       0",
        "input_digest_after_build: " + "cd" * 32 + "   (unchanged)",
        "binary_source:    target/release/arc-node",
        "binary_sha256:    " + digest,
        f"binary_bytes:     {len(NODE)}",
        f"immutable_copy:   {immutable}   (read-only; this is what the run executes)",
        f"manifest:         {record}", ""]))
    write(os.path.join(prov, "build-20260922T030000Z.log"), "Finished release\n")
    os.symlink(record, os.path.join(prov, "latest-provenance.txt"))
    os.symlink(immutable, os.path.join(prov, "latest-arc-node"))
    if cli:
        write(os.path.join(collected, "arc-cli"), CLI)
        write(os.path.join(out, "arc-cli-provenance.txt"),
              f"binary:        arc (package crates/arc-cli)\nsource_revision: 757935ed\nprofile:       release\n"
              f"binary_sha256: {sha(CLI)}\nbinary_bytes:  {len(CLI)}\n")
    write_sums(out)
    write(os.path.join(out, "arc-chain.cdx.json"), json.dumps({
        "bomFormat": "CycloneDX", "specVersion": "1.5", "version": 1,
        "components": [{"type": "library", "name": "serde", "version": "1.0.0", "bom-ref": "pkg:cargo/serde@1.0.0"}]}))
    return out


def snapshot(root):
    out = {}
    for base, dirs, files in os.walk(root):
        rel = os.path.relpath(base, root)
        out[("dir", rel)] = os.lstat(base).st_mtime_ns
        for f in files:
            p = os.path.join(base, f)
            if os.path.islink(p):
                out[("link", os.path.join(rel, f))] = os.readlink(p)
                continue
            with open(p, "rb") as fh:
                out[("file", os.path.join(rel, f))] = (sha(fh.read()), os.stat(p).st_mtime_ns)
    return out


def run(out):
    stdout, stderr = io.StringIO(), io.StringIO()
    with redirect_stdout(stdout), redirect_stderr(stderr):
        rc = v.main([out])
    text = stdout.getvalue()
    return rc, (json.loads(text) if text.strip() else None)


class Verify(unittest.TestCase):
    def setUp(self):
        self.root = tempfile.mkdtemp()
        self.out = make_output(self.root)

    def tearDown(self):
        shutil.rmtree(self.root, ignore_errors=True)

    def failing(self, check):
        before = snapshot(self.out)
        rc, report = run(self.out)
        self.assertEqual(rc, 1)
        self.assertEqual(report["status"], "FAIL")
        self.assertFalse(report["checks"][check]["pass"])
        self.assertEqual(snapshot(self.out), before)  # a failing verification writes nothing either
        return report["checks"][check]

    def test_a_complete_build_passes_and_is_left_untouched(self):
        before = snapshot(self.out)
        rc, report = run(self.out)
        self.assertEqual(rc, 0, json.dumps(report, indent=1))
        self.assertEqual(report["status"], "PASS")
        self.assertEqual(report["checks"]["checksums"]["entries"], 4)
        self.assertEqual(report["checks"]["provenance"]["binary_sha256"], sha(NODE))
        self.assertEqual(report["checks"]["sbom"]["components"], 1)
        self.assertEqual(snapshot(self.out), before)

    def test_a_build_without_the_cli_passes(self):
        root = tempfile.mkdtemp()
        try:
            rc, report = run(make_output(root, cli=False))
            self.assertEqual(rc, 0)
            self.assertEqual(report["checks"]["provenance"]["arc_cli"], {"built": False})
        finally:
            shutil.rmtree(root)

    def test_a_digest_that_no_longer_matches_fails(self):
        write(os.path.join(self.out, "collected", "UNSIGNED-ARC_0.8.0_aarch64.dmg"), b"changed after summing")
        self.assertIn("UNSIGNED-ARC_0.8.0_aarch64.dmg", self.failing("checksums")["mismatched"])

    def test_a_collected_file_left_out_of_the_sums_fails(self):
        write(os.path.join(self.out, "collected", "stray.bin"), b"left over from an earlier run")
        self.assertEqual(self.failing("checksums")["omitted"], ["stray.bin"])

    def test_an_entry_for_a_missing_file_fails(self):
        os.remove(os.path.join(self.out, "collected", "arc-cli"))
        os.remove(os.path.join(self.out, "arc-cli-provenance.txt"))
        self.assertEqual(self.failing("checksums")["missing"], ["arc-cli"])

    def test_missing_sums_fail(self):
        os.remove(os.path.join(self.out, "SHA256SUMS"))
        self.assertEqual(self.failing("checksums")["problem"], "SHA256SUMS missing")

    def test_a_desktop_artifact_that_was_never_collected_fails(self):
        # The script copies desktop artifacts with `cp ... || true`: a failed copy
        # would leave them out of collected/ and out of SHA256SUMS silently.
        os.remove(os.path.join(self.out, "collected", "UNSIGNED-ARC_0.8.0_aarch64.dmg"))
        write_sums(self.out)
        self.assertEqual(self.failing("checksums")["desktop_not_collected"], ["UNSIGNED-ARC_0.8.0_aarch64.dmg"])

    def test_a_path_outside_collected_is_never_read(self):
        with open(os.path.join(self.out, "SHA256SUMS"), "a") as fh:
            fh.write(f"{'0' * 64}  ../arc-chain.cdx.json\n{'0' * 64}  /etc/hosts\n")
        self.assertEqual(len(self.failing("checksums")["invalid_lines"]), 2)

    def test_a_node_binary_that_differs_from_its_record_fails(self):
        write(os.path.join(self.out, "collected", "arc-node"), b"a different binary")
        write_sums(self.out)
        problems = self.failing("provenance")["problems"]
        self.assertIn("collected/arc-node differs from the record's binary_sha256", problems)

    def test_an_immutable_copy_that_differs_fails(self):
        prov = os.path.join(self.out, "arc-node-provenance")
        immutable = [n for n in os.listdir(prov) if n.startswith("arc-node-")][0]
        write(os.path.join(prov, immutable), b"swapped")
        self.assertIn("the immutable copy's digest differs from the record", self.failing("provenance")["problems"])

    def test_a_failed_build_record_fails(self):
        prov = os.path.join(self.out, "arc-node-provenance")
        record = os.path.join(prov, "provenance-20260922T030000Z.txt")
        with open(record) as fh:
            text = fh.read().replace("build_exit:       0", "build_exit:       101")
        write(record, text)
        self.assertIn("the record reports build_exit '101'", self.failing("provenance")["problems"])

    def test_several_records_without_latest_are_ambiguous(self):
        prov = os.path.join(self.out, "arc-node-provenance")
        os.remove(os.path.join(prov, "latest-provenance.txt"))
        shutil.copyfile(os.path.join(prov, "provenance-20260922T030000Z.txt"),
                        os.path.join(prov, "provenance-20260921T010000Z.txt"))
        self.assertTrue(any("ambiguous" in p for p in self.failing("provenance")["problems"]))

    def test_a_cli_that_differs_from_its_record_fails(self):
        write(os.path.join(self.out, "collected", "arc-cli"), b"other cli")
        write_sums(self.out)
        self.assertIn("collected/arc-cli differs from arc-cli-provenance.txt", self.failing("provenance")["problems"])

    def test_the_sbom_must_be_cyclonedx_1_5_with_components(self):
        path = os.path.join(self.out, "arc-chain.cdx.json")
        for doc, expected in ((json.dumps({"bomFormat": "SPDX", "specVersion": "1.5", "components": [{}]}), "bomFormat"),
                              (json.dumps({"bomFormat": "CycloneDX", "specVersion": "1.4", "components": [{}]}), "specVersion"),
                              (json.dumps({"bomFormat": "CycloneDX", "specVersion": "1.5", "components": []}), "components"),
                              ("{not json", "not valid JSON")):
            write(path, doc)
            self.assertTrue(any(expected in p for p in self.failing("sbom")["problems"]), expected)

    def test_a_signature_file_anywhere_fails(self):
        for name in ("desktop-unsigned/UNSIGNED-ARC.app.tar.gz.sig", "arc-node.minisig", "collected/ARC.ticket"):
            root = tempfile.mkdtemp()
            try:
                self.out = make_output(root)
                write(os.path.join(self.out, name), b"signature")
                write_sums(self.out)
                self.assertIn(name, self.failing("unsigned")["signature_files"])
            finally:
                shutil.rmtree(root)

    def test_a_stapled_ticket_inside_the_archived_app_fails(self):
        for folder in ("desktop-unsigned", "collected"):
            app_tarball(os.path.join(self.out, folder, "UNSIGNED-ARC.app.tar.gz"), stapled=True)
        write_sums(self.out)
        tickets = self.failing("unsigned")["stapled_tickets"]
        self.assertIn("desktop-unsigned/UNSIGNED-ARC.app.tar.gz:ARC.app/Contents/CodeResources", tickets)

    def test_the_ad_hoc_code_signature_is_not_a_signing_claim(self):
        rc, report = run(self.out)  # the fixture's app carries Contents/_CodeSignature/CodeResources
        self.assertEqual(rc, 0)
        self.assertEqual(report["checks"]["unsigned"]["stapled_tickets"], [])

    def test_the_not_a_release_label_is_required(self):
        os.remove(os.path.join(self.out, "desktop-unsigned", "NOT-A-RELEASE.txt"))
        self.assertIn("desktop-unsigned/NOT-A-RELEASE.txt", self.failing("unsigned")["missing_labels"])

    def test_an_unlabelled_desktop_artifact_fails(self):
        write(os.path.join(self.out, "desktop-unsigned", "ARC_0.8.0_aarch64.dmg"), DMG)
        self.assertIn("desktop-unsigned/ARC_0.8.0_aarch64.dmg", self.failing("unsigned")["unlabelled_desktop_artifacts"])

    def test_a_missing_or_foreign_directory_is_not_an_output(self):
        self.assertEqual(run(os.path.join(self.root, "nope"))[0], 2)
        foreign = os.path.join(self.root, "foreign")
        write(os.path.join(foreign, "SHA256SUMS"), "")
        rc, report = run(foreign)
        self.assertEqual(rc, 2)
        self.assertEqual(report["status"], "NOT_AN_OUTPUT")


if __name__ == "__main__":
    unittest.main()
