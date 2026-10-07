"""Tests for arc_conformance.mla_moe_reference.

Run from scripts/:  python3 -m unittest arc_conformance.tests.test_mla_moe_reference
Spec: docs/protocol/integer-profile-mla-moe-dyadic-v1.md. Everything here is
synthetic (tiny DeepSeek-V3-shaped BF16 models written by
scripts/arc_mla/make_tiny_mla_model.py) and runs in well under a minute.
"""

from __future__ import annotations

import importlib.util
import json
import random
import tempfile
import unittest
from pathlib import Path

import numpy as np

from arc_conformance import mla_moe_reference as mm
from arc_conformance import modern_reference as dy

SCRIPTS = Path(__file__).resolve().parents[2]


def _load_generator():
    spec = importlib.util.spec_from_file_location("make_tiny_mla_model",
                                                  SCRIPTS / "arc_mla" / "make_tiny_mla_model.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


GEN = _load_generator()
# Both tiny shapes with INT8 dyadic experts and with INT4 group-32 experts.
CASES = [("moonlight", "i8"), ("kimi", "i8"), ("moonlight", "i4g32"), ("kimi", "i4g32")]
_TMP: tempfile.TemporaryDirectory = None
ROOT: Path = None


def setUpModule():
    global _TMP, ROOT
    _TMP = tempfile.TemporaryDirectory()
    ROOT = Path(_TMP.name)
    for variant in ("moonlight", "kimi"):
        GEN.write(ROOT / variant, variant)


def tearDownModule():
    _TMP.cleanup()


def _bf16(value: float) -> int:
    return int(np.array([value], dtype=np.float32).view(np.uint32)[0]) >> 16


def _f32(value: float) -> int:
    return int(np.array([value], dtype=np.float32).view(np.uint32)[0])


class IdentityAndConstants(unittest.TestCase):
    def test_profile_identity(self):
        self.assertEqual(dy.blake3_hex(mm.PROFILE_ID.encode()), mm.PROFILE_BLAKE3)
        self.assertEqual(dy.blake3_hex(mm.PROFILE_ID_I4G32.encode()), mm.PROFILE_I4G32_BLAKE3)

    def test_attention_lambda(self):
        self.assertEqual(mm.attention_lambda(192), 77_490_641)
        self.assertEqual(mm.attention_lambda(128), 94_906_265)

    def test_moonlight_config(self):
        config = json.loads((SCRIPTS / "arc_mla" / "moonlight_config.json").read_text())
        m = mm.model_from_config(config, 4096)
        self.assertEqual(m["routed_scaling_q32"], 10_505_490_006)
        self.assertEqual(m["rms_eps_q32"], 42_950)
        self.assertEqual((m["q_lora_rank"], m["n_routed_experts"], m["n_shared_experts"]), (0, 64, 2))
        bad = dict(config, rope_scaling={"type": "yarn"})
        with self.assertRaises(dy.PreparationError):
            mm.model_from_config(bad, 4096)


class RouterAndRouting(unittest.TestCase):
    def test_router_rows_match_the_literal_rule(self):
        rng = random.Random(7)
        rows = []
        for _ in range(200):
            exps = rng.randint(100, 126)
            row = [(rng.getrandbits(1) << 15) | (rng.randint(max(0, exps - 30), exps) << 7) | rng.getrandbits(7)
                   for _ in range(37)]
            row[rng.randrange(37)] = 0x8000
            row[rng.randrange(37)] = 0x0001
            rows.append(row)
        rows.append([0] * 37)
        q, k = mm.quantize_router_rows(np.array(rows, dtype=np.uint16))
        for i, row in enumerate(rows):
            eq, ek = mm.quantize_router_row_exact(row)
            self.assertEqual(q[i].tolist(), eq)
            self.assertEqual(int(k[i]), ek)

    def test_router_spec_examples(self):
        row = [_bf16(1.0), _bf16(-0.5), _bf16(2.0 ** -10), 0x3B81, 0xBB81, 0x0001, 0x8000]
        self.assertEqual(mm.quantize_router_row_exact(row), ([16384, -8192, 16, 65, -65, 0, 0], 14))
        with self.assertRaises(dy.PreparationError):
            mm.quantize_router_rows(np.array([[_bf16(65536.0)]], dtype=np.uint16))

    def test_bias_rounding(self):
        self.assertEqual(mm.bias_q32(*dy.bf16_parts(_bf16(1.0))), 1 << 32)
        self.assertEqual(mm.bias_q32(*mm.f32_parts(_f32(2.0 ** -33))), 1)
        self.assertEqual(mm.bias_q32(*mm.f32_parts(_f32(-(2.0 ** -33)))), -1)
        self.assertEqual(mm.bias_q32(*mm.f32_parts(_f32(2.0 ** -34))), 0)
        self.assertEqual(mm.bias_q32(*mm.f32_parts(_f32(1.5 * 2.0 ** -32))), 2)
        self.assertEqual(mm.bias_q32(*mm.f32_parts(1)), 0)
        with self.assertRaises(dy.DomainError):
            mm.bias_q32(*mm.f32_parts(_f32(2.0 ** 31)))

    def test_selection_ties_and_groups(self):
        keys = [5, 9, 9, 1, 9]
        self.assertEqual(mm.select_experts_int(keys, 3, 1, 1), [1, 2, 4])
        self.assertEqual(mm.select_experts_int([1, 2, 10, 0, 3, 3, 9, 9], 3, 4, 2), [2, 6, 7])
        self.assertEqual(mm.select_experts_int([5, 5, 4, 6, 1, 1, 0, 0], 2, 4, 1), [0, 1])
        self.assertEqual(mm.select_experts_int([5, 5, 4, 6, 1, 1, 0, 0], 2, 4, 2), [3, 0])

    def test_weights_and_combine(self):
        rho = 1 << 32
        self.assertEqual(mm.routing_weights_int([30000, 20000, 10000], rho, True),
                         [2147483648, 1431655765, 715827882])
        self.assertEqual(mm.routing_weights_int([40000, 40000], 10_505_490_006, True),
                         [5_252_745_003, 5_252_745_003])
        self.assertEqual(mm.combine_int([1 << 31, 1 << 30], [[4, -4], [8, 1]], [1, 1]), [5, -1])
        self.assertEqual(mm.combine_int([1 << 31] * 3, [[1], [1], [1]], [0]), [1])

    def test_int4_quantiser_matches_the_literal_rule(self):
        rng = random.Random(11)
        groups = []
        for _ in range(300):
            top = rng.randint(100, 126)
            g = [(rng.getrandbits(1) << 15) | (rng.randint(max(1, top - 12), top) << 7) | rng.getrandbits(7)
                 for _ in range(32)]
            g[rng.randrange(32)] = 0x8000
            g[rng.randrange(32)] = 0x0001
            groups.append(g)
        groups.append([0] * 32)
        packed, scales = mm.quantize_q4_rows(np.array(groups, dtype=np.uint16))
        values = mm.unpack_q4(packed)
        for i, g in enumerate(groups):
            scale, q = mm.quantize_q4_group_exact(g)
            self.assertEqual(int(scales[i][0]), scale, i)
            self.assertEqual(values[i].tolist(), q, i)

    def test_int4_spec_examples(self):
        row = [_bf16(1.0), _bf16(-0.5), _bf16(0.25)] + [0] * 29
        scale, q = mm.quantize_q4_group_exact(row)
        self.assertEqual(scale, (124 << 7) | 18)
        self.assertEqual(q[:3], [7, -4, 2])
        values = [[1] * 32 + [-2] * 32]
        self.assertEqual(mm.q4_project_int(list(range(1, 65)), values, [[_bf16(0.5), _bf16(0.25)]]), [-512])
        self.assertEqual(mm.q4_project_int([-1] + [0] * 31, [[1] + [0] * 31], [[_bf16(0.75)]]), [-1])
        self.assertEqual(mm.unpack_q4(mm.pack_q4(np.array([list(range(-8, 8))]))).tolist(), [list(range(-8, 8))])

    def test_rope_pairs(self):
        self.assertEqual(mm.rope_pairs_int([1 << 16, 3, -(1 << 16), 5], [0, 0], [1 << 16, 1 << 16]),
                         [-3, 1 << 16, -5, -(1 << 16)])
        self.assertEqual(mm.rope_pairs_int([1, 0], [1 << 15], [-(1 << 15)]), [0, -1])


class TinyModels(unittest.TestCase):
    def _prepare(self, variant: str, layers=None, name="full", fmt="i8"):
        src = ROOT / variant
        out = ROOT / f"{variant}-{fmt}-{name}.arcspkg"
        first, end = (None, None) if layers is None else layers
        ident = mm.prepare_stage(src, src / "tiny-mla.source.json", first, end, out, fmt)
        return out, ident

    def test_stage_packages_share_segments_with_the_whole_model(self):
        for variant, fmt in CASES:
            full, ident = self._prepare(variant, fmt=fmt)
            self.assertIn("model_root", ident)
            by_name = {s["name"]: s for s in ident["segments"]}
            for cut in ((0, 2), (2, 4), (1, 3)):
                _, part = self._prepare(variant, cut, f"{cut[0]}-{cut[1]}", fmt)
                for seg in part["segments"]:
                    self.assertEqual(seg, by_name[seg["name"]], (variant, cut, seg["name"]))
            # Digest-only preparation agrees with the written file.
            src = ROOT / variant
            again = mm.prepare_stage(src, src / "tiny-mla.source.json", None, None, None, fmt)
            self.assertEqual(again["sha256"], ident["sha256"])
            pkg = mm.StagePackage(full)
            self.assertEqual(pkg.segment_digests(), ident["segments"])

    def test_a_stage_converts_from_its_own_shard_only(self):
        src = ROOT / "moonlight"
        lone = ROOT / "moonlight-second-shard"
        lone.mkdir(exist_ok=True)
        for name in ("config.json", "model-00002-of-00002.safetensors"):
            (lone / name).write_bytes((src / name).read_bytes())
        ident = mm.prepare_stage(lone, src / "tiny-mla.source.json", 2, 4, ROOT / "lone.arcspkg")
        _, full = self._prepare("moonlight", (2, 4), "2-4-check")
        self.assertEqual(ident["sha256"], full["sha256"])
        with self.assertRaises(dy.PreparationError):
            mm.prepare_stage(lone, src / "tiny-mla.source.json", 0, 2, None)

    def test_fast_engine_equals_slow_engine(self):
        for variant, fmt in CASES:
            path, _ = self._prepare(variant, None, "engines", fmt)
            pkg = mm.StagePackage(path)
            seqs = [[1, 99, 123, 7, 64], [256, 256, 256], [5]]
            fast = mm.FastEngine(pkg)
            _, logits = fast.run(seqs)
            slow = mm.SlowEngine(pkg)
            row = 0
            for seq in seqs:
                slow.reset()
                for t in seq:
                    _, lg = slow.forward(token=t)
                    self.assertEqual(lg, logits[row].tolist(), (variant, fmt, seq, t))
                    row += 1

    def test_pipeline_layouts_are_byte_identical(self):
        for variant, fmt in CASES:
            full_path, ident = self._prepare(variant, None, "layout", fmt)
            root = ident["model_root"]
            seqs = [[1, 99, 123, 7, 64, 3], [256, 2]]
            meta = [{"id": f"s{i}", "tokens": s, "prompt_len": 2, "selection": "rp64-argmax", "eos": [7],
                     "max_tokens": 8} for i, s in enumerate(seqs)]
            reference = {}

            def record(layer, rows):
                reference[layer] = rows.copy()

            _, ref_logits = mm.FastEngine(mm.StagePackage(full_path)).run(seqs, on_layer=record)
            for cuts in ([0, 4], [0, 2, 4], [0, 1, 2, 3, 4]):
                data = None
                for a, b in zip(cuts, cuts[1:]):
                    path, _ = self._prepare(variant, (a, b), f"{a}-{b}", fmt)
                    pkg = mm.StagePackage(path)
                    inputs = None if data is None else [s["values"] for s in mm.read_boundary(data)["sequences"]]
                    h, logits = mm.FastEngine(pkg).run(seqs, inputs)
                    np.testing.assert_array_equal(h, reference[b])
                    start, out = 0, []
                    for s in meta:
                        n = len(s["tokens"])
                        out.append(dict(s, values=h[start:start + n]))
                        start += n
                    data, _ = mm.write_boundary(None, b, pkg.model["d_model"], root, out, pkg.profile)
                    if b == 4:
                        np.testing.assert_array_equal(logits, ref_logits)

    def test_boundary_files_round_trip_and_refuse_tampering(self):
        values = np.arange(12, dtype=np.int64).reshape(3, 4) - 5
        seq = {"id": "a", "tokens": [1, 2, 3], "prompt_len": 1, "selection": "argmax", "eos": [], "max_tokens": 3,
               "values": values}
        data, _ = mm.write_boundary(None, 2, 4, "ab" * 32, [seq])
        back = mm.read_boundary(data)
        np.testing.assert_array_equal(back["sequences"][0]["values"], values)
        flipped = bytearray(data)
        flipped[-1] ^= 1
        with self.assertRaises(dy.PackageError):
            mm.read_boundary(bytes(flipped))

    def test_generation_and_verification_agree(self):
        for variant, fmt in CASES:
            path, _ = self._prepare(variant, None, "gen", fmt)
            run = mm.generate_run(path, ROOT / variant / "tiny-mla.cases.json")
            run_path = ROOT / f"{variant}-{fmt}-run.json"
            run_path.write_text(json.dumps(run))
            golden, problems = mm.verify_run(path, run_path)
            self.assertEqual(problems, [], (variant, fmt))
            self.assertEqual(golden["matrix_digest"], run["matrix_digest"])


if __name__ == "__main__":
    unittest.main()
