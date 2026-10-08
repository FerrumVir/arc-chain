"""Run: PYTHONPATH=scripts python3 -m arc_autocluster.simulate --output report.json

Offline synthetic scenarios based on research-7 section 4's UNVERIFIED priors.
Nothing here discovers machines, contacts nodes, or measures Kimi inference.
JSON contains every swarm, its stage allocation, link RTTs and projections.
"""

import argparse
from collections import Counter, defaultdict
from dataclasses import asdict
import json
from pathlib import Path
import random

from .planner import Consent, Device, Link, Model, Policy, form, project


# Research-7 §4.1: percentages total 100. The two metros per region below are
# an explicit simplification, NOT the research's 47-city distribution.
REGIONS = (
    ("US-East", 16, "NA", ("NYC", "Boston")),
    ("US-Central", 9, "NA", ("Chicago", "Dallas")),
    ("US-West", 13, "NA", ("SF", "Seattle")),
    ("Canada", 4, "NA", ("Toronto", "Montreal")),
    ("Mexico", 1, "NA", ("Mexico City", "Monterrey")),
    ("UK", 8, "EU", ("London", "Manchester")),
    ("Netherlands", 3, "EU", ("Amsterdam", "Rotterdam")),
    ("France", 4, "EU", ("Paris", "Lyon")),
    ("Germany", 7, "EU", ("Frankfurt", "Berlin")),
    ("Poland", 2, "EU", ("Warsaw", "Krakow")),
    ("Nordics", 2, "EU", ("Stockholm", "Oslo")),
    ("Spain", 2, "EU", ("Madrid", "Barcelona")),
    ("Italy", 2, "EU", ("Milan", "Rome")),
    ("India", 5, "AS", ("Mumbai", "Delhi")),
    ("Japan", 3, "AS", ("Tokyo", "Osaka")),
    ("Korea", 2, "AS", ("Seoul", "Busan")),
    ("Singapore", 2, "AS", ("Singapore", "Singapore")),
    ("Indonesia", 1, "AS", ("Jakarta", "Surabaya")),
    ("Philippines", 1, "AS", ("Manila", "Cebu")),
    ("Australia", 3, "OC", ("Sydney", "Melbourne")),
    ("Brazil", 4, "SA", ("Sao Paulo", "Rio")),
    ("Argentina", 1, "SA", ("Buenos Aires", "Cordoba")),
    ("Colombia", 1, "SA", ("Bogota", "Medellin")),
    ("Nigeria", 2, "AF", ("Lagos", "Abuja")),
    ("Kenya", 1, "AF", ("Nairobi", "Mombasa")),
    ("South Africa", 1, "AF", ("Johannesburg", "Cape Town")),
)

# Explicit adjacency shortlist, not a claim that all continent pairs are close.
NEIGHBORS = (
    ("US-East", "US-Central"), ("US-East", "Canada"),
    ("US-Central", "US-West"), ("US-Central", "Mexico"),
    ("UK", "Netherlands"), ("Netherlands", "Germany"),
    ("France", "Germany"), ("France", "Spain"), ("France", "Italy"),
    ("Germany", "Poland"), ("Germany", "Nordics"),
    ("Japan", "Korea"), ("Singapore", "Indonesia"),
    ("Indonesia", "Philippines"), ("Brazil", "Argentina"),
)

# (class, research share %, chosen decimal GB, assumed reference speed).
# Midpoints/endpoints and speeds are new assumptions, not ENG-6 measurements.
HARDWARE = (
    ("gpu8", 10, 8, 1.0), ("gpu12", 15, 12, 1.2),
    ("gpu16", 15, 16, 1.5), ("gpu24", 20, 24, 2.0),
    ("gpu32", 8, 32, 2.5), ("mac24", 10, 24, .5),
    ("mac48", 12, 48, .7), ("unified128", 6, 128, 1.0),
    ("ultra384", 1, 384, 1.5), ("cpu32", 3, 32, .15),
)
NOW = 1000.0


def kimi_model():
    # 600 decimal GB is the ISSUE's sizing assumption, not a measured package.
    # 61 equal layers include experts; actual manifests must replace this proxy.
    sizes = tuple(600_000_000_000 // 61 + (i < 600_000_000_000 % 61) for i in range(61))
    return Model("synthetic-kimi-class-600GB-61-layer-proxy", sizes,
                 (2.0,) * 61, (576,) * 61, max_sequences=32)


def inventory(size, seed, availability):
    rng = random.Random(seed)
    geo_weights = [share * ((1.55 if size >= 10000 else 1.25) if continent == "AS"
                           and size >= 1000 else (1.6 if size >= 10000 else 1.2)
                           if continent == "AF" and size >= 1000 else 1)
                   for _, share, continent, _ in REGIONS]
    devices = []
    classes = Counter()
    lan_fraction = .08 if size >= 10000 else .05 if size >= 1000 else .03
    lan_nodes = int(size * lan_fraction) // 3 * 3
    site_region = None
    for i in range(size):
        if i >= lan_nodes or i % 3 == 0:
            site_region = rng.choices(REGIONS, weights=geo_weights)[0]
            metro = rng.choice(site_region[3])
        region = site_region[0]
        kind, _, gb, speed = rng.choices(HARDWARE, weights=[h[1] for h in HARDWARE])[0]
        classes[kind] += 1
        device_id = f"node-{i:05d}"
        owner = f"owner-{i // 3}" if i < lan_nodes else f"owner-single-{i}"
        opted = rng.random() < .9
        devices.append(Device(
            device_id, owner, metro, region, f"lan-{i // 3}" if i < lan_nodes else "",
            int(gb * 1e9 * .85), speed, NOW,
            Consent(owner, device_id, opted, opted, NOW + 3600),
            online=rng.random() < availability, evidence="synthetic"))
    return devices, dict(classes)


def assumed_link(a, b):
    if a.lan and a.lan == b.lan:
        return Link(.2, .3, 10000, NOW, evidence="synthetic")
    if a.metro == b.metro:
        rtt = 15
    elif a.region == b.region:
        rtt = 35
    elif (a.region, b.region) in NEIGHBORS or (b.region, a.region) in NEIGHBORS:
        rtt = 55
    else:
        return None
    return Link(rtt, rtt * 1.2, 50, NOW, evidence="synthetic")


def simulate(size, seed=7, availability=.7):
    devices, classes = inventory(size, seed, availability)
    model = kimi_model()
    policy = Policy(allow_synthetic=True)
    remaining = {d.id: d for d in devices if d.eligible(NOW, policy.ttl_seconds)}
    eligible = len(remaining)
    plans = []

    def consume(group, scope):
        pool = [d for d in group if d.id in remaining]
        while pool:
            plan = form(pool, model, assumed_link, NOW, policy, scope)
            if plan is None:
                break
            plans.append((plan, scope))
            used = {s.device.id for s in plan.stages} | {plan.spare.id}
            for device_id in used:
                del remaining[device_id]
            pool = [d for d in pool if d.id not in used]

    # Reserve genuine same-LAN big-memory opportunities before consuming their
    # members in regional pipelines; both tiers share a single disjoint pool.
    for scope in ("lan", "metro", "region"):
        groups = defaultdict(list)
        for d in remaining.values():
            key = getattr(d, scope)
            if key:
                groups[key].append(d)
        for key in sorted(groups):
            consume(groups[key], scope)
    for left, right in NEIGHBORS:
        consume([d for d in remaining.values() if d.region in (left, right)], "neighbor")

    rows = []
    for plan, scope in plans:
        baseline = project(plan, assumed_link)
        speculative = max((project(plan, assumed_link, draft_depth=k) for k in range(6)),
                          key=lambda p: p["single_answer_tok_s"])
        rows.append({"id": plan.id, "tier": plan.tier, "scope": scope,
                     "regions": sorted({s.device.region for s in plan.stages}),
                     "stages": [{"device": s.device.id, "layers": [s.start, s.end],
                                 "reserved_bytes": s.reserved_bytes,
                                 "usable_bytes": s.device.usable_bytes,
                                 "compute_ms": s.compute_ms} for s in plan.stages],
                     "spare": plan.spare.id, "spare_usable_bytes": plan.spare.usable_bytes,
                     "without_speculation": baseline, "with_speculation": speculative})
    aggregate = sum(r["without_speculation"]["aggregate_tok_s"] for r in rows)
    spec_aggregate = sum(r["with_speculation"]["aggregate_tok_s"] for r in rows)
    return {"nodes": size, "seed": seed, "availability": availability,
            "hardware_counts": classes, "eligible_nodes": eligible,
            "unallocated_eligible_nodes": len(remaining),
            "regional_swarms": sum(p.tier == "T2-batch" for p, _ in plans),
            "big_memory_islands": sum(p.tier != "T2-batch" for p, _ in plans),
            "aggregate_tok_s": aggregate, "speculative_aggregate_tok_s": spec_aggregate,
            "tokens_per_day": aggregate * 86400,
            "speculative_tokens_per_day": spec_aggregate * 86400, "swarms": rows}


def assumptions():
    return {
        "classification": "SYNTHETIC CAPACITY PROJECTIONS; no community inventory or ENG-6 measurements",
        "source": "User-supplied research-7 section 4.1 priors; research-6 sections 3 and 6",
        "seed": 7, "geography": REGIONS, "neighbor_pairs": NEIGHBORS,
        "hardware": HARDWARE, "usable_memory_fraction": .85,
        "consent_fraction": .9, "availability_scenarios": [.7, .5],
        "units": "decimal GB; milliseconds; Mbit/s",
        "model": asdict(kimi_model()),
        "links": "LAN 0.2ms p95/0.3ms p99, 10Gbit/s; metro 15ms; region 35ms; adjacent region 55ms; WAN 50Mbit/s",
        "speculation": "k=0..5; alpha=.7; expected committed=sum(alpha**i,i=0..k); verify cost=1+.25*k; draft cost=2ms/token; choose best single-answer rate",
        "batching": "min(32,2*stages) sequences, each reserves 4096 context tokens; no batch compute speedup",
        "spares": "one reserved device large enough for any stage; no simulated weights loaded; not evidence of a warm runtime",
        "limitations": [
            "Two equally weighted metros per area except Singapore; not the research's full 47-city model",
            "Geography priors normalized; Asia/Africa growth multipliers applied; no unspecified Latin America multiplier invented",
            "Greedy prefix packing and nearest-neighbor routing may miss feasible or more numerous swarms",
            "All experts of each layer colocated; no sublayer expert splitting or tensor parallel backend",
            "No actual Kimi package, probes, inference, golden digest or speculative decoder executed",
            "Daily total is a fully loaded steady-state ceiling at sampled availability, not delivered volume; no churn/repair, prefill, audit or idle-demand overhead",
            "Single-answer rate assumes one sequence; loaded-answer rate and aggregate include pipeline contention",
            "ENG-6 measurements must replace equal layer costs, hardware speeds, link assumptions and speculative acceptance before performance claims",
        ],
    }


def markdown(report):
    lines = ["# ARC-AC v0 synthetic Kimi capacity simulation", "",
             "All rates below are projections, not measured Kimi performance or community counts.", "",
             "| Nodes | Online assumption | Eligible | Regional swarms | Big-memory islands | Aggregate tok/s, plain / spec | Tokens/day, plain / spec |",
             "|---:|---:|---:|---:|---:|---:|---:|"]
    for s in report["scenarios"]:
        lines.append(f'| {s["nodes"]:,} | {s["availability"]:.0%} | {s["eligible_nodes"]} | '
                     f'{s["regional_swarms"]} | {s["big_memory_islands"]} | '
                     f'{s["aggregate_tok_s"]:.1f} / {s["speculative_aggregate_tok_s"]:.1f} | '
                     f'{s["tokens_per_day"]:,.0f} / {s["speculative_tokens_per_day"]:,.0f} |')
    lines += ["", "600 decimal GB in 61 equal layers; 10% weight headroom plus KV; 85% hardware memory usable; 90% consent assumption. Hardware and regional weights follow research-7 §4.1 with the simplifications recorded in the JSON. One capacity-qualified spare per swarm. Seed 7.",
              "", "Rates assume 2ms/reference layer, hardware speed factors 0.15–2.5, WAN 50Mbit/s and 15/35/55ms RTT. Batch depth=min(32,2×stages). Speculation includes verification and draft costs; acceptance=.7. These assumptions are not ENG-6 measurements.",
              "", "The JSON artifact contains every stage, each hop RTT, pass latency, batch depth, speculative depth, single-answer and loaded-answer tok/s. Daily totals assume continuous demand; repair, prefill and verification overhead are excluded.",
              "", "## Each swarm (all rates projected)", "",
              "| Nodes / online | Swarm | Scope | Stages / hops | Network ms | Pass ms plain / spec | Single-answer tok/s plain / spec | Loaded-answer tok/s plain / spec | Batch / draft |",
              "|---|---|---|---:|---:|---:|---:|---:|---:|"]
    for scenario in report["scenarios"]:
        for i, row in enumerate(scenario["swarms"]):
            b, s = row["without_speculation"], row["with_speculation"]
            lines.append(f'| {scenario["nodes"]}/{scenario["availability"]} | {i+1} | {row["scope"]} | '
                         f'{len(row["stages"])}/{b["hop_count"]} | {b["network_ms"]:.1f} | '
                         f'{b["pass_latency_ms"]:.1f}/{s["pass_latency_ms"]:.1f} | '
                         f'{b["single_answer_tok_s"]:.2f}/{s["single_answer_tok_s"]:.2f} | '
                         f'{b["loaded_answer_tok_s"]:.2f}/{s["loaded_answer_tok_s"]:.2f} | '
                         f'{b["batch_depth"]}/{s["draft_depth"]} |')
    lines += ["", "## Limits", ""] + [f"- {x}" for x in report["assumptions"]["limitations"]]
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--summary", type=Path)
    args = parser.parse_args()
    report = {"schema": "arc.autocluster-simulation.v1", "assumptions": assumptions(),
              "scenarios": [simulate(n, availability=a) for a in (.7, .5)
                            for n in (100, 130, 1000, 10000)]}
    args.output.write_text(json.dumps(report, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    if args.summary:
        args.summary.write_text(markdown(report), encoding="utf-8")
    print(json.dumps([{k: v for k, v in s.items() if k not in ("swarms", "hardware_counts")}
                      for s in report["scenarios"]], indent=2))


if __name__ == "__main__":
    main()
