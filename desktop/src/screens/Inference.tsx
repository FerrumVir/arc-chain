import { useMutation } from "@tanstack/react-query";
import {
  ArrowUpRight,
  Coins,
  Copy,
  ClipboardCheck,
  Globe,
  Loader2,
  Send,
  ShieldCheck,
  Sparkles,
  Zap,
} from "lucide-react";
import { useEffect, useState } from "react";
import { Card, CardHeader } from "../components/Card";
import { InfoPopover } from "../components/InfoPopover";
import { api } from "../lib/tauri";
import { useAppStore } from "../lib/store";
import { formatHash } from "../lib/format";
import type {
  InferenceResult,
  PaidInferenceResult,
  Tier1Result,
} from "../lib/types";

const EXAMPLES = [
  "What is the largest planet in our solar system?",
  "Explain zero-knowledge proofs in one sentence.",
  "Write a haiku about blockchains.",
  "Who invented the transistor?",
];

/// Milestone A (#35): local node may be an observer with no model loaded
/// and return 503 / connection error. Silently fall back to a seed
/// coordinator via /inference/run_consensus so a fresh-install user gets
/// a real answer without configuring anything.
///
/// Second fallback: if /inference/run_consensus fails on every coordinator
/// with "Pipeline gap" (chain-side bug: retired SAO+JNB shards still in
/// the ShardRegistry trip every coordinator's planner before any token is
/// emitted), retry against the same coordinators using /inference/run
/// directly. Loses k-of-n consensus, but the coordinator still emits an
/// on-chain attestation and the user gets a real answer instead of an
/// error. Once the ShardRegistry is cleaned up, this third tier becomes
/// dormant.
async function runInferenceSmart(
  prompt: string,
  maxTokens: number,
): Promise<InferenceResult> {
  const fallbackToCoordinator = async (): Promise<InferenceResult> => {
    try {
      return await api.runInferenceViaCoordinator(prompt, maxTokens);
    } catch (consensusErr) {
      const cmsg = String(
        consensusErr instanceof Error ? consensusErr.message : consensusErr,
      );
      if (
        cmsg.includes("Pipeline gap") ||
        cmsg.includes("503") ||
        cmsg.includes("all") // "all 6 coordinators failed; last: ..."
      ) {
        return await api.runInferenceViaCoordinatorDirect(prompt, maxTokens);
      }
      throw consensusErr;
    }
  };

  try {
    const r = await api.runInference(prompt, maxTokens);
    // Even a 200 from the local node can be an empty/placeholder result
    // (some observer builds short-circuit); fall back on empty output so
    // the user always sees *something*.
    if (r.output.trim().length === 0 && !r.coordinator) {
      return await fallbackToCoordinator();
    }
    return r;
  } catch (err) {
    const msg = String(err instanceof Error ? err.message : err);
    // Local node returned 503 (observer / no model), or wasn't reachable,
    // or disagreed about the request shape. Try the coordinator path.
    if (
      msg.includes("503") ||
      msg.includes("SERVICE") ||
      msg.includes("fetch") ||
      msg.includes("error sending request") ||
      msg.toLowerCase().includes("connection") ||
      msg.includes("No shards")
    ) {
      return await fallbackToCoordinator();
    }
    throw err;
  }
}

function coordinatorLabel(url: string): string {
  const host = url.replace(/^https?:\/\//, "").split(":")[0];
  const byIp: Record<string, string> = {
    "149.28.32.76": "NYC",
    "140.82.16.112": "LAX",
    "136.244.109.1": "AMS",
    "104.238.171.11": "LHR",
    "202.182.107.41": "NRT",
    "149.28.153.31": "SGP",
  };
  return byIp[host] ?? host;
}

export function Inference() {
  const [prompt, setPrompt] = useState("");
  const [maxTokens, setMaxTokens] = useState(32);
  const [copied, setCopied] = useState<string | null>(null);
  // Milestone B (#36): toggle between free run_consensus and the paid path
  // (signs InferenceEscrowOpen locally, runs inference gated on that
  // escrow, coordinator auto-submits the 40/25/15/20 release).
  const [paidMode, setPaidMode] = useState(false);
  const [maxFee, setMaxFee] = useState(10_000);
  // Tier 1 on-chain mode. Selected in Settings. Default `coordinator`
  // (legacy path) until Phase C cuts over.
  // HIDDEN 2026-06-04: tier-1 on-chain submit never lands on the live seeds
  // — the InferenceRequest tx is accepted but never included in a block
  // (see docs/INFERENCE_TIER1_INVESTIGATION_2026-06-04.md). Force the working
  // coordinator (community) path until the consensus inclusion bug is fixed.
  // Flip TIER1_ONCHAIN_ENABLED back to true to restore the on-chain UI.
  const TIER1_ONCHAIN_ENABLED = false;
  const storedInferenceMode = useAppStore((s) => s.inferenceMode);
  const inferenceMode = TIER1_ONCHAIN_ENABLED ? storedInferenceMode : "coordinator";
  // HIDDEN 2026-06-04: the paid "Pay per request" path opens an on-chain
  // InferenceEscrowOpen tx that also never lands on the live seeds (same
  // inclusion bug). Hide it so only the working free community path shows.
  // Flip back to true to restore paid/escrow inference.
  const PAID_ONCHAIN_ENABLED = false;
  const [tier1RequestId, setTier1RequestId] = useState<string | null>(null);
  const [tier1Result, setTier1Result] = useState<Tier1Result | null>(null);
  // Tier 1 user-tunable params. Defaults match the production target
  // (committee=5, reward=10). On a solo dev chain set committee=1.
  const [tier1CommitteeSize, setTier1CommitteeSize] = useState(1);
  const [tier1MaxReward, setTier1MaxReward] = useState(10);

  const run = useMutation<InferenceResult, Error, void>({
    mutationFn: async () => {
      if (!prompt.trim()) throw new Error("Prompt is empty");
      return await runInferenceSmart(prompt.trim(), maxTokens);
    },
  });
  const paidRun = useMutation<PaidInferenceResult, Error, void>({
    mutationFn: async () => {
      if (!prompt.trim()) throw new Error("Prompt is empty");
      return await api.runPaidInference(prompt.trim(), maxTokens, maxFee);
    },
  });
  const tier1Run = useMutation<{ requestId: string }, Error, void>({
    mutationFn: async () => {
      if (!prompt.trim()) throw new Error("Prompt is empty");
      const sub = await api.tier1Submit(
        prompt.trim(),
        maxTokens,
        tier1MaxReward,
        20, // deadline_blocks (keep default)
        tier1CommitteeSize,
      );
      setTier1RequestId(sub.requestId);
      setTier1Result(null);
      return { requestId: sub.requestId };
    },
  });

  // Poll the chain for status until terminal (Finalized / Refunded).
  // Cancellation is via setTier1RequestId(null) on a new submission.
  useEffect(() => {
    if (!tier1RequestId) return;
    let cancelled = false;
    const poll = async () => {
      while (!cancelled) {
        try {
          const r = await api.tier1Result(tier1RequestId);
          if (cancelled) return;
          setTier1Result(r);
          if (r.status === "Finalized" || r.status === "Refunded") return;
        } catch (e) {
          // Transient error — keep polling unless the request is gone.
          const msg = String(e instanceof Error ? e.message : e);
          if (msg.includes("404")) return;
        }
        await new Promise((res) => setTimeout(res, 500));
      }
    };
    poll();
    return () => {
      cancelled = true;
    };
  }, [tier1RequestId]);

  const copy = async (key: string, value: string) => {
    await navigator.clipboard.writeText(value);
    setCopied(key);
    setTimeout(() => setCopied(null), 1200);
  };

  return (
    <div className="main-inner" data-testid="inference-screen">
      <div className="page-header">
        <div>
          <h1 className="page-title">Inference</h1>
          <p className="page-subtitle">
            Submit a prompt. The network runs a sharded Llama-7B, returns a
            response, and posts a bonded attestation on-chain.
          </p>
        </div>
      </div>

      <Card featured style={{ marginBottom: "var(--space-6)" }}>
        <CardHeader
          title={
            <span style={{ display: "inline-flex", alignItems: "center" }}>
              Prompt
              <InfoPopover title="How this works">
                <p>
                  Your prompt goes to the local node's{" "}
                  <code>POST /inference/run</code>. That endpoint:
                </p>
                <p>
                  1. Runs the prompt through the model (or the sharded
                  pipeline if this node doesn't hold all layers).
                </p>
                <p>
                  2. Hashes the output tokens with BLAKE3.
                </p>
                <p>
                  3. Submits an <code>InferenceAttestation</code> transaction
                  with <code>(input_hash, output_hash, model_hash)</code> + a
                  1,000 ARC bond.
                </p>
                <p>
                  4. Returns the output and the attestation's{" "}
                  <code>tx_hash</code> - which you can see below.
                </p>
              </InfoPopover>
            </span>
          }
          action={
            <span
              style={{
                fontSize: "var(--text-xs)",
                color: "var(--text-muted)",
              }}
            >
              model: Llama-2-7B-Chat Q4
            </span>
          }
        />

        <textarea
          className="input"
          style={{ fontFamily: "var(--font-sans)", minHeight: 92, resize: "vertical" }}
          placeholder="Ask the network anything…"
          value={prompt}
          onChange={(e) => setPrompt(e.target.value)}
          data-testid="inference-prompt"
          maxLength={500}
        />

        <div
          style={{
            display: "flex",
            flexWrap: "wrap",
            gap: "var(--space-2)",
            marginTop: "var(--space-3)",
          }}
        >
          <span
            style={{
              fontSize: "var(--text-xs)",
              color: "var(--text-muted)",
              marginRight: "var(--space-2)",
              alignSelf: "center",
            }}
          >
            Try:
          </span>
          {EXAMPLES.map((ex) => (
            <button
              key={ex}
              className="btn btn-ghost btn-sm"
              onClick={() => setPrompt(ex)}
              data-testid={`example-${ex.slice(0, 10)}`}
            >
              {ex.length > 42 ? ex.slice(0, 42) + "…" : ex}
            </button>
          ))}
        </div>

        <div
          style={{
            display: "flex",
            flexWrap: "wrap",
            alignItems: "flex-end",
            columnGap: "var(--space-3)",
            rowGap: "var(--space-3)",
            marginTop: "var(--space-5)",
          }}
        >
          <label
            style={{
              display: "flex",
              flexDirection: "column",
              gap: 4,
              flex: "0 0 140px",
            }}
          >
            <span className="field-label">Max tokens</span>
            <input
              className="input input-mono"
              type="number"
              min={1}
              max={256}
              value={maxTokens}
              onChange={(e) => setMaxTokens(parseInt(e.target.value, 10) || 32)}
              data-testid="inference-max-tokens"
            />
          </label>
          {inferenceMode === "onchain" ? (
            <>
              <label
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: 4,
                  flex: "0 0 140px",
                }}
              >
                <span className="field-label">Committee size</span>
                <input
                  className="input input-mono"
                  type="number"
                  min={1}
                  max={15}
                  value={tier1CommitteeSize}
                  onChange={(e) =>
                    setTier1CommitteeSize(parseInt(e.target.value, 10) || 1)
                  }
                  data-testid="tier1-committee-size"
                />
              </label>
              <label
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: 4,
                  flex: "0 0 140px",
                }}
              >
                <span className="field-label">Max reward (ARC)</span>
                <input
                  className="input input-mono"
                  type="number"
                  min={1}
                  max={1_000_000}
                  value={tier1MaxReward}
                  onChange={(e) =>
                    setTier1MaxReward(parseInt(e.target.value, 10) || 10)
                  }
                  data-testid="tier1-max-reward"
                />
              </label>
            </>
          ) : PAID_ONCHAIN_ENABLED ? (
            <>
              <label
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: 4,
                  flex: "0 0 140px",
                  opacity: paidMode ? 1 : 0.5,
                }}
              >
                <span className="field-label">Max fee (ARC)</span>
                <input
                  className="input input-mono"
                  type="number"
                  min={1}
                  max={1_000_000}
                  step={1}
                  value={maxFee}
                  onChange={(e) =>
                    setMaxFee(parseInt(e.target.value, 10) || 10_000)
                  }
                  disabled={!paidMode}
                  data-testid="inference-max-fee"
                />
              </label>
              <label
                style={{
                  display: "inline-flex",
                  alignItems: "center",
                  gap: 6,
                  fontSize: "var(--text-sm)",
                  color: "var(--text-muted)",
                  whiteSpace: "nowrap",
                  paddingBottom: 10,
                }}
                data-testid="paid-mode-toggle-label"
              >
                <input
                  type="checkbox"
                  checked={paidMode}
                  onChange={(e) => setPaidMode(e.target.checked)}
                  data-testid="paid-mode-toggle"
                />
                Pay per request
              </label>
            </>
          ) : null}
          <div style={{ flex: 1, minWidth: "var(--space-3)" }} />
          <button
            className="btn btn-primary btn-lg"
            onClick={() => {
              if (inferenceMode === "onchain") {
                tier1Run.mutate();
              } else if (paidMode) {
                paidRun.mutate();
              } else {
                run.mutate();
              }
            }}
            disabled={
              run.isPending ||
              paidRun.isPending ||
              tier1Run.isPending ||
              (tier1RequestId != null &&
                tier1Result?.status !== "Finalized" &&
                tier1Result?.status !== "Refunded") ||
              !prompt.trim()
            }
            data-testid="btn-run-inference"
          >
            {run.isPending || paidRun.isPending || tier1Run.isPending ? (
              <>
                <Loader2
                  size={16}
                  style={{ animation: "spin 1s linear infinite" }}
                />{" "}
                {inferenceMode === "onchain"
                  ? "Submitting to chain…"
                  : paidMode
                    ? "Paying + computing…"
                    : "Computing…"}
              </>
            ) : inferenceMode === "onchain" ? (
              <>
                <ShieldCheck size={16} /> Submit on-chain
              </>
            ) : paidMode ? (
              <>
                <Coins size={16} /> Pay {maxFee.toLocaleString()} ARC & run
              </>
            ) : (
              <>
                <Send size={16} /> Run inference
              </>
            )}
          </button>
        </div>

        {(run.isError || paidRun.isError || tier1Run.isError) && (
          <div
            style={{
              marginTop: "var(--space-4)",
              padding: "var(--space-3) var(--space-4)",
              background: "var(--danger-bg)",
              border: "1px solid rgba(240, 115, 115, 0.2)",
              borderRadius: "var(--radius-sm)",
              color: "var(--danger)",
              fontSize: "var(--text-sm)",
            }}
            data-testid="inference-error"
          >
            {((run.error || paidRun.error || tier1Run.error) as Error).message}
          </div>
        )}
      </Card>

      {/* Tier 1 on-chain status panel */}
      {inferenceMode === "onchain" && tier1RequestId && tier1Result && (
        <Card
          style={{ marginBottom: "var(--space-6)" }}
          data-testid="tier1-status"
        >
          <CardHeader
            title={
              <span style={{ display: "inline-flex", alignItems: "center" }}>
                <ShieldCheck size={16} style={{ marginRight: 8 }} />
                On-chain Tier 1 — {tier1Result.status}
                <InfoPopover title="What is this?">
                  <>
                    Every committee member runs the model locally, submits
                    their <code>output_hash</code> as a vote tx, and the
                    chain finalizes once <code>min_agreement</code> votes
                    agree. No off-chain coordinator.
                  </>
                </InfoPopover>
              </span>
            }
            action={
              <span style={{ fontSize: "var(--text-sm)", color: "var(--text-muted)" }}>
                {tier1Result.voteCount} / {tier1Result.committeeSize} votes ·
                anchor #{tier1Result.anchorHeight}
              </span>
            }
          />
          <div
            style={{
              padding: "var(--space-3) var(--space-4)",
              display: "grid",
              gap: "var(--space-2)",
              fontSize: "var(--text-sm)",
            }}
          >
            <div>
              <strong>Request:</strong>{" "}
              <code>{formatHash(tier1Result.requestId)}</code>{" "}
              <button
                className="btn btn-ghost btn-sm"
                onClick={() => copy("tier1-req", tier1Result.requestId)}
              >
                {copied === "tier1-req" ? (
                  <ClipboardCheck size={12} />
                ) : (
                  <Copy size={12} />
                )}
              </button>
            </div>
            <div>
              <strong>Lock:</strong> {tier1Result.maxReward} ARC in escrow ·
              deadline +{tier1Result.deadlineBlocks} blocks
            </div>
            {tier1Result.votes.length > 0 && (
              <div>
                <strong>Votes:</strong>
                <ul style={{ marginTop: 4, paddingLeft: 18 }}>
                  {tier1Result.votes.map((v) => (
                    <li key={v.voter}>
                      <code>{formatHash(v.voter)}</code> →{" "}
                      <code>{formatHash(v.outputHash)}</code>
                    </li>
                  ))}
                </ul>
              </div>
            )}
            {tier1Result.status === "Voting" && (
              <div style={{ color: "var(--text-muted)" }}>
                <Loader2
                  size={14}
                  style={{
                    animation: "spin 1s linear infinite",
                    marginRight: 6,
                    verticalAlign: "middle",
                  }}
                />
                Waiting for{" "}
                {tier1Result.committeeSize - tier1Result.voteCount} more vote(s)…
              </div>
            )}
            {tier1Result.status === "Finalized" &&
              (tier1Result.outputText || tier1Result.outputBlob) && (
                <div
                  style={{
                    marginTop: "var(--space-2)",
                    padding: "var(--space-3)",
                    background: "var(--surface-2)",
                    borderRadius: "var(--radius-sm)",
                    whiteSpace: "pre-wrap",
                  }}
                >
                  <strong style={{ color: "var(--success)" }}>
                    ✓ Verified on-chain consensus
                  </strong>
                  <div style={{ marginTop: 8 }}>
                    {tier1Result.outputText ?? tier1Result.outputBlob}
                  </div>
                  {tier1Result.outputHash && (
                    <div style={{ marginTop: 8, color: "var(--text-muted)", fontSize: 12 }}>
                      hash: <code>{formatHash(tier1Result.outputHash)}</code>
                    </div>
                  )}
                </div>
              )}
            {tier1Result.status === "Refunded" && (
              <div style={{ color: "var(--warning)" }}>
                ⚠ Request refunded (timeout or disagreement). Locked ARC
                returned minus 1 ARC anti-spam fee.
              </div>
            )}
          </div>
        </Card>
      )}

      {paidRun.isSuccess && paidRun.data && (
        <Card data-testid="paid-inference-result">
          <CardHeader
            title={
              <span style={{ display: "inline-flex", alignItems: "center", gap: 8 }}>
                Paid response
                <Coins size={14} style={{ color: "var(--accent, #e5a84f)" }} />
              </span>
            }
            action={
              <span
                style={{
                  fontSize: "var(--text-xs)",
                  color: "var(--text-muted)",
                  fontFamily: "var(--font-mono)",
                  fontVariantNumeric: "tabular-nums",
                }}
              >
                {paidRun.data.tokensGenerated} tokens · {paidRun.data.inferenceMs}ms
              </span>
            }
          />

          <div
            style={{
              display: "flex",
              flexWrap: "wrap",
              alignItems: "center",
              gap: "var(--space-3)",
              padding: "var(--space-3) var(--space-4)",
              marginBottom: "var(--space-4)",
              background: "var(--success-bg, rgba(80, 200, 120, 0.08))",
              border: "1px solid rgba(80, 200, 120, 0.25)",
              borderRadius: "var(--radius-sm)",
              fontSize: "var(--text-sm)",
            }}
            data-testid="paid-summary"
          >
            <Globe size={14} style={{ color: "var(--success)" }} />
            <span>
              Paid{" "}
              <strong>{paidRun.data.maxFee.toLocaleString()} ARC</strong>{" "}
              to{" "}
              <strong>
                {coordinatorLabel(paidRun.data.coordinator)}
              </strong>{" "}
              · k={paidRun.data.consensus.k} ·{" "}
              {paidRun.data.consensus.unanimous}/
              {paidRun.data.consensus.votesTotal}{" "}
              {paidRun.data.consensus.split === 0 &&
              paidRun.data.consensus.majority === 0
                ? "unanimous"
                : `${paidRun.data.consensus.majority} majority / ${paidRun.data.consensus.split} split`}
            </span>
          </div>

          <div
            style={{
              padding: "var(--space-4)",
              background: "var(--bg)",
              border: "1px solid var(--border)",
              borderRadius: "var(--radius-md)",
              fontFamily: "var(--font-sans)",
              fontSize: "var(--text-md)",
              color: "var(--text)",
              lineHeight: 1.6,
              marginBottom: "var(--space-4)",
              whiteSpace: "pre-wrap",
            }}
            data-testid="paid-inference-output"
          >
            {paidRun.data.output.trim() || "(empty)"}
          </div>

          <div style={{ display: "grid", gap: "var(--space-2)", fontSize: "var(--text-sm)" }}>
            <HashRow
              label="Escrow open"
              value={paidRun.data.openTxHash}
              copied={copied === "open-tx"}
              onCopy={() => copy("open-tx", paidRun.data!.openTxHash)}
              icon={Coins}
            />
            {paidRun.data.releaseTxHash && (
              <HashRow
                label="Release tx"
                value={paidRun.data.releaseTxHash}
                copied={copied === "release-tx"}
                onCopy={() => copy("release-tx", paidRun.data!.releaseTxHash)}
                icon={Sparkles}
              />
            )}
            <HashRow
              label="Output hash"
              value={paidRun.data.outputHash}
              copied={copied === "paid-out"}
              onCopy={() => copy("paid-out", paidRun.data!.outputHash)}
              icon={Zap}
            />
            <HashRow
              label="Payer"
              value={paidRun.data.payerAddress}
              copied={copied === "payer"}
              onCopy={() => copy("payer", paidRun.data!.payerAddress)}
              icon={ShieldCheck}
            />
          </div>
        </Card>
      )}

      {run.isSuccess && run.data && (
        <Card data-testid="inference-result">
          <CardHeader
            title={
              <span style={{ display: "inline-flex", alignItems: "center", gap: 8 }}>
                Response
                <ShieldCheck size={14} style={{ color: "var(--success)" }} />
              </span>
            }
            action={
              <span
                style={{
                  fontSize: "var(--text-xs)",
                  color: "var(--text-muted)",
                  fontFamily: "var(--font-mono)",
                  fontVariantNumeric: "tabular-nums",
                }}
              >
                {run.data.tokensGenerated} tokens · {run.data.inferenceMs}ms
              </span>
            }
          />

          {run.data.coordinator && run.data.consensus && (
            <div
              style={{
                display: "flex",
                flexWrap: "wrap",
                alignItems: "center",
                gap: "var(--space-3)",
                padding: "var(--space-3) var(--space-4)",
                marginBottom: "var(--space-4)",
                background: "var(--success-bg, rgba(80, 200, 120, 0.08))",
                border: "1px solid rgba(80, 200, 120, 0.25)",
                borderRadius: "var(--radius-sm)",
                fontSize: "var(--text-sm)",
                color: "var(--text)",
              }}
              data-testid="inference-consensus"
            >
              <Globe size={14} style={{ color: "var(--success)" }} />
              <span>
                Served by{" "}
                <strong data-testid="inference-coordinator">
                  {coordinatorLabel(run.data.coordinator)}
                </strong>{" "}
                · k={run.data.consensus.k} · {run.data.consensus.unanimous}/
                {run.data.consensus.votesTotal}{" "}
                {run.data.consensus.split === 0 &&
                run.data.consensus.majority === 0
                  ? "unanimous"
                  : `${run.data.consensus.majority} majority / ${run.data.consensus.split} split`}
                {run.data.consensus.divergentReplicaCount > 0 && (
                  <>
                    {" "}
                    ·{" "}
                    <span style={{ color: "var(--danger)" }}>
                      {run.data.consensus.divergentReplicaCount} divergent
                    </span>
                  </>
                )}
              </span>
            </div>
          )}

          <div
            style={{
              padding: "var(--space-4)",
              background: "var(--bg)",
              border: "1px solid var(--border)",
              borderRadius: "var(--radius-md)",
              fontFamily: "var(--font-sans)",
              fontSize: "var(--text-md)",
              color: "var(--text)",
              lineHeight: 1.6,
              marginBottom: "var(--space-4)",
              whiteSpace: "pre-wrap",
            }}
            data-testid="inference-output"
          >
            {run.data.output.trim() || "(empty)"}
          </div>

          <div
            style={{
              display: "grid",
              gap: "var(--space-2)",
              fontSize: "var(--text-sm)",
            }}
          >
            {run.data.txHash && (
              <HashRow
                label="Attestation tx"
                value={run.data.txHash}
                copied={copied === "tx"}
                onCopy={() => copy("tx", run.data!.txHash)}
                icon={Sparkles}
              />
            )}
            <HashRow
              label="Output hash"
              value={run.data.outputHash}
              copied={copied === "out"}
              onCopy={() => copy("out", run.data!.outputHash)}
              icon={Zap}
            />
            {run.data.modelHash && (
              <HashRow
                label="Model hash"
                value={run.data.modelHash}
                copied={copied === "model"}
                onCopy={() => copy("model", run.data!.modelHash)}
                icon={ShieldCheck}
              />
            )}
          </div>

          <div
            style={{
              marginTop: "var(--space-4)",
              paddingTop: "var(--space-4)",
              borderTop: "1px solid var(--border)",
              display: "flex",
              justifyContent: "space-between",
              alignItems: "center",
              fontSize: "var(--text-xs)",
              color: "var(--text-muted)",
            }}
          >
            <span>
              Engine: {run.data.engine}{" "}
              {run.data.deterministic && "· deterministic"}
            </span>
            {run.data.explorerUrl && (
              <button
                className="btn btn-ghost btn-sm"
                onClick={() =>
                  api.openExternal(
                    `http://140.82.16.112:3200${run.data!.explorerUrl}`,
                  )
                }
                data-testid="btn-open-explorer-tx"
              >
                View on explorer <ArrowUpRight size={12} />
              </button>
            )}
          </div>
        </Card>
      )}

      <style>{`@keyframes spin { to { transform: rotate(360deg); } }`}</style>
    </div>
  );
}

function HashRow({
  label,
  value,
  copied,
  onCopy,
  icon: Icon,
}: {
  label: string;
  value: string;
  copied: boolean;
  onCopy: () => void;
  icon: typeof Sparkles;
}) {
  return (
    <div
      style={{
        display: "grid",
        gridTemplateColumns: "auto 1fr auto",
        alignItems: "center",
        gap: "var(--space-3)",
        padding: "6px 0",
        borderBottom: "1px solid var(--border)",
      }}
    >
      <Icon size={12} style={{ color: "var(--text-muted)" }} />
      <div style={{ display: "flex", gap: "var(--space-3)", alignItems: "center" }}>
        <span
          style={{
            color: "var(--text-muted)",
            fontSize: "var(--text-xs)",
            width: 100,
          }}
        >
          {label}
        </span>
        <code
          style={{
            fontFamily: "var(--font-mono)",
            fontSize: "var(--text-xs)",
            color: "var(--text)",
          }}
        >
          {formatHash(value, 18)}
        </code>
      </div>
      <button
        className="btn btn-ghost btn-sm"
        style={{ padding: "2px 8px" }}
        onClick={onCopy}
        aria-label={`Copy ${label.toLowerCase()}`}
      >
        {copied ? <ClipboardCheck size={12} /> : <Copy size={12} />}
      </button>
    </div>
  );
}
