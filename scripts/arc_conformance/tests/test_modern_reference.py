"""Tests for arc_conformance.modern_reference.

Run from scripts/:  python3 -m unittest arc_conformance.tests.test_modern_reference
Spec: docs/protocol/integer-profile-hf-llama-dyadic-v1.md. Everything here is
synthetic (a 64-wide, 4-layer BF16 model written by
scripts/arc_modern/make_tiny_model.py) and runs in seconds.
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import random
import shutil
import tempfile
import unittest
from fractions import Fraction
from pathlib import Path

import numpy as np

from arc_conformance import modern_reference as mr

SCRIPTS = Path(__file__).resolve().parents[2]

# Regression pin: make_tiny_model.py + this preparer must give these exact bytes on every OS.
# (Re-pin deliberately if the tiny-model generator changes; CI separately requires the Rust
# converter to produce the same package.)
TINY_PACKAGE_SHA256 = "642f58445725b7cd19c6619655d86aa4553feeefcffa95c9e3ed832080f5e851"


def _load_tiny_generator():
    spec = importlib.util.spec_from_file_location("make_tiny_model", SCRIPTS / "arc_modern" / "make_tiny_model.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


TINY = _load_tiny_generator()
_TMP: tempfile.TemporaryDirectory = None
SRC: Path = None
PKG: Path = None
IDENT: dict = None


def setUpModule():
    global _TMP, SRC, PKG, IDENT
    _TMP = tempfile.TemporaryDirectory()
    SRC = Path(_TMP.name) / "tiny"
    TINY.write(SRC)
    PKG = Path(_TMP.name) / "tiny.pkg"
    IDENT = mr.prepare_package(SRC, SRC / "tiny-smollm3.source.json", PKG)


def tearDownModule():
    _TMP.cleanup()


class Tables(unittest.TestCase):
    def test_exp_table_is_pinned_and_correctly_rounded(self):
        t = mr.exp_table()
        self.assertEqual(mr.exp_table_blake3(t), mr.EXP_TABLE_BLAKE3)
        self.assertEqual((t[4096], t[3840], t[0:4]), (65536, 24109, [0, 0, 0, 0]))
        self.assertTrue(all(a <= b for a, b in zip(t, t[1:])))

    def test_rope_tables_for_smollm3_are_pinned(self):
        cos, sin = mr.build_rope_tables(5_000_000, 128, 4096)
        self.assertEqual(cos.shape, (4096, 64))
        self.assertEqual(mr.rope_tables_blake3(cos, sin), mr.ROPE_SMOLLM3_BLAKE3)
        self.assertEqual(cos[1][:3].tolist(), [35409, 46321, 53432])
        self.assertEqual(sin[1][:3].tolist(), [55147, 46361, 37947])
        self.assertEqual(int(sin[4095][63]), 68)
        self.assertEqual((int(cos[0][0]), int(sin[0][0])), (65536, 0))

    def test_rope_refuses_non_integer_or_odd_geometry(self):
        with self.assertRaises(mr.PreparationError):
            mr.build_rope_tables(10000, 7, 4)
        with self.assertRaises(mr.PreparationError):
            mr.build_rope_tables(1.5, 8, 4)

    def test_identity_strings(self):
        for text, digest in mr.IDENTITY_BLAKE3.items():
            self.assertEqual(mr.blake3_hex(text.encode()), digest)

    def test_exp_operator(self):
        t = mr.exp_table()
        self.assertEqual(mr.exp_q16(0), 65536)
        self.assertEqual(mr.exp_q16(-16 * 65536), 0)
        self.assertEqual(mr.exp_q16(-(1 << 40)), 0)
        self.assertEqual(mr.exp_q16(-65536), 24109)
        x = -65536 + 128  # o = 15*2^16 + 128: i = 3840, f = 128
        self.assertEqual(mr.exp_q16(x), t[3840] + (((t[3841] - t[3840]) * 128) >> 8))
        self.assertEqual(mr.exp_q16(-1), t[4095] + (((t[4096] - t[4095]) * 255) >> 8))
        with self.assertRaises(mr.DomainError):
            mr.exp_q16(1)
        xs = np.array([0, -1, -255, -256, -65536, -16 * 65536 + 1, -16 * 65536, -(1 << 50)], dtype=np.int64)
        self.assertEqual(mr._exp_vec(xs).tolist(), [mr.exp_q16(int(v)) for v in xs])


class Bf16(unittest.TestCase):
    def test_exact_values(self):
        self.assertEqual(mr.bf16_parts(0x3F80), (0, 128, -7))
        self.assertEqual(mr.bf16_value(0x3F80), 1)
        self.assertEqual(mr.bf16_value(0xBF80), -1)
        self.assertEqual(mr.bf16_value(0x0001), Fraction(1, 2 ** 133))  # smallest subnormal
        self.assertEqual(mr.bf16_parts(0x007F), (0, 127, -133))  # largest subnormal
        self.assertEqual(mr.bf16_value(0x0080), Fraction(1, 2 ** 126))  # smallest normal
        self.assertEqual(mr.bf16_parts(0x8000), (1, 0, -133))  # negative zero
        self.assertEqual(mr.bf16_value(0x8000), 0)
        self.assertEqual(mr.bf16_value(0x7F7F), 255 * 2 ** 120)  # largest finite

    def test_infinity_and_nan_are_refused(self):
        for bits in (0x7F80, 0xFF80, 0x7FC0, 0x7F81, 0xFFFF):
            with self.assertRaises(mr.PreparationError):
                mr.bf16_parts(bits)
        with self.assertRaises(mr.PreparationError):
            mr.quantize_rows(np.array([[0x3F80, 0x7F80]], dtype=np.uint16))

    def test_magnitude_orders_as_the_low_15_bits(self):
        rng = random.Random(7)
        finite = [b for b in (rng.randrange(0x10000) for _ in range(400)) if (b >> 7) & 0xFF != 0xFF]
        for a, b in zip(finite, finite[1:]):
            self.assertEqual(abs(mr.bf16_value(a)) < abs(mr.bf16_value(b)), (a & 0x7FFF) < (b & 0x7FFF))


class Quantization(unittest.TestCase):
    def check(self, bits, q, mu, k):
        self.assertEqual(mr.quantize_row_exact(bits), (q, mu, k))
        vq, vmu, vk = mr.quantize_rows(np.array([bits], dtype=np.uint16))
        self.assertEqual((vq[0].tolist(), int(vmu[0]), int(vk[0])), (q, mu, k))

    def test_hand_checked_rows(self):
        # [1, -1/2, 1/4, 0]: 127*(-1/2) = -63.5 rounds away from zero to -64; 31.75 -> 32.
        # t = 30 (128*2^30 >= 127*2^30), mu = rha(2^37/127) = 1082196484, k = 30 + 7.
        self.check([0x3F80, 0xBF00, 0x3E80, 0x0000], [127, -64, 32, 0], 1082196484, 37)
        # Maximum tied between 3 and -3; 127/3 = 42.33 -> 42; mu = rha(192*2^30/127), k = 30 + 6.
        self.check([0x4040, 0xC040, 0x3F80], [127, -127, 42], 1623294726, 36)
        # Positive half-way tie: 127 * 1/2 = 63.5 -> 64.
        self.check([0x3F80, 0x3F00], [127, 64], 1082196484, 37)

    def test_zero_rows_and_tiny_elements(self):
        self.check([0x8000, 0x0000, 0x8000], [0, 0, 0], 0, 16)
        # 2^-70 is 70 binades below the maximum (d > 60): q = 0. Subnormal below 1.0: q = 0.
        self.check([0x3F80, 0x1C80, 0x0001], [127, 0, 0], 1082196484, 37)

    def test_out_of_range_scales_are_refused(self):
        with self.assertRaises(mr.PreparationError):  # only subnormals: k = 37 + 133 > 62
            mr.quantize_row_exact([0x0001, 0x0000])
        with self.assertRaises(mr.PreparationError):
            mr.quantize_rows(np.array([[0x0001, 0x0000]], dtype=np.uint16))
        # 2^22: e_A = 15, t = 30, k = 15 < 16 refused; 2^21 gives k = 16, accepted.
        with self.assertRaises(mr.PreparationError):
            mr.quantize_rows(np.array([[0x4A80, 0x3F80]], dtype=np.uint16))
        self.assertEqual(int(mr.quantize_rows(np.array([[0x4A00, 0x3F80]], dtype=np.uint16))[2][0]), 16)

    def test_random_rows_match_the_literal_rule_and_its_properties(self):
        rng = random.Random(11)
        rows = []
        for _ in range(300):
            width = rng.randrange(1, 24)
            top = rng.randrange(95, 155)  # around the accepted k range, so some rows are refused
            spread = rng.choice([2, 8, 30, 90])
            row = []
            for _ in range(width):
                r = rng.random()
                if r < 0.08:
                    row.append(rng.choice([0x0000, 0x8000]))
                elif r < 0.14:
                    row.append(rng.randrange(1, 0x80) | (rng.randrange(2) << 15))  # subnormal
                else:
                    e = max(1, top - rng.randrange(spread))
                    row.append((rng.randrange(2) << 15) | (e << 7) | rng.randrange(128))
            rows.append(row)
        accepted = refused = 0
        for row in rows:
            try:
                q, mu, k = mr.quantize_row_exact(row)
            except mr.PreparationError:
                with self.assertRaises(mr.PreparationError):
                    mr.quantize_rows(np.array([row], dtype=np.uint16))
                refused += 1
                continue
            accepted += 1
            vq, vmu, vk = mr.quantize_rows(np.array([row], dtype=np.uint16))
            self.assertEqual((vq[0].tolist(), int(vmu[0]), int(vk[0])), (q, mu, k))
            self.assertTrue(all(-127 <= v <= 127 for v in q))
            amax = max(abs(mr.bf16_value(b)) for b in row)
            if amax == 0:
                self.assertEqual((mu, k), (0, 16))
                continue
            self.assertTrue(1 << 30 <= mu < 1 << 31 and 16 <= k <= 62)
            self.assertLessEqual(abs(Fraction(mu, 2 ** k) - amax / 127), (amax / 127) / 2 ** 31)
            for b, qv in zip(row, q):
                exact = 127 * mr.bf16_value(b) / amax
                self.assertLessEqual(abs(exact - qv), Fraction(1, 2))
                if abs(exact - qv) == Fraction(1, 2):
                    self.assertGreater(abs(qv), abs(exact))  # ties away from zero
        self.assertGreater(accepted, 200)
        self.assertGreater(refused, 0)


class NormsAndConfig(unittest.TestCase):
    def test_norm_gain_rounding(self):
        cases = {0x3F80: 65536, 0xBF80: -65536, 0x37C0: 2, 0xB7C0: -2, 0x3820: 3, 0x3700: 1,
                 0xB700: -1, 0x0003: 0, 0x8000: 0, 0x3680: 0, 0x4040: 3 * 65536}
        for bits, gain in cases.items():
            self.assertEqual(mr.norm_gain(bits), gain, hex(bits))
            v = mr.bf16_value(bits) * 65536
            self.assertEqual(gain, mr.rha_ratio(v.numerator, v.denominator))
        with self.assertRaises(mr.PreparationError):
            mr.norm_gain(0x7F7F)  # 255 * 2^136 does not fit i64

    def test_rms_eps(self):
        self.assertEqual(mr.rms_eps_q32(1e-06), 4295)
        self.assertEqual(mr.rms_eps_q32(1e-05), 42950)
        for bad in (0, 1e-12, None, True):
            with self.assertRaises(mr.PreparationError):
                mr.rms_eps_q32(bad)

    def test_unsupported_configs_are_refused(self):
        base = dict(TINY.CONFIG)
        good = mr.model_from_config(base, 64)
        self.assertEqual(good["rope_layers"], [1, 1, 1, 0])
        self.assertEqual((good["d_head"], good["rms_eps_q32"], good["rope_theta"]), (16, 4295, 5000000))
        for key, value in (("hidden_act", "gelu"), ("rope_scaling", {"type": "yarn"}), ("attention_bias", True),
                           ("mlp_bias", True), ("tie_word_embeddings", False), ("use_sliding_window", True),
                           ("rope_theta", 5000000.5), ("no_rope_layers", [1, 1, 1]), ("head_dim", 8),
                           ("num_key_value_heads", 3), ("rms_norm_eps", 0.0)):
            cfg = dict(base, **{key: value})
            with self.assertRaises(mr.PreparationError, msg=key):
                mr.model_from_config(cfg, 64)
        cfg = dict(base)
        del cfg["no_rope_layers"]
        with self.assertRaises(mr.PreparationError):
            mr.model_from_config(cfg, 64)


class PackageFile(unittest.TestCase):
    def test_prepare_is_deterministic(self):
        again = Path(_TMP.name) / "again.pkg"
        ident = mr.prepare_package(SRC, SRC / "tiny-smollm3.source.json", again)
        self.assertEqual(ident, IDENT)
        self.assertEqual(again.read_bytes(), PKG.read_bytes())
        self.assertEqual(IDENT["sha256"], TINY_PACKAGE_SHA256)

    def test_layout_and_contents(self):
        pkg = mr.Package(PKG, check_values=True, check_tables=True)
        self.assertEqual(pkg.identity(), IDENT)
        data = PKG.read_bytes()
        self.assertEqual(data[:8], b"ARCIPKG1")
        hlen = int.from_bytes(data[8:16], "little")
        self.assertEqual(data[16:16 + hlen], mr.canonical_json(pkg.header))
        self.assertEqual(pkg.data_start % 64, 0)
        self.assertEqual(len(data) % 64, 0)
        names = [e["name"] for e in pkg.header["tensors"]]
        self.assertEqual(names[:6], ["embed.q", "embed.mu", "embed.k", "final_norm", "rope.cos", "rope.sin"])
        self.assertEqual(names[6:9], ["layers.0.attn_norm", "layers.0.wq.q", "layers.0.wq.mu"])
        self.assertEqual(len(names), 6 + 4 * 23)
        self.assertTrue(all(e["offset"] % 64 == 0 for e in pkg.header["tensors"]))
        self.assertEqual(pkg.header["source"]["files"],
                         json.loads((SRC / "tiny-smollm3.source.json").read_text())["files"])
        shard = mr.SafetensorsShard(SRC / "model.safetensors")
        emb = np.asarray(shard.bf16("model.embed_tokens.weight"))
        for row in (0, TINY.ZERO_EMBED_ROW, 299):
            q, mu, k = mr.quantize_row_exact(emb[row].tolist())
            self.assertEqual(pkg.tensor("embed.q")[row].tolist(), q)
            self.assertEqual((int(pkg.tensor("embed.mu")[row]), int(pkg.tensor("embed.k")[row])), (mu, k))
        self.assertEqual(int(pkg.tensor("embed.mu")[TINY.ZERO_EMBED_ROW]), 0)
        self.assertEqual(int(pkg.tensor("layers.1.wk.mu")[0]), 0)
        norm_bits = np.asarray(shard.bf16("model.layers.0.input_layernorm.weight")).tolist()
        self.assertEqual(pkg.tensor("layers.0.attn_norm").tolist(), [mr.norm_gain(b) for b in norm_bits])
        self.assertEqual(pkg.tensor("layers.0.attn_norm")[:5].tolist(), [2, -2, 3, 0, 0])

    def _mutated(self, edit) -> Path:
        data = bytearray(PKG.read_bytes())
        edit(data)
        path = Path(_TMP.name) / "mutated.pkg"
        path.write_bytes(bytes(data))
        return path

    def test_damaged_packages_are_refused(self):
        pkg = mr.Package(PKG, check_values=False)
        q_off = pkg.data_start + pkg.entries["layers.0.wq.q"]["offset"]
        mu_end = pkg.data_start + pkg.entries["embed.mu"]["offset"] + pkg.entries["embed.mu"]["bytes"]

        def set_byte(pos, value):
            def edit(d):
                d[pos] = value
            return edit

        edits = {
            "magic": set_byte(0, ord("X")),
            "minus 128": set_byte(q_off, 0x80),
            "padding": set_byte(mu_end, 1),
            "header space": set_byte(16 + 1, ord(" ")),
            "small mu": set_byte(pkg.data_start + pkg.entries["embed.mu"]["offset"] + 3, 0x00),
            "truncated": lambda d: d.__delitem__(slice(len(d) - 64, None)),
        }
        for label, edit in edits.items():
            with self.assertRaises(mr.PackageError, msg=label):
                mr.Package(self._mutated(edit), check_values=True)

    def test_source_files_are_verified_before_conversion(self):
        bad = Path(_TMP.name) / "bad-src"
        shutil.copytree(SRC, bad)
        config = (bad / "config.json").read_bytes()
        (bad / "config.json").write_bytes(config.replace(b'"hidden_size": 64', b'"hidden_size": 65'))
        with self.assertRaises(mr.PreparationError):
            mr.prepare_package(bad, bad / "tiny-smollm3.source.json", Path(_TMP.name) / "x.pkg")
        (bad / "config.json").write_bytes(config + b" ")
        with self.assertRaisesRegex(mr.PreparationError, "bytes"):
            mr.prepare_package(bad, bad / "tiny-smollm3.source.json", Path(_TMP.name) / "x.pkg")
        self.assertFalse((Path(_TMP.name) / "x.pkg").exists())


class ExactFastArithmetic(unittest.TestCase):
    def test_mul_u31_shr_matches_python(self):
        rng = np.random.default_rng(3)
        a = np.concatenate([rng.integers(-(1 << 62), 1 << 62, size=(64, 1)),
                            rng.integers(-(1 << 40), 1 << 40, size=(64, 1))], axis=0)
        a = np.repeat(a, 47, axis=1)
        a[0, :] = (1 << 63) - 1
        a[1, :] = -(1 << 63)
        mu = rng.integers(0, 1 << 31, size=47)
        mu[0] = (1 << 31) - 1
        ks = np.arange(16, 63)
        small = a.copy()
        small[:64] >>= 30  # keep the k < 32 columns in range for the int64 path
        y = mr._mul_u31_shr(small, mu, ks)
        for i in range(small.shape[0]):
            for j in range(47):
                self.assertEqual(int(y[i, j]), (int(small[i, j]) * int(mu[j])) >> int(ks[j]))
        big = mr._mul_u31_shr(a[:, 16:], mu[16:], ks[16:])  # k >= 32: never needs Python
        for i in range(a.shape[0]):
            for j in range(big.shape[1]):
                self.assertEqual(int(big[i, j]), (int(a[i, 16 + j]) * int(mu[16 + j])) >> int(ks[16 + j]))
        with self.assertRaises(mr._NeedSlow):
            mr._mul_u31_shr(a[:2, :1], mu[:1], ks[:1])

    def test_mul_shr_matches_python(self):
        rng = np.random.default_rng(5)
        a = rng.integers(-(1 << 62), (1 << 62) + 1, size=400)
        b = rng.integers(-(1 << 20), 1 << 20, size=400)
        a[:3] = [1 << 62, -(1 << 62), 0]
        for s in (46, 62):
            y = mr._mul_shr(a, b, s)
            self.assertEqual(y.tolist(), [(int(x) * int(z)) >> s for x, z in zip(a, b)])
        c = rng.integers(-(1 << 40), 1 << 40, size=400)
        d = rng.integers(-(1 << 40), 1 << 40, size=400)
        for s in (32, 46):
            self.assertEqual(mr._mul_shr(c, d, s).tolist(), [(int(x) * int(z)) >> s for x, z in zip(c, d)])
        with self.assertRaises(mr._NeedSlow):
            mr._mul_shr(np.array([1 << 62]), np.array([1 << 62]), 46)

    def test_matmul_exact_with_and_without_limbs(self):
        rng = np.random.default_rng(9)
        # Plain float path; limbs on the left operand (weights); limbs on both operands.
        for amag, bmag, k in ((1 << 20, 127, 64), (1 << 44, 127, 64), (1 << 46, 127, 300), (1 << 30, 1 << 25, 16)):
            a = rng.integers(-amag, amag, size=(5, k))
            b = rng.integers(-bmag, bmag + 1, size=(k, 7))
            want = [[sum(int(a[i, t]) * int(b[t, j]) for t in range(k)) for j in range(7)] for i in range(5)]
            if bmag == 127:
                got = mr._matmul_exact(a, b_float=b.astype(np.float64), b_max=127, guard=False)
            else:
                got = mr._matmul_exact(a, b)
            self.assertEqual(got.tolist(), want)
        with self.assertRaises(mr._NeedSlow):
            mr._matmul_exact(np.full((1, 4), 1 << 60), np.full((4, 1), 1 << 10))


class Selection(unittest.TestCase):
    def test_argmax_and_rp64(self):
        logits = [10, 12, -6, 12]
        self.assertEqual(mr.select_next(logits, [], "argmax"), 1)  # lowest id on ties
        self.assertEqual(mr.select_next(logits, [1], "rp64-argmax"), 3)  # 12 -> 10
        self.assertEqual(mr.select_next(logits, [1, 3], "rp64-argmax"), 0)  # both 10: lowest id
        self.assertEqual(mr.select_next([10, 11, -6, 0], [1, 1], "rp64-argmax"), 0)  # 11 -> 9 -> 7
        self.assertEqual(mr.select_next([-6, -7], [0], "rp64-argmax"), 0)  # -6 -> -7 (tdiv(-36, 5))
        self.assertEqual(mr.select_next([-6, -7], [0, 0], "rp64-argmax"), 1)  # -7 -> -8
        self.assertEqual(mr.select_next([0, -1], [0], "rp64-argmax"), 0)  # 0 stays 0
        # Only the 64 most recent generated tokens count: token 0 is 65th newest, then 64th.
        self.assertEqual(mr.select_next([12, 11, 11], [0] + [2] * 64, "rp64-argmax"), 0)
        self.assertEqual(mr.select_next([12, 11, 11], [0] + [2] * 63, "rp64-argmax"), 1)
        with self.assertRaises(mr.DomainError):
            mr.select_next([-(1 << 62), 0], [0], "rp64-argmax")

    def test_digests(self):
        self.assertEqual(mr.output_hash([1, 2]), mr.blake3_hex(b"\x01\x00\x00\x00\x02\x00\x00\x00"))
        raw = mr.logits_hash_raw([-1, 2])
        self.assertEqual(raw, mr.blake3_raw(b"\xff" * 8 + b"\x02" + b"\x00" * 7))
        self.assertEqual(mr.logits_digest([raw, raw]), mr.blake3_hex(raw + raw))
        cases = [{"id": "a", "tokens": [3], "output_hash": "x", "logits_digest": "y", "extra": 1}]
        self.assertEqual(mr.matrix_digest(cases), mr.blake3_hex(
            b'[{"id":"a","logits_digest":"y","output_hash":"x","tokens":[3]}]'))


class Engines(unittest.TestCase):
    SEQUENCES = [[1, 99, 123, 7, 64, 299, 17, 200, 5, 5, 11], [TINY.ZERO_EMBED_ROW, 0, 62],
                 [256, 2, 100, 227, 227, 156]]

    @classmethod
    def setUpClass(cls):
        cls.pkg = mr.Package(PKG)
        slow = mr.SlowEngine(cls.pkg)
        cls.expected = []
        for seq in cls.SEQUENCES:
            slow.reset()
            cls.expected.append([slow.forward(t) for t in seq])

    def assert_fast_matches(self, engine):
        got = engine.all_logits(self.SEQUENCES)
        for s, (want_seq, got_seq) in enumerate(zip(self.expected, got)):
            self.assertEqual(len(want_seq), len(got_seq))
            for p, (w, g) in enumerate(zip(want_seq, got_seq)):
                self.assertEqual(g.tolist(), w, f"sequence {s} position {p}")

    def test_fast_batched_forward_equals_slow_forward(self):
        engine = mr.FastEngine(self.pkg)
        self.assert_fast_matches(engine)
        self.assertEqual(engine.fallbacks, {})

    def test_fast_forward_with_forced_limb_splitting(self):
        saved = mr.FLOAT_EXACT_LIMIT
        mr.FLOAT_EXACT_LIMIT = float(1 << 22)
        try:
            self.assert_fast_matches(mr.FastEngine(self.pkg))
        finally:
            mr.FLOAT_EXACT_LIMIT = saved

    def test_operators_on_large_values_match_python(self):
        engine = mr.FastEngine(self.pkg)
        rng = np.random.default_rng(17)
        x = rng.integers(-(1 << 44), 1 << 44, size=(3, 64))  # mean square stays below 2^92
        g = rng.integers(-(1 << 30), 1 << 30, size=64)
        want = [mr.rmsnorm_int(r, g.tolist(), 4295) for r in x.tolist()]
        self.assertEqual(engine._rmsnorm_fast(x, g, "t").tolist(), want)
        gate = rng.integers(-(1 << 40), 1 << 40, size=(2, 50))  # |g*sigma*u| >> 32 stays below 2^62
        gate[0, :6] = [0, -1, 1, -(16 << 16), 16 << 16, 5]
        up = rng.integers(-(1 << 35), 1 << 35, size=(2, 50))
        self.assertEqual(engine._silu(gate, up, "t").tolist(),
                         [[mr.silu_gate_int(a, b) for a, b in zip(gr, ur)] for gr, ur in zip(gate.tolist(), up.tolist())])
        q = rng.integers(-(1 << 40), 1 << 40, size=(4, 2 * 16))
        pos = np.array([0, 1, 30, 63])
        rot = engine._rope(q, 2, pos, "t").tolist()
        for i, p in enumerate(pos.tolist()):
            c, s = engine.cos[p].tolist(), engine.sin[p].tolist()
            row = q[i].tolist()
            self.assertEqual(rot[i], mr.rope_int(row[:16], c, s) + mr.rope_int(row[16:], c, s))
        self.assertEqual(engine.fallbacks, {})

    def test_domain_refusals(self):
        slow = mr.SlowEngine(self.pkg)
        with self.assertRaises(mr.DomainError):
            slow.forward(300)
        with self.assertRaises(mr.DomainError):
            mr.FastEngine(self.pkg).all_logits([[0] * 65])
        with self.assertRaises(mr.DomainError):
            mr.project_int([1 << 60] * 64, [[1] * 64], [1 << 30], [40])
        with self.assertRaises(mr.DomainError):
            mr.check_i32([1 << 31], "K")


class Runs(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.run_doc = mr.generate_run(PKG, SRC / "tiny-smollm3.cases.json")
        cls.run_path = Path(_TMP.name) / "run.json"
        cls.run_path.write_text(json.dumps(cls.run_doc))

    def test_generation_follows_the_stop_rule(self):
        self.assertEqual(self.run_doc["package"], {"sha256": IDENT["sha256"], "bytes": IDENT["bytes"]})
        for case in self.run_doc["cases"]:
            self.assertTrue(mr.stop_rule_ok(case))
            self.assertEqual(len(case["logits_hashes"]), len(case["prompt_tokens"]) + len(case["tokens"]) - 1)
        self.assertEqual(self.run_doc["matrix_digest"], mr.matrix_digest(self.run_doc["cases"]))

    def test_verify_run_accepts_an_honest_run(self):
        golden, problems = mr.verify_run(PKG, self.run_path)
        self.assertEqual(problems, [])
        self.assertTrue(golden["checks"]["all_match"])
        self.assertEqual(golden["matrix_digest"], self.run_doc["matrix_digest"])
        out = Path(_TMP.name) / "golden.json"
        with contextlib.redirect_stdout(io.StringIO()):
            code = mr.main(["verify-run", "--package", str(PKG), "--run", str(self.run_path), "--out", str(out)])
        self.assertEqual(code, 0)

    def _tampered(self, edit) -> Path:
        run = json.loads(json.dumps(self.run_doc))
        edit(run)
        path = Path(_TMP.name) / "tampered.json"
        path.write_text(json.dumps(run))
        return path

    def test_verify_run_rejects_tampering(self):
        def flip_hash(run):
            h = run["cases"][2]["logits_hashes"][4]
            run["cases"][2]["logits_hashes"][4] = ("0" if h[0] != "0" else "1") + h[1:]

        def change_token(run):
            run["cases"][1]["tokens"][3] = (run["cases"][1]["tokens"][3] + 1) % 300

        def shorten(run):
            run["cases"][0]["tokens"] = run["cases"][0]["tokens"][:-1]

        def overrun(run):
            case = run["cases"][3]
            case["tokens"] = case["tokens"] + [1] * (case["max_tokens"] + 60)

        for label, edit, needle in (("hash", flip_hash, "position 4"), ("token", change_token, "token 3"),
                                    ("short", shorten, "stop rule"), ("long", overrun, "stop rule")):
            path = self._tampered(edit)
            _, problems = mr.verify_run(PKG, path)
            self.assertTrue(problems, label)
            self.assertTrue(any(needle in p for p in problems), (label, problems))
            err = io.StringIO()
            with contextlib.redirect_stderr(err):
                code = mr.main(["verify-run", "--package", str(PKG), "--run", str(path),
                                "--out", str(Path(_TMP.name) / "g.json")])
            self.assertEqual(code, 1, label)
            self.assertIn("MISMATCH", err.getvalue())


if __name__ == "__main__":
    unittest.main()
