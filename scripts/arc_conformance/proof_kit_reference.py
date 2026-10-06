"""Independent reference for the ARC Proof Kit (docs/proof-kit.md).

Written from docs/proof-kit.md, not translated from the Rust kit, so that the
two implementations agreeing in CI is evidence that the written rules are
complete. Three things live here:

* ``challenge_prompt``: the anti-faking challenge prompt (``arc.proof-challenge.v1``)
  derived from a server seed: SHA-256 picks four words from the public word
  list docs/protocol/proof-kit/challenge-words-v1.txt and fills a fixed,
  harmless sentence template. A Hash Wall server, the Rust kit and this file
  must derive the identical text.
* ``validate_result``: the strict checker for one volunteer result
  (``arc.proof-result.v1``): exact field sets, formats, sizes and the internal
  consistency of every digest. It is the reference for the Hash Wall's
  server-side validator.
* A command line: ``derive``, ``vectors`` and ``validate``.

Standard library only, except BLAKE3: the digest consistency checks need the
``blake3`` package (``pip install blake3``) and are reported as skipped
without it.

Run from scripts/:  python3 -m arc_conformance.proof_kit_reference --help
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence

try:  # Only the digest consistency checks need BLAKE3.
    import blake3 as _blake3_module  # type: ignore
except ImportError:  # pragma: no cover - exercised only without the package
    _blake3_module = None

REPO_ROOT = Path(__file__).resolve().parents[2]
WORDS_PATH = REPO_ROOT / "docs" / "protocol" / "proof-kit" / "challenge-words-v1.txt"
VECTORS_PATH = REPO_ROOT / "docs" / "protocol" / "proof-kit" / "challenge-vectors-v1.json"

RESULT_SCHEMA = "arc.proof-result.v1"
CHALLENGE_SCHEME = "arc.proof-challenge.v1"
SPEED_METHOD = "arc.proof-speed.v1"
MAX_RESULT_BYTES = 8192

# --- the challenge (docs/proof-kit.md, "The challenge prompt") -------------

CHALLENGE_DOMAIN = CHALLENGE_SCHEME.encode("ascii") + b"\x00"
CHALLENGE_TEMPLATE = "Write one short sentence that mentions {0}, {1}, {2} and {3}."
WORDS_SHA256 = "4eda9812f9ae77ed55b5e73f10506e8f3cf3f294dc1f086c77884be95917ad60"
CHALLENGE_TODAY = "06 October 2026"
CHALLENGE_MAX_TOKENS = 24
CHALLENGE_EOS = [128012]
CHALLENGE_SELECTION = "rp64-argmax"

TEST_CHALLENGE = {
    "challenge_id": "test-challenge-v1",
    "seed": "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
    "expires_at": "2099-12-31T23:59:59Z",
    "signature": "unsigned-test-challenge",
}
# The test challenge's digest from the kit's own CI dry run (both CPU kernels,
# Proof Kit CI run 37507415675); every computer should reproduce it.
TEST_CHALLENGE_DIGEST = "fad7f4483e2092bd21f669fad7bc70e6f203792a81ddce440026917ffaba4f6c"

# --- the pinned workload ----------------------------------------------------

PUBLISHED_GOLDEN_DIGEST = "3e43f342c00cf3e3be3072e654e9d1c43f547a73b8a4fc6e906b7119e5cb49f2"
PINNED_MODEL = {
    "repo": "HuggingFaceTB/SmolLM3-3B",
    "revision": "a07cc9a04f16550a088caea529712d1d335b0ac1",
    "profile": "arc.hf-llama.i8-dyadic-row.q16.v1",
    "package_sha256": "19c67496ee23fe5da0e12eb1f22cb17f6386c560071587b5b8dfa68731c0aa91",
    "manifest_blake3": "af388d01c3578c5f97238fd74aaa3d0d8194d29fc6fbd99f3aae226064659fa2",
}
# Golden case ids in order, with each case's token budget
# (scripts/arc_modern/smollm3_cases.json).
GOLDEN_CASES = [("capital", 24), ("haiku", 24), ("integers", 24), ("primes", 24),
                ("product-greedy", 16)]
VOCAB_SIZE = 128256

# --- closed vocabularies -----------------------------------------------------

BACKENDS = ("cpu-scalar", "cpu-simd", "gpu-wgpu")
ISAS = ("avx2", "neon-dotprod")
GPU_APIS = ("vulkan", "metal", "dx12", "gl")
VERDICTS = ("MATCH", "MISMATCH")
OPS = ("tokenizer", "embed", "attn_norm", "wq", "wk", "wv", "rope_q", "rope_k", "attention",
       "wo", "attn_residual", "ffn_norm", "w_gate", "w_up", "silu", "w_down", "ffn_residual",
       "final_norm", "lm_head", "select")
CPU_FEATURES = ("avx2", "fma", "avx512f", "avx512bw", "dotprod", "i8mm", "sve", "sve2")
MEMORY_CLASSES_GB = (1, 2, 4, 8, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024)
VRAM_CLASSES_GB = (1, 2, 4, 6, 8, 10, 12, 16, 20, 24, 32, 40, 48, 64, 80, 96, 128, 192)
NETWORK_CLASSES_MBPS = (1, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000)

HEX64 = re.compile(r"^[0-9a-f]{64}$")
NONCE = re.compile(r"^[0-9a-f]{32}$")
VERSION = re.compile(r"^[0-9]{1,4}\.[0-9]{1,4}\.[0-9]{1,6}$")
CHALLENGE_ID = re.compile(r"^[A-Za-z0-9._-]{1,128}$")
SIGNATURE = re.compile(r"^[A-Za-z0-9._~+/=-]{1,512}$")
TIMESTAMP = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")
LABEL = re.compile(r"^[A-Za-z0-9][A-Za-z0-9 ()@.,+/_-]{0,63}$")
OS_NAME = re.compile(r"^[a-z][a-z0-9]{1,15}$")
ARCH = re.compile(r"^[a-z][a-z0-9_]{1,15}$")


class ChallengeError(ValueError):
    """A malformed seed, word list or challenge object."""


def load_words(path: Path = WORDS_PATH) -> List[str]:
    """The 256 challenge words, checked against the published SHA-256."""
    words = path.read_text(encoding="ascii").splitlines()
    if len(words) != 256 or len(set(words)) != 256:
        raise ChallengeError("the word list must hold 256 distinct words")
    if any(not re.fullmatch(r"[a-z]+", w) for w in words):
        raise ChallengeError("every word must be lowercase ASCII letters")
    digest = hashlib.sha256("".join(w + "\n" for w in words).encode("ascii")).hexdigest()
    if digest != WORDS_SHA256:
        raise ChallengeError(f"word list SHA-256 is {digest}, expected {WORDS_SHA256}")
    return words


def parse_seed(seed_hex: str) -> bytes:
    """A seed is exactly 64 lowercase hex characters (32 bytes)."""
    if not isinstance(seed_hex, str) or not HEX64.fullmatch(seed_hex):
        raise ChallengeError("a challenge seed is 64 lowercase hex characters")
    return bytes.fromhex(seed_hex)


def word_indices(seed_hex: str) -> List[int]:
    digest = hashlib.sha256(CHALLENGE_DOMAIN + parse_seed(seed_hex)).digest()
    return [digest[0], digest[1], digest[2], digest[3]]


def challenge_prompt(seed_hex: str, words: Optional[Sequence[str]] = None) -> str:
    """docs/proof-kit.md: the user message of the challenge prompt."""
    vocabulary = list(words) if words is not None else load_words()
    picks = [vocabulary[i] for i in word_indices(seed_hex)]
    return CHALLENGE_TEMPLATE.format(*picks)


def challenge_case(seed_hex: str) -> Dict[str, Any]:
    """The challenge as an arc.modern-cases.v1 case (same rendering as the golden prompts)."""
    return {"id": "challenge", "user": challenge_prompt(seed_hex), "today": CHALLENGE_TODAY,
            "thinking": False, "max_tokens": CHALLENGE_MAX_TOKENS, "eos": list(CHALLENGE_EOS),
            "selection": CHALLENGE_SELECTION}


def sha256_hex(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


# --- digests (spec of the golden digest: arc-modern golden) ----------------

def canonical_json(obj: Any) -> bytes:
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode("ascii")


def have_blake3() -> bool:
    return _blake3_module is not None


def blake3_hex(data: bytes) -> str:
    if _blake3_module is None:
        raise RuntimeError("the 'blake3' package is required (pip install blake3)")
    return _blake3_module.blake3(data).hexdigest()


def tokens_hash(tokens: Sequence[int]) -> str:
    """output_hash: BLAKE3 of the generated token ids as little-endian u32."""
    return blake3_hex(b"".join(int(t).to_bytes(4, "little") for t in tokens))


def matrix_digest(entries: Sequence[Dict[str, Any]]) -> str:
    """Golden/challenge digest: BLAKE3 of the canonical JSON list of case entries."""
    return blake3_hex(canonical_json([{"id": e["id"], "logits_digest": e["logits_digest"],
                                       "output_hash": e["output_hash"],
                                       "tokens": list(e["tokens"])} for e in entries]))


# --- the strict result validator --------------------------------------------

def _is_int(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def _is_number(value: Any) -> bool:
    return (isinstance(value, (int, float)) and not isinstance(value, bool)
            and value == value and value not in (float("inf"), float("-inf")))


class _Checker:
    def __init__(self) -> None:
        self.errors: List[str] = []

    def fail(self, where: str, message: str) -> None:
        self.errors.append(f"{where}: {message}")

    def keys(self, where: str, value: Any, expected: Sequence[str]) -> bool:
        if not isinstance(value, dict):
            self.fail(where, "must be an object")
            return False
        missing = [k for k in expected if k not in value]
        extra = sorted(k for k in value if k not in expected)
        if missing:
            self.fail(where, f"missing fields {missing}")
        if extra:
            self.fail(where, f"unknown fields {extra}")
        return not missing and not extra

    def match(self, where: str, value: Any, pattern: "re.Pattern[str]",
              nullable: bool = False) -> None:
        if value is None and nullable:
            return
        if not isinstance(value, str) or not pattern.fullmatch(value):
            self.fail(where, "has the wrong format")

    def one_of(self, where: str, value: Any, allowed: Sequence[Any], nullable: bool = False) -> None:
        if value is None and nullable:
            return
        if isinstance(value, bool) or value not in allowed:
            self.fail(where, f"must be one of {list(allowed)}")

    def int_range(self, where: str, value: Any, low: int, high: int,
                  nullable: bool = False) -> None:
        if value is None and nullable:
            return
        if not _is_int(value) or not low <= value <= high:
            self.fail(where, f"must be an integer in [{low}, {high}]")


def _check_case(c: _Checker, where: str, case: Any, case_id: str, max_tokens: int) -> None:
    if not c.keys(where, case, ("id", "tokens", "output_hash", "logits_digest")):
        return
    if case["id"] != case_id:
        c.fail(f"{where}.id", f"must be {case_id!r}")
    tokens = case["tokens"]
    if (not isinstance(tokens, list) or not 1 <= len(tokens) <= max_tokens
            or any(not _is_int(t) or not 0 <= t < VOCAB_SIZE for t in tokens)):
        c.fail(f"{where}.tokens", f"must be 1..{max_tokens} token ids below {VOCAB_SIZE}")
    c.match(f"{where}.output_hash", case["output_hash"], HEX64)
    c.match(f"{where}.logits_digest", case["logits_digest"], HEX64)


def validate_result(payload: Any, raw_bytes: Optional[int] = None,
                    check_digests: bool = True) -> List[str]:
    """Every rule of docs/proof-kit.md "Validation rules"; returns the problems found.

    ``raw_bytes`` is the size of the submitted body. ``check_digests`` recomputes
    every BLAKE3 digest (needs the blake3 package).
    """
    c = _Checker()
    if raw_bytes is not None and raw_bytes > MAX_RESULT_BYTES:
        c.fail("body", f"{raw_bytes} bytes is over the {MAX_RESULT_BYTES}-byte limit")
    top = ("schema", "kit_version", "arc_version", "nonce", "model", "verdict", "golden",
           "challenge", "speed_method", "runs", "device", "island")
    if not c.keys("result", payload, top):
        return c.errors
    if payload["schema"] != RESULT_SCHEMA:
        c.fail("schema", f"must be {RESULT_SCHEMA!r}")
    c.match("kit_version", payload["kit_version"], VERSION)
    c.match("arc_version", payload["arc_version"], VERSION)
    c.match("nonce", payload["nonce"], NONCE)
    if c.keys("model", payload["model"], tuple(PINNED_MODEL)):
        for key, pinned in PINNED_MODEL.items():
            if payload["model"][key] != pinned:
                c.fail(f"model.{key}", f"must be the pinned value {pinned!r}")
    c.one_of("verdict", payload["verdict"], VERDICTS)
    if payload["speed_method"] != SPEED_METHOD:
        c.fail("speed_method", f"must be {SPEED_METHOD!r}")

    golden = payload["golden"]
    golden_ok = c.keys("golden", golden, ("published_digest", "digest", "cases"))
    if golden_ok:
        if golden["published_digest"] != PUBLISHED_GOLDEN_DIGEST:
            c.fail("golden.published_digest", "must be the published golden digest")
        c.match("golden.digest", golden["digest"], HEX64)
        cases = golden["cases"]
        if not isinstance(cases, list) or len(cases) != len(GOLDEN_CASES):
            c.fail("golden.cases", f"must list the {len(GOLDEN_CASES)} golden cases in order")
            golden_ok = False
        else:
            for index, (case_id, budget) in enumerate(GOLDEN_CASES):
                _check_case(c, f"golden.cases[{index}]", cases[index], case_id, budget)

    challenge = payload["challenge"]
    challenge_keys = ("challenge_id", "seed", "expires_at", "signature", "prompt_sha256",
                      "digest", "case")
    challenge_ok = c.keys("challenge", challenge, challenge_keys)
    if challenge_ok:
        c.match("challenge.challenge_id", challenge["challenge_id"], CHALLENGE_ID)
        c.match("challenge.seed", challenge["seed"], HEX64)
        c.match("challenge.expires_at", challenge["expires_at"], TIMESTAMP)
        c.match("challenge.signature", challenge["signature"], SIGNATURE)
        c.match("challenge.prompt_sha256", challenge["prompt_sha256"], HEX64)
        c.match("challenge.digest", challenge["digest"], HEX64)
        _check_case(c, "challenge.case", challenge["case"], "challenge", CHALLENGE_MAX_TOKENS)
        if isinstance(challenge["seed"], str) and HEX64.fullmatch(challenge["seed"]):
            if challenge["prompt_sha256"] != sha256_hex(challenge_prompt(challenge["seed"])):
                c.fail("challenge.prompt_sha256", "is not the SHA-256 of the prompt the seed derives")

    runs = payload["runs"]
    run_keys = ("backend", "isa", "verdict", "golden_digest", "challenge_digest",
                "prefill_tok_s", "decode_tok_s", "threads", "vector_projections", "adapter",
                "divergence")
    runs_ok = isinstance(runs, list) and 1 <= len(runs) <= 4
    if not runs_ok:
        c.fail("runs", "must list 1 to 4 runs")
    else:
        backends = [r.get("backend") if isinstance(r, dict) else None for r in runs]
        if len(set(backends)) != len(backends):
            c.fail("runs", "backends must be distinct")
        if not any(isinstance(b, str) and b.startswith("cpu-") for b in backends):
            c.fail("runs", "must include a CPU backend")
        for index, run in enumerate(runs):
            where = f"runs[{index}]"
            if not c.keys(where, run, run_keys):
                runs_ok = False
                continue
            c.one_of(f"{where}.backend", run["backend"], BACKENDS)
            c.one_of(f"{where}.isa", run["isa"], ISAS, nullable=True)
            if run["backend"] == "cpu-scalar" and run["isa"] is not None:
                c.fail(f"{where}.isa", "must be null for cpu-scalar")
            c.one_of(f"{where}.verdict", run["verdict"], VERDICTS)
            c.match(f"{where}.golden_digest", run["golden_digest"], HEX64)
            c.match(f"{where}.challenge_digest", run["challenge_digest"], HEX64)
            for key in ("prefill_tok_s", "decode_tok_s"):
                if not _is_number(run[key]) or not 0 <= run[key] <= 100000:
                    c.fail(f"{where}.{key}", "must be a number in [0, 100000]")
            c.int_range(f"{where}.threads", run["threads"], 1, 4096, nullable=True)
            vp = run["vector_projections"]
            if vp is not None and c.keys(f"{where}.vector_projections", vp,
                                         ("attempted", "accepted")):
                c.int_range(f"{where}.vector_projections.attempted", vp["attempted"], 0, 2**53)
                c.int_range(f"{where}.vector_projections.accepted", vp["accepted"], 0, 2**53)
                if _is_int(vp["attempted"]) and _is_int(vp["accepted"]) and \
                        vp["accepted"] > vp["attempted"]:
                    c.fail(f"{where}.vector_projections", "accepted exceeds attempted")
            adapter = run["adapter"]
            if run["backend"] in ("cpu-scalar", "cpu-simd"):
                if adapter is not None:
                    c.fail(f"{where}.adapter", "must be null for a CPU backend")
            elif adapter is not None and c.keys(f"{where}.adapter", adapter,
                                                ("vendor", "device", "backend", "driver")):
                c.match(f"{where}.adapter.vendor", adapter["vendor"], LABEL, nullable=True)
                c.match(f"{where}.adapter.device", adapter["device"], LABEL, nullable=True)
                c.one_of(f"{where}.adapter.backend", adapter["backend"], GPU_APIS,
                         nullable=True)
                c.match(f"{where}.adapter.driver", adapter["driver"], LABEL, nullable=True)
            div = run["divergence"]
            if run["verdict"] == "MATCH" and div is not None:
                c.fail(f"{where}.divergence", "must be null when the run matches")
            if run["verdict"] == "MISMATCH" and div is None:
                c.fail(f"{where}.divergence", "must be an object when the run does not match")
            if div is not None and c.keys(f"{where}.divergence", div,
                                          ("case", "position", "layer", "op")):
                c.one_of(f"{where}.divergence.case", div["case"],
                         [cid for cid, _ in GOLDEN_CASES] + ["challenge"], nullable=True)
                c.int_range(f"{where}.divergence.position", div["position"], 0, 65535,
                            nullable=True)
                c.int_range(f"{where}.divergence.layer", div["layer"], 0, 1023, nullable=True)
                c.one_of(f"{where}.divergence.op", div["op"], OPS, nullable=True)

    device = payload["device"]
    if c.keys("device", device, ("os", "os_version", "arch", "cpu_model", "logical_cpus",
                                 "cpu_features", "gpu_model")):
        c.match("device.os", device["os"], OS_NAME)
        c.match("device.os_version", device["os_version"], LABEL, nullable=True)
        c.match("device.arch", device["arch"], ARCH)
        c.match("device.cpu_model", device["cpu_model"], LABEL, nullable=True)
        c.int_range("device.logical_cpus", device["logical_cpus"], 1, 4096)
        features = device["cpu_features"]
        if (not isinstance(features, list) or len(set(map(str, features))) != len(features)
                or any(f not in CPU_FEATURES for f in features)):
            c.fail("device.cpu_features", f"must be distinct values from {list(CPU_FEATURES)}")
        c.match("device.gpu_model", device["gpu_model"], LABEL, nullable=True)

    island = payload["island"]
    if island is not None and c.keys("island", island, (
            "memory_class_gb", "unified_memory", "gpu_vram_class_gb", "thunderbolt5",
            "download_mbps_class")):
        c.one_of("island.memory_class_gb", island["memory_class_gb"], MEMORY_CLASSES_GB,
                 nullable=True)
        if not isinstance(island["unified_memory"], bool):
            c.fail("island.unified_memory", "must be true or false")
        c.one_of("island.gpu_vram_class_gb", island["gpu_vram_class_gb"], VRAM_CLASSES_GB,
                 nullable=True)
        if island["thunderbolt5"] is not None and not isinstance(island["thunderbolt5"], bool):
            c.fail("island.thunderbolt5", "must be true, false or null")
        c.one_of("island.download_mbps_class", island["download_mbps_class"],
                 NETWORK_CLASSES_MBPS, nullable=True)

    # Verdicts follow from the digests alone.
    if runs_ok and golden_ok and challenge_ok and not c.errors:
        all_match = True
        for index, run in enumerate(runs):
            expected = ("MATCH" if run["golden_digest"] == PUBLISHED_GOLDEN_DIGEST
                        and run["challenge_digest"] == challenge["digest"] else "MISMATCH")
            if run["verdict"] != expected:
                c.fail(f"runs[{index}].verdict", f"must be {expected} for these digests")
            all_match = all_match and expected == "MATCH"
        if payload["verdict"] != ("MATCH" if all_match else "MISMATCH"):
            c.fail("verdict", "must be MATCH exactly when every run matches")
        if runs[0]["golden_digest"] != golden["digest"]:
            c.fail("runs[0].golden_digest", "must equal golden.digest (the cases come from runs[0])")
        if runs[0]["challenge_digest"] != challenge["digest"]:
            c.fail("runs[0].challenge_digest", "must equal challenge.digest")
        if check_digests and have_blake3():
            for index, case in enumerate(golden["cases"]):
                if case["output_hash"] != tokens_hash(case["tokens"]):
                    c.fail(f"golden.cases[{index}].output_hash", "is not the hash of its tokens")
            if golden["digest"] != matrix_digest(golden["cases"]):
                c.fail("golden.digest", "is not the digest of golden.cases")
            if challenge["case"]["output_hash"] != tokens_hash(challenge["case"]["tokens"]):
                c.fail("challenge.case.output_hash", "is not the hash of its tokens")
            if challenge["digest"] != matrix_digest([challenge["case"]]):
                c.fail("challenge.digest", "is not the digest of challenge.case")
    return c.errors


# --- command line ------------------------------------------------------------

def vector_seeds(count: int) -> List[str]:
    """Deterministic seeds for the published vectors (plus the edge cases)."""
    seeds = [TEST_CHALLENGE["seed"], "00" * 32, "ff" * 32]
    for n in range(count):
        seeds.append(hashlib.sha256(f"arc.proof-challenge vector {n}".encode("ascii")).hexdigest())
    return seeds


def make_vectors(count: int) -> Dict[str, Any]:
    words = load_words()
    vectors = []
    for seed in vector_seeds(count):
        prompt = challenge_prompt(seed, words)
        vectors.append({"seed": seed, "word_indices": word_indices(seed), "prompt": prompt,
                        "prompt_sha256": sha256_hex(prompt)})
    return {"schema": "arc.proof-challenge-vectors.v1", "scheme": CHALLENGE_SCHEME,
            "words_sha256": WORDS_SHA256, "template": CHALLENGE_TEMPLATE,
            "domain_hex": CHALLENGE_DOMAIN.hex(), "vectors": vectors}


def main(argv: Sequence[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    d = sub.add_parser("derive", help="print the challenge prompt a seed derives")
    d.add_argument("--seed", action="append", default=[])
    d.add_argument("--seeds-file", help="one seed per line")
    d.add_argument("--json", action="store_true", help="one JSON object per seed")
    v = sub.add_parser("vectors", help="print the challenge test vectors")
    v.add_argument("--count", type=int, default=8)
    c = sub.add_parser("validate", help="check one arc.proof-result.v1 file")
    c.add_argument("path")
    c.add_argument("--expect-verdict", choices=VERDICTS)
    args = parser.parse_args(argv)
    if args.command == "derive":
        words = load_words()
        seeds = list(args.seed)
        if args.seeds_file:
            seeds.extend(line.strip() for line in
                         Path(args.seeds_file).read_text(encoding="ascii").splitlines()
                         if line.strip())
        if not seeds:
            parser.error("derive needs --seed or --seeds-file")
        for seed in seeds:
            prompt = challenge_prompt(seed, words)
            if args.json:
                print(json.dumps({"seed": seed, "prompt": prompt,
                                  "prompt_sha256": sha256_hex(prompt)}))
            else:
                print(prompt)
        return 0
    if args.command == "vectors":
        print(json.dumps(make_vectors(args.count), indent=2))
        return 0
    raw = Path(args.path).read_bytes()
    try:
        payload = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        print(f"not JSON: {error}", file=sys.stderr)
        return 1
    errors = validate_result(payload, raw_bytes=len(raw))
    if not have_blake3():
        print("note: blake3 is not installed; digest consistency was not checked",
              file=sys.stderr)
    if args.expect_verdict and isinstance(payload, dict) and \
            payload.get("verdict") != args.expect_verdict:
        errors.append(f"verdict: expected {args.expect_verdict}, got {payload.get('verdict')}")
    for error in errors:
        print(error, file=sys.stderr)
    if errors:
        return 1
    print(f"valid {RESULT_SCHEMA}: verdict {payload['verdict']}, {len(raw)} bytes, "
          f"{len(payload['runs'])} run(s), challenge {payload['challenge']['challenge_id']}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
