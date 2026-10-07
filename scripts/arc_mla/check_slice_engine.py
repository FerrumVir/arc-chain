#!/usr/bin/env python3
"""Consume slice_checks.sh fixtures in the actual ENG-5 engine; no downloads.

Usage: check_slice_engine.py ARC_MLA_BINARY WORKDIR EVIDENCE_DIR
Timing in the engine reports is synthetic fixture timing, never Kimi evidence.
"""
import json
import subprocess
import sys
from pathlib import Path


def read(path):
    return json.loads(path.read_text())


def main():
    binary, work, evidence = (Path(p).resolve() for p in sys.argv[1:])
    root = Path(__file__).resolve().parents[2]
    out = evidence / "engine"
    out.mkdir()
    checks = []

    def run(*args, error=None):
        result = subprocess.run([str(binary), *map(str, args)], capture_output=True, text=True)
        if error is not None:
            assert result.returncode != 0, (args, "unexpected success")
            assert error in result.stderr, (args, result.stderr)
        else:
            assert result.returncode == 0, (args, result.stdout, result.stderr)
        with (out / "commands.log").open("a", encoding="utf-8") as log:
            log.write(f"{list(map(str, args))}\nexit={result.returncode}\n{result.stdout}{result.stderr}\n")
        return result

    for variant in ("plain", "edge"):
        source = work / variant
        slices = work / f"{variant}-streamed"
        manifest = evidence / variant / "manifest.json"
        package = work / f"{variant}-engine.arcspkg"
        stage_manifest = out / f"{variant}-stage-manifest.json"
        direct = work / f"{variant}-direct.arcspkg"
        run("slice-assemble", "--manifest", manifest, "--slices", slices, "--out", package)
        # A vision-only source shard is deliberately absent. Its pinned entry
        # remains, proving it is not required by the text loader or planner.
        source_pin = source / "tiny-kimi-packed.source.json"
        index = read(source / "model.safetensors.index.json")["weight_map"]
        vision = {f for n, f in index.items() if n.startswith(("vision_tower.", "mm_projector."))}
        text = {f for n, f in index.items() if n.startswith("language_model.")}
        assert vision and not vision.intersection(text)
        plan = read(slices / "plan.json")
        assert not any(f in json.dumps(plan) for f in vision), "planner requested vision"
        for f in vision:
            (source / f).rename(source / (f + ".unused"))
        run("convert", "--source-dir", source, "--source-manifest", source_pin,
            "--experts", "i4g32", "--out", direct, "--manifest-out", stage_manifest)
        assert package.read_bytes() == direct.read_bytes(), variant
        run("verify", "--package", package, "--manifest", stage_manifest, "--full-digest")
        checks.append(f"{variant}: prefix + packed experts + absent vision -> exact package")

        scalar = out / f"{variant}-scalar.json"
        simd = out / f"{variant}-simd.json"
        cases = source / "tiny-kimi-packed.cases.json"
        for kernel, threads, report in (("scalar", 1, scalar), ("simd", 3, simd)):
            run("golden", "--package", package, "--cases", cases, "--out", report,
                "--kernel", kernel, "--threads", threads)
        a, b = read(scalar), read(simd)
        exact_keys = ("tokens", "output_hash", "logits_hashes", "logits_digest", "boundary_digests")
        assert a["model_root"] == b["model_root"] == read(manifest)["model_root"]
        assert len(a["cases"]) == len(b["cases"]) > 0
        for x, y in zip(a["cases"], b["cases"]):
            assert all(x[k] == y[k] for k in exact_keys), variant
        if variant == "plain":
            twin = out / "bf16-twin.json"
            run("golden", "--package", work / "bf16.arcspkg", "--cases", cases,
                "--out", twin, "--kernel", "scalar", "--threads", 1)
            for x, y in zip(a["cases"], read(twin)["cases"]):
                assert all(x[k] == y[k] for k in exact_keys), "BF16 twin mismatch"
        checks.append(f"{variant}: engine tokens/logits/boundaries/root scalar=SIMD (1/3 threads)")

        # Real file-backed StageModel consumption of 1/2/4 assembled packages.
        layouts = []
        for count in (1, 2, 4):
            reports = []
            previous = None
            for first in range(0, 4, 4 // count):
                end = first + 4 // count
                tag = f"{variant}-{count}-{first}-{end}"
                stage = work / f"{tag}.arcspkg"
                report, boundary = out / f"{tag}.json", work / f"{tag}.bin"
                run("slice-assemble", "--manifest", manifest, "--slices", slices,
                    "--layers", f"{first}:{end}", "--out", stage)
                run("stage", "--package", stage, "--manifest", stage_manifest,
                    *(('--input', previous) if previous else ('--run', scalar)),
                    "--report", report, "--out", boundary, "--threads", 1,
                    "--kernel", "scalar" if first % 2 == 0 else "simd")
                reports.append(report)
                previous = boundary
            layouts.extend(["--layout", f"{count}=" + ",".join(map(str, reports))])
        subprocess.run([sys.executable, str(root / "scripts/arc_mla/layout_check.py"),
                        "--run", str(scalar), "--label", f"synthetic packed {variant}",
                        *layouts, "--out", str(out / f"{variant}-layouts.json"),
                        "--summary-md", str(out / f"{variant}-layouts.md")], check=True)
        checks.append(f"{variant}: 1/2/4 assembled stages preserve every boundary/logit/token")

        # A dense-only range must also refuse an incompatible model profile,
        # before writing a plausible-looking partial package.
        for layers in ("0:1", "0:4"):
            refused = work / f"refused-{variant}-{layers.replace(':', '-')}.arcspkg"
            run("convert", "--source-dir", source, "--source-manifest", source_pin,
                "--layers", layers, "--experts", "i8", "--out", refused,
                error="pre-quantised INT4 experts require --experts i4g32")
            assert not refused.exists(), "refusal wrote a partial package"
        checks.append(f"{variant}: i8 refusal precedes output (dense-only and whole model)")

    yarn = work / "yarn"
    manifest = evidence / "yarn/manifest.json"
    pending = read(manifest)
    assert pending["pending"] == ["rope_scaling yarn"]
    assert all(pending[k] is None for k in ("model", "tables", "model_root"))
    for command in ("convert", "slice-assemble"):
        refused = work / f"refused-yarn-{command}.arcspkg"
        args = (("--source-dir", yarn, "--source-manifest", yarn / "tiny-kimi-packed.source.json",
                 "--experts", "i4g32") if command == "convert" else
                ("--manifest", manifest, "--slices", work / "yarn-streamed"))
        run(command, *args, "--out", refused, error="rope_scaling yarn")
        assert not refused.exists(), "YaRN refusal wrote a package"
    checks.append("YaRN: slices allowed; convert/assemble refuse before output; no model/tables/root")
    (out / "summary.json").write_text(json.dumps({
        "scope": "synthetic packed fixtures; no Kimi forward or performance claim",
        "checks": checks, "pass": True,
    }, indent=2) + "\n")
    print(json.dumps(checks, indent=2))


if __name__ == "__main__":
    main()
