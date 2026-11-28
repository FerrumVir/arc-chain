#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import sys
import time
import urllib.error
import urllib.request

LATEST_RELEASE_URL = "https://api.github.com/repos/FerrumVir/arc-chain/releases/latest"
MAX_METADATA_BYTES = 4 * 1024 * 1024


class RejectMetadataRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def fetch_latest_release_metadata(attempts: int = 3) -> dict[str, object]:
    headers = {
        "User-Agent": "arc-legacy-bridge-acceptance",
        "Accept": "application/vnd.github+json",
    }
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    opener = urllib.request.build_opener(RejectMetadataRedirects())
    payload = b""
    for attempt in range(1, attempts + 1):
        request = urllib.request.Request(LATEST_RELEASE_URL, headers=headers)
        try:
            with opener.open(request, timeout=60) as response:
                payload = response.read(MAX_METADATA_BYTES + 1)
            break
        except urllib.error.HTTPError as error:
            if error.code < 500 or attempt == attempts:
                raise
        except urllib.error.URLError:
            if attempt == attempts:
                raise
        time.sleep(attempt)
    if len(payload) > MAX_METADATA_BYTES:
        raise ValueError("Latest release metadata exceeds 4 MiB")
    parsed = json.loads(payload)
    if not isinstance(parsed, dict) or not isinstance(parsed.get("tag_name"), str):
        raise ValueError("Latest release metadata is missing string tag_name")
    return parsed


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    metadata = fetch_latest_release_metadata()
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(metadata, indent=2) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
