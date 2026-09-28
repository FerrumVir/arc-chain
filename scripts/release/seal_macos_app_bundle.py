#!/usr/bin/env python3
"""Ad-hoc seal a Tauri macOS app, then rebuild and verify its archive and DMG.

This creates an app-bundle code seal only. It does not provide Developer ID
signing or notarization, and intentionally uses no signing secrets.
"""

from __future__ import annotations

import argparse
import hashlib
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile


APP_NAME = "ARC Node.app"
APP_IDENTIFIER = "network.arc.desktop"
APP_VERSION = "0.8.0"
EXECUTABLE_NAME = "arc-desktop"
CODE_SIGN = Path("/usr/bin/codesign")
HDITIL = Path("/usr/bin/hdiutil")
TAR = Path("/usr/bin/tar")


def run(command: list[str], *, env: dict[str, str] | None = None) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(command, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          timeout=180, env=env)


def one_match(directory: Path, pattern: str, description: str) -> Path:
    matches = sorted(directory.glob(pattern))
    if len(matches) != 1 or matches[0].is_symlink() or not matches[0].is_file():
        raise RuntimeError(f"expected exactly one {description}; found {len(matches)}")
    return matches[0]


def app_identity(app: Path) -> Path:
    if app.is_symlink() or not app.is_dir() or app.name != APP_NAME:
        raise RuntimeError("unexpected macOS app bundle path")
    info = app / "Contents" / "Info.plist"
    try:
        document = plistlib.loads(info.read_bytes())
    except (OSError, ValueError, plistlib.InvalidFileException) as error:
        raise RuntimeError("macOS app Info.plist is missing or invalid") from error
    if (document.get("CFBundleIdentifier") != APP_IDENTIFIER
            or document.get("CFBundleShortVersionString") != APP_VERSION
            or document.get("CFBundleExecutable") != EXECUTABLE_NAME):
        raise RuntimeError("macOS app identity/version/executable does not match release pins")
    executable = app / "Contents" / "MacOS" / EXECUTABLE_NAME
    if executable.is_symlink() or not executable.is_file() or not os.access(executable, os.X_OK):
        raise RuntimeError("macOS app executable is missing or not executable")
    return executable


def verify_app(app: Path) -> str:
    executable = app_identity(app)
    run([str(CODE_SIGN), "--verify", "--deep", "--strict", "--verbose=2", str(app)])
    return hashlib.sha256(executable.read_bytes()).hexdigest()


def seal_and_repackage(bundle_root: Path) -> dict[str, str]:
    """Seal the one Tauri app and regenerate both shipped macOS formats."""
    bundle_root = bundle_root.resolve(strict=True)
    app_dir = bundle_root / "macos"
    dmg_dir = bundle_root / "dmg"
    if (not app_dir.is_dir() or app_dir.is_symlink()
            or not dmg_dir.is_dir() or dmg_dir.is_symlink()):
        raise RuntimeError("Tauri macOS bundle directories are missing or symlinked")
    apps = sorted(app_dir.glob("*.app"))
    if len(apps) != 1:
        raise RuntimeError(f"expected one Tauri app bundle; found {len(apps)}")
    app = apps[0]
    executable = app_identity(app)
    if not CODE_SIGN.is_file() or not HDITIL.is_file() or not TAR.is_file():
        raise RuntimeError("required macOS packaging tool is missing")
    archive = one_match(app_dir, "*.app.tar.gz", "app updater archive")
    dmg = one_match(dmg_dir, "*.dmg", "disk image")

    # Use Apple's ad-hoc identity to seal the executable, Info.plist, and
    # resources. Updater-signing remains a separate later workflow step.
    run([str(CODE_SIGN), "--force", "--deep", "--sign", "-", str(app)])
    source_hash = verify_app(app)

    with tempfile.TemporaryDirectory(prefix="arc-sealed-macos-", dir=bundle_root) as temp:
        work = Path(temp)
        archive_tmp = app_dir / f".{archive.name}.sealed.tmp"
        dmg_tmp = dmg_dir / f".{dmg.stem}.sealed.tmp.dmg"
        stage = work / "dmg-root"
        stage.mkdir()
        shutil.copytree(app, stage / APP_NAME, symlinks=True)
        (stage / "Applications").symlink_to("/Applications")
        mount = work / "mount"
        mount.mkdir()
        attached = False
        try:
            # Tauri's updater expects an app-root tarball. Keep the archive
            # rooted at exactly `ARC Node.app/`, with executable modes intact.
            run([str(TAR), "-czf", str(archive_tmp), "-C", str(app_dir), APP_NAME],
                env={**os.environ, "COPYFILE_DISABLE": "1"})
            with tempfile.TemporaryDirectory(prefix="arc-app-archive-", dir=work) as extract_raw:
                extracted_root = Path(extract_raw)
                run([str(TAR), "-xzf", str(archive_tmp), "-C", str(extracted_root)],
                    env={**os.environ, "COPYFILE_DISABLE": "1"})
                archived_app = extracted_root / APP_NAME
                if verify_app(archived_app) != source_hash:
                    raise RuntimeError("updater archive app differs from sealed source app")

            run([str(HDITIL), "create", "-ov", "-format", "UDZO", "-volname", "ARC Node",
                 "-srcfolder", str(stage), str(dmg_tmp)])
            run([str(HDITIL), "verify", str(dmg_tmp)])
            run([str(HDITIL), "attach", "-readonly", "-nobrowse", "-noautoopen",
                 "-mountpoint", str(mount), str(dmg_tmp)])
            attached = True
            mounted_apps = sorted(mount.glob("*.app"))
            if len(mounted_apps) != 1:
                raise RuntimeError(f"rebuilt DMG must contain exactly one app; found {len(mounted_apps)}")
            if verify_app(mounted_apps[0]) != source_hash:
                raise RuntimeError("DMG app differs from sealed source app")
            run([str(HDITIL), "detach", str(mount), "-quiet"])
            attached = False
            os.replace(archive_tmp, archive)
            os.replace(dmg_tmp, dmg)
        finally:
            if attached:
                run([str(HDITIL), "detach", str(mount), "-quiet"])
            archive_tmp.unlink(missing_ok=True)
            dmg_tmp.unlink(missing_ok=True)

    # Recheck the exact outputs after the atomic replacements.
    with tempfile.TemporaryDirectory(prefix="arc-final-app-", dir=bundle_root) as temp:
        root = Path(temp)
        run([str(TAR), "-xzf", str(archive), "-C", str(root)],
            env={**os.environ, "COPYFILE_DISABLE": "1"})
        if verify_app(root / APP_NAME) != source_hash:
            raise RuntimeError("final updater archive failed strict bundle verification")
    return {"app": str(app), "archive": str(archive), "dmg": str(dmg),
            "executable_sha256": source_hash}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle-root", type=Path, required=True,
                        help="Tauri target/<rust-target>/release/bundle directory")
    args = parser.parse_args()
    result = seal_and_repackage(args.bundle_root)
    print("ad-hoc sealed app and rebuilt verified macOS packages:", result)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
