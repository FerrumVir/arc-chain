import { useMutation } from "@tanstack/react-query";
import {
  Coins,
  Copy,
  ClipboardCheck,
  Globe,
  Loader2,
  Search,
  Send,
  ShieldCheck,
  Sparkles,
  Zap,
} from "lucide-react";
import { useState } from "react";
import { Card, CardHeader } from "../components/Card";
import { InfoPopover } from "../components/InfoPopover";
import { api } from "../lib/tauri";
import { formatHash } from "../lib/format";
import { useAppStore } from "../lib/store";
import type {
  InferenceResult,
  PaidInferenceResult,
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
  const lookupHash = useAppStore((s) => s.lookupHash);
  const [prompt, setPrompt] = useState("");
  const [maxTokens, setMaxTokens] = useState(16);
  const [copied, setCopied] = useState<string | null>(null);
  // Milestone B (#36): toggle between free run_consensus and the paid path
  // (signs InferenceEscrowOpen locally, runs inference gated on that
  // escrow, coordinator auto-submits the 40/25/15/20 release).
  const [paidMode, setPaidMode] = useState(false);
  const [maxFee, setMaxFee] = useState(10_000);

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
                {/* This used to claim the prompt goes to the local node.
                    It did not — the call was routed to a remote seed. Now
                    it is true: the local node is tried first, and the UI
                    labels which machine actually served each response. */}
                <p>
                  Your prompt goes to your own node first, at{" "}
                  <code>POST /inference/run</code> on{" "}
                  <code>127.0.0.1</code>. If your node isn&rsquo;t running or
                  has no model loaded, it falls back to the fastest
                  reachable seed coordinator. Either way the response below
                  says which machine served it.
                </p>
                <p>
                  1. Runs the prompt through the model (or the sharded
                  pipeline if that node doesn't hold all layers).
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
          <div style={{ flex: 1, minWidth: "var(--space-3)" }} />
          <button
            className="btn btn-primary btn-lg"
            onClick={() => {
              if (paidMode) {
                paidRun.mutate();
              } else {
                run.mutate();
              }
            }}
            disabled={
              run.isPending || paidRun.isPending || !prompt.trim()
            }
            data-testid="btn-run-inference"
          >
            {run.isPending || paidRun.isPending ? (
              <>
                <Loader2
                  size={16}
                  style={{ animation: "spin 1s linear infinite" }}
                />{" "}
                {paidMode ? "Paying + computing…" : "Computing…"}
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

        {(run.isError || paidRun.isError) && (
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
            {((run.error || paidRun.error) as Error).message}
          </div>
        )}
      </Card>

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

          {/* Who served this. Always shown — a local answer is as much a
              fact worth stating as a remote one, and it's the difference
              between "the network did this" and "your machine did this". */}
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
                {run.data.servedLocally
                  ? "your node"
                  : run.data.coordinator
                    ? coordinatorLabel(run.data.coordinator)
                    : "the network"}
              </strong>
              {run.data.consensus && (
                <>
                  {" "}
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
                </>
              )}
            </span>
          </div>

          {/* Per-hop pipeline trace. The chain returns `shard_trace` on
              sharded runs and the app was discarding it — this is the
              evidence that the model really was split across machines. */}
          {run.data.trace && run.data.trace.length > 0 && (
            <div
              style={{ marginBottom: "var(--space-4)" }}
              data-testid="inference-trace"
            >
              <div
                style={{
                  fontSize: "var(--text-xs)",
                  color: "var(--text-muted)",
                  textTransform: "uppercase",
                  letterSpacing: "var(--tracking-wide)",
                  marginBottom: "var(--space-2)",
                }}
              >
                Pipeline · {run.data.trace.length} hops
              </div>
              <div style={{ display: "grid", gap: 4 }}>
                {run.data.trace.map((h) => (
                  <div
                    key={h.hop}
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: "var(--space-3)",
                      fontFamily: "var(--font-mono)",
                      fontSize: "var(--text-xs)",
                      color: "var(--text-secondary)",
                      padding: "4px 8px",
                      background: "var(--bg)",
                      borderRadius: "var(--radius-sm)",
                    }}
                  >
                    <span style={{ color: "var(--text-muted)", minWidth: 24 }}>
                      {h.hop}
                    </span>
                    <span style={{ flex: 1 }}>{h.node}</span>
                    <span style={{ color: "var(--text-muted)" }}>
                      layers {h.layers}
                    </span>
                    <span style={{ fontVariantNumeric: "tabular-nums" }}>
                      {h.computeMs}ms
                    </span>
                    {h.isTerminal && (
                      <span style={{ color: "var(--success)" }}>output</span>
                    )}
                  </div>
                ))}
              </div>
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
            {/* Was openExternal to `http://140.82.16.112:3200<explorerUrl>`:
                a hardcoded LAX IP, on a page that is a network dashboard
                rather than a block explorer, for a chain that is usually not
                the one this session is pinned to. The in-app lookup resolves
                the hash against the pinned host, which is the only place it
                can honestly be confirmed — including telling the user it is
                not in a block yet. */}
            {run.data.txHash && (
              <button
                className="btn btn-ghost btn-sm"
                onClick={() => lookupHash(run.data!.txHash)}
                data-testid="btn-lookup-tx"
              >
                Look up this attestation <Search size={12} />
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
