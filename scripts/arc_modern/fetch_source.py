"""Download a pinned Hugging Face source (public BF16 weights) and verify it.

Reads a source manifest (`arc.hf-source.v1`, e.g.
docs/protocol/packages/smollm3-3b.source.json) and fetches every pinned file
from `https://huggingface.co/{repo}/resolve/{revision}/{name}` into a
directory, refusing any file whose byte length or SHA-256 differs from the
manifest. ARC hosts nothing: this is the on-device distribution path.

    python scripts/arc_modern/fetch_source.py --manifest SRC.json --dir DIR [--only tokenizer]

Standard library only, so it behaves the same on every runner OS.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
import time
import urllib.request
from pathlib import Path


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(8 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def fetch(url: str, dest: Path, size: int, sha256: str) -> str:
    if dest.exists() and dest.stat().st_size == size and sha256_of(dest) == sha256:
        return "already present"
    partial = dest.with_name(dest.name + ".part")
    last_error = None
    for attempt in range(1, 7):
        try:
            request = urllib.request.Request(url, headers={"User-Agent": "arc-chain-ci/1"})
            digest = hashlib.sha256()
            received = 0
            with urllib.request.urlopen(request, timeout=120) as response, partial.open("wb") as out:
                while True:
                    block = response.read(8 << 20)
                    if not block:
                        break
                    out.write(block)
                    digest.update(block)
                    received += len(block)
            if received != size or digest.hexdigest() != sha256:
                raise ValueError(
                    f"{dest.name}: received {received} bytes with SHA-256 {digest.hexdigest()}; "
                    f"pinned {size} bytes with SHA-256 {sha256}"
                )
            partial.replace(dest)
            return "downloaded"
        except Exception as error:  # noqa: BLE001 - retried, then reported
            last_error = error
            print(f"attempt {attempt} for {dest.name} failed: {error}", file=sys.stderr)
            time.sleep(min(60, 10 * attempt))
    raise SystemExit(f"could not fetch {url}: {last_error}")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", required=True)
    parser.add_argument("--dir", required=True)
    parser.add_argument(
        "--only",
        choices=["all", "tokenizer"],
        default="all",
        help="tokenizer: only the tokenizer, chat template and reference files",
    )
    args = parser.parse_args(argv)
    manifest = json.loads(Path(args.manifest).read_text(encoding="utf-8"))
    if manifest.get("schema") != "arc.hf-source.v1":
        raise SystemExit("not an arc.hf-source.v1 manifest")
    entries = []
    if args.only == "all":
        entries.extend(manifest["files"])
    for key in ("tokenizer", "chat_template"):
        if key in manifest:
            entries.append(manifest[key])
    entries.extend(manifest.get("reference_files", []))
    target = Path(args.dir)
    target.mkdir(parents=True, exist_ok=True)
    template = manifest.get("url_template", "https://huggingface.co/{repo}/resolve/{revision}/{name}")
    start = time.time()
    total = 0
    for entry in entries:
        name = entry["name"]
        if "/" in name or "\\" in name or name.startswith("."):
            raise SystemExit(f"refusing non-plain file name {name!r}")
        url = template.format(repo=manifest["repo"], revision=manifest["revision"], name=name)
        status = fetch(url, target / name, int(entry["bytes"]), entry["sha256"])
        total += int(entry["bytes"])
        print(f"{name}: {status} ({entry['bytes']} bytes, sha256 {entry['sha256']})")
    print(f"verified {len(entries)} files, {total} bytes, in {time.time() - start:.1f} s")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
