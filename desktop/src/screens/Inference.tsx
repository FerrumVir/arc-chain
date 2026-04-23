import { useMutation } from "@tanstack/react-query";
import {
  ArrowUpRight,
  Copy,
  ClipboardCheck,
  Globe,
  Loader2,
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
import type { InferenceResult } from "../lib/types";

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
async function runInferenceSmart(
  prompt: string,
  maxTokens: number,
): Promise<InferenceResult> {
  try {
    const r = await api.runInference(prompt, maxTokens);
    // Even a 200 from the local node can be an empty/placeholder result
    // (some observer builds short-circuit); fall back on empty output so
    // the user always sees *something*.
    if (r.output.trim().length === 0 && !r.coordinator) {
      return await api.runInferenceViaCoordinator(prompt, maxTokens);
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
      return await api.runInferenceViaCoordinator(prompt, maxTokens);
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

  const run = useMutation<InferenceResult, Error, void>({
    mutationFn: async () => {
      if (!prompt.trim()) throw new Error("Prompt is empty");
      return await runInferenceSmart(prompt.trim(), maxTokens);
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
                  <code>tx_hash</code> — which you can see below.
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
            alignItems: "center",
            gap: "var(--space-3)",
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
          <div style={{ flex: 1 }} />
          <button
            className="btn btn-primary btn-lg"
            onClick={() => run.mutate()}
            disabled={run.isPending || !prompt.trim()}
            data-testid="btn-run-inference"
          >
            {run.isPending ? (
              <>
                <Loader2
                  size={16}
                  style={{ animation: "spin 1s linear infinite" }}
                />{" "}
                Computing…
              </>
            ) : (
              <>
                <Send size={16} /> Run inference
              </>
            )}
          </button>
        </div>

        {run.isError && (
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
            {(run.error as Error).message}
          </div>
        )}
      </Card>

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
