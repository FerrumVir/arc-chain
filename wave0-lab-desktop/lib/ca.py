#!/usr/bin/env python3
"""Per-run throwaway certificate authority for the Wave 0 desktop lab (THROWAWAY LAB FILE, never merged).

The desktop isolation test needs the RELEASED v0.7.11 app (its tauri-plugin-updater, which verifies TLS against the
operating system trust store) to talk to a local recording server that answers for github.com. For that, every CI job
generates its OWN certificate authority inside the runner, trusts it in the runner's trust store only, and throws it away
with the runner.

Rules this module enforces:
  * ECDSA P-256, one-day validity, a CA (pathlen 0) and one server certificate whose SAN list is exactly the hostnames given.
  * Every private key (CA key and server key) lives under <outdir>/private/ and NEVER leaves the runner: the only
    files meant for evidence are ca.crt and ca.sha256 (public_files). ``scan_for_private_keys`` and the ``scan`` command
    refuse a directory that contains any PEM private key, so an upload step can be gated on it.
  * ca_sha256 is the SHA-256 of the DER form of ca.crt (what certutil / security / openssl x509 -fingerprint print).

Only the ``openssl`` command line is used (OpenSSL 3 on Linux, Git-for-Windows openssl.exe, LibreSSL on macOS). Extensions
are written to small config files because ``-addext`` is not available in every LibreSSL build.

CLI:
  ca.py make --out DIR --hosts a,b,c     create the CA and the server certificate under DIR
  ca.py public --out DIR --dest DIR2     copy ONLY ca.crt and ca.sha256 to DIR2 (evidence)
  ca.py scan --root DIR                  exit 1 if DIR holds any PEM private key

UNVERIFIED ON CI: that the Windows runner's Git-for-Windows openssl.exe accepts the Windows paths this module passes;
that rustls-platform-verifier on Windows accepts a certificate without revocation information (CRL/OCSP);
that macOS Security.framework accepts the one-day ECDSA leaf (it satisfies Apple's published server certificate rules).
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import os
import re
import secrets
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Dict, Iterable, List, Optional

PEM_BLOCK = re.compile(r"-----BEGIN CERTIFICATE-----\s*(.+?)\s*-----END CERTIFICATE-----", re.DOTALL)
PRIVATE_KEY_MARK = re.compile(r"-----BEGIN (?:[A-Z0-9 ]+ )?PRIVATE KEY-----")
DEFAULT_HOSTS = (
    "github.com",
    "api.github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "github-releases.githubusercontent.com",
    "codeload.github.com",
)


class CaError(RuntimeError):
    """The CA could not be created or a safety rule was violated."""


def openssl_binary() -> str:
    """The openssl executable to use; OPENSSL_BIN overrides (Git for Windows ships one outside PATH)."""
    override = os.environ.get("OPENSSL_BIN")
    if override:
        return override
    found = shutil.which("openssl")
    if found:
        return found
    for candidate in (r"C:\Program Files\Git\usr\bin\openssl.exe", r"C:\Program Files\Git\mingw64\bin\openssl.exe"):
        if os.path.exists(candidate):
            return candidate
    raise CaError("openssl was not found (set OPENSSL_BIN)")


def run_openssl(args: List[str]) -> str:
    done = subprocess.run([openssl_binary()] + args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=False)
    text = done.stdout.decode("utf-8", "replace")
    if done.returncode != 0:
        raise CaError("openssl %s failed (%d): %s" % (" ".join(args[:2]), done.returncode, text[-400:]))
    return text


def validate_hostnames(hostnames: Iterable[str]) -> List[str]:
    names: List[str] = []
    for raw in hostnames:
        name = raw.strip().lower()
        if not name:
            continue
        if not re.fullmatch(r"[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)*", name):
            raise CaError("not a plain DNS name: %r" % raw)
        if name not in names:
            names.append(name)
    if not names:
        raise CaError("at least one hostname is required")
    return names


def pem_to_der(pem_text: str) -> bytes:
    match = PEM_BLOCK.search(pem_text)
    if not match:
        raise CaError("no PEM certificate found")
    return base64.b64decode("".join(match.group(1).split()))


def cert_sha256(path: Path) -> str:
    """SHA-256 (hex) of the DER encoding of the PEM certificate at ``path``."""
    return hashlib.sha256(pem_to_der(path.read_text(encoding="ascii"))).hexdigest()


def _write(path: Path, text: str) -> None:
    path.write_text(text, encoding="ascii")


def make_ca(outdir, hostnames: Iterable[str] = DEFAULT_HOSTS, common_name: Optional[str] = None) -> Dict[str, object]:
    """Create the CA and a server certificate for ``hostnames`` under ``outdir``.

    Returns {"ca_cert","ca_key","server_cert","server_key","ca_sha256","hostnames"} (paths as strings). Public files are
    outdir/ca.crt and outdir/ca.sha256 and outdir/server.crt; everything private is under outdir/private/."""
    names = validate_hostnames(hostnames)
    out = Path(outdir).resolve()
    private = out / "private"
    out.mkdir(parents=True, exist_ok=True)
    private.mkdir(parents=True, exist_ok=True)
    try:
        os.chmod(str(private), 0o700)
    except OSError:
        pass  # Windows: directory modes do not apply
    suffix = secrets.token_hex(4)
    ca_cn = common_name or ("Wave0 Lab Desktop CA %s (throwaway, CI only)" % suffix)
    ca_key, ca_crt = private / "ca.key", out / "ca.crt"
    srv_key, srv_csr, srv_crt = private / "server.key", private / "server.csr", out / "server.crt"

    _write(private / "ca.cnf", "\n".join([
        "[req]",
        "distinguished_name = dn",
        "prompt = no",
        "x509_extensions = v3_ca",
        "[dn]",
        "CN = %s" % ca_cn,
        "[v3_ca]",
        "basicConstraints = critical,CA:TRUE,pathlen:0",
        "keyUsage = critical,keyCertSign,cRLSign",
        "subjectKeyIdentifier = hash",
        "",
    ]))
    _write(private / "server.cnf", "\n".join([
        "[req]",
        "distinguished_name = dn",
        "prompt = no",
        "[dn]",
        "CN = %s" % names[0],
        "",
    ]))
    _write(private / "server.ext", "\n".join([
        "[server_ext]",
        "basicConstraints = critical,CA:FALSE",
        "keyUsage = critical,digitalSignature",
        "extendedKeyUsage = serverAuth",
        "subjectKeyIdentifier = hash",
        "authorityKeyIdentifier = keyid",
        "subjectAltName = %s" % ",".join("DNS:%s" % name for name in names),
        "",
    ]))

    run_openssl(["ecparam", "-name", "prime256v1", "-genkey", "-noout", "-out", str(ca_key)])
    run_openssl(["req", "-x509", "-new", "-nodes", "-key", str(ca_key), "-sha256", "-days", "1",
                 "-config", str(private / "ca.cnf"), "-out", str(ca_crt)])
    run_openssl(["ecparam", "-name", "prime256v1", "-genkey", "-noout", "-out", str(srv_key)])
    run_openssl(["req", "-new", "-key", str(srv_key), "-config", str(private / "server.cnf"), "-out", str(srv_csr)])
    run_openssl(["x509", "-req", "-in", str(srv_csr), "-CA", str(ca_crt), "-CAkey", str(ca_key), "-CAcreateserial",
                 "-CAserial", str(private / "ca.srl"), "-out", str(srv_crt), "-days", "1", "-sha256",
                 "-extfile", str(private / "server.ext"), "-extensions", "server_ext"])
    for key in (ca_key, srv_key):
        try:
            os.chmod(str(key), 0o600)
        except OSError:
            pass
    digest = cert_sha256(ca_crt)
    _write(out / "ca.sha256", digest + "\n")
    return {
        "ca_cert": str(ca_crt),
        "ca_key": str(ca_key),
        "server_cert": str(srv_crt),
        "server_key": str(srv_key),
        "ca_sha256": digest,
        "hostnames": names,
    }


def public_files(outdir) -> List[Path]:
    """The only files of a CA directory that may be uploaded as evidence: ca.crt and ca.sha256."""
    out = Path(outdir)
    files = [out / "ca.crt", out / "ca.sha256"]
    for path in files:
        if not path.is_file():
            raise CaError("missing public file %s" % path)
        if PRIVATE_KEY_MARK.search(path.read_text(encoding="ascii", errors="replace")):
            raise CaError("%s contains a private key" % path)
    return files


def copy_public(outdir, dest) -> List[Path]:
    """Copy ONLY ca.crt and ca.sha256 into ``dest`` and return the new paths."""
    target = Path(dest)
    target.mkdir(parents=True, exist_ok=True)
    copied: List[Path] = []
    for path in public_files(outdir):
        shutil.copyfile(str(path), str(target / path.name))
        copied.append(target / path.name)
    return copied


def scan_for_private_keys(root) -> List[str]:
    """Paths under ``root`` whose content holds a PEM private key (or that sit in a ``private`` directory)."""
    found: List[str] = []
    base = Path(root)
    if not base.exists():
        return found
    for current, dirs, files in os.walk(str(base)):
        # relative to the scanned root: the root itself may live under a directory that happens to be called "private"
        in_private = "private" in Path(current).relative_to(base).parts
        for name in files:
            path = os.path.join(current, name)
            if in_private:
                found.append(path)
                continue
            try:
                with open(path, "rb") as handle:
                    head = handle.read(1 << 20)
            except OSError:
                continue
            if PRIVATE_KEY_MARK.search(head.decode("latin-1")):
                found.append(path)
    return sorted(found)


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    make = sub.add_parser("make")
    make.add_argument("--out", required=True)
    make.add_argument("--hosts", default=",".join(DEFAULT_HOSTS))
    public = sub.add_parser("public")
    public.add_argument("--out", required=True)
    public.add_argument("--dest", required=True)
    scan = sub.add_parser("scan")
    scan.add_argument("--root", required=True)
    args = parser.parse_args(argv)
    if args.command == "make":
        info = make_ca(args.out, args.hosts.split(","))
        print("ca_sha256=%s" % info["ca_sha256"])
        print("server_cert=%s" % info["server_cert"])
        print("hosts=%s" % ",".join(info["hostnames"]))
        return 0
    if args.command == "public":
        for path in copy_public(args.out, args.dest):
            print(path)
        return 0
    leaks = scan_for_private_keys(args.root)
    for path in leaks:
        print("PRIVATE KEY MATERIAL: %s" % path, file=sys.stderr)
    return 1 if leaks else 0


if __name__ == "__main__":
    sys.exit(main())
