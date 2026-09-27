// Native paid requests: prepare a prompt, sign and submit a
// paid request from this wallet, follow it on chain, and see what it cost.
//
// Everything past "submitted" comes from the request's receipt on the pinned
// chain host (see lib/native-request.ts). Signing happens in Rust
// (src-tauri/src/native_paid.rs); this screen passes only the prompt, the
// price choices and public identifiers. Cancellation follows finality: an
// unsigned request can be withdrawn; a signed one runs to finalized,
// dropped, or refunded, and a refund after expiry is a real transaction.

import { useQuery } from "@tanstack/react-query";
import { Coins, Loader2, RotateCcw, Send, Undo2 } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { Card, CardHeader } from "./Card";
import { api } from "../lib/tauri";
import { formatHash } from "../lib/format";
import { useAppStore } from "../lib/store";
import {
  canClaimRefund,
  createNativeRequest,
  nextToSign,
  phaseLabel,
  reduce,
  reservedInEscrow,
  restoreFromJournal,
  RETRY_ATTEMPT_LIMIT,
  submitEvents,
  TERMINAL_PHASES,
  type NativeRequestEvent,
  type NativeRequestState,
  type PreparedInput,
} from "../lib/native-request";

const POLL_MS = 1_500;
/** While a request is not admitted, resend its identical signed bytes this often. */
const RESUBMIT_EVERY_POLLS = 8;
const BASE_UNITS = 1_000_000_000;
/** The wallet core's answer when a journaled transaction's nonce is used or it expired. */
const NO_LONGER_ADMISSIBLE = /can no longer be admitted/;
/** The node's own validation refusals: the only final ones (as the wallet core decides). */
const FINAL_REFUSAL_STATUSES = new Set([400, 413, 422]);

interface Draft {
  prompt: string;
  prepared: PreparedInput;
  maxTokens: number;
  priceArc: string;
  reserveArc: string;
  expiryBlocks: number;
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function arc(base: number | undefined): string {
  if (base === undefined) return "-";
  const whole = Math.floor(base / BASE_UNITS);
  const fraction = base % BASE_UNITS;
  return fraction === 0
    ? `${whole}`
    : `${whole}.${`${fraction}`.padStart(9, "0").replace(/0+$/, "")}`;
}

function accountKey(address: string | undefined): string | undefined {
  return address?.trim().replace(/^0x/i, "").toLowerCase() || undefined;
}

export function NativePaidRequests() {
  const identity = useAppStore((s) => s.identity);
  const account = accountKey(identity?.address);
  const context = useQuery({
    queryKey: ["native-context"],
    queryFn: api.nativeContext,
    refetchInterval: 15_000,
    retry: false,
  });
  const balance = useQuery({
    queryKey: ["native-balance"],
    queryFn: api.fetchBalance,
    refetchInterval: 3_000,
    enabled: context.data?.trackingAvailable === true,
  });

  const [requests, setRequests] = useState<NativeRequestState[]>([]);
  const requestsRef = useRef(requests);
  requestsRef.current = requests;
  const drafts = useRef(new Map<string, Draft>());
  const polls = useRef(new Map<string, number>());
  const busy = useRef(false);
  const accountRef = useRef(account);
  accountRef.current = account;

  const [prompt, setPrompt] = useState("");
  const [maxTokens, setMaxTokens] = useState(8);
  const [priceArc, setPriceArc] = useState("0.01");
  const [reserveArc, setReserveArc] = useState("0.01");
  const [expiryBlocks, setExpiryBlocks] = useState(2000);
  const [review, setReview] = useState<PreparedInput | null>(null);
  const [formError, setFormError] = useState<string | null>(null);
  const [journalError, setJournalError] = useState<string | null>(null);
  const [preparing, setPreparing] = useState(false);

  const dispatch = useCallback((localId: string, event: NativeRequestEvent) => {
    setRequests((previous) => {
      const next = previous.map((state) =>
        state.localId === localId ? reduce(state, event) : state,
      );
      requestsRef.current = next;
      return next;
    });
  }, []);

  // A restart resumes following every journaled request from its receipt.
  const trackingAvailable = context.data?.trackingAvailable === true;
  useEffect(() => {
    if (!account) return;
    let cancelled = false;
    api
      .nativeJournal()
      .then((entries) => {
        if (cancelled) return;
        const restored = restoreFromJournal(entries, account);
        setRequests((previous) => {
          const known = new Set(previous.map((state) => state.requestId).filter(Boolean));
          const next = [...restored.filter((state) => !known.has(state.requestId)), ...previous];
          requestsRef.current = next;
          return next;
        });
      })
      .catch((error) => {
        // The wallet core refuses to sign while its journal cannot be read
        // (it may list signed requests still in flight), so say why.
        if (!cancelled) setJournalError(errorText(error));
      });
    return () => {
      cancelled = true;
    };
  }, [account]);

  const signAndSubmit = useCallback(
    async (localId: string) => {
      const draft = drafts.current.get(localId);
      if (!draft) {
        dispatch(localId, { type: "sign_started" });
        dispatch(localId, { type: "sign_failed", reason: "its prepared input was lost; queue it again" });
        return;
      }
      dispatch(localId, { type: "sign_started" });
      try {
        const result = await api.nativeSubmit({
          inputHex: draft.prepared.inputHex,
          inputKind: draft.prepared.inputKind,
          promptPreview: draft.prompt,
          maxTokens: draft.maxTokens,
          priceArc: draft.priceArc,
          reserveArc: draft.reserveArc,
          expiryBlocks: draft.expiryBlocks,
        });
        dispatch(localId, {
          type: "signed",
          requestId: result.requestId,
          nonce: result.nonce,
          expiresAt: result.expiresAt,
          txHash: result.txHash,
        });
        dispatch(localId, { type: "submit_started" });
        for (const event of submitEvents(result)) dispatch(localId, event);
        drafts.current.delete(localId);
      } catch (error) {
        dispatch(localId, { type: "sign_failed", reason: errorText(error) });
      }
    },
    [dispatch],
  );

  const follow = useCallback(
    async (state: NativeRequestState) => {
      if (!state.requestId) return;
      const count = (polls.current.get(state.localId) ?? 0) + 1;
      polls.current.set(state.localId, count);
      // Delivery unknown or refused for now: send the identical bytes again,
      // at every poll for the first few attempts and then with backoff.
      const due = state.attempts < RETRY_ATTEMPT_LIMIT || count % RESUBMIT_EVERY_POLLS === 0;
      if (state.phase === "submitting" && state.txHash && due) {
        dispatch(state.localId, { type: "submit_started" });
        try {
          const result = await api.nativeResubmit(state.txHash);
          for (const event of submitEvents(result)) dispatch(state.localId, event);
        } catch (error) {
          const message = errorText(error);
          if (NO_LONGER_ADMISSIBLE.test(message)) {
            // Its nonce is used or it expired: only the receipt can say more.
            dispatch(state.localId, { type: "submit_refused", status: 409, body: "" });
          } else {
            dispatch(state.localId, { type: "network_error", message });
          }
        }
      }
      let found = false;
      try {
        const view = await api.nativeReceipt(state.requestId);
        found = view.found;
        dispatch(state.localId, {
          type: "receipt",
          receipt: view.found ? view.receipt : null,
          height: view.height,
        });
      } catch (error) {
        dispatch(state.localId, { type: "network_error", message: errorText(error) });
        return;
      }
      if (!found && state.phase === "submitted" && state.txHash && count % RESUBMIT_EVERY_POLLS === 0) {
        // A node that restarted may have lost it from its mempool.
        await api.nativeResubmit(state.txHash).catch(() => undefined);
      }
      if (state.phase === "refund_submitted" && state.refundTxHash && count % RESUBMIT_EVERY_POLLS === 0) {
        // A refund can be dropped from a mempool too. Resend its identical
        // bytes; once they can no longer be admitted while the request is
        // still pending, it can be claimed again.
        try {
          await api.nativeResubmit(state.refundTxHash);
        } catch (error) {
          const message = errorText(error);
          if (NO_LONGER_ADMISSIBLE.test(message)) {
            dispatch(state.localId, {
              type: "refund_refused",
              reason: "the refund transaction was not admitted; claim it again",
            });
          }
        }
      }
    },
    [dispatch],
  );

  useEffect(() => {
    if (!trackingAvailable) return;
    const tick = async () => {
      if (busy.current) return;
      busy.current = true;
      try {
        const current = accountRef.current;
        const next = context.data?.compatible && current
          ? nextToSign(requestsRef.current, current)
          : undefined;
        if (next) await signAndSubmit(next);
        for (const state of requestsRef.current) {
          if (!TERMINAL_PHASES.has(state.phase) && state.phase !== "waiting") {
            await follow(state);
          }
        }
      } finally {
        busy.current = false;
      }
    };
    const timer = window.setInterval(() => void tick(), POLL_MS);
    void tick();
    return () => window.clearInterval(timer);
  }, [trackingAvailable, context.data?.compatible, follow, signAndSubmit]);

  const prepare = async () => {
    if (!context.data?.compatible) {
      setFormError(context.data?.reason ?? "New native requests are not currently available.");
      return;
    }
    setFormError(null);
    setReview(null);
    setPreparing(true);
    try {
      setReview(await api.nativePrepare(prompt));
    } catch (error) {
      setFormError(errorText(error));
    } finally {
      setPreparing(false);
    }
  };

  const queue = () => {
    if (!context.data?.compatible || !review || !account) return;
    const localId = `local-${Date.now()}-${Math.random().toString(16).slice(2)}`;
    drafts.current.set(localId, {
      prompt,
      prepared: review,
      maxTokens,
      priceArc,
      reserveArc,
      expiryBlocks,
    });
    setRequests((previous) => {
      const next = [...previous, createNativeRequest(localId, account, prompt.slice(0, 120))];
      requestsRef.current = next;
      return next;
    });
    setReview(null);
    setPrompt("");
  };

  /** Queue a request again that was refused before anything was signed. */
  const requeue = (state: NativeRequestState) => {
    const draft = drafts.current.get(state.localId);
    if (!context.data?.compatible || !draft || !account) return;
    const localId = `local-${Date.now()}-${Math.random().toString(16).slice(2)}`;
    drafts.current.set(localId, draft);
    drafts.current.delete(state.localId);
    setRequests((previous) => {
      const next = [...previous, createNativeRequest(localId, account, draft.prompt.slice(0, 120))];
      requestsRef.current = next;
      return next;
    });
  };

  const withdraw = (localId: string) => {
    drafts.current.delete(localId);
    dispatch(localId, { type: "withdraw" });
  };

  const claimRefund = async (state: NativeRequestState) => {
    if (!state.requestId || !canClaimRefund(requestsRef.current, state.localId)) return;
    dispatch(state.localId, { type: "refund_started" });
    try {
      const result = await api.nativeRefund(state.requestId);
      const final = result.httpStatus !== null && FINAL_REFUSAL_STATUSES.has(result.httpStatus);
      if (result.accepted || !final) {
        // Accepted, or signed and journaled with delivery unknown or refused
        // for now (busy, rate-limited, "already have it"): keep its handle,
        // so it is resent and "Resend refund" works. The wallet keeps these
        // bytes and signs nothing else for this account until they are
        // admitted or can no longer be.
        dispatch(state.localId, { type: "refund_ok", txHash: result.txHash });
        if (result.networkError !== null) {
          dispatch(state.localId, { type: "network_error", message: result.networkError });
        }
      } else {
        dispatch(state.localId, {
          type: "refund_refused",
          reason: result.body || `the node refused the refund (HTTP ${result.httpStatus})`,
        });
      }
    } catch (error) {
      dispatch(state.localId, { type: "refund_refused", reason: errorText(error) });
    }
  };

  const resubmitRefund = async (state: NativeRequestState) => {
    if (!state.refundTxHash) return;
    try {
      const result = await api.nativeResubmit(state.refundTxHash);
      if (!result.accepted && result.httpStatus !== 409 && result.networkError === null) {
        dispatch(state.localId, {
          type: "refund_refused",
          reason: result.body || `the node refused the refund (HTTP ${result.httpStatus})`,
        });
      }
    } catch (error) {
      // Its nonce was used or its hold lapsed: claim again if still pending.
      dispatch(state.localId, { type: "refund_refused", reason: errorText(error) });
    }
  };

  if (context.isLoading || context.data === null || context.data === undefined) {
    // No native-inference context yet, or still asking the host.
    return null;
  }
  const view = context.data;

  const header = (
    <CardHeader
      title={
        <span style={{ display: "inline-flex", alignItems: "center", gap: 8 }}>
          <Coins size={16} /> Native paid requests
        </span>
      }
    />
  );

  const escrow = account ? reservedInEscrow(requests, account) : 0;
  const positionLimit = view.serving?.maxPositions ?? null;
  const overPositions =
    review && review.tokenCount !== null && positionLimit !== null && 1 + review.tokenCount + maxTokens > positionLimit
      ? { needed: 1 + review.tokenCount + maxTokens, limit: positionLimit }
      : null;
  const executor = view.serving?.executor === "deterministic_test"
    ? "deterministic TEST executor"
    : view.serving?.executor ?? "unavailable";
  const visible = [...requests].reverse();

  return (
    <Card data-testid="native-paid-card" style={{ marginBottom: "var(--space-6)" }}>
      {header}
      <div
        role="status"
        data-testid="native-chain-status"
        style={{ fontSize: "var(--text-sm)", color: "var(--text-secondary)", marginBottom: "var(--space-3)" }}
      >
        Chain at block {view.height} · {view.members} validators · executor: {executor}
        {view.nodeVersion ? ` · node ${view.nodeVersion}` : ""}
      </div>
      {!view.compatible ? (
        <div role="status" data-testid="native-context-status" style={{ fontSize: "var(--text-sm)", marginBottom: "var(--space-3)" }}>
          <strong>New paid requests are unavailable.</strong> {view.reason}
          <div style={{ color: "var(--text-muted)", marginTop: 4 }}>
            {view.trackingAvailable
              ? "Existing signed requests continue to be tracked and can be refunded or resubmitted."
              : "Existing signed requests remain journaled locally, but this host context prevents receipt tracking and refund operations."}
          </div>
        </div>
      ) : null}
      <div data-testid="native-balance" style={{ fontSize: "var(--text-sm)", marginBottom: "var(--space-3)" }}>
        Available:{" "}
        <strong data-testid="native-available">
          {balance.data ? `${balance.data.balanceArc} ARC` : "-"}
        </strong>
        {" · "}
        <span data-testid="native-reserved">Reserved in escrow: {arc(escrow)} ARC</span>
      </div>

      {journalError ? (
        <div role="alert" data-testid="native-journal-error" style={{ color: "var(--danger)", marginBottom: "var(--space-3)" }}>
          {journalError}
        </div>
      ) : null}
      <label style={{ display: "flex", flexDirection: "column", gap: 4 }}>
        <span className="field-label">Prompt</span>
        <textarea
          className="input"
          rows={3}
          value={prompt}
          onChange={(event) => {
            setPrompt(event.target.value);
            setReview(null);
          }}
          data-testid="native-prompt"
        />
      </label>
      <div style={{ display: "flex", flexWrap: "wrap", gap: "var(--space-3)", marginTop: "var(--space-3)" }}>
        <label style={{ display: "flex", flexDirection: "column", gap: 4, flex: "0 0 110px" }}>
          <span className="field-label">Max tokens</span>
          <input
            className="input input-mono"
            type="number"
            min={1}
            max={view.maxTokens ?? undefined}
            value={maxTokens}
            onChange={(event) => setMaxTokens(parseInt(event.target.value, 10) || 0)}
            data-testid="native-max-tokens"
          />
        </label>
        <label style={{ display: "flex", flexDirection: "column", gap: 4, flex: "0 0 130px" }}>
          <span className="field-label">Price (ARC)</span>
          <input
            className="input input-mono"
            value={priceArc}
            onChange={(event) => setPriceArc(event.target.value)}
            data-testid="native-price"
          />
        </label>
        <label style={{ display: "flex", flexDirection: "column", gap: 4, flex: "0 0 130px" }}>
          <span className="field-label">Reserve (ARC)</span>
          <input
            className="input input-mono"
            value={reserveArc}
            onChange={(event) => setReserveArc(event.target.value)}
            data-testid="native-reserve"
          />
        </label>
        <label style={{ display: "flex", flexDirection: "column", gap: 4, flex: "0 0 130px" }}>
          <span className="field-label">Expires after (blocks)</span>
          <input
            className="input input-mono"
            type="number"
            min={30}
            value={expiryBlocks}
            onChange={(event) => setExpiryBlocks(parseInt(event.target.value, 10) || 0)}
            data-testid="native-expiry"
          />
        </label>
        <div style={{ flex: 1 }} />
        <button
          className="btn btn-secondary"
          onClick={prepare}
          disabled={!view.compatible || preparing || !prompt.trim()}
          data-testid="btn-native-review"
          style={{ alignSelf: "flex-end" }}
        >
          {preparing ? <Loader2 size={14} style={{ animation: "spin 1s linear infinite" }} /> : null} Review
        </button>
      </div>
      {formError ? (
        <div role="alert" data-testid="native-form-error" style={{ color: "var(--danger)", marginTop: 8 }}>
          {formError}
        </div>
      ) : null}

      {review ? (
        <div
          data-testid="native-review"
          style={{
            marginTop: "var(--space-3)",
            padding: "10px 12px",
            border: "1px solid var(--border)",
            borderRadius: "var(--radius-sm)",
            fontSize: "var(--text-sm)",
          }}
        >
          <div>
            You sign {review.tokenCount !== null ? `${review.tokenCount} token ids` : `${review.byteLen} bytes`}.{" "}
            {review.note}
          </div>
          <div style={{ marginTop: 6 }}>
            {reserveArc} ARC leaves your balance when a block admits the request. If validators
            certify an output, {priceArc} ARC pays them and the rest comes back. If no block admits
            it within {expiryBlocks} blocks, nothing is charged. If it is admitted but not certified
            by then, you claim the reservation back. A signed request cannot be recalled.
          </div>
          {overPositions !== null ? (
            <div role="alert" data-testid="native-positions-error" style={{ color: "var(--danger)", marginTop: 6 }}>
              This request needs {overPositions.needed} positions (1 + {review.tokenCount} prompt tokens +{" "}
              {maxTokens}); the host&apos;s executor holds at most {overPositions.limit}, so it would never
              vote on it. Lower max tokens or shorten the prompt.
            </div>
          ) : null}
          <button
            className="btn btn-primary"
            onClick={queue}
            style={{ marginTop: 8 }}
            disabled={!view.compatible || overPositions !== null}
            data-testid="btn-native-sign"
          >
            <Send size={14} /> Sign and submit
          </button>
        </div>
      ) : null}

      {visible.length > 0 ? (
        <ul style={{ listStyle: "none", padding: 0, margin: "var(--space-4) 0 0", display: "grid", gap: 8 }}>
          {visible.map((state) => (
            <li
              key={state.localId}
              data-testid="native-request-row"
              data-phase={state.phase}
              style={{
                padding: "8px 10px",
                border: "1px solid var(--border)",
                borderRadius: "var(--radius-sm)",
                fontSize: "var(--text-sm)",
              }}
            >
              <div style={{ display: "flex", gap: 8, alignItems: "baseline", flexWrap: "wrap" }}>
                <strong data-testid="native-request-phase">{phaseLabel(state)}</strong>
                <span style={{ color: "var(--text-muted)" }}>{state.promptPreview}</span>
              </div>
              <div className="input-mono" style={{ color: "var(--text-muted)", fontSize: "var(--text-xs)" }}>
                {state.requestId ? `request ${formatHash(state.requestId)}` : "not signed"}
                {state.txHash ? ` · tx ${formatHash(state.txHash)}` : ""}
                {state.expiresAt !== undefined ? ` · expires at block ${state.expiresAt}` : ""}
                {state.admissionHeight !== undefined ? ` · admitted at ${state.admissionHeight}` : ""}
              </div>
              {state.phase === "finalized" ? (
                <div data-testid="native-output" style={{ marginTop: 4 }}>
                  {state.outputText ? (
                    <span>
                      {state.outputText}{" "}
                      <span style={{ color: "var(--text-muted)" }}>
                        (display text from this host's tokenizer; the certificate covers the output
                        bytes {state.outputHex ? `0x${state.outputHex.slice(0, 16)}…` : ""})
                      </span>
                    </span>
                  ) : (
                    <span className="input-mono">output bytes {state.outputHex ?? "-"}</span>
                  )}
                  {view.serving?.executor === "deterministic_test" ? (
                    <span style={{ color: "var(--text-muted)" }}> (test executor output, not an answer)</span>
                  ) : null}
                  <div style={{ color: "var(--text-muted)" }}>
                    Paid {arc(state.executionPrice)} ARC · {state.certificateVotes ?? "-"} validator votes ·
                    settlement {state.reconciled ? "reconciles with the reservation" : "does NOT reconcile"}
                  </div>
                </div>
              ) : null}
              {state.phase === "refunded" ? (
                <div style={{ marginTop: 4, color: "var(--text-muted)" }}>
                  Returned {arc(state.reservedMaxPayment)} ARC
                </div>
              ) : null}
              {state.reason && !TERMINAL_PHASES.has(state.phase) ? (
                <div style={{ marginTop: 4, color: "var(--text-muted)" }}>{state.reason}</div>
              ) : null}
              {state.reason && (state.phase === "rejected" || state.phase === "dropped") ? (
                <div style={{ marginTop: 4, color: "var(--text-muted)" }}>{state.reason}</div>
              ) : null}
              <div style={{ display: "flex", gap: 8, marginTop: 6 }}>
                {state.phase === "waiting" ? (
                  <button className="btn btn-ghost btn-sm" onClick={() => withdraw(state.localId)} data-testid="btn-native-withdraw">
                    <Undo2 size={13} /> Withdraw (not signed yet)
                  </button>
                ) : null}
                {state.phase === "rejected" &&
                state.reason?.startsWith("not signed:") &&
                drafts.current.has(state.localId) ? (
                  <button className="btn btn-ghost btn-sm" onClick={() => requeue(state)} disabled={!view.compatible} data-testid="btn-native-requeue">
                    <RotateCcw size={13} /> Queue again
                  </button>
                ) : null}
                {state.phase === "refund_due" ? (
                  <button
                    className="btn btn-secondary btn-sm"
                    onClick={() => void claimRefund(state)}
                    disabled={!canClaimRefund(requests, state.localId)}
                    data-testid="btn-native-refund"
                  >
                    <RotateCcw size={13} /> Claim {arc(state.reservedMaxPayment)} ARC back
                  </button>
                ) : null}
                {state.phase === "refund_submitted" ? (
                  <button
                    className="btn btn-ghost btn-sm"
                    onClick={() => void resubmitRefund(state)}
                    data-testid="btn-native-resubmit-refund"
                  >
                    Resend refund
                  </button>
                ) : null}
              </div>
            </li>
          ))}
        </ul>
      ) : null}
      <div style={{ marginTop: "var(--space-3)", fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
        Queued requests are signed one at a time and are not kept if the app closes before
        signing. Signed requests are recorded on this device and followed again after a restart.
      </div>
    </Card>
  );
}
