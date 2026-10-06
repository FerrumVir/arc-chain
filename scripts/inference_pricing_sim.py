#!/usr/bin/env python3
"""Capacity-aware inference pricing v0: base-fee simulation.

Reproduces the tables in docs/inference-pricing.md. Standard library only,
seeded, deterministic, and it runs in well under a second:

    python3 scripts/inference_pricing_sim.py

The base-fee controller below is the same integer rule as
`next_base_fee` in crates/arc-node/src/inference_pricing.rs. The
"parity vector" section prints a fixed utilization sequence and the fees
it produces; the Rust unit test `base_fee_matches_python_parity_vector`
asserts the same numbers.

Every price is USD-equivalent. v0 has no ARC/USD oracle and testnet ARC has
no monetary value, so this script never prints an ARC price. A USD column
is what an operator would have to set the floor to, in whatever unit, for
an honest worker to recover its electricity. It is not a forecast of what
anyone will earn.
"""

from __future__ import annotations

import math
import random
from dataclasses import dataclass

BPS = 10_000

# ---------------------------------------------------------------------------
# The controller (must match crates/arc-node/src/inference_pricing.rs).
# ---------------------------------------------------------------------------


def next_base_fee(current: int, utilization_bps: int | None, *, floor: int,
                  ceiling: int, target_bps: int, max_step_bps: int) -> int:
    """One epoch of the EIP-1559-style update, in integer arithmetic.

    None means "no verified capacity was observed", which holds the fee.
    The step is proportional to (utilization - target) / target, capped at
    max_step_bps of the current fee in either direction. An increase is at
    least one unit, so the fee can always leave a tiny value under
    congestion. The result is clamped to [floor, ceiling].
    """
    current = min(max(current, floor), ceiling)
    if utilization_bps is None:
        return current
    u = min(utilization_bps, 2 * target_bps)
    if u > target_bps:
        delta = current * max_step_bps * (u - target_bps) // (target_bps * BPS)
        nxt = current + max(delta, 1)
    elif u < target_bps:
        delta = current * max_step_bps * (target_bps - u) // (target_bps * BPS)
        nxt = current - delta
    else:
        nxt = current
    return min(max(nxt, floor), ceiling)


# ---------------------------------------------------------------------------
# Inputs. Each has its source; [ASSUMPTION] marks a number nobody measured.
# ---------------------------------------------------------------------------

US_RESIDENTIAL_USD_PER_KWH = 0.183  # EIA, Jul 2026 (18.31 c/kWh)

# API benchmarks, $ per 1M tokens (input, output), OpenRouter endpoints API,
# read 2026-10-04.
API = {
    "Llama-3.1-8B, DeepInfra (cheapest)": (0.02, 0.04),
    "Llama-3.1-8B, Groq": (0.05, 0.08),
    "Llama-3.1-8B, Together": (0.14, 0.14),
}
# Centralized cost floor: H100 rented at $2.20/h serving an 8B model at
# 15,000 tok/s (TensorRT-LLM FP8) = $0.041 per 1M tokens.
H100_RENTAL_FLOOR = 2.20 / (15_000 * 3600 / 1e6)


def api_blend(prices: tuple[float, float], tokens_in: int, tokens_out: int) -> float:
    """Blended $ per 1M processed tokens for a traffic mix."""
    p_in, p_out = prices
    return (p_in * tokens_in + p_out * tokens_out) / (tokens_in + tokens_out)


CHAT_MIX = (500, 500)  # processed tokens per job: 500 in, 500 out
PROMPT_HEAVY_MIX = (6_000, 400)
JOB_TOKENS = sum(CHAT_MIX)


@dataclass(frozen=True)
class Engine:
    name: str
    tokens_per_sec: float  # processed tokens per second per node
    watts: float  # wall power while computing
    basis: str

    def usd_per_mtok(self) -> float:
        joules_per_token = self.watts / self.tokens_per_sec
        kwh_per_mtok = joules_per_token * 1e6 / 3.6e6
        return kwh_per_mtok * US_RESIDENTIAL_USD_PER_KWH


ENGINES = {
    "cpu_today": Engine(
        "ARC INT8 CPU engine, single stream (today)", 1.4, 40.0,
        "1.4 tok/s measured: live receipt, community MacBook Pro, 716 ms/token; "
        "40 W [ASSUMPTION]",
    ),
    "cpu_simd": Engine(
        "ARC INT8 CPU engine with SIMD kernels [TARGET]", 7.2, 40.0,
        "7.2 tok/s: historical M2 Ultra CPU run (139 ms/token, README); "
        "40 W [ASSUMPTION]",
    ),
    "gpu_batched": Engine(
        "Batched GPU/Max-Mac fleet [ASSUMPTION: GPU determinism proven]",
        # 30% RTX 4090-class nodes at 3,000 tok/s and 550 W (batched vLLM
        # benchmarks), 70% M4 Max-class at 248 tok/s and 80 W (an estimate from
        # llama.cpp batched-bench ratios); capacity-weighted. Wall power is
        # assumed, not measured.
        0.3 * 3_000 + 0.7 * 248, 0.3 * 550 + 0.7 * 80,
        "30% RTX 4090 (3,000 tok/s, 550 W) + 70% M4 Max (248 tok/s, 80 W), batched",
    ),
}

# A smaller model of equal measured quality moves fewer weight bytes per
# token. Factor 0.4 ~ a modern 3B model in place of a 7B one [ASSUMPTION:
# quality parity must be measured on ARC's integer engine first].
CHEAPER_MODEL_FACTOR = 0.4


@dataclass(frozen=True)
class Verification:
    name: str
    worker_executions: int  # community executions paid per job
    extra_executions: float  # expected validator/auditor executions per job
    treasury_bps: int
    verifier_bps: int

    @property
    def worker_pool_bps(self) -> int:
        return BPS - self.treasury_bps - self.verifier_bps

    @property
    def per_execution_share(self) -> float:
        return self.worker_pool_bps / BPS / self.worker_executions


VERIFICATION = {
    # PR #139: two community workers per job; validators recompute a 5% spot
    # check (each recompute runs every range on 3 replicas = 3 executions).
    "twin": Verification("twin execution, 5% validator spot checks", 2,
                         0.05 * 3, 1_000, 700),
    # A deterministic replay is one prefill pass, estimated at 0.4-0.8 of the
    # original GPU time (0.6 used here [ASSUMPTION]), at audit rate p.
    "audit_p5": Verification("sampled audits, p = 5%", 1, 0.05 * 0.6, 1_000, 300),
    "audit_p1": Verification("sampled audits, p = 1%", 1, 0.01 * 0.6, 1_000, 100),
}


def floor_usd(engine_cost: float, ver: Verification, margin: float = 0.0) -> float:
    """Base-fee floor that pays each executing worker its electricity.

    Same rule as `PricingConfig::base_fee_floor`: worker floor x executions /
    worker-pool share.
    """
    return engine_cost * (1 + margin) * ver.worker_executions / (ver.worker_pool_bps / BPS)


# ---------------------------------------------------------------------------
# Demand model [ASSUMPTION throughout]: utilization of verified capacity at
# the floor price, with a daily cycle, an optional daily burst and noise.
# Demand responds to price with constant elasticity.
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Demand:
    mean_util_at_floor: float
    diurnal_amplitude: float
    burst_multiplier: float
    burst_hours: float
    noise: float
    elasticity: float = 0.5

    def util_at_floor(self, hour: float, rng: random.Random) -> float:
        diurnal = 1 + self.diurnal_amplitude * math.sin(2 * math.pi * (hour - 9) / 24)
        in_burst = 14 <= hour % 24 < 14 + self.burst_hours
        burst = self.burst_multiplier if in_burst else 1.0
        noise = 1 + rng.uniform(-self.noise, self.noise)
        return max(0.0, self.mean_util_at_floor * diurnal * burst * noise)


@dataclass
class RunResult:
    mean_fee_x: float  # load-weighted mean base fee / floor (what users pay)
    p95_fee_x: float
    max_fee_x: float
    share_above_target: float  # time with utilization above the target
    share_backlog: float  # time with demand above 100% of verified capacity
    mean_served_util: float


STEP_SECS = 600  # demand is evaluated every 10 minutes


def simulate(demand: Demand, *, days: int = 14, epoch_secs: int = 600,
             target_bps: int = 5_000, max_step_bps: int = 1_250,
             seed: int = 7) -> RunResult:
    """Run the controller over `days` of 10-minute demand steps.

    The fee changes once per epoch from the mean utilization of the steps
    in that epoch, as the coordinator does with its 15-second samples. The
    fee is held in micro-floor units (floor = 1,000,000) so the integer
    controller keeps full precision.
    """
    rng = random.Random(seed)
    floor = 1_000_000
    ceiling = 1_000 * floor
    fee = floor
    steps = days * 86_400 // STEP_SECS
    steps_per_epoch = max(1, epoch_secs // STEP_SECS)
    fees_paid: list[tuple[float, float]] = []  # (fee multiple, served load)
    fee_samples: list[float] = []
    above = backlog = 0
    served_total = 0.0
    epoch_util = 0.0
    epoch_steps = 0
    for k in range(steps):
        hour = (k + 0.5) * STEP_SECS / 3_600
        u_floor = demand.util_at_floor(hour, rng)
        fee_x = fee / floor
        util = u_floor * fee_x ** (-demand.elasticity)
        served = min(util, 1.0)
        fees_paid.append((fee_x, served))
        fee_samples.append(fee_x)
        served_total += served
        above += util * BPS > target_bps
        backlog += util > 1.0
        epoch_util += min(util, 2.0)
        epoch_steps += 1
        if epoch_steps == steps_per_epoch:
            u_bps = int(round(epoch_util / epoch_steps * BPS))
            fee = next_base_fee(fee, u_bps, floor=floor, ceiling=ceiling,
                                target_bps=target_bps, max_step_bps=max_step_bps)
            epoch_util = 0.0
            epoch_steps = 0
    load = sum(s for _, s in fees_paid) or 1.0
    ordered = sorted(fee_samples)
    return RunResult(
        mean_fee_x=sum(f * s for f, s in fees_paid) / load,
        p95_fee_x=ordered[int(0.95 * (len(ordered) - 1))],
        max_fee_x=ordered[-1],
        share_above_target=above / steps,
        share_backlog=backlog / steps,
        mean_served_util=served_total / steps,
    )


@dataclass(frozen=True)
class Stage:
    nodes: int
    label: str
    engine: str
    verification: str
    cheaper_model: bool
    demand: Demand


STAGES = [
    Stage(3, "today", "cpu_today", "twin", False,
          # A few demo users saturate 3 nodes: daily 2 h demo burst x4.
          Demand(0.30, 0.5, 4.0, 2.0, 0.20)),
    Stage(100, "community beta", "cpu_simd", "twin", False,
          Demand(0.30, 0.5, 2.0, 1.0, 0.10)),
    Stage(1_000, "GPU path proven", "gpu_batched", "audit_p5", False,
          Demand(0.30, 0.4, 1.0, 0.0, 0.05)),
    Stage(100_000, "at scale", "gpu_batched", "audit_p1", True,
          Demand(0.30, 0.3, 1.0, 0.0, 0.02)),
]


def fmt_usd(value: float) -> str:
    if value >= 1:
        return f"${value:,.2f}"
    if value >= 0.01:
        return f"${value:.3f}"
    if value >= 0.0001:
        return f"${value:.4f}"
    return f"${value:.2e}"


def fmt_ratio(value: float) -> str:
    return f"{value:,.0f}x" if value >= 10 else f"{value:.2f}x"


def stage_table() -> None:
    cheapest = api_blend(API["Llama-3.1-8B, DeepInfra (cheapest)"], *CHAT_MIX)
    print("## Simulation: price per 1M processed tokens vs node count\n")
    print("| Nodes | Stage | Engine | Verification | Electricity per execution, $/1M "
          "| Floor, $/1M | Base fee, load-weighted mean (p95) x floor "
          "| User price, $/1M | vs cheapest API ($0.030) "
          "| Each worker's income per 1,000-token job "
          "| Each worker's electricity per job | Time above target / in backlog |")
    print("|---:|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|")
    for st in STAGES:
        eng = ENGINES[st.engine]
        ver = VERIFICATION[st.verification]
        cost = eng.usd_per_mtok() * (CHEAPER_MODEL_FACTOR if st.cheaper_model else 1.0)
        floor = floor_usd(cost, ver)
        run = simulate(st.demand)
        price = floor * run.mean_fee_x
        income_per_job = price * JOB_TOKENS / 1e6 * ver.per_execution_share
        elec_per_job = cost * JOB_TOKENS / 1e6
        engine_name = eng.name + (" + cheaper model [ASSUMPTION]" if st.cheaper_model else "")
        print(f"| {st.nodes:,} | {st.label} | {engine_name} | {ver.name} "
              f"| {fmt_usd(cost)} | {fmt_usd(floor)} "
              f"| {run.mean_fee_x:.2f} ({run.p95_fee_x:.2f}) | {fmt_usd(price)} "
              f"| {fmt_ratio(price / cheapest)} | {fmt_usd(income_per_job)} "
              f"| {fmt_usd(elec_per_job)} "
              f"| {run.share_above_target:.0%} / {run.share_backlog:.0%} |")
    print()
    print("API benchmarks, blended per 1M processed tokens (chat mix 500 in / 500 out; "
          "prompt-heavy 6,000 / 400):\n")
    print("| Benchmark | Chat mix | Prompt-heavy mix |")
    print("|---|---:|---:|")
    for name, prices in API.items():
        print(f"| {name} | {fmt_usd(api_blend(prices, *CHAT_MIX))} "
              f"| {fmt_usd(api_blend(prices, *PROMPT_HEAVY_MIX))} |")
    print(f"| H100 rented at $2.20/h, 8B at 15,000 tok/s (cost, not price) "
          f"| {fmt_usd(H100_RENTAL_FLOOR)} | {fmt_usd(H100_RENTAL_FLOOR)} |")
    print()


def lever_table() -> None:
    cheapest = api_blend(API["Llama-3.1-8B, DeepInfra (cheapest)"], *CHAT_MIX)
    print("## Levers, one at a time (floor price, $/1M processed tokens)\n")
    print("| Step | Lever | Electricity per execution, $/1M | Paid executions per job "
          "| Worker share per execution | Floor, $/1M | Change | vs cheapest API |")
    print("|---:|---|---:|---:|---:|---:|---:|---:|")
    steps = [
        ("Today: CPU INT8 single stream, twin execution", "cpu_today", "twin", False),
        ("Faster CPU kernels (SIMD target)", "cpu_simd", "twin", False),
        ("Batched GPUs and Max-class Macs (needs GPU determinism proof)",
         "gpu_batched", "twin", False),
        ("Verification: twin to sampled audits, p = 5%", "gpu_batched", "audit_p5", False),
        ("Verification: p = 1%", "gpu_batched", "audit_p1", False),
        ("Cheaper model of equal quality", "gpu_batched", "audit_p1", True),
    ]
    previous = None
    for index, (label, engine, ver_key, cheaper) in enumerate(steps):
        ver = VERIFICATION[ver_key]
        cost = ENGINES[engine].usd_per_mtok() * (CHEAPER_MODEL_FACTOR if cheaper else 1.0)
        floor = floor_usd(cost, ver)
        change = "-" if previous is None else f"{floor / previous - 1:+.0%}"
        print(f"| {index} | {label} | {fmt_usd(cost)} | {ver.worker_executions} "
              f"| {ver.per_execution_share:.1%} | {fmt_usd(floor)} | {change} "
              f"| {fmt_ratio(floor / cheapest)} |")
        previous = floor
    print("| - | Capacity growth | - | - | - | unchanged | removes the congestion "
          "premium above the floor (next table) | - |")
    print()


def capacity_step_table() -> None:
    """Fixed demand; verified capacity doubles at epoch 0."""
    print("## Capacity growth: congestion premium falling back to the floor\n")
    print("Demand fixed at 80% of today's verified capacity at the floor price "
          "(elasticity 0.5), so the fee first settles where utilization is 50%. "
          "At epoch 0 verified capacity doubles.\n")
    floor = 1_000_000
    ceiling = 1_000 * floor
    elasticity = 0.5
    fee = floor
    for _ in range(400):  # settle at the congested equilibrium first
        util = 0.8 * (fee / floor) ** (-elasticity)
        fee = next_base_fee(fee, int(round(util * BPS)), floor=floor,
                            ceiling=ceiling, target_bps=5_000, max_step_bps=1_250)
    print("| Epoch | Verified capacity | Utilization | Base fee x floor |")
    print("|---:|---:|---:|---:|")
    print(f"| before | 1x | {0.8 * (fee / floor) ** (-elasticity):.0%} | {fee / floor:.2f} |")
    for epoch in range(0, 61):
        util = 0.4 * (fee / floor) ** (-elasticity)
        if epoch in (0, 6, 12, 24, 36, 48, 60):
            print(f"| {epoch} | 2x | {util:.0%} | {fee / floor:.2f} |")
        fee = next_base_fee(fee, int(round(util * BPS)), floor=floor,
                            ceiling=ceiling, target_bps=5_000, max_step_bps=1_250)
    print()


def sensitivity_table() -> None:
    print("## Sensitivity: epoch length and target utilization\n")
    print("| Demand | Epoch | Target | Base fee, load-weighted mean (p95, max) x floor "
          "| Time above target | Time in backlog |")
    print("|---|---|---:|---:|---:|---:|")
    epochs = ((600, "10 min"), (3_600, "1 h"), (49_800, "13.8 h (reward epoch)"))
    for stage in (STAGES[0], STAGES[1]):
        for epoch_secs, label in epochs:
            for target in (5_000, 7_000):
                run = simulate(stage.demand, epoch_secs=epoch_secs, target_bps=target)
                print(f"| {stage.nodes:,} nodes ({stage.label}) | {label} "
                      f"| {target / 100:.0f}% | {run.mean_fee_x:.2f} "
                      f"({run.p95_fee_x:.2f}, {run.max_fee_x:.2f}) "
                      f"| {run.share_above_target:.0%} | {run.share_backlog:.0%} |")
    print()


def parity_vector() -> None:
    print("## Parity vector (Rust test base_fee_matches_python_parity_vector)\n")
    # None = an epoch with no verified capacity (the fee holds). The run of
    # zeros at the end walks the fee down until the floor clamps it.
    sequence = [0, 2_500, 5_000, 7_500, 10_000, 20_000, 9_000, 4_999, 5_001, None,
                0, 0, 0, 0, 0, 0]
    fee = 1_000_000
    fees = []
    for util in sequence:
        fee = next_base_fee(fee, util, floor=600_000, ceiling=2_000_000,
                            target_bps=5_000, max_step_bps=1_250)
        fees.append(fee)
    print("floor 600000, ceiling 2000000, target 5000, step 1250, start 1000000")
    print("utilization_bps:", sequence)
    print("fees:", fees)
    print()


def main() -> None:
    stage_table()
    lever_table()
    capacity_step_table()
    sensitivity_table()
    parity_vector()


if __name__ == "__main__":
    main()
