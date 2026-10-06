#!/usr/bin/env python3
"""Check ARC's Rust tiktoken encoder and chat prompt against the references (spec 8).

    python3 scripts/arc_mla/tokenizer_check.py corpus --out CORPUS.jsonl INPUT.jsonl ...
    python3 scripts/arc_mla/tokenizer_check.py compare --model-dir DIR --corpus CORPUS.jsonl \
        --rust RUST.jsonl --out CHECK.json
    python3 scripts/arc_mla/tokenizer_check.py tokens --model-dir DIR --text FILE --max-tokens N --out TOKENS.json

The reference encoder is the `tiktoken` library built exactly as Moonlight's
pinned `tokenization_moonshot.py` builds it: the split pattern is read from
that file (not retyped here), the vocabulary from the pinned `tiktoken.model`,
the special-token names from the pinned `tokenizer_config.json` (ids
163584..163841, the wrapper's 256 + 2), and text is cut the way the wrapper's
`encode` cuts it before calling `tiktoken.Encoding.encode(..., allowed_special="all")`.
Chat rows are rendered with jinja2 from the pinned `chat_template`, as
transformers' `apply_chat_template` does (sandboxed environment, trim_blocks,
lstrip_blocks), and must equal the Rust rendering.
"""

from __future__ import annotations

import argparse
import ast
import base64
import json
import sys
from pathlib import Path

N_SPECIAL = 258  # tokenization_moonshot.py: num_reserved_special_tokens (256) + 2
MAX_ENCODE_CHARS = 400_000
MAX_RUN_CHARS = 25_000


def read_jsonl(path: Path) -> list:
    # Split on "\n" only: str.splitlines() also breaks at U+2028, U+0085 and
    # other separators that may appear raw inside JSON strings of the corpus.
    return [json.loads(line) for line in path.read_text(encoding="utf-8").split("\n") if line.strip()]


def wrapper_pattern(model_dir: Path) -> str:
    """The pat_str class attribute of the pinned tokenization_moonshot.py, evaluated from its AST."""
    source = (model_dir / "tokenization_moonshot.py").read_text(encoding="utf-8")
    if "self.num_reserved_special_tokens + 2" not in source:
        raise SystemExit("tokenization_moonshot.py no longer reserves 256 + 2 special ids")
    for node in ast.walk(ast.parse(source)):
        if isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id == "pat_str" for t in node.targets):
            call = node.value
            if (isinstance(call, ast.Call) and isinstance(call.func, ast.Attribute) and call.func.attr == "join"
                    and isinstance(call.func.value, ast.Constant)):
                return call.func.value.value.join(ast.literal_eval(call.args[0]))
    raise SystemExit("pat_str not found in tokenization_moonshot.py")


def load_ranks(path: Path) -> dict:
    """tiktoken's mergeable ranks: one `base64(token) rank` line per token.

    This is what tiktoken.load.load_tiktoken_bpe returns after reading the
    file; reading it here avoids that function's blobfile dependency for local
    paths (tiktoken 0.9).
    """
    ranks = {}
    for line in path.read_bytes().splitlines():
        if line.strip():
            token, rank = line.split()
            ranks[base64.b64decode(token)] = int(rank)
    return ranks


def reference_encoder(model_dir: Path):
    import tiktoken

    ranks = load_ranks(model_dir / "tiktoken.model")
    config = json.loads((model_dir / "tokenizer_config.json").read_text(encoding="utf-8"))
    names = {int(k): v["content"] for k, v in config.get("added_tokens_decoder", {}).items()}
    n_base = len(ranks)
    special = {names.get(i, f"<|reserved_token_{i}|>"): i for i in range(n_base, n_base + N_SPECIAL)}
    return tiktoken.Encoding(name="moonlight-16b-a3b", pat_str=wrapper_pattern(model_dir),
                             mergeable_ranks=ranks, special_tokens=special), config


def split_runs(s: str, limit: int = MAX_RUN_CHARS):
    """The wrapper's _split_whitespaces_or_nonwhitespaces."""
    current_len = 0
    current_is_space = s[0].isspace() if len(s) > 0 else False
    start = 0
    for i in range(len(s)):
        is_space = s[i].isspace()
        if current_is_space ^ is_space:
            current_len = 1
            current_is_space = is_space
        else:
            current_len += 1
            if current_len > limit:
                yield s[start:i]
                start = i
                current_len = 1
    yield s[start:]


def reference_encode(encoder, text: str) -> list:
    ids = []
    for i in range(0, len(text), MAX_ENCODE_CHARS):
        for piece in split_runs(text[i:i + MAX_ENCODE_CHARS]):
            ids.extend(encoder.encode(piece, allowed_special="all"))
    return ids


def reference_render(config: dict, chat: dict) -> str:
    from jinja2.sandbox import ImmutableSandboxedEnvironment

    env = ImmutableSandboxedEnvironment(trim_blocks=True, lstrip_blocks=True)
    messages = []
    if chat.get("system") is not None:
        messages.append({"role": "system", "content": chat["system"]})
    messages.append({"role": "user", "content": chat.get("user", "")})
    return env.from_string(config["chat_template"]).render(messages=messages, add_generation_prompt=True)


def cmd_corpus(args: argparse.Namespace) -> int:
    rows = []
    for path in args.inputs:
        for row in read_jsonl(Path(path)):
            row = dict(row)
            row["id"] = f"{Path(path).stem}:{row['id']}"
            if "chat" in row:
                row["chat"] = {k: v for k, v in row["chat"].items() if k in ("system", "user")}
            rows.append(row)
    rows.append({"id": "generated:run-25010-a", "text": "a" * 25_010 + " b"})
    rows.append({"id": "generated:run-26000-space", "text": "x" + " " * 26_000 + "y"})
    Path(args.out).write_text("".join(json.dumps(r, ensure_ascii=True) + "\n" for r in rows), encoding="utf-8")
    print(f"{len(rows)} corpus rows")
    return 0


def cmd_compare(args: argparse.Namespace) -> int:
    encoder, config = reference_encoder(Path(args.model_dir))
    corpus = {str(r["id"]): r for r in read_jsonl(Path(args.corpus))}
    rust = {str(r["id"]): r for r in read_jsonl(Path(args.rust))}
    results, failures = [], 0
    for rid, row in corpus.items():
        got = rust.get(rid)
        if "chat" in row:
            text = reference_render(config, row["chat"])
        else:
            text = row["text"]
        ref_ids = reference_encode(encoder, text)
        entry = {"id": rid, "kind": "chat" if "chat" in row else "text", "tokens": len(ref_ids),
                 "ids_equal": got is not None and got["ids"] == ref_ids,
                 "text_equal": got is not None and got.get("text") == text}
        entry["ok"] = entry["ids_equal"] and entry["text_equal"]
        if not entry["ok"]:
            failures += 1
            entry["reference_ids"] = ref_ids[:200]
            entry["rust_ids"] = None if got is None else got["ids"][:200]
            entry["reference_text"] = text[:500]
            entry["rust_text"] = None if got is None else got.get("text", "")[:500]
        results.append(entry)
    report = {"schema": "arc.mla-tokenizer-check.v1", "reference": "tiktoken library + pinned wrapper rules",
              "rows": len(results), "failures": failures,
              "chat_rows": sum(1 for r in corpus.values() if "chat" in r), "results": results}
    Path(args.out).write_text(json.dumps(report, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"tokenizer check: {len(results) - failures}/{len(results)} rows identical")
    for entry in results:
        if not entry["ok"]:
            print(json.dumps(entry, ensure_ascii=False)[:2000])
    return 1 if failures else 0


def cmd_tokens(args: argparse.Namespace) -> int:
    encoder, _ = reference_encoder(Path(args.model_dir))
    text = Path(args.text).read_text(encoding="utf-8")
    ids = reference_encode(encoder, text)[:args.max_tokens]
    Path(args.out).write_text(json.dumps({"source": args.text, "tokens": ids}) + "\n", encoding="utf-8")
    print(f"{len(ids)} tokens")
    return 0


def main(argv: list) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("corpus")
    p.add_argument("--out", required=True)
    p.add_argument("inputs", nargs="+")
    p.set_defaults(func=cmd_corpus)
    p = sub.add_parser("compare")
    p.add_argument("--model-dir", required=True)
    p.add_argument("--corpus", required=True)
    p.add_argument("--rust", required=True)
    p.add_argument("--out", required=True)
    p.set_defaults(func=cmd_compare)
    p = sub.add_parser("tokens")
    p.add_argument("--model-dir", required=True)
    p.add_argument("--text", required=True)
    p.add_argument("--max-tokens", type=int, default=1024)
    p.add_argument("--out", required=True)
    p.set_defaults(func=cmd_tokens)
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
