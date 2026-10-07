"""Deterministic, capacity-constrained pipeline planning and lifecycle.

All layers include all their experts; no weights or arithmetic are changed.
Packing and route search are bounded heuristics, not a global fleet optimum.
Inside a selected route, dynamic programming minimizes the slowest stage.
"""

from dataclasses import dataclass, replace
from enum import Enum
import hashlib
import json
import math
from typing import Callable, Mapping, Optional


def positive(value, name):
    if isinstance(value, bool) or not math.isfinite(value) or value <= 0:
        raise ValueError(f"{name} must be positive and finite")


@dataclass(frozen=True)
class Consent:
    """Trusted owner-side input, NEVER derived from a Proof Kit submission.

    compute_enabled is the resolved #138 setting (including its legacy worker
    migration). Island participation additionally requires explicit opt-in.
    The embedding application authenticates this grant; this library does not.
    """

    owner: str
    device_id: str
    compute_enabled: bool = False
    island_enabled: bool = False
    expires_at: float = 0

    def permits(self, device, now):
        return (
            self.owner == device.owner and self.device_id == device.id
            and self.compute_enabled is True and self.island_enabled is True
            and math.isfinite(self.expires_at) and self.expires_at > now
        )


@dataclass(frozen=True)
class Device:
    id: str
    owner: str
    metro: str
    region: str
    lan: str
    usable_bytes: int
    layer_speed: float  # reference layer cost / measured device layer time
    measured_at: float
    consent: Consent
    online: bool = True
    leased: bool = False
    memory_class_gb: Optional[int] = None
    unified_memory: bool = False
    gpu_vram_class_gb: Optional[int] = None
    thunderbolt5: Optional[bool] = None
    download_mbps_class: Optional[int] = None
    evidence: str = "measured"

    def __post_init__(self):
        if not self.id or not self.owner:
            raise ValueError("device and owner IDs are required")
        positive(self.usable_bytes, "usable bytes")
        positive(self.layer_speed, "layer speed")
        if type(self.usable_bytes) is not int:
            raise ValueError("memory must be integral bytes")
        if not math.isfinite(self.measured_at):
            raise ValueError("invalid measurement time")

    def eligible(self, now, ttl):
        return (self.online and not self.leased and self.consent.permits(self, now)
                and 0 <= now - self.measured_at <= ttl)


def from_proof_result(result, **measurements):
    """Reuse PR #149's exact island field names, without summing RAM and VRAM.

    Caller supplies identity, consent, usable_bytes and model-specific speed.
    Proof Kit's SmolLM3 decode rate is NOT a Kimi performance measurement.
    """
    if result.get("schema") != "arc.proof-result.v1":
        raise ValueError("unsupported Proof Kit schema")
    facts = result.get("island") or {}
    fields = ("memory_class_gb", "unified_memory", "gpu_vram_class_gb",
              "thunderbolt5", "download_mbps_class")
    values = {name: facts[name] for name in fields if name in facts}
    for name in ("memory_class_gb", "gpu_vram_class_gb", "download_mbps_class"):
        if values.get(name) is not None:
            if type(values[name]) is not int:
                raise ValueError(f"invalid {name}")
            positive(values[name], name)
    for name in ("unified_memory", "thunderbolt5"):
        if name in values and values[name] is not None and type(values[name]) is not bool:
            raise ValueError(f"invalid {name}")
    return Device(**measurements, **values)


@dataclass(frozen=True)
class Link:
    p95_ms: float
    p99_ms: float
    mbps: float
    measured_at: float
    loss: float = 0
    evidence: str = "measured"

    def __post_init__(self):
        for value, name in ((self.p95_ms, "RTT"), (self.p99_ms, "RTT"),
                            (self.mbps, "bandwidth")):
            positive(value, name)
        if self.p99_ms < self.p95_ms or not 0 <= self.loss <= 1:
            raise ValueError("invalid RTT quantiles or loss")
        if not math.isfinite(self.measured_at):
            raise ValueError("invalid link measurement time")

    @classmethod
    def from_samples(cls, rtt_ms, mbps, measured_at, sent):
        """Offline probe reducer. Missing replies count as loss, not zero RTT."""
        values = sorted(rtt_ms)
        if not values or sent < len(values) or sent <= 0:
            raise ValueError("invalid probe sample count")
        for value in values:
            positive(value, "RTT sample")
        return cls(values[math.ceil(.95 * len(values)) - 1],
                   values[math.ceil(.99 * len(values)) - 1], mbps,
                   measured_at, 1 - len(values) / sent)


@dataclass(frozen=True)
class Model:
    identity: str  # caller's content-addressed model + quantization profile
    layer_bytes: tuple
    layer_ms: tuple  # reference cost at batch 1; measured or explicitly assumed
    kv_bytes_per_layer_token: tuple
    context: int = 4096
    max_sequences: int = 32
    headroom: float = .10
    activation_bytes: int = 14336

    def __post_init__(self):
        if not self.identity or not self.layer_bytes:
            raise ValueError("model identity and layers are required")
        if not (len(self.layer_bytes) == len(self.layer_ms)
                == len(self.kv_bytes_per_layer_token)):
            raise ValueError("layer vectors must have equal lengths")
        for value in self.layer_bytes + self.layer_ms + self.kv_bytes_per_layer_token:
            positive(value, "layer parameter")
        for value in self.layer_bytes + self.kv_bytes_per_layer_token:
            if type(value) is not int:
                raise ValueError("memory sizes must be integral")
        for value in (self.context, self.max_sequences, self.activation_bytes):
            if type(value) is not int or value <= 0:
                raise ValueError("context, sequence and activation budgets must be positive integers")
        if not math.isfinite(self.headroom) or not 0 <= self.headroom < 1:
            raise ValueError("invalid headroom")

    def reserved(self, start, end):
        return sum(math.ceil(w * (1 + self.headroom))
                   + kv * self.context * self.max_sequences
                   for w, kv in zip(self.layer_bytes[start:end],
                                    self.kv_bytes_per_layer_token[start:end]))


@dataclass(frozen=True)
class Stage:
    device: Device
    start: int
    end: int  # exclusive; all experts in these layers
    reserved_bytes: int
    compute_ms: float


@dataclass(frozen=True)
class Policy:
    max_stages: int = 30
    max_rtt_ms: float = 60
    ttl_seconds: float = 120
    max_loss: float = .01
    require_spare: bool = True
    allow_synthetic: bool = False

    def __post_init__(self):
        if type(self.max_stages) is not int or not 1 <= self.max_stages <= 30:
            raise ValueError("stage limit must be 1..30")
        positive(self.max_rtt_ms, "RTT limit")
        positive(self.ttl_seconds, "measurement TTL")
        if not 0 <= self.max_loss <= 1:
            raise ValueError("invalid loss limit")


def link_ok(link, policy, now, lan=False):
    return (link is not None and 0 <= now - link.measured_at <= policy.ttl_seconds
            and (link.evidence == "measured" or policy.allow_synthetic)
            and link.loss <= policy.max_loss and link.p95_ms <= policy.max_rtt_ms
            and (not lan or (link.p99_ms <= .5 and link.mbps >= 10000)))


def partition(devices, model, capacity_cap=None):
    """Exact minimax contiguous partition for a fixed route, with hard memory caps."""
    count = len(model.layer_bytes)
    if not devices or len(devices) > count:
        return None
    memory = [0]
    cost = [0.0]
    for i in range(count):
        memory.append(memory[-1] + model.reserved(i, i + 1))
        cost.append(cost[-1] + model.layer_ms[i])
    previous = {0: (0.0, ())}
    for d in devices:
        current = {}
        for end in range(1, count + 1):
            for start, (slowest, spans) in previous.items():
                if start >= end or memory[end] - memory[start] > min(d.usable_bytes, capacity_cap or d.usable_bytes):
                    continue
                time = (cost[end] - cost[start]) / d.layer_speed
                candidate = (max(slowest, time), spans + ((start, end),))
                if end not in current or candidate < current[end]:
                    current[end] = candidate
        previous = current
    if count not in previous:
        return None
    return tuple(Stage(d, start, end, memory[end] - memory[start],
                       (cost[end] - cost[start]) / d.layer_speed)
                 for d, (start, end) in zip(devices, previous[count][1]))


@dataclass(frozen=True)
class Plan:
    model: Model
    stages: tuple
    spare: Optional[Device]
    tier: str
    policy: Policy
    batch_depth: int
    parallelism: str = "contiguous-layer pipeline (all experts retained)"

    @property
    def id(self):
        value = (self.model.identity, self.model.layer_bytes, self.model.layer_ms,
                 self.model.kv_bytes_per_layer_token, self.model.context,
                 self.model.max_sequences, self.model.headroom,
                 [(s.device.id, s.start, s.end) for s in self.stages],
                 self.spare.id if self.spare else None, self.tier, self.batch_depth)
        return hashlib.sha256(json.dumps(value, separators=(",", ":")).encode()).hexdigest()


def route_edges(devices):
    """Activation chain plus last-to-first token feedback (one ring)."""
    if len(devices) <= 1:
        return ()
    return tuple(zip(devices, devices[1:] + devices[:1]))


def form(candidates, model, links, now, policy=Policy(), scope="region"):
    """Form within one explicit candidate cell; geo labels alone never admit.

    Largest memory first approximates minimum machine count; nearest-neighbor
    routing tries each selected device as the start. Failure returns None,
    never a partial model. Spare must fit ANY stage and link to EVERY member.
    `links(a,b)` must return conservative bidirectional measured link facts.
    """
    if scope not in ("lan", "owner", "metro", "region", "neighbor"):
        raise ValueError("unknown scope")
    ids = [d.id for d in candidates]
    if len(set(ids)) != len(ids):
        raise ValueError("duplicate device IDs")
    pool = sorted((d for d in candidates if d.eligible(now, policy.ttl_seconds)
                   and (d.evidence == "measured" or policy.allow_synthetic)),
                  key=lambda d: (-d.usable_bytes, -d.layer_speed, d.id))
    if scope != "neighbor":
        field = "owner" if scope == "owner" else scope
        labels = {getattr(d, field) for d in pool}
        if len(labels) != 1 or "" in labels:
            return None
    required = model.reserved(0, len(model.layer_bytes))
    layer_floor = min(model.reserved(i, i + 1) for i in range(len(model.layer_bytes)))
    # Try each size before expanding, with two deterministic spare placements:
    # largest spare or largest active prefix. This avoids forcing a small spare
    # to replace a giant stage and wasting the rest of the pool.
    for size in range(1, min(len(pool), policy.max_stages, len(model.layer_bytes)) + 1):
        variants = [(pool[:size], pool[size:])]
        if len(pool) > size:
            variants.append((pool[1:size + 1], [pool[0]] + pool[size + 1:]))
        if not policy.require_spare:
            variants.append((pool[:size], []))
        for selected, spare_pool in variants:
            spare = next((d for d in spare_pool
                          if all(link_ok(links(d, a), policy, now, scope == "lan")
                                 and link_ok(links(a, d), policy, now, scope == "lan")
                                 for a in selected)), None)
            if spare is None and policy.require_spare:
                continue
            cap = spare.usable_bytes if spare else None
            capacities = [min(d.usable_bytes, cap or d.usable_bytes) for d in selected]
            if (sum(capacities) < required
                    or sum(c // layer_floor for c in capacities) < len(model.layer_bytes)):
                continue
            routes = []
            for first in selected:
                route = [first]
                remaining = [d for d in selected if d.id != first.id]
                while remaining:
                    options = [(links(route[-1], d), d) for d in remaining]
                    options = [(edge.p95_ms, d.id, d) for edge, d in options
                               if link_ok(edge, policy, now, scope == "lan")]
                    if not options:
                        break
                    d = min(options, key=lambda x: x[:2])[2]
                    route.append(d)
                    remaining.remove(d)
                if remaining or any(not link_ok(links(a, b), policy, now, scope == "lan")
                                    for a, b in route_edges(route)):
                    continue
                routes.append((sum(links(a, b).p95_ms for a, b in route_edges(route)),
                               tuple(d.id for d in route), route))
            for _, _, route in sorted(routes, key=lambda x: x[:2]):
                stages = partition(route, model, cap)
                if stages is None:
                    continue
                tier = "T0" if size == 1 else ("T1b" if scope == "lan" else "T2-batch")
                # TB5 presence alone does not certify RDMA/collective performance.
                return Plan(model, stages, spare, tier, policy,
                            min(model.max_sequences, max(1, 2 * size)))
    return None


def project(plan, links, *, draft_depth=0, acceptance=.7,
            verify_extra_cost=.25, draft_token_ms=2.0):
    """Assumption-based capacity estimate, never a measured serving rate.

    Serial pass = sum(compute) + sum(RTT/2 + transfer + 1ms/hop).
    Feedback carries a token (8 bytes); activations carry k+1 positions.
    Aggregate is bounded by pipeline service, batch/latency and KV capacity.
    No batch compute speedup is assumed. Draft and verification costs included.
    """
    if type(draft_depth) is not int or not 0 <= draft_depth <= 5:
        raise ValueError("draft depth must be 0..5")
    if not 0 <= acceptance <= 1:
        raise ValueError("invalid acceptance probability")
    for value in (verify_extra_cost, draft_token_ms):
        if not math.isfinite(value) or value < 0:
            raise ValueError("invalid speculative cost")
    stages = plan.stages
    devices = [s.device for s in stages]
    edges = route_edges(devices)
    transfers = []
    rtts = []
    for i, (a, b) in enumerate(edges):
        edge = links(a, b)
        if edge is None:
            raise ValueError("missing pipeline link")
        payload = 8 if i == len(edges) - 1 else plan.model.activation_bytes * (draft_depth + 1)
        transfers.append(payload * 8 / (edge.mbps * 1000))
        rtts.append(edge.p95_ms)
    compute = [s.compute_ms * (1 + verify_extra_cost * draft_depth) for s in stages]
    network = sum(r / 2 + t + 1 for r, t in zip(rtts, transfers))
    latency = sum(compute) + network + draft_depth * draft_token_ms
    committed = sum(acceptance ** i for i in range(draft_depth + 1))
    service = max(max(compute), max(transfers, default=0), draft_depth * draft_token_ms)
    aggregate = min(1000 * committed / service,
                    plan.batch_depth * 1000 * committed / latency)
    return {"hop_count": len(edges), "hop_rtt_ms": rtts,
            "network_ms": network, "pass_latency_ms": latency,
            "single_answer_tok_s": 1000 * committed / latency,
            "loaded_answer_tok_s": aggregate / plan.batch_depth,
            "aggregate_tok_s": aggregate, "tokens_per_day": aggregate * 86400,
            "draft_depth": draft_depth, "expected_committed_tokens": committed,
            "batch_depth": plan.batch_depth}


class State(str, Enum):
    FORMED = "formed"
    HEALTHY = "healthy"
    READY = "ready"
    SERVING = "serving"
    RECOVERING = "recovering"
    DISSOLVED = "dissolved"


class Island:
    """Fail-closed lifecycle. All callbacks execute synchronously.

    Golden callback receives the COMPLETE plan and returns the executor's
    digest for pinned prompts, not a hash of the plan. The expected digest is
    externally pinned for this model/profile. Synthetic evidence cannot serve.
    Recovery callback must restore mirrored KV or replay committed tokens and
    return the trusted checkpoint digest; new golden qualification is required.
    No KV transport, sampler, or model executor is implemented here.
    """

    def __init__(self, plan, golden_digest):
        if len(golden_digest) != 64 or any(c not in "0123456789abcdef" for c in golden_digest):
            raise ValueError("expected a pinned 32-byte lowercase hex golden digest")
        self.plan = plan
        self.golden_digest = golden_digest
        self.state = State.FORMED
        self.active = set()
        self.warm_spare = False
        self.health_at = None

    def dissolve(self):
        self.state = State.DISSOLVED
        self.active.clear()
        self.warm_spare = False

    def health_check(self, current: Mapping[str, Device], links, now):
        if self.state == State.DISSOLVED:
            return False
        p = self.plan
        members = [s.device for s in p.stages] + ([p.spare] if p.spare else [])
        for old in members:
            d = current.get(old.id)
            if (d is None or d.owner != old.owner or d.evidence != "measured"
                    or not d.eligible(now, p.policy.ttl_seconds)):
                self.dissolve()
                return False
            needed = max(s.reserved_bytes for s in p.stages) if old == p.spare else next(
                s.reserved_bytes for s in p.stages if s.device.id == old.id)
            if d.usable_bytes < needed:
                self.dissolve()
                return False
        edges = list(route_edges([s.device for s in p.stages]))
        if p.spare:
            edges += [(p.spare, s.device) for s in p.stages]
            edges += [(s.device, p.spare) for s in p.stages]
        strict = replace(p.policy, allow_synthetic=False)
        if any(not link_ok(links(a, b), strict, now, p.tier == "T1b") for a, b in edges):
            self.dissolve()
            return False
        self.health_at = now
        # Admission estimates must use current probe speeds, not formation-time
        # speeds after thermal throttling or changing load.
        self.plan = replace(p, stages=tuple(
            replace(s, device=current[s.device.id],
                    compute_ms=sum(p.model.layer_ms[s.start:s.end]) / current[s.device.id].layer_speed)
            for s in p.stages), spare=current[p.spare.id] if p.spare else None)
        if self.state in (State.FORMED, State.RECOVERING):
            self.state = State.HEALTHY
        return True

    def qualify(self, execute_golden: Callable, warm_spare: Callable):
        if self.state != State.HEALTHY:
            raise ValueError("health check required before golden test")
        try:
            if execute_golden(self.plan) != self.golden_digest:
                self.dissolve()
                return False
            self.warm_spare = bool(self.plan.spare and warm_spare(self.plan))
            if self.plan.policy.require_spare and not self.warm_spare:
                self.dissolve()
                return False
        except Exception:
            self.dissolve()
            raise
        self.state = State.READY
        return True

    def admit(self, request_id, context, current, links, now,
              min_tok_s=0, prefill_queue_ms=0, max_prefill_queue_ms=1000):
        if self.state not in (State.READY, State.SERVING):
            return False
        if not self.health_check(current, links, now):
            return False
        if (not request_id or request_id in self.active or type(context) is not int
                or not 0 < context <= self.plan.model.context
                or len(self.active) >= self.plan.batch_depth
                or any(not math.isfinite(x) or x < 0 for x in
                       (min_tok_s, prefill_queue_ms, max_prefill_queue_ms))
                or prefill_queue_ms > max_prefill_queue_ms
                or project(self.plan, links)["loaded_answer_tok_s"] < min_tok_s):
            return False
        self.active.add(request_id)
        self.state = State.SERVING
        return True

    def complete(self, request_id):
        self.active.discard(request_id)
        if self.state == State.SERVING and not self.active:
            self.state = State.READY

    def replenish_spare(self, candidate_id, current, links, now):
        """Reserve replacement capacity after promotion; warming is requalified.

        The caller must reserve the device exclusively before scheduling the
        plan. There is no fleet-wide lease registry in this offline library.
        """
        if self.state != State.HEALTHY or self.plan.spare is not None:
            return False
        if candidate_id in {s.device.id for s in self.plan.stages}:
            return False
        candidate = current.get(candidate_id)
        if candidate is None:
            return False
        self.plan = replace(self.plan, spare=candidate)
        return self.health_check(current, links, now)

    def promote(self, failed_id, current, links, now, checkpoint_digest, recover):
        """Stop admission, restore state, and require fresh health + golden test.

        Checkpoint digest must be supplied from a trusted committed-token
        ledger, not from the recovering worker. Failed recovery dissolves.
        Caller reroutes interrupted requests; they are never silently resumed.
        """
        if self.state not in (State.READY, State.SERVING):
            return False
        p = self.plan
        if failed_id not in {s.device.id for s in p.stages}:
            raise ValueError("failed device is not an active stage")
        self.state = State.RECOVERING
        self.active.clear()
        spare = current.get(p.spare.id) if p.spare else None
        if (not self.warm_spare or spare is None or spare.owner != p.spare.owner
                or not spare.eligible(now, p.policy.ttl_seconds)
                or not checkpoint_digest):
            self.dissolve()
            return False
        stages = tuple(replace(s, device=spare,
                               compute_ms=sum(p.model.layer_ms[s.start:s.end]) / spare.layer_speed)
                       if s.device.id == failed_id else s for s in p.stages)
        # The promoted plan may operate without redundancy only when policy
        # explicitly permits it; otherwise remain closed until re-formation.
        self.plan = replace(p, stages=stages, spare=None)
        self.warm_spare = False
        if not self.health_check(current, links, now):
            return False
        try:
            if recover(self.plan, checkpoint_digest) != checkpoint_digest:
                self.dissolve()
                return False
        except Exception:
            self.dissolve()
            raise
        return True
