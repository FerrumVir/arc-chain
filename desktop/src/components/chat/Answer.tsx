// One answer in the Inference thread, with the evidence that came back with it.
//
// The receipt, settlement and quorum logic here is the Inference screen's
// original logic, moved into a per-answer component unchanged, so every answer
// in the thread polls and judges its own receipt exactly as before. Only the
// latest answer carries the canonical test ids; earlier ones carry "-earlier".

import { useQuery } from "@tanstack/react-query";
import { Check, ClipboardCheck, Coins, Copy, Globe, RotateCcw, Search, ShieldCheck, Sparkles, Zap } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api } from "../../lib/tauri";
import { formatHash } from "../../lib/format";
import { hostLabel } from "../../lib/hosts";
import { useAppStore } from "../../lib/store";
import type { InferenceSettlement } from "../../lib/types";
import type { ChatTurn } from "../../lib/chat";
import { Markdown } from "./Markdown";
import { StageTrack, timeOf } from "./Stages";

/** What the answer says about a second computer re-running the work. */
type CheckState = "agreed" | "reported" | "none";

const ARC_HASH_RE = /^0x[0-9a-f]{64}$/;
const COMMUNITY_REWARD_ARC = 2.5;
// Match the operator walkthrough's 180-second settlement budget. A healthy
// inference must not look failed merely because block inclusion took longer
// than the old one-minute UI window.
const MAX_RECEIPT_POLLS = 61;
const MAX_RECEIPT_POLL_MS = 180_000;

export function Answer({
  turn,
  latest,
  earlier,
  onAskAgain,
  onFacts,
}: {
  turn: ChatTurn;
  latest: boolean;
  /** An earlier answer to the same prompt and token budget, for comparing fingerprints. */
  earlier: ChatTurn | null;
  onAskAgain: () => void;
  /** Told what this answer has shown about being checked and recorded, whenever that changes (for the chat aqueduct). */
  onFacts?: (facts: { agreed: boolean; height: number | null }) => void;
}) {
  const result = turn.result!;
  const tid = (name: string) => (latest ? name : `${name}-earlier`);
  const lookupHash = useAppStore((s) => s.lookupHash);
  const [copied, setCopied] = useState<string | null>(null);
  const [receiptPollingExhausted, setReceiptPollingExhausted] = useState(false);
  const receiptPollBudget = useRef({ key: "", attempts: 0, startedAt: 0 });
  const copy = async (key: string, value: string) => {
    await navigator.clipboard.writeText(value);
    setCopied(key);
    setTimeout(() => setCopied(null), 1200);
  };

  const communityWorker = result.routedVia?.startsWith("community:") ? result.routedVia.slice("community:".length) : null;
  // A quorum flag without an exact execution-profile binding is incomplete
  // evidence: it cannot establish that every vote covered the same model and
  // protocol profile. Keep the result visible, but do not promote it to
  // authenticated consensus in the product copy.
  const authenticatedQuorum = Boolean(result.quorumVerified === true && result.profileBound === true);
  const settlement = result.settlement;
  const isCommunityRewardTx = Boolean(settlement?.submitted === true && settlement.txType === "0x25" && ARC_HASH_RE.test(settlement.txHash));
  const exactCommunityReceiptUrl = settlement?.txHash ? `/community/reward_receipt/${settlement.txHash}` : "";
  const receiptExpectation =
    settlement?.submitted === true &&
    settlement.txType === "0x25" &&
    ARC_HASH_RE.test(settlement.txHash) &&
    ARC_HASH_RE.test(settlement.jobId) &&
    ARC_HASH_RE.test(settlement.worker) &&
    settlement.worker === communityWorker &&
    settlement.receiptUrl === exactCommunityReceiptUrl &&
    typeof result.coordinator === "string" &&
    result.coordinator.length > 0
      ? { sourceHost: result.coordinator, txHash: settlement.txHash, jobId: settlement.jobId, worker: settlement.worker, receiptUrl: settlement.receiptUrl }
      : null;
  const receiptPollKey = receiptExpectation
    ? [receiptExpectation.sourceHost, receiptExpectation.txHash, receiptExpectation.jobId, receiptExpectation.worker, receiptExpectation.receiptUrl].join("|")
    : "";
  useEffect(() => {
    receiptPollBudget.current = { key: receiptPollKey, attempts: 0, startedAt: Date.now() };
    setReceiptPollingExhausted(false);
  }, [receiptPollKey]);
  const rewardReceipt = useQuery<InferenceSettlement, Error>({
    queryKey: ["community-reward-receipt", receiptExpectation?.sourceHost ?? "", receiptExpectation?.txHash ?? "", receiptExpectation?.jobId ?? "", receiptExpectation?.worker ?? "", receiptExpectation?.receiptUrl ?? ""],
    queryFn: async () => {
      if (!receiptExpectation) throw new Error("reward receipt expectation is unavailable");
      if (receiptPollBudget.current.key !== receiptPollKey) {
        receiptPollBudget.current = { key: receiptPollKey, attempts: 0, startedAt: Date.now() };
      }
      receiptPollBudget.current.attempts += 1;
      try {
        const receipt = await api.fetchCommunityRewardReceipt(receiptExpectation.sourceHost, receiptExpectation.txHash, receiptExpectation.jobId, receiptExpectation.worker, receiptExpectation.receiptUrl);
        if (
          receipt.status === "pending_mined_receipt" &&
          (receiptPollBudget.current.attempts >= MAX_RECEIPT_POLLS || Date.now() - receiptPollBudget.current.startedAt >= MAX_RECEIPT_POLL_MS)
        ) {
          setReceiptPollingExhausted(true);
        }
        return receipt;
      } catch (error) {
        if (receiptPollBudget.current.attempts >= MAX_RECEIPT_POLLS || Date.now() - receiptPollBudget.current.startedAt >= MAX_RECEIPT_POLL_MS) {
          setReceiptPollingExhausted(true);
        }
        throw error;
      }
    },
    enabled: receiptExpectation !== null,
    retry: false,
    refetchInterval: (query) => {
      const status = query.state.data?.status;
      return status === "mined_success" || status === "mined_failed" || status === "receipt_unavailable" || receiptPollingExhausted ? false : 3_000;
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
  const canonicalReceiptUnavailable = Boolean(independentReceiptMatchesExpectation && rewardReceipt.data?.status === "receipt_unavailable");
  const confirmedCommunityRewardAmount = rewardReceipt.data?.rewardArc == null ? "Amount not reported" : `${rewardReceipt.data.rewardArc} ARC`;
  let communitySettlementMessage: string;
  if (confirmedCommunityReward) {
    communitySettlementMessage = `${confirmedCommunityRewardAmount} confirmed for the serving worker by an independently fetched successful mined 0x25 receipt`;
  } else if (settlement?.txType && settlement.txType !== "0x25") {
    communitySettlementMessage = `unrecognized settlement type ${settlement.txType}; no community reward credited`;
  } else if (settlement && !settlement.submitted) {
    communitySettlementMessage = `No 0x25 was submitted (${settlement.status.replaceAll("_", " ")}); no ARC reward can be confirmed`;
  } else if (minedFailedCommunityReward) {
    communitySettlementMessage = "0x25 receipt was mined but failed; no ARC reward was earned";
  } else if (canonicalReceiptUnavailable || rewardReceipt.isError) {
    communitySettlementMessage = "Reward receipt unavailable; ARC earnings cannot be confirmed";
  } else if (settlement?.submitted === true && !receiptExpectation) {
    communitySettlementMessage = "Reward receipt unavailable; the submission did not bind the exact transaction, job, worker, receipt URL, and serving host";
  } else if (receiptExpectation && receiptPollingExhausted) {
    communitySettlementMessage = "0x25 is still pending after bounded receipt checks; no ARC reward is confirmed";
  } else if (receiptExpectation) {
    communitySettlementMessage = "0x25 submitted; waiting for an independently fetched mined receipt";
  } else {
    communitySettlementMessage = `no confirmed 0x25 reward (${settlement?.status?.replaceAll("_", " ") ?? "no settlement"}); inference verification is separate from payment`;
  }

  let displayedReceiptStatus = rewardReceipt.data?.status ?? "pending_mined_receipt";
  if (confirmedCommunityReward) displayedReceiptStatus = "mined_success";
  else if (minedFailedCommunityReward) displayedReceiptStatus = "mined_failed";
  else if (settlement && !settlement.submitted) displayedReceiptStatus = settlement.status;
  else if (canonicalReceiptUnavailable || rewardReceipt.isError || (settlement && !receiptExpectation)) displayedReceiptStatus = "receipt_unavailable";
  const recheckCommunityReward = () => {
    receiptPollBudget.current = { key: receiptPollKey, attempts: 0, startedAt: Date.now() };
    setReceiptPollingExhausted(false);
    void rewardReceipt.refetch();
  };

  // what the chat aqueduct may draw: agreement only when authenticated, a block only from a confirmed mined receipt
  const factHeight = confirmedCommunityReward && rewardReceipt.data?.blockHeight != null ? rewardReceipt.data.blockHeight : null;
  const onFactsRef = useRef(onFacts);
  onFactsRef.current = onFacts;
  useEffect(() => {
    onFactsRef.current?.({ agreed: authenticatedQuorum, height: factHeight });
  }, [authenticatedQuorum, factHeight]);

  // ---- the plain-language provenance: who served it, whether it was checked, what reached the chain
  const servedBy = result.servedLocally ? "your node" : result.coordinator ? hostLabel(result.coordinator) : "the network";
  const check: CheckState = authenticatedQuorum ? "agreed" : result.consensus ? "reported" : "none";
  const checkLine =
    check === "agreed"
      ? result.consensus
        ? `Replicas agreed: k=${result.consensus.k}, ${result.consensus.unanimous}/${result.consensus.votesTotal} votes, authenticated`
        : "A second computer re-ran it and agreed (authenticated)"
      : check === "reported"
        ? "Agreement reported by the coordinator, not authenticated"
        : "Not checked by a second computer";
  const chainLine = settlement
    ? communitySettlementMessage
    : result.txHash
      ? "Computation claim (0x16) submitted: a claim, not proof or payment"
      : "No on-chain claim came back";
  const same = earlier?.result?.outputHash ? earlier.result.outputHash === result.outputHash : null;
  const output = result.output.trim();

  return (
    <article
      className={`answer${latest ? " answer-latest" : ""}`}
      data-testid={tid("inference-result")}
      data-output-hash={result.outputHash}
      data-model-id={result.modelHash}
      data-routed-via={result.routedVia ?? ""}
      data-coordinator={result.coordinator ?? ""}
      aria-label="Answer"
    >
      <header className="answer-head">
        <span className="answer-mark" aria-hidden="true" />
        <span className="answer-by">
          {communityWorker ? `Community worker ${formatHash(communityWorker, 6)} via ${servedBy}` : `Served by ${servedBy}`}
        </span>
        <span className="answer-stats">
          {result.tokensGenerated} tokens · {result.inferenceMs.toLocaleString()} ms
        </span>
        {turn.detached && <span className="answer-late">arrived after you stopped waiting</span>}
      </header>

      <Markdown text={output || "(empty)"} className={`answer-text${output ? "" : " answer-empty"}`} testId={tid("inference-output")} />

      <div className="answer-tools">
        <button type="button" className="chip-btn" onClick={() => void copy("answer", output)} aria-label="Copy answer">
          {copied === "answer" ? <Check size={13} /> : <Copy size={13} />} {copied === "answer" ? "Copied" : "Copy"}
        </button>
        <button type="button" className="chip-btn" onClick={onAskAgain} aria-label="Ask the same prompt again">
          <RotateCcw size={13} /> Ask again
        </button>
        <span className="answer-fingerprint" title={result.outputHash}>
          fingerprint <code>{formatHash(result.outputHash, 8)}</code>
          {same === true && <span className="fp-same"> · same as your earlier answer</span>}
          {same === false && <span className="fp-diff"> · differs from your earlier answer</span>}
        </span>
      </div>

      <StageTrack
        label="What happened to this prompt"
        stages={[
          { name: "Sent", state: "done", detail: timeOf(turn.sentAt) },
          {
            name: "Served",
            state: "done",
            detail: `${communityWorker ? `community worker via ${servedBy}` : servedBy}${
              result.trace && result.trace.length > 1 ? ` · ${result.trace.length} hops` : ""
            }${turn.settledAt ? ` · ${((turn.settledAt - turn.sentAt) / 1000).toFixed(1)} s` : ""}`,
          },
          { name: "Checked", state: check === "agreed" ? "done" : "open", detail: checkLine },
          { name: "On chain", state: confirmedCommunityReward ? "done" : settlement || result.txHash ? "wait" : "open", detail: chainLine },
        ]}
      />

      <details className="evidence" open={latest || undefined}>
        <summary>Evidence</summary>
        <div className="evidence-line" data-testid={tid("inference-consensus")}>
          <Globe size={14} aria-hidden="true" />
          <span>
            {communityWorker ? (
              <>
                Computed by{" "}
                <strong data-testid={tid("inference-community-worker")}>community worker {formatHash(communityWorker, 12)}</strong>{" "}
                via{" "}
                <strong data-testid={tid("inference-coordinator")}>
                  {result.servedLocally ? "your node" : result.coordinator ? hostLabel(result.coordinator) : "the network"}
                </strong>
              </>
            ) : (
              <>
                Served by{" "}
                <strong data-testid={tid("inference-coordinator")}>
                  {result.servedLocally ? "your node" : result.coordinator ? hostLabel(result.coordinator) : "the network"}
                </strong>
              </>
            )}
            {result.consensus ? (
              <>
                {" "}· coordinator reports k={result.consensus.k} · {result.consensus.unanimous}/{result.consensus.votesTotal}{" "}
                {result.consensus.split === 0 && result.consensus.majority === 0
                  ? "unanimous"
                  : `${result.consensus.majority} majority / ${result.consensus.split} split`}
                {result.consensus.divergentReplicaCount > 0 && (
                  <>
                    {" "}· <span className="evidence-bad">{result.consensus.divergentReplicaCount} divergent</span>
                  </>
                )}
              </>
            ) : authenticatedQuorum && communityWorker ? (
              <> · independently checked with authenticated replica agreement</>
            ) : (
              <> · no independent replica-agreement evidence returned</>
            )}
            {result.profileBound ? " · exact execution profile bound" : " · execution profile not proven"}
            {authenticatedQuorum ? " · authenticated quorum verified" : " · quorum not verified"}
          </span>
        </div>

        {settlement && (
          <div
            className={`evidence-line evidence-settlement${confirmedCommunityReward ? " is-confirmed" : ""}`}
            data-testid={tid("community-settlement")}
            data-receipt-status={displayedReceiptStatus}
            data-tx-type={settlement.txType}
            data-tx-hash={settlement.txHash}
            data-job-id={settlement.jobId}
            data-worker={settlement.worker}
            data-receipt-url={settlement.receiptUrl}
            data-submitted={String(settlement.submitted)}
          >
            <Coins size={14} aria-hidden="true" />
            <span>
              <strong>Community reward:</strong> {communitySettlementMessage}
            </span>
            {receiptExpectation && (receiptPollingExhausted || canonicalReceiptUnavailable || rewardReceipt.isError) && (
              <button type="button" className="btn btn-ghost btn-sm" onClick={recheckCommunityReward} data-testid={tid("btn-recheck-community-reward")}>
                Recheck receipt
              </button>
            )}
          </div>
        )}

        {/* Per-hop pipeline trace, shown only when the serving node reports one: the evidence that the model
            really was split across machines. */}
        {result.trace && result.trace.length > 0 && (
          <div className="trace" data-testid={tid("inference-trace")}>
            <div className="trace-title">Pipeline · {result.trace.length} hops</div>
            {result.trace.map((h) => (
              <div key={h.hop} className="trace-row">
                <span className="trace-hop">{h.hop}</span>
                <span className="trace-node">{h.node}</span>
                <span className="trace-layers">layers {h.layers}</span>
                <span className="trace-ms">{h.computeMs}ms</span>
                {h.isTerminal && <span className="trace-out">output</span>}
              </div>
            ))}
          </div>
        )}

        <div className="hash-rows">
          {result.txHash && <HashRow label="0x16 claim tx (unpaid)" value={result.txHash} copied={copied === "tx"} onCopy={() => void copy("tx", result.txHash)} icon={Sparkles} />}
          {isCommunityRewardTx && settlement?.txHash && (
            <HashRow label="0x25 reward tx" value={settlement.txHash} copied={copied === "reward"} onCopy={() => void copy("reward", settlement.txHash)} icon={Coins} />
          )}
          <HashRow label="Output hash" value={result.outputHash} copied={copied === "out"} onCopy={() => void copy("out", result.outputHash)} icon={Zap} />
          {result.modelHash && <HashRow label="Reported model ID" value={result.modelHash} copied={copied === "model"} onCopy={() => void copy("model", result.modelHash)} icon={ShieldCheck} />}
        </div>

        <div className="evidence-foot">
          <span>
            Engine: {result.engine} {result.deterministic && "· serving host reports deterministic"}
          </span>
          {/* The in-app lookup resolves the hash against the pinned host, the only place it can honestly be
              confirmed, including telling the person it is not in a block yet. */}
          <span className="evidence-links">
            {isCommunityRewardTx && settlement?.txHash && (
              <button className="btn btn-ghost btn-sm" onClick={() => lookupHash(settlement.txHash)} data-testid={tid("btn-lookup-reward")}>
                Track reward receipt <Search size={12} />
              </button>
            )}
            {result.txHash && (
              <button className="btn btn-ghost btn-sm" onClick={() => lookupHash(result.txHash)} data-testid={tid("btn-lookup-tx")}>
                Look up this claim <Search size={12} />
              </button>
            )}
          </span>
        </div>
      </details>
    </article>
  );
}

function HashRow({ label, value, copied, onCopy, icon: Icon }: { label: string; value: string; copied: boolean; onCopy: () => void; icon: typeof Sparkles }) {
  return (
    <div className="hash-row">
      <Icon size={12} aria-hidden="true" />
      <span className="hash-label">{label}</span>
      <code className="hash-value" title={value}>{formatHash(value, 18)}</code>
      <button className="btn btn-ghost btn-sm hash-copy" onClick={onCopy} aria-label={`Copy ${label.toLowerCase()}`}>
        {copied ? <ClipboardCheck size={12} /> : <Copy size={12} />}
      </button>
    </div>
  );
}
