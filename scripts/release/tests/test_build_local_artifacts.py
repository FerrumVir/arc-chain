"""FIXTURE tests for scripts/release/build-local-artifacts.sh (R2).

The script runs from a copy inside a throwaway tree whose builders are stubs:
cargo "builds" a few bytes, npm "bundles" a fake .app and .dmg, and pgrep,
uname and git answer as each test says. Nothing is compiled, bundled or
signed, and the throwaway tree has no sources a real builder could build. The
real scripts/arc-build-provenance.sh and scripts/release/sbom.py run
unmodified inside it.

What this proves: the script's own guards (running soak, OUTPUT_DIR, signing
and notarization environment), that a refused run never reaches cargo or the
bundler, that no signing variable reaches either of them, and that the layout
the script writes passes verify_local_artifacts.py and the runbook's
`shasum -a 256 -c` step. What it does not prove: that a real build of this
checkout succeeds or verifies. That build runs after the soak.
"""

import importlib.util
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

RELEASE = Path(__file__).resolve().parents[1]
SCRIPTS = RELEASE.parent
SCRIPT = RELEASE / "build-local-artifacts.sh"

SPEC = importlib.util.spec_from_file_location("verify_local_artifacts", RELEASE / "verify_local_artifacts.py")
v = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(v)

# Every signing or notarization input the pinned Tauri CLI (2.11.4) reads,
# taken from the names in its own binary - listed here independently of the
# script, so a name dropped from the script's list fails this test.
SIGNING_NAMES = (
    "TAURI_SIGNING_PRIVATE_KEY", "TAURI_SIGNING_PRIVATE_KEY_PASSWORD", "TAURI_SIGNING_PRIVATE_KEY_PATH",
    "TAURI_PRIVATE_KEY", "TAURI_PRIVATE_KEY_PASSWORD", "TAURI_PRIVATE_KEY_PATH",
    "APPLE_SIGNING_IDENTITY", "APPLE_CERTIFICATE", "APPLE_CERTIFICATE_PASSWORD",
    "APPLE_ID", "APPLE_PASSWORD", "APPLE_TEAM_ID", "APPLE_PROVIDER_SHORT_NAME",
    "APPLE_API_KEY", "APPLE_API_ISSUER", "APPLE_API_KEY_PATH",
)

APP = "ARC Node.app"
DMG = "ARC Node_0.8.0_aarch64.dmg"

STUBS = {
    # cargo: records argv and environment; `build -p X` writes target/<profile>/<bin>.
    "cargo": r'''#!/bin/sh
printf '%s\n' "$*" >> "$FIXTURE_LOG/cargo.argv"
[ "$1" = "--version" ] && { echo "cargo 0.0.0-fixture"; exit 0; }
[ "$1" = "build" ] || exit 97
profile=debug; pkg=; prev=
for a in "$@"; do
  [ "$a" = "--release" ] && profile=release
  [ "$prev" = "-p" ] && pkg=$a
  prev=$a
done
env > "$FIXTURE_LOG/cargo-$pkg.env"
case $pkg in arc-node) bin=arc-node ;; arc-cli) bin=arc ;; *) exit 97 ;; esac
mkdir -p "target/$profile"
printf 'fixture %s binary\n' "$pkg" > "target/$profile/$bin"
''',
    "rustc": '#!/bin/sh\necho "rustc 0.0.0-fixture"\n',
    # npm: `ci` is a no-op; `run tauri:build` lays out a bundle like Tauri's.
    "npm": r'''#!/bin/sh
printf '%s\n' "$*" >> "$FIXTURE_LOG/npm.argv"
env > "$FIXTURE_LOG/npm-$1.env"
case $1 in
  ci) exit 0 ;;
  run) [ "$2" = "tauri:build" ] || exit 97 ;;
  *) exit 97 ;;
esac
b=src-tauri/target/release/bundle
# An empty bundle directory: Tauri ran but produced nothing.
[ -n "${FIXTURE_NO_BUNDLE:-}" ] && { mkdir -p "$b/macos"; exit 0; }
app="$b/macos/ARC Node.app/Contents"
mkdir -p "$app/MacOS" "$app/_CodeSignature" "$b/dmg"
printf 'fixture desktop binary\n' > "$app/MacOS/arc-node-desktop"
printf '<plist/>\n' > "$app/Info.plist"
printf 'ad-hoc signature\n' > "$app/_CodeSignature/CodeResources"
printf 'koly fixture dmg\n' > "$b/dmg/ARC Node_0.8.0_aarch64.dmg"
[ -n "${FIXTURE_LOCK_COLLECTED:-}" ] && chmod a-w "$FIXTURE_LOCK_COLLECTED"
exit 0
''',
    "node": "#!/bin/sh\nexit 0\n",
    "git": r'''#!/bin/sh
case $1 in
  rev-parse) echo 0123456789abcdef0123456789abcdef01234567 ;;
  status) ;;
  *) exit 97 ;;
esac
''',
    # pgrep -f PATTERN "matches" only the pattern the test names.
    "pgrep": '#!/bin/sh\n[ -n "${FIXTURE_PGREP_MATCH:-}" ] && [ "$2" = "$FIXTURE_PGREP_MATCH" ] && exit 0\nexit 1\n',
    "uname": '#!/bin/sh\ncase "$*" in -m) echo arm64 ;; -srm) echo "Darwin 24.0.0 arm64" ;; *) echo Darwin ;; esac\n',
}

CARGO_LOCK = '''version = 4

[[package]]
name = "arc-node"
version = "0.8.0"
'''
NPM_LOCK = '{"lockfileVersion": 3, "packages": {"": {"name": "arc-node-desktop", "version": "0.8.0"}}}\n'


class BuildLocalArtifacts(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="arc-build-local-fixture-"))
        self.root = self.tmp / "repo"
        self.log = self.tmp / "log"
        self.out = self.tmp / "out"
        shim = self.tmp / "shim"
        for d in (self.root / "scripts/release", self.root / "crates/arc-node/src", self.root / "crates/arc-cli/src",
                  self.root / "desktop/src-tauri", self.log, shim, self.tmp / "home"):
            d.mkdir(parents=True)
        shutil.copy2(SCRIPT, self.root / "scripts/release/build-local-artifacts.sh")
        shutil.copy2(RELEASE / "sbom.py", self.root / "scripts/release/sbom.py")
        shutil.copy2(SCRIPTS / "arc-build-provenance.sh", self.root / "scripts/arc-build-provenance.sh")
        (self.root / "Cargo.toml").write_text('[workspace.package]\nversion = "0.8.0"\n')
        (self.root / "Cargo.lock").write_text(CARGO_LOCK)
        (self.root / "crates/arc-node/src/main.rs").write_text("fn main() {}\n")
        (self.root / "crates/arc-cli/src/main.rs").write_text("fn main() {}\n")
        (self.root / "desktop/package-lock.json").write_text(NPM_LOCK)
        for name, body in STUBS.items():
            path = shim / name
            path.write_text(body)
            path.chmod(0o755)
        self.env = {
            # The stubs come first; HOME is empty so arc-build-provenance.sh's
            # $HOME/.local/bin prefix adds nothing.
            "PATH": os.pathsep.join([str(shim), os.path.dirname(sys.executable), "/usr/bin", "/bin"]),
            "HOME": str(self.tmp / "home"),
            "LC_ALL": "C",
            "FIXTURE_LOG": str(self.log),
        }

    def tearDown(self):
        for dirpath, dirnames, _ in os.walk(self.tmp):
            for d in dirnames:
                p = os.path.join(dirpath, d)
                os.chmod(p, os.stat(p).st_mode | stat.S_IWUSR)
        shutil.rmtree(self.tmp)

    def run_script(self, out=None, **extra):
        env = dict(self.env, **extra)
        return subprocess.run(["bash", str(self.root / "scripts/release/build-local-artifacts.sh"),
                               str(out or self.out)], cwd=str(self.root), env=env,
                              capture_output=True, text=True, timeout=120)

    def assert_passes(self, proc):
        self.assertEqual(proc.returncode, 0, proc.stdout[-2000:] + proc.stderr[-2000:])

    def assert_refused(self, proc, message):
        self.assertEqual(proc.returncode, 1, proc.stdout[-2000:] + proc.stderr[-2000:])
        self.assertIn(message, proc.stderr)

    def cargo_ran(self):
        return (self.log / "cargo.argv").exists()

    def logged_env_files(self):
        return sorted(self.log.glob("*.env"))

    def test_a_fixture_build_passes_the_verifier_and_the_runbook_checksum_step(self):
        self.assert_passes(self.run_script())
        report = v.verify(str(self.out))
        self.assertEqual(report["status"], "PASS", report)
        sums = (self.out / "SHA256SUMS").read_text().splitlines()
        self.assertEqual(sorted(line.split("  ", 1)[1] for line in sums),
                         sorted(["arc-node", "arc-cli", f"UNSIGNED-{APP}.tar.gz", f"UNSIGNED-{DMG}"]))
        # Runbook step: the entries are relative to collected/, so check from there.
        check = subprocess.run(["shasum", "-a", "256", "-c", "../SHA256SUMS"], cwd=str(self.out / "collected"),
                               capture_output=True, text=True, timeout=60)
        self.assertEqual(check.returncode, 0, check.stdout + check.stderr)
        build = [line for line in (self.log / "npm.argv").read_text().splitlines() if line.startswith("run ")]
        self.assertEqual(len(build), 1)
        self.assertIn("--bundles app,dmg", build[0])
        self.assertIn('--config {"bundle":{"createUpdaterArtifacts":false}}', build[0])

    def test_without_an_arc_cli_crate_the_cli_is_skipped_and_the_output_still_verifies(self):
        shutil.rmtree(self.root / "crates/arc-cli")
        self.assert_passes(self.run_script())
        self.assertFalse((self.out / "arc-cli-provenance.txt").exists())
        self.assertEqual(v.verify(str(self.out))["status"], "PASS")

    def test_a_non_empty_output_dir_is_refused_before_anything_runs(self):
        self.out.mkdir()
        (self.out / "stale.txt").write_text("from an earlier run\n")
        self.assert_refused(self.run_script(), "refusing a non-empty OUTPUT_DIR")
        self.assertEqual(os.listdir(self.out), ["stale.txt"])
        self.assertEqual((self.out / "stale.txt").read_text(), "from an earlier run\n")
        self.assertFalse(self.cargo_ran())

    def test_an_existing_empty_output_dir_is_accepted(self):
        self.out.mkdir()
        self.assert_passes(self.run_script())
        self.assertEqual(v.verify(str(self.out))["status"], "PASS")

    def test_each_signing_or_notarization_variable_is_refused_without_printing_its_value(self):
        for name in SIGNING_NAMES:
            with self.subTest(name=name):
                proc = self.run_script(out=self.tmp / f"out-{name}", **{name: "fixture-secret-value"})
                self.assert_refused(proc, name)
                self.assertNotIn("fixture-secret-value", proc.stdout + proc.stderr)
                self.assertFalse(self.cargo_ran())
                self.assertFalse((self.log / "npm.argv").exists())

    def test_set_but_empty_signing_variables_never_reach_cargo_or_the_bundler(self):
        self.assert_passes(self.run_script(**{name: "" for name in SIGNING_NAMES}))
        seen = self.logged_env_files()
        self.assertEqual({p.name for p in seen},
                         {"cargo-arc-node.env", "cargo-arc-cli.env", "npm-ci.env", "npm-run.env"})
        for path in seen:
            names = {line.split("=", 1)[0] for line in path.read_text().splitlines() if "=" in line}
            self.assertEqual(names & set(SIGNING_NAMES), set(), path.name)

    def test_a_running_soak_refuses_the_build(self):
        for pattern in ("arc-soak", "/tmp/arc-provenance/arc-node"):
            with self.subTest(pattern=pattern):
                proc = self.run_script(FIXTURE_PGREP_MATCH=pattern)
                self.assert_refused(proc, "refusing to build")
                self.assertFalse(self.cargo_ran())
                self.assertFalse(self.out.exists())

    def test_a_bundle_step_that_produces_nothing_fails_the_build(self):
        self.assert_refused(self.run_script(FIXTURE_NO_BUNDLE="1"), "produced no bundle artifacts")
        self.assertFalse((self.out / "SHA256SUMS").exists())

    @unittest.skipIf(hasattr(os, "geteuid") and os.geteuid() == 0, "root ignores the read-only directory")
    def test_a_desktop_artifact_that_cannot_be_collected_fails_the_build_loudly(self):
        # Before the fix this copy failed silently and SHA256SUMS left the
        # desktop bundle out.
        proc = self.run_script(FIXTURE_LOCK_COLLECTED=str(self.out / "collected"))
        self.assert_refused(proc, "could not copy the desktop artifacts")
        self.assertFalse((self.out / "SHA256SUMS").exists())


if __name__ == "__main__":
    unittest.main()
