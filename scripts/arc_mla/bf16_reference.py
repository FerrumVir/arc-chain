#!/usr/bin/env python3
"""Streaming float reference for DeepSeek-V3-architecture BF16 checkpoints.

    python3 scripts/arc_mla/bf16_reference.py ppl --model-dir DIR --tokens TOKENS.json \
        --rust INTEGER_PPL.json --window 512 --text-label LABEL --out QUALITY.json
    python3 scripts/arc_mla/bf16_reference.py check-hf --model-dir TINY_DIR --out CHECK.json

The perplexity baseline for the integer engine: the published BF16 weights,
upcast exactly to float32, run by a plain float32 forward pass. It streams one
layer at a time from the safetensors shards (and only the routed experts that
a window actually uses), so a 32 GB checkpoint is scored on a 16 GB CI runner;
transformers would need the whole model in memory.

`check-hf` validates this implementation against transformers'
DeepseekV3ForCausalLM (float32, eager attention) on a tiny checkpoint: the
logits must agree to 1e-3 and every argmax must match.

Semantics (DeepSeek-V3 / Moonlight / Kimi K2 text model, no YaRN):
RMSNorm without mean subtraction; queries direct or through the LoRA pair with
its norm; the KV latent normalised, then up-projected per head; RoPE on
adjacent pairs with w_i = theta^(-2i/R); softmax scale (N+R)^-1/2; sigmoid
router scores plus the correction bias for selection (group limit when
n_group > 1); weights = selected scores, normalised, times
routed_scaling_factor; routed experts plus shared experts; untied LM head.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
import time
from pathlib import Path

import torch


class Weights:
    """Tensors of every *.safetensors shard in a directory, loaded on demand as float32."""

    def __init__(self, model_dir: Path):
        from safetensors import safe_open

        self.where = {}
        for path in sorted(Path(model_dir).glob("*.safetensors")):
            with safe_open(str(path), framework="pt") as f:
                for name in f.keys():
                    self.where[name] = path

    def get(self, name: str) -> torch.Tensor:
        from safetensors import safe_open

        with safe_open(str(self.where[name]), framework="pt") as f:
            return f.get_tensor(name).to(torch.float32)


def rmsnorm(x: torch.Tensor, w: torch.Tensor, eps: float) -> torch.Tensor:
    return w * (x * torch.rsqrt(x.pow(2).mean(-1, keepdim=True) + eps))


def rope(x: torch.Tensor, cos: torch.Tensor, sin: torch.Tensor) -> torch.Tensor:
    """Rotate adjacent pairs (x[..., 2i], x[..., 2i+1]); cos/sin are [T, R/2], broadcast over heads."""
    while cos.dim() < x.dim():
        cos, sin = cos.unsqueeze(-2), sin.unsqueeze(-2)
    a, b = x[..., 0::2], x[..., 1::2]
    out = torch.empty_like(x)
    out[..., 0::2] = a * cos - b * sin
    out[..., 1::2] = a * sin + b * cos
    return out


def mlp(w: Weights, prefix: str, x: torch.Tensor) -> torch.Tensor:
    gate = x @ w.get(prefix + ".gate_proj.weight").T
    up = x @ w.get(prefix + ".up_proj.weight").T
    return (torch.nn.functional.silu(gate) * up) @ w.get(prefix + ".down_proj.weight").T


def forward(w: Weights, cfg: dict, tokens: list) -> torch.Tensor:
    """Logits [T, V] (float32) for one sequence."""
    t = len(tokens)
    h_heads, nope, rd, vh = cfg["num_attention_heads"], cfg["qk_nope_head_dim"], cfg["qk_rope_head_dim"], cfg["v_head_dim"]
    rank, eps = cfg["kv_lora_rank"], float(cfg["rms_norm_eps"])
    e, k = cfg["n_routed_experts"], cfg["num_experts_per_tok"]
    ids = torch.tensor(tokens, dtype=torch.long)
    x = w.get("model.embed_tokens.weight")[ids]
    pos = torch.arange(t, dtype=torch.float64)
    inv = 1.0 / (float(cfg["rope_theta"]) ** (torch.arange(0, rd, 2, dtype=torch.float64) / rd))
    ang = pos[:, None] * inv[None, :]
    cos, sin = torch.cos(ang).to(torch.float32), torch.sin(ang).to(torch.float32)
    scale = (nope + rd) ** -0.5
    causal = torch.ones(t, t, dtype=torch.bool).tril()
    for layer in range(cfg["num_hidden_layers"]):
        p = f"model.layers.{layer}"
        hn = rmsnorm(x, w.get(p + ".input_layernorm.weight"), eps)
        if cfg.get("q_lora_rank") is None:
            q = hn @ w.get(p + ".self_attn.q_proj.weight").T
        else:
            qa = rmsnorm(hn @ w.get(p + ".self_attn.q_a_proj.weight").T, w.get(p + ".self_attn.q_a_layernorm.weight"), eps)
            q = qa @ w.get(p + ".self_attn.q_b_proj.weight").T
        q = q.view(t, h_heads, nope + rd)
        q_nope, q_pe = q[..., :nope], rope(q[..., nope:], cos, sin)
        kv = hn @ w.get(p + ".self_attn.kv_a_proj_with_mqa.weight").T
        c = rmsnorm(kv[:, :rank], w.get(p + ".self_attn.kv_a_layernorm.weight"), eps)
        k_pe = rope(kv[:, rank:], cos, sin)
        kvb = (c @ w.get(p + ".self_attn.kv_b_proj.weight").T).view(t, h_heads, nope + vh)
        k_nope, v = kvb[..., :nope], kvb[..., nope:]
        scores = (torch.einsum("thd,shd->hts", q_nope, k_nope) + torch.einsum("thr,sr->hts", q_pe, k_pe)) * scale
        scores = scores.masked_fill(~causal, float("-inf"))
        probs = torch.softmax(scores, dim=-1, dtype=torch.float32)
        o = torch.einsum("hts,shd->thd", probs, v).reshape(t, h_heads * vh)
        x = x + o @ w.get(p + ".self_attn.o_proj.weight").T
        hn = rmsnorm(x, w.get(p + ".post_attention_layernorm.weight"), eps)
        if layer < cfg["first_k_dense_replace"]:
            x = x + mlp(w, p + ".mlp", hn)
            continue
        logits = hn @ w.get(p + ".mlp.gate.weight").T
        scores_e = torch.sigmoid(logits)
        choice = scores_e + w.get(p + ".mlp.gate.e_score_correction_bias")[None, :]
        if cfg["n_group"] > 1:
            groups = choice.view(t, cfg["n_group"], -1)
            group_scores = groups.topk(2, dim=-1).values.sum(dim=-1)
            keep = torch.zeros_like(group_scores, dtype=torch.bool)
            keep.scatter_(1, group_scores.topk(cfg["topk_group"], dim=-1).indices, True)
            choice = choice.masked_fill(~keep.repeat_interleave(e // cfg["n_group"], dim=1), float("-inf"))
        top = choice.topk(k, dim=-1).indices
        weights = scores_e.gather(1, top)
        if cfg["norm_topk_prob"] and k > 1:
            weights = weights / (weights.sum(dim=-1, keepdim=True) + 1e-20)
        weights = weights * float(cfg["routed_scaling_factor"])
        y = torch.zeros_like(hn)
        for expert in torch.unique(top).tolist():
            rows, slots = (top == expert).nonzero(as_tuple=True)
            y.index_add_(0, rows, weights[rows, slots, None] * mlp(w, f"{p}.mlp.experts.{expert}", hn[rows]))
        x = x + y + mlp(w, p + ".mlp.shared_experts", hn)
    x = rmsnorm(x, w.get("model.norm.weight"), eps)
    return x @ w.get("lm_head.weight").T


def cmd_ppl(args: argparse.Namespace) -> int:
    torch.set_num_threads(max(1, args.threads))
    cfg = json.loads((Path(args.model_dir) / "config.json").read_text())
    if cfg.get("rope_scaling") is not None:
        raise SystemExit("rope_scaling (YaRN) is not implemented by this reference")
    tokens = json.loads(Path(args.tokens).read_text())["tokens"]
    w = Weights(Path(args.model_dir))
    nll_sum, scored, argmax = 0.0, 0, []
    start = time.time()
    with torch.no_grad():
        for s in range(0, len(tokens), args.window):
            chunk = tokens[s:s + args.window]
            if len(chunk) < 2:
                continue
            logits = forward(w, cfg, chunk)[:-1]
            logp = torch.log_softmax(logits.to(torch.float64), dim=-1)
            target = torch.tensor(chunk[1:], dtype=torch.long)
            nll_sum += float(-logp.gather(1, target[:, None]).sum())
            scored += len(chunk) - 1
            argmax.extend(int(i) for i in logits.argmax(dim=-1))
    seconds = time.time() - start
    reference = {"implementation": "streaming float32 forward from the BF16 weights (scripts/arc_mla/bf16_reference.py; "
                                   "checked against transformers DeepseekV3ForCausalLM by check-hf)",
                 "torch": torch.__version__, "scored_tokens": scored, "nll_sum": nll_sum,
                 "ppl": math.exp(nll_sum / scored), "seconds": seconds}
    rust = json.loads(Path(args.rust).read_text())
    if rust["scored_tokens"] != scored or rust["window"] != args.window:
        print(f"scored tokens differ: integer {rust['scored_tokens']} vs reference {scored}")
        return 1
    agree = sum(1 for a, b in zip(argmax, rust["argmax"]) if a == b)
    report = {"schema": "arc.mla-quality.v1", "text": args.text_label, "window": args.window,
              "scored_tokens": scored, "bf16_reference": reference,
              "integer_engine": {"profile": rust["profile"], "kernel": rust["kernel"], "ppl": rust["ppl"],
                                 "nll_sum": rust["nll_sum"], "logits_digest": rust["logits_digest"],
                                 "tok_s": rust["tok_s"], "seconds": rust["seconds"]},
              "ppl_delta_percent": 100.0 * (rust["ppl"] / reference["ppl"] - 1.0),
              "top1_agreement": agree / scored}
    Path(args.out).write_text(json.dumps(report, indent=1) + "\n")
    print(json.dumps({k: report[k] for k in ("text", "scored_tokens", "ppl_delta_percent", "top1_agreement")}, indent=1))
    print(f"BF16 reference ppl {reference['ppl']:.4f}; integer ppl {rust['ppl']:.4f}")
    return 0


def cmd_check_hf(args: argparse.Namespace) -> int:
    from transformers import DeepseekV3Config, DeepseekV3ForCausalLM

    torch.manual_seed(0)
    model_dir = Path(args.model_dir)
    cfg = json.loads((model_dir / "config.json").read_text())
    hf_cfg = {k: v for k, v in cfg.items() if k not in ("model_type", "architectures", "auto_map")}
    config = DeepseekV3Config(**hf_cfg)
    config.rope_interleave = True
    config._attn_implementation = "eager"
    model = DeepseekV3ForCausalLM(config).to(torch.float32).eval()
    w = Weights(model_dir)
    state = {name: w.get(name) for name in w.where}
    missing, unexpected = model.load_state_dict(state, strict=False)
    missing = [m for m in missing if not m.endswith("rotary_emb.inv_freq")]
    unexpected = [u for u in unexpected if not u.endswith("rotary_emb.inv_freq")]
    if missing or unexpected:
        print(json.dumps({"missing": missing, "unexpected": unexpected}, indent=1))
        raise SystemExit("transformers' DeepseekV3 parameter names differ from the checkpoint")
    sequences = [[1, 99, 123, 7, 64, 299, 17, 200, 17, 5, 11, 42], [256, 256, 256, 2], [5]]
    rows = []
    worst = 0.0
    with torch.no_grad():
        for seq in sequences:
            hf = model(torch.tensor([seq], dtype=torch.long)).logits[0].to(torch.float32)
            mine = forward(w, cfg, seq)
            diff = float((hf - mine).abs().max())
            worst = max(worst, diff)
            rows.append({"tokens": seq, "max_abs_diff": diff,
                         "argmax_equal": bool(torch.equal(hf.argmax(-1), mine.argmax(-1)))})
    ok = worst < 1e-3 and all(r["argmax_equal"] for r in rows)
    import transformers
    report = {"schema": "arc.mla-float-reference-check.v1", "model_dir": str(model_dir),
              "transformers": transformers.__version__, "torch": torch.__version__,
              "max_abs_logit_diff": worst, "sequences": rows, "ok": ok}
    Path(args.out).write_text(json.dumps(report, indent=1) + "\n")
    print(json.dumps(report, indent=1))
    return 0 if ok else 1


def main(argv: list) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("ppl")
    p.add_argument("--model-dir", required=True)
    p.add_argument("--tokens", required=True)
    p.add_argument("--rust", required=True)
    p.add_argument("--window", type=int, default=512)
    p.add_argument("--threads", type=int, default=4)
    p.add_argument("--text-label", default="")
    p.add_argument("--out", required=True)
    p.set_defaults(func=cmd_ppl)
    p = sub.add_parser("check-hf")
    p.add_argument("--model-dir", required=True)
    p.add_argument("--out", required=True)
    p.set_defaults(func=cmd_check_hf)
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
