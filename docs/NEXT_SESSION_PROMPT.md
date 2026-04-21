# Next Session Prompt

Paste the following into a fresh Claude Code session in the `~/arc-chain` directory.

---

I'm TJ. You're picking up ARC Chain — a blockchain for verifiable AI inference where every node must produce bitwise-identical output for any given `(model, prompt)` pair. Cross-platform determinism is the core IP; FP16-quality parity is a secondary research goal.

**Read these files first, in this order, before doing anything else:**

1. `docs/SCALE_ARCHITECTURE.md` — the true spec, what the chain must become, and a frank admission of last session's mistakes.
2. `INFERENCE_DETERMINISM.md` — what's actually true today about determinism and quality.
3. Memory file `project_i16_ppl_bug.md` at `/Users/tjdunham/.claude/projects/-Users-tjdunham/memory/` — the five-iteration saga of chasing a forward-pass bug that didn't exist.
4. Memory file `feedback_audit_before_claiming.md` — read before concluding anything is "broken".
5. Memory file `feedback_never_restart_chain.md` — ABSOLUTE RULE, rolling upgrade 1 seed at a time.

**Verified truth as of last session (commit `6af24d6a` on `main`):**

- Integer forward matches candle's Q8_0 reference on argmax across every tested position (6 positions through "The capital of France is", argmax + top-10 + logit magnitude within 5%). `probe_candle_multi.rs` is the proof.
- Block-i8 matmul is correct at ratio 1.000 vs f32 ground truth on real Llama tensors. `probe_block_i8.rs` is the proof.
- Cross-platform determinism holds bitwise on ARM Mac + x86 Vultr. 112 lib tests pass.
- 5× PPL tail-distribution gap vs candle Q8_0 on 256-token snippets (not an argmax bug — a cumulative integer-arithmetic noise in non-top logit confidence). Does NOT affect generation output quality for real prompts.

**Current deployment state:**

- 8 Vultr seeds running v0.5.2 with 1-node-per-shard config (layers [0,5), [5,10), ..., [30,32)).
- Single-point-of-failure: any seed going offline kills the shard pipeline. Observed live last session when JNB + SAO dropped.
- Mac runs full 32-layer model + coordinates. NYC:10000 has an SSH reverse tunnel to Mac (see `scripts/arc-tunnel-watchdog.sh`).
- Dashboard at `http://140.82.16.112:3200/` routes inference through `SHARDED_COORDINATOR = http://149.28.32.76:10000` (the tunnel).
- Mac node tends to fall behind consensus when running heavy PPL evals.

**The immediate architectural problem and the fix, per `SCALE_ARCHITECTURE.md`:**

- 7B fits on every seed (4GB Q8_0 vs 8GB RAM). Sharding it across 8 seeds with 1 replica each is the worst possible tradeoff: more fragile AND slower than replication.
- Fix is full-model replication for models that fit. Keep sharding code for future 70B. See `docs/SCALE_ARCHITECTURE.md` §2.3 for the sizing math and §3 Phase 1 for the concrete actions.

**What you should do in this session, in priority order:**

1. **Phase 1 rolling upgrade**: restart each seed WITHOUT `--shard-start/--shard-end` flags, one at a time, verifying it loads the full 4GB model and rejoins consensus before moving to the next. Do NOT restart Mac — keep it up as a stable reference. This fixes the live outage (JNB + SAO offline breaks shard path).
2. **Dashboard update**: rewire "Run on All 8" to fire 8 parallel `/inference/run` requests against the now-replicated seeds (not `SHARDED_COORDINATOR`). All 8 should return identical output + hash. Update copy to say "8 independent machines, cross-architecture verified" (which it actually is now).
3. **Committee consensus endpoint**: add `/inference/committee` that picks `k` replicas and runs k-of-m hash agreement. Start with k=3.
4. **Tear down the tunnel + watchdog** only after steps 1+2 succeed — once every seed serves real inference independently, the Mac-as-coordinator hack is obsolete.

**Hard constraints — do not violate:**

- Rolling upgrade ONE seed at a time. Verify it rejoins consensus (`/health` shows `dag_round` increasing, `peers >= 4`) before the next.
- Never restart all seeds simultaneously. NEVER.
- Mac stays up throughout. It's the stable reference until the seeds prove they can carry inference without it.
- Every change must preserve cross-platform determinism (ARM Mac hash == x86 Vultr hash). Verify via `/inference/run` → same hash.
- If an eval regresses quality, REVERT that specific change immediately. Don't chain speculative fixes.

**Probes to run before debugging anything:**

- `./target/release/examples/probe_candle_multi` — should match on all 6 positions. If not, forward IS broken (different from last session's state).
- `./target/release/examples/probe_block_i8` — ratio 1.000. If not, matmul regressed.

**What to ignore:**

- The 5× PPL gap on benchmark snippets. It's a known quality-tail issue, not a correctness issue. Phase 5 of the roadmap (SmoothQuant calibration) addresses it, but it's lower priority than network redundancy.
- The "I16 dual-quantize" rabbit hole. Committed probes prove it's mathematically equivalent to block-i8; block-i8 wins on granularity. Do not re-investigate unless the probe suite shows a regression.

**Start by:**

1. Running `/inference/run_sharded` against the tunnel to see if it still returns coherent text ("Paris." for France prompt). If yes, baseline demo still works. If not, that's the first thing to fix (probably just restart the tunnel via `/tmp/arc-tunnel-watchdog.sh` if it died).
2. Running `probe_candle_multi` to confirm the forward still matches candle. If argmaxes match, quality is fine for Phase 1 work; move to the rolling upgrade.
3. Picking one seed to upgrade first (recommend NYC since it's the one dashboard defaults to). Walk through the upgrade carefully, confirm it rejoins, then move on.

Be brutally honest. Match probes before making claims. Revert fast if quality regresses. The goal is a network that scales to any model on any device without ever breaking — not a PPL number.
