"""Tests for scripts/arc_mla/stream_slices.py: the one-shard disk bound.

Run from the repository root:  python3 -m unittest scripts/arc_mla/tests/test_stream_slices.py

No model is downloaded and arc-mla is not run: fetch() and the arc-mla calls
are replaced by fakes that record which shards are on disk at every download.
"""

from __future__ import annotations

import importlib.util
import hashlib
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[1] / "stream_slices.py"


def _load():
    spec = importlib.util.spec_from_file_location("stream_slices", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


SHARDS = [f"model-0000{i}-of-00003.safetensors" for i in (1, 2, 3)]


class Interrupted(Exception):
    """A crash after a step's conversion, before its shards are deleted."""


class Fakes:
    def __init__(self, work: Path, out: Path, crash_after_unit=None):
        self.work, self.out = work, out
        self.crash_after_unit = crash_after_unit
        self.fetches = []  # (name, shards on disk when the download starts)

    def shards_on_disk(self):
        return sorted(p.name for p in self.work.glob("model-*.safetensors"))

    def fetch(self, url, dest, size, sha256):
        dest = Path(dest)
        if dest.exists():
            return "already present"
        if dest.name.endswith(".safetensors"):
            self.fetches.append((dest.name, self.shards_on_disk()))
        dest.write_bytes(b'{"model_type":"kimi_k2"}' if dest.name=='config.json' else b"\0" * size)
        return "downloaded"

    def run(self, cmd):
        command = cmd[1]
        value = lambda flag: cmd[cmd.index(flag) + 1]  # noqa: E731
        if command == "slice-plan":
            steps = [{"units": [f"layer.{i}"], "shards": [s], "release": [s], "source_bytes": 10}
                     for i, s in enumerate(SHARDS)]
            Path(value("--out")).write_text(json.dumps({"steps": steps, "peak_source_bytes": 10}))
            return "{}", 0.0, None
        if command == "slice":
            a, b = (int(x) for x in value("--layers").split(":"))
            units = [f"layer.{i}" for i in range(a, b)]
            (self.out / "units").mkdir(parents=True, exist_ok=True)
            for u in units:
                (self.out / "units" / f"{u}.json").write_text(json.dumps({'context': {'precision': dict(version=1, **{k:'int16' for k in ('attention','dense','shared','embedding','head')})}}))
                if u == self.crash_after_unit:
                    self.crash_after_unit = None
                    raise Interrupted(u)
            return json.dumps({"units": [{"unit": u} for u in units]}), 0.0, None
        if command == "slice-manifest":
            return json.dumps({"manifest_blake3": "0" * 64}), 0.0, None
        raise AssertionError(cmd)


class OneShardBound(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.work, self.out = root / "work", root / "slices"
        entry = lambda name: {"name": name, "bytes": len(b'{"model_type":"kimi_k2"}') if name=='config.json' else 10, "sha256": hashlib.sha256(b'{"model_type":"kimi_k2"}').hexdigest() if name=='config.json' else "0" * 64}  # noqa: E731
        self.manifest = root / "source.json"
        self.manifest.write_text(json.dumps({
            "schema": "arc.hf-source.v1", "repo": "arc-test/stream", "revision": "0", "max_seq": 8,
            "files": [entry("config.json")] + [entry(s) for s in SHARDS],
            "index": entry("model.safetensors.index.json"),
        }))
        self.module = _load()

    def tearDown(self):
        self.tmp.cleanup()

    def main(self, fakes, *extra):
        argv = ["--arc-mla", "arc-mla", "--source-manifest", str(self.manifest), "--work", str(self.work),
                "--out", str(self.out), "--report", str(self.out / "report.json"), *extra]
        with mock.patch.object(self.module, "fetch", fakes.fetch), mock.patch.object(self.module, "run", fakes.run):
            return self.module.main(argv)

    def test_each_shard_is_deleted_before_the_next_download(self):
        fakes = Fakes(self.work, self.out)
        self.assertEqual(self.main(fakes), 0)
        self.assertEqual([name for name, _ in fakes.fetches], SHARDS)
        self.assertTrue(all(on_disk == [] for _, on_disk in fakes.fetches), fakes.fetches)
        self.assertEqual(fakes.shards_on_disk(), [])

    def test_resume_after_an_interruption_deletes_the_converted_shard_first(self):
        # The first run converts layer.0, then stops before deleting its shard.
        first = Fakes(self.work, self.out, crash_after_unit="layer.0")
        with self.assertRaises(Interrupted):
            self.main(first)
        self.assertEqual(first.shards_on_disk(), [SHARDS[0]])
        # The resumed run skips layer.0 and must delete its shard before it
        # downloads the next one.
        resumed = Fakes(self.work, self.out)
        self.assertEqual(self.main(resumed, "--resume"), 0)
        self.assertEqual([name for name, _ in resumed.fetches], SHARDS[1:])
        self.assertTrue(all(on_disk == [] for _, on_disk in resumed.fetches), resumed.fetches)
        self.assertEqual(resumed.shards_on_disk(), [])
        report = json.loads((self.out / "report.json").read_text())
        self.assertEqual(report["steps"][0]["skipped"], "unit records exist")
        self.assertEqual(report["steps"][0]["deleted"], [SHARDS[0]])

    def test_resume_rejects_precision_change_before_fetch_or_delete(self):
        self.work.mkdir(); (self.out / "units").mkdir(parents=True)
        source=self.work / SHARDS[0]; source.write_bytes(b"retained source")
        old={"version":1, **{k:"int16" for k in ["attention","dense","shared","embedding","head"]}}
        (self.out / "units/layer.0.json").write_text(json.dumps({"context":{"precision":old}}))
        mixed = self.work / "mixed.json"
        mixed.write_text(json.dumps(dict(old, embedding="int8", shared="int8")))
        for label, settings in [("historical", ["--historical-int8"]), ("different-non-null", ["--precision", str(mixed)])]:
            with self.subTest(policy=label):
                with mock.patch.object(self.module,"fetch") as fetch, mock.patch.object(self.module,"run") as run:
                    with self.assertRaisesRegex(SystemExit,"resume precision"):
                        self.module.main(["--arc-mla","arc-mla","--source-manifest",str(self.manifest),"--work",str(self.work),"--out",str(self.out),"--resume", *settings])
                    fetch.assert_not_called();run.assert_not_called()
                self.assertEqual(source.read_bytes(),b"retained source")

    def test_omitted_policy_rejects_legacy_resume_before_fetch_or_delete(self):
        first = Fakes(self.work, self.out, crash_after_unit="layer.0")
        with self.assertRaises(Interrupted): self.main(first)
        record = self.out / "units/layer.0.json"
        record.write_text(json.dumps({"context": {}}))
        before = {p.name:p.read_bytes() for p in self.work.iterdir()}
        with mock.patch.object(self.module,"fetch") as fetch, mock.patch.object(self.module,"run") as run:
            with self.assertRaisesRegex(SystemExit,"resume precision differs"):
                self.module.main(["--arc-mla","arc-mla","--source-manifest",str(self.manifest),"--work",str(self.work),"--out",str(self.out),"--resume"])
            fetch.assert_not_called(); run.assert_not_called()
        self.assertEqual(before,{p.name:p.read_bytes() for p in self.work.iterdir()})

    def test_keep_source_still_keeps_shards_when_resuming(self):
        first = Fakes(self.work, self.out, crash_after_unit="layer.0")
        with self.assertRaises(Interrupted):
            self.main(first, "--keep-source")
        resumed = Fakes(self.work, self.out)
        self.assertEqual(self.main(resumed, "--resume", "--keep-source"), 0)
        self.assertEqual(resumed.shards_on_disk(), SHARDS)
        report = json.loads((self.out / "report.json").read_text())
        self.assertEqual(report["steps"][0]["deleted"], [])


if __name__ == "__main__":
    unittest.main()
