import { useMutation, useQuery } from "@tanstack/react-query";
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
import { useEffect, useRef, useState } from "react";
import { Card, CardHeader } from "../components/Card";
import { InfoPopover } from "../components/InfoPopover";
import { api } from "../lib/tauri";
import { formatHash } from "../lib/format";
import { hostLabel } from "../lib/hosts";
import { useAppStore } from "../lib/store";
import type { InferenceResult, InferenceSettlement } from "../lib/types";

const EXAMPLES = [
  "The largest planet is",
  "Water boils at",
  "The sun is a",
  "Bitcoin is a",
];

const ARC_HASH_RE = /^0x[0-9a-f]{64}$/;
const COMMUNITY_REWARD_ARC = 2.5;
const MAX_RECEIPT_POLLS = 20;
const MAX_RECEIPT_POLL_MS = 60_000;

/// Local execution stays first. If this machine cannot serve, call a seed's
/// `/inference/run` before the standalone consensus route: that endpoint gives
/// registered community workers first refusal, then safely falls through to
/// the seed's sharded/local execution. Only when every direct coordinator
/// fails before completing a job do we use `/inference/run_consensus`.
async function runInferenceSmart(
  prompt: string,
  maxTokens: number,
): Promise<InferenceResult> {
  const fallbackToCoordinator = async (): Promise<InferenceResult> => {
    try {
      return await api.runInferenceViaCoordinatorDirect(prompt, maxTokens);
    } catch (directErr) {
      const message = String(
        directErr instanceof Error ? directErr.message : directErr,
      );
      // The direct command retries every coordinator for service/topology
      // failures. Reaching this aggregate error means no community assignment
      // completed, so a sharded-consensus fallback cannot duplicate a reward.
      if (
        message.includes("all coordinators failed (direct path)") ||
        message.includes("no coordinator answered /health")
      ) {
        return await api.runInferenceViaCoordinator(prompt, maxTokens);
      }
      // In particular, never fall back after a 504 that says a claimed
      // community assignment may still settle.
      throw directErr;
    }
  };

  try {
    const r = await api.runInference(prompt, maxTokens);
    // An empty successful completion can be a legitimate immediate EOS. Do
    // not duplicate it on another coordinator merely to manufacture text.
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
      msg.includes("No shards") ||
      msg.includes("No model loaded")
    ) {
      return await fallbackToCoordinator();
    }
    throw err;
  }
}

export function Inference() {
  const lookupHash = useAppStore((s) => s.lookupHash);
  const [prompt, setPrompt] = useState("");
  const [maxTokens, setMaxTokens] = useState(16);
  const [copied, setCopied] = useState<string | null>(null);
  const [receiptPollingExhausted, setReceiptPollingExhausted] = useState(false);
  const receiptPollBudget = useRef({ key: "", attempts: 0, startedAt: 0 });
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
  const communityWorker = run.data?.routedVia?.startsWith("community:")
    ? run.data.routedVia.slice("community:".length)
    : null;
  const settlement = run.data?.settlement;
  const isCommunityRewardTx = Boolean(
    settlement?.submitted === true &&
    settlement.txType === "0x25" &&
    ARC_HASH_RE.test(settlement.txHash),
  );
  const exactCommunityReceiptUrl = settlement?.txHash
    ? `/community/reward_receipt/${settlement.txHash}`
    : "";
  const receiptExpectation =
    run.isSuccess &&
    settlement?.submitted === true &&
    settlement.txType === "0x25" &&
    ARC_HASH_RE.test(settlement.txHash) &&
    ARC_HASH_RE.test(settlement.jobId) &&
    ARC_HASH_RE.test(settlement.worker) &&
    settlement.worker === communityWorker &&
    settlement.receiptUrl === exactCommunityReceiptUrl &&
    typeof run.data?.coordinator === "string" &&
    run.data.coordinator.length > 0
      ? {
          sourceHost: run.data.coordinator,
          txHash: settlement.txHash,
          jobId: settlement.jobId,
          worker: settlement.worker,
          receiptUrl: settlement.receiptUrl,
        }
      : null;
  const receiptPollKey = receiptExpectation
    ? [
        receiptExpectation.sourceHost,
        receiptExpectation.txHash,
        receiptExpectation.jobId,
        receiptExpectation.worker,
        receiptExpectation.receiptUrl,
      ].join("|")
    : "";
  useEffect(() => {
    receiptPollBudget.current = {
      key: receiptPollKey,
      attempts: 0,
      startedAt: Date.now(),
    };
    setReceiptPollingExhausted(false);
  }, [receiptPollKey]);
  const rewardReceipt = useQuery<InferenceSettlement, Error>({
    queryKey: [
      "community-reward-receipt",
      receiptExpectation?.sourceHost ?? "",
      receiptExpectation?.txHash ?? "",
      receiptExpectation?.jobId ?? "",
      receiptExpectation?.worker ?? "",
      receiptExpectation?.receiptUrl ?? "",
    ],
    queryFn: async () => {
      if (!receiptExpectation) {
        throw new Error("reward receipt expectation is unavailable");
      }
      if (receiptPollBudget.current.key !== receiptPollKey) {
        receiptPollBudget.current = {
          key: receiptPollKey,
          attempts: 0,
          startedAt: Date.now(),
        };
      }
      receiptPollBudget.current.attempts += 1;
      try {
        const receipt = await api.fetchCommunityRewardReceipt(
          receiptExpectation.sourceHost,
          receiptExpectation.txHash,
          receiptExpectation.jobId,
          receiptExpectation.worker,
          receiptExpectation.receiptUrl,
        );
        if (
          receipt.status === "pending_mined_receipt" &&
          (receiptPollBudget.current.attempts >= MAX_RECEIPT_POLLS ||
            Date.now() - receiptPollBudget.current.startedAt >=
              MAX_RECEIPT_POLL_MS)
        ) {
          setReceiptPollingExhausted(true);
        }
        return receipt;
      } catch (error) {
        if (
          receiptPollBudget.current.attempts >= MAX_RECEIPT_POLLS ||
          Date.now() - receiptPollBudget.current.startedAt >=
            MAX_RECEIPT_POLL_MS
        ) {
          setReceiptPollingExhausted(true);
        }
        throw error;
      }
    },
    enabled: receiptExpectation !== null,
    retry: false,
    refetchInterval: (query) => {
      const status = query.state.data?.status;
      return status === "mined_success" ||
        status === "mined_failed" ||
        status === "receipt_unavailable" ||
        receiptPollingExhausted
        ? false
        : 3_000;
    },
  });
  const independentReceiptMatchesExpectation = Boolean(
    receiptExpectation &&
    rewardReceipt.data?.txType === "0x25" &&
    rewardReceipt.data.txHash === receiptExpectation.txHash &&
    rewardReceipt.data.jobId === receiptExpectation.jobId &&
    rewardReceipt.data.worker === receiptExpectation.worker &&
    rewardReceipt.data.receiptUrl === receiptExpectation.receiptUrl,
  );
  const independentInclusionEvidence = Boolean(
    rewardReceipt.data &&
    Number.isSafeInteger(rewardReceipt.data.blockHeight) &&
    (rewardReceipt.data.blockHeight ?? -1) >= 0 &&
    ARC_HASH_RE.test(rewardReceipt.data.blockHash ?? "") &&
    Number.isSafeInteger(rewardReceipt.data.index) &&
    (rewardReceipt.data.index ?? -1) >= 0,
  );
  const confirmedCommunityReward = Boolean(
    independentReceiptMatchesExpectation &&
    rewardReceipt.data?.status === "mined_success" &&
    rewardReceipt.data.submitted &&
    rewardReceipt.data.included &&
    rewardReceipt.data.confirmed &&
    rewardReceipt.data.success === true &&
    rewardReceipt.data.rewardBase === 2_500_000_000 &&
    rewardReceipt.data.rewardArc === COMMUNITY_REWARD_ARC &&
    independentInclusionEvidence,
  );
  const minedFailedCommunityReward = Boolean(
    independentReceiptMatchesExpectation &&
    rewardReceipt.data?.status === "mined_failed" &&
    rewardReceipt.data.submitted &&
    rewardReceipt.data.included &&
    !rewardReceipt.data.confirmed &&
    rewardReceipt.data.success === false &&
    rewardReceipt.data.rewardBase === null &&
    rewardReceipt.data.rewardArc === null &&
    independentInclusionEvidence,
  );
  const canonicalReceiptUnavailable = Boolean(
    independentReceiptMatchesExpectation &&
    rewardReceipt.data?.status === "receipt_unavailable",
  );
  const confirmedCommunityRewardAmount =
    rewardReceipt.data?.rewardArc == null
      ? "Amount not reported"
      : `${rewardReceipt.data.rewardArc} ARC`;
  let communitySettlementMessage: string;
  if (confirmedCommunityReward) {
    communitySettlementMessage = `${confirmedCommunityRewardAmount} confirmed for the serving worker by an independently fetched successful mined 0x25 receipt`;
  } else if (settlement?.txType && settlement.txType !== "0x25") {
    communitySettlementMessage = `unrecognized settlement type ${settlement.txType}; no community reward credited`;
  } else if (settlement && !settlement.submitted) {
    communitySettlementMessage = `No 0x25 was submitted (${settlement.status.replaceAll("_", " ")}); no ARC reward can be confirmed`;
  } else if (minedFailedCommunityReward) {
    communitySettlementMessage =
      "0x25 receipt was mined but failed; no ARC reward was earned";
  } else if (canonicalReceiptUnavailable || rewardReceipt.isError) {
    communitySettlementMessage =
      "Reward receipt unavailable; ARC earnings cannot be confirmed";
  } else if (settlement?.submitted === true && !receiptExpectation) {
    communitySettlementMessage =
      "Reward receipt unavailable; the submission did not bind the exact transaction, job, worker, receipt URL, and serving host";
  } else if (receiptExpectation && receiptPollingExhausted) {
    communitySettlementMessage =
      "0x25 is still pending after bounded receipt checks; no ARC reward is confirmed";
  } else if (receiptExpectation) {
    communitySettlementMessage =
      "0x25 submitted; waiting for an independently fetched mined receipt";
  } else {
    communitySettlementMessage = `no confirmed 0x25 reward (${settlement?.status?.replaceAll("_", " ") ?? "no settlement"}); inference verification is separate from payment`;
  }

  let displayedReceiptStatus =
    rewardReceipt.data?.status ?? "pending_mined_receipt";
  if (confirmedCommunityReward) {
    displayedReceiptStatus = "mined_success";
  } else if (minedFailedCommunityReward) {
    displayedReceiptStatus = "mined_failed";
  } else if (settlement && !settlement.submitted) {
    displayedReceiptStatus = settlement.status;
  } else if (
    canonicalReceiptUnavailable ||
    rewardReceipt.isError ||
    (settlement && !receiptExpectation)
  ) {
    displayedReceiptStatus = "receipt_unavailable";
  }

  return (
    <div className="main-inner" data-testid="inference-screen">
      <div className="page-header">
        <div>
          <h1 className="page-title">Inference</h1>
          <p className="page-subtitle">
            Submit a prompt and inspect who served it, which agreement evidence
            came back, and whether any computation claim reached this host. An
            inference claim is not a community reward.
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
                  has no compatible model loaded, it falls back to a reachable
                  coordinator&rsquo;s community-first <code>/inference/run</code>
                  route. Either way the response below says where the compute
                  ran and what the coordinator actually verified.
                </p>
                <p>
                  1. Attempts the prompt on the selected execution path. A
                  trace shows shard hops only when the coordinator reports one.
                </p>
                <p>
                  2. Returns the reported output commitment and model ID. On
                  the protocol-v3 path the model ID hashes every artifact byte.
                  Older nodes may report only a shape-derived ID, which is not
                  exact artifact identity.
                </p>
                <p>
                  3. The serving coordinator may submit an{" "}
                  <code>InferenceAttestation</code> (<code>0x16</code>) with{" "}
                  <code>(input_hash, output_hash, model_hash)</code>. It is a
                  computation claim, not a payment or proof of correctness.
                </p>
                <p>
                  4. If a claim hash is returned, the in-app lookup can confirm
                  whether this host mined it successfully. Community payment is
                  a separate <code>0x25</code> transaction and is never inferred
                  from this result.
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
              data-testid="inference-model-policy"
            >
              model identity: reported with response
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
          <div
            role="status"
            data-testid="paid-mode-unavailable"
            style={{
              display: "inline-flex",
              alignItems: "flex-start",
              gap: 8,
              maxWidth: 470,
              padding: "8px 10px",
              border: "1px solid rgba(229, 168, 79, 0.28)",
              borderRadius: "var(--radius-sm)",
              background: "rgba(229, 168, 79, 0.07)",
              color: "var(--text-secondary)",
              fontSize: "var(--text-xs)",
              lineHeight: 1.5,
            }}
          >
            <Coins size={15} style={{ flexShrink: 0, marginTop: 1 }} />
            <span>
              <strong>Prompts are free; worker rewards are separate.</strong>{" "}
              This app does not sign or submit a paid requester escrow. A
              coordinator may still assign the prompt to an eligible community
              worker and return a validator-authorized <code>0x25</code> reward
              transaction for that worker. It is pending until the selected
              chain host reports a successful mined receipt; the person
              submitting the prompt is neither charged nor rewarded. VRF or
              replica selection alone is not payment approval.
            </span>
          </div>
          <div style={{ flex: 1, minWidth: "var(--space-3)" }} />
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
                <Zap size={14} style={{ color: "var(--text-muted)" }} />
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
              {communityWorker ? (
                <>
                  Computed by{" "}
                  <strong data-testid="inference-community-worker">
                    community worker {formatHash(communityWorker, 12)}
                  </strong>{" "}
                  via{" "}
                  <strong data-testid="inference-coordinator">
                    {run.data.servedLocally
                      ? "your node"
                      : run.data.coordinator
                        ? hostLabel(run.data.coordinator)
                        : "the network"}
                  </strong>
                </>
              ) : (
                <>
                  Served by{" "}
                  <strong data-testid="inference-coordinator">
                    {run.data.servedLocally
                      ? "your node"
                      : run.data.coordinator
                        ? hostLabel(run.data.coordinator)
                        : "the network"}
                  </strong>
                </>
              )}
              {run.data.consensus ? (
                <>
                  {" "}· coordinator reports k={run.data.consensus.k} ·{" "}
                  {run.data.consensus.unanimous}/
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
              ) : run.data.quorumVerified && communityWorker ? (
                <> · independently checked with authenticated 2-of-3 range quorums</>
              ) : (
                <> · no independent replica-agreement evidence returned</>
              )}
              {run.data.profileBound
                ? " · exact execution profile bound"
                : " · execution profile not proven"}
              {run.data.quorumVerified
                ? " · authenticated quorum verified"
                : " · quorum not verified"}
            </span>
          </div>

          {run.data.settlement && (
            <div
              data-testid="community-settlement"
              data-receipt-status={displayedReceiptStatus}
              style={{
                display: "flex",
                alignItems: "center",
                gap: "var(--space-3)",
                padding: "var(--space-3) var(--space-4)",
                marginBottom: "var(--space-4)",
                border: `1px solid ${
                  confirmedCommunityReward
                    ? "rgba(80, 200, 120, 0.3)"
                    : "var(--border)"
                }`,
                borderRadius: "var(--radius-sm)",
                background: confirmedCommunityReward
                  ? "var(--success-bg, rgba(80, 200, 120, 0.08))"
                  : "var(--bg)",
                fontSize: "var(--text-sm)",
              }}
            >
              <Coins size={14} style={{ color: "var(--accent)" }} />
              <span>
                <strong>Community reward:</strong>{" "}
                {communitySettlementMessage}
              </span>
            </div>
          )}

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
                label="0x16 claim tx (unpaid)"
                value={run.data.txHash}
                copied={copied === "tx"}
                onCopy={() => copy("tx", run.data!.txHash)}
                icon={Sparkles}
              />
            )}
            {isCommunityRewardTx && run.data.settlement?.txHash && (
              <HashRow
                label="0x25 reward tx"
                value={run.data.settlement.txHash}
                copied={copied === "reward"}
                onCopy={() => copy("reward", run.data!.settlement!.txHash)}
                icon={Coins}
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
                label="Reported model ID"
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
              {run.data.deterministic && "· serving host reports deterministic"}
            </span>
            {/* Was openExternal to `http://140.82.16.112:3200<explorerUrl>`:
                a hardcoded LAX IP, on a page that is a network dashboard
                rather than a block explorer, for a chain that is usually not
                the one this session is pinned to. The in-app lookup resolves
                the hash against the pinned host, which is the only place it
                can honestly be confirmed — including telling the user it is
                not in a block yet. */}
            <span style={{ display: "inline-flex", gap: "var(--space-2)" }}>
              {isCommunityRewardTx && run.data.settlement?.txHash && (
                <button
                  className="btn btn-ghost btn-sm"
                  onClick={() => lookupHash(run.data!.settlement!.txHash)}
                  data-testid="btn-lookup-reward"
                >
                  Track reward receipt <Search size={12} />
                </button>
              )}
              {run.data.txHash && (
                <button
                  className="btn btn-ghost btn-sm"
                  onClick={() => lookupHash(run.data!.txHash)}
                  data-testid="btn-lookup-tx"
                >
                  Look up this claim <Search size={12} />
                </button>
              )}
            </span>
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
