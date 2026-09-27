// Lifecycle of one native paid inference request (protocol 4), as the
// desktop shows it. Pure: no fetch, no timers, no Tauri. The screen feeds it
// what it observed (a signing result, a submit response, a receipt, a lost
// connection) and renders the state it returns.
//
// The chain is the source of truth. A status is never inferred from a local
// output or a transaction hash: only the request's receipt at
// /native-inference/receipt/{request_id} moves a request past "submitted".
//
// Nonces: the node admits only the account's next nonce, so an account has
// at most one transaction that is signed but not yet in a block. Later
// requests wait unsigned ("waiting") and are signed just in time with the
// nonce the node reports, so a dropped request never strands the ones behind
// it. A refund is a transaction too and takes its turn the same way.
//
// Cancellation follows finality. A request that is still "waiting" was never
// signed and can be withdrawn: nothing exists anywhere. A signed request
// cannot be recalled. If no block admits it before its expiry it is dropped
// and nothing is reserved. Once admitted, it either finalizes or, after its
// expiry, its reservation is claimed back with a refund transaction.

export type NativeRequestPhase =
  | "waiting" // queued behind an earlier transaction of the same account; not signed
  | "withdrawn" // removed while still unsigned: nothing was signed or sent
  | "signing"
  | "submitting"
  | "submitted" // in a mempool; not yet admitted into a block
  | "awaiting_certificate" // admitted: funds reserved, validators executing and voting
  | "refund_due" // admitted, expired without a certificate: the reservation can be claimed
  | "refund_submitting"
  | "refund_submitted" // refund transaction accepted by a node; waiting for a block
  | "finalized"
  | "refunded" // the reservation was returned
  | "rejected" // refused as signed; it will never be admitted
  | "dropped"; // accepted by a node but never admitted before expiry; nothing reserved

export const TERMINAL_PHASES: ReadonlySet<NativeRequestPhase> = new Set([
  "withdrawn",
  "finalized",
  "refunded",
  "rejected",
  "dropped",
]);

/** Phases in which one of the account's nonces is taken but not yet consumed. */
const NONCE_IN_FLIGHT: ReadonlySet<NativeRequestPhase> = new Set([
  "signing",
  "submitting",
  "submitted",
  "refund_submitting",
  "refund_submitted",
]);

/** Admitted and not settled: the reservation sits in the request's escrow. */
const RESERVED: ReadonlySet<NativeRequestPhase> = new Set([
  "awaiting_certificate",
  "refund_due",
  "refund_submitting",
  "refund_submitted",
]);

export interface NativeReceipt {
  request_id: string;
  observed_status: string;
  admission_height?: number | null;
  expires_at?: number | null;
  output_hash?: string | null;
  output_hex?: string | null;
  output_text?: string | null;
  certificate_votes?: number | null;
  execution_price?: number;
  reserved_max_payment?: number;
  settlement_credits?: { payee: string; amount: number }[];
}

export interface NativeRequestState {
  localId: string;
  account: string;
  phase: NativeRequestPhase;
  /** What the user asked, for the list only. */
  promptPreview?: string;
  requestId?: string;
  txHash?: string;
  nonce?: number;
  /** The request's signed expiry height. */
  expiresAt?: number;
  executionPrice?: number;
  reservedMaxPayment?: number;
  admissionHeight?: number;
  outputHash?: string;
  /** The certified output bytes, hex. */
  outputHex?: string;
  /** Display text for the output, from the host's tokenizer. Not certified. */
  outputText?: string;
  certificateVotes?: number;
  /** For a settled request: do its credits add up to exactly the reservation? */
  reconciled?: boolean;
  refundTxHash?: string;
  /** Why the request is rejected, dropped, or being retried. */
  reason?: string;
  /** Submissions attempted for this signed transaction. */
  attempts: number;
  /**
   * A copy of this signed transaction may have reached a node: an attempt
   * was accepted, answered "already have it", or failed with delivery
   * unknown. A later refusal of another copy is then never final: only the
   * receipt or the expiry can say what happened.
   */
  mayBeDelivered?: boolean;
  /** Set while the node cannot be reached; the phase is the last one observed. */
  offline: boolean;
}

export type RefusalClass =
  | "wait_for_prior" // a nonce ahead of the account: an earlier transaction is still pending
  | "stale_nonce" // this nonce was consumed: look the request up before concluding anything
  | "already_pending" // the node already holds this exact transaction
  | "retry_elsewhere" // this node cannot propose right now (rebuilding its DAG)
  | "retry_later" // busy or rate limited for now
  | "never"; // refused as signed: bad signature, unfunded, expired, over limits

export interface Refusal {
  kind: RefusalClass;
  reason: string;
}

export type NativeRequestEvent =
  | { type: "withdraw" }
  | { type: "sign_started" }
  /** Signed and journaled; `txHash` lets a resubmission send the same bytes. */
  | { type: "signed"; requestId: string; nonce: number; expiresAt: number; txHash?: string }
  | { type: "sign_failed"; reason: string }
  | { type: "submit_started" }
  | { type: "submit_ok"; txHash: string }
  | { type: "submit_refused"; status: number; body: string }
  | { type: "network_error"; message: string }
  | { type: "connection_restored" }
  /** A receipt read: `receipt` is null when the node answered 404. */
  | { type: "receipt"; receipt: NativeReceipt | null; height: number }
  | { type: "refund_started" }
  | { type: "refund_ok"; txHash: string }
  /** The refund was not submitted or not accepted; the chain still decides. */
  | { type: "refund_refused"; reason: string };

/** Submissions of one signed transaction resent at every poll; later ones back off. */
export const RETRY_ATTEMPT_LIMIT = 5;

const NONCE = /invalid nonce: expected (\d+), got (\d+)/;

export function classifyRefusal(status: number, body: string): Refusal {
  const text = (body || "").trim();
  if (status === 503) {
    if (/rebuilding its DAG/i.test(text)) {
      return { kind: "retry_elsewhere", reason: "the node is rebuilding its DAG; submit to another validator" };
    }
    return { kind: "retry_later", reason: text || "the node cannot accept native requests right now" };
  }
  if (status === 429) {
    return { kind: "retry_later", reason: "the node is rate-limiting this account; retrying" };
  }
  if (status === 409) {
    // The node already holds this exact transaction (a resubmission), or its
    // mempool will not take it. Either way only the receipt can say more.
    return { kind: "already_pending", reason: "the node already holds this transaction" };
  }
  if (status === 400) {
    const nonce = NONCE.exec(text);
    if (nonce) {
      const expected = Number(nonce[1]);
      const got = Number(nonce[2]);
      if (got > expected) {
        return { kind: "wait_for_prior", reason: `an earlier transaction (nonce ${expected}) is not yet in a block` };
      }
      return { kind: "stale_nonce", reason: `nonce ${got} was already used (account is at ${expected})` };
    }
    if (/account not found/i.test(text)) {
      return { kind: "never", reason: "the paying account does not exist on this chain (unfunded)" };
    }
    if (/insufficient balance/i.test(text)) {
      return { kind: "never", reason: text };
    }
    if (/expired/i.test(text)) {
      return { kind: "never", reason: "the request expired before it could be admitted" };
    }
    return { kind: "never", reason: text || "the node refused the request" };
  }
  return { kind: "never", reason: text || `the node refused the request (HTTP ${status})` };
}

export function createNativeRequest(localId: string, account: string, promptPreview?: string): NativeRequestState {
  return { localId, account, phase: "waiting", attempts: 0, offline: false, promptPreview };
}

/**
 * Can the request be refunded in the next block? The chain refunds at the
 * first block whose height is at or past the expiry, and the next block is
 * `height + 1`.
 */
function refundable(expiresAt: number | undefined, height: number): boolean {
  return expiresAt !== undefined && height + 1 >= expiresAt;
}

function settle(state: NativeRequestState, receipt: NativeReceipt): NativeRequestState {
  const credits = receipt.settlement_credits ?? [];
  const credited = credits.reduce((sum, credit) => sum + Number(credit.amount ?? 0), 0);
  const reserved = Number(receipt.reserved_max_payment ?? Number.NaN);
  return {
    ...state,
    admissionHeight: receipt.admission_height ?? state.admissionHeight,
    outputHash: receipt.output_hash ?? undefined,
    outputHex: receipt.output_hex || undefined,
    outputText: receipt.output_text ?? undefined,
    certificateVotes: receipt.certificate_votes ?? undefined,
    reconciled: Number.isFinite(reserved) && credited === reserved,
  };
}

function applyReceipt(
  state: NativeRequestState,
  receipt: NativeReceipt | null,
  height: number,
): NativeRequestState {
  if (receipt === null) {
    // Not admitted (yet). Once the chain is past the request's own expiry it
    // can never be admitted, and nothing was reserved.
    if (
      (state.phase === "submitted" || state.phase === "submitting") &&
      state.expiresAt !== undefined &&
      height >= state.expiresAt
    ) {
      return { ...state, phase: "dropped", reason: "never admitted before its expiry height; nothing was reserved" };
    }
    return state;
  }
  if (state.requestId !== undefined && receipt.request_id.replace(/^0x/, "") !== state.requestId.replace(/^0x/, "")) {
    // A receipt for another request must never move this one.
    return state;
  }
  const expiresAt = receipt.expires_at ?? state.expiresAt;
  const admitted = {
    ...state,
    expiresAt,
    admissionHeight: receipt.admission_height ?? state.admissionHeight,
    executionPrice: receipt.execution_price ?? state.executionPrice,
    reservedMaxPayment: receipt.reserved_max_payment ?? state.reservedMaxPayment,
  };
  switch (receipt.observed_status) {
    case "Pending":
      if (state.phase === "refund_submitting" || state.phase === "refund_submitted") {
        // The refund is not in a block yet; the reservation is still held.
        return admitted;
      }
      if (refundable(expiresAt, height)) {
        return {
          ...admitted,
          phase: "refund_due",
          reason: "expired without a certificate; the reservation stays in escrow until a refund is claimed",
        };
      }
      return { ...admitted, phase: "awaiting_certificate", reason: undefined };
    case "Finalized":
      return { ...settle(admitted, receipt), phase: "finalized", reason: undefined };
    case "Refunded":
      return { ...settle(admitted, receipt), phase: "refunded", reason: "no certificate before expiry; the reservation was returned" };
    default:
      // An unknown chain status is shown as-is, never mapped to a guess.
      return { ...state, reason: `unrecognised chain status ${receipt.observed_status}` };
  }
}

export function reduce(state: NativeRequestState, event: NativeRequestEvent): NativeRequestState {
  if (TERMINAL_PHASES.has(state.phase)) {
    // Terminal states are final; only the connection flag can still change.
    if (event.type === "network_error") return { ...state, offline: true };
    if (event.type === "connection_restored") return { ...state, offline: false };
    return state;
  }
  switch (event.type) {
    case "withdraw":
      // Only an unsigned request can be withdrawn. After signing, the chain's
      // expiry and refund rules are the only way out.
      return state.phase === "waiting"
        ? { ...state, phase: "withdrawn", reason: "withdrawn before signing; nothing was signed or sent" }
        : state;
    case "sign_started":
      return state.phase === "waiting" ? { ...state, phase: "signing" } : state;
    case "signed":
      if (state.phase !== "signing") return state;
      return {
        ...state,
        phase: "submitting",
        requestId: event.requestId,
        nonce: event.nonce,
        expiresAt: event.expiresAt,
        txHash: event.txHash ?? state.txHash,
        attempts: 0,
      };
    case "sign_failed":
      // The wallet refused before signing (unfunded, bounds, an earlier
      // transaction still pending). Nothing was signed, so nothing is in
      // flight; the user can queue the same request again.
      return state.phase === "signing"
        ? { ...state, phase: "rejected", reason: `not signed: ${event.reason}` }
        : state;
    case "submit_started":
      return state.phase === "submitting" ? { ...state, attempts: state.attempts + 1, offline: false } : state;
    case "submit_ok":
      return state.phase === "submitting"
        ? { ...state, phase: "submitted", txHash: event.txHash, reason: undefined, mayBeDelivered: true }
        : state;
    case "submit_refused": {
      if (state.phase !== "submitting" && state.phase !== "submitted") return state;
      const refusal = classifyRefusal(event.status, event.body);
      switch (refusal.kind) {
        case "retry_elsewhere":
        case "retry_later":
          // Transient. These exact signed bytes can still be admitted, and the
          // wallet keeps them (and signs nothing else for this account) until
          // they are in a block or expired, so the request is never shown as
          // rejected here: it stays in flight, is resent with backoff, and is
          // dropped at its expiry with nothing charged.
          return { ...state, reason: refusal.reason };
        case "already_pending":
        case "stale_nonce":
          // The nonce may have been consumed by this very request (an earlier
          // attempt that did get in). Only its receipt can tell; keep polling.
          return { ...state, phase: "submitted", reason: refusal.reason, mayBeDelivered: true };
        case "wait_for_prior":
          if (state.phase === "submitted") return { ...state, reason: refusal.reason };
          // Signed too early: release the nonce and re-sign when this account's
          // earlier transaction is in a block.
          return {
            ...state,
            phase: "waiting",
            requestId: undefined,
            txHash: undefined,
            nonce: undefined,
            expiresAt: undefined,
            reason: refusal.reason,
          };
        case "never":
          // A refusal of one copy says nothing about another copy that may
          // already be in a mempool (a resubmission, or a first attempt whose
          // delivery is unknown): then the receipt or the expiry decides.
          if (state.phase === "submitted" || state.mayBeDelivered) {
            return { ...state, phase: "submitted", reason: refusal.reason };
          }
          return { ...state, phase: "rejected", reason: refusal.reason };
      }
      return state;
    }
    case "network_error":
      // During submission a lost connection leaves delivery unknown.
      return {
        ...state,
        offline: true,
        reason: event.message,
        mayBeDelivered:
          state.mayBeDelivered || state.phase === "submitting" || state.phase === "submitted",
      };
    case "connection_restored":
      return { ...state, offline: false };
    case "receipt":
      return { ...applyReceipt(state, event.receipt, event.height), offline: false };
    case "refund_started":
      return state.phase === "refund_due" ? { ...state, phase: "refund_submitting", reason: undefined } : state;
    case "refund_ok":
      return state.phase === "refund_submitting"
        ? { ...state, phase: "refund_submitted", refundTxHash: event.txHash, reason: undefined }
        : state;
    case "refund_refused":
      return state.phase === "refund_submitting" || state.phase === "refund_submitted"
        ? { ...state, phase: "refund_due", reason: event.reason }
        : state;
  }
  return state;
}

/**
 * Which waiting request of `account` may be signed now, if any: the oldest
 * one, and only when no transaction of that account holds an unconsumed nonce.
 */
export function nextToSign(states: readonly NativeRequestState[], account: string): string | undefined {
  const own = states.filter((state) => state.account === account);
  if (own.some((state) => NONCE_IN_FLIGHT.has(state.phase))) return undefined;
  return own.find((state) => state.phase === "waiting")?.localId;
}

/** May a refund be signed for this request now? Same one-nonce rule. */
export function canClaimRefund(states: readonly NativeRequestState[], localId: string): boolean {
  const state = states.find((candidate) => candidate.localId === localId);
  if (!state || state.phase !== "refund_due") return false;
  return !states.some((other) => other.account === state.account && NONCE_IN_FLIGHT.has(other.phase));
}

/** Base units this account has reserved in escrow for admitted, unsettled requests. */
export function reservedInEscrow(states: readonly NativeRequestState[], account: string): number {
  return states
    .filter((state) => state.account === account && RESERVED.has(state.phase))
    .reduce((sum, state) => sum + (state.reservedMaxPayment ?? 0), 0);
}

// ── IPC shapes of the native commands (src-tauri/src/native_paid.rs) ─────

export interface NativeServingView {
  executor: string;
  inputFormat: string;
  tokenizeEndpoint: boolean;
  tokenizerProfile: string | null;
  /** Most positions (1 + prompt + max tokens) the host's executor holds for one job. */
  maxPositions: number | null;
}

/** The pinned host's native-inference context, or why new requests are unavailable. */
export interface NativeContextView {
  host: string;
  /** False when this app cannot build requests the host's chain admits. */
  compatible: boolean;
  reason: string | null;
  height: number | null;
  members: number | null;
  executions: number | null;
  maxTokens: number | null;
  serving: NativeServingView | null;
  /** "canonical_tokens" or "test_executor_bytes". */
  inputKind: string | null;
  nodeVersion: string | null;
  appContractVersion: number;
  /** Actual chain protocol major (can be 3 for migrated/recovered chains). */
  chainProtocol: number | null;
  /** True only when the chain accepts native transactions exclusively. */
  nativeOnlyChain: boolean | null;
  /** False when admission is closed or the node is too old to advertise it. */
  requestAdmissionOpen: boolean;
}

/** Migrated protocol-3 chains can run native inference without banning transfers. */
export function isNativeOnlyChain(
  context: Pick<NativeContextView, "chainProtocol" | "nativeOnlyChain"> | null | undefined,
): boolean {
  return context?.chainProtocol === 4 && context.nativeOnlyChain === true;
}

export interface PreparedInput {
  inputHex: string;
  inputHash: string;
  inputKind: string;
  tokenCount: number | null;
  byteLen: number;
  /** What this chain's executor does with the input, in words. */
  note: string;
}

export interface NativeSubmitResult {
  kind: "request" | "refund";
  requestId: string;
  txHash: string;
  nonce: number;
  expiresAt: number;
  /** The node acknowledged exactly this transaction as pending. */
  accepted: boolean;
  httpStatus: number | null;
  body: string | null;
  /** Delivery unknown: the signed bytes stay journaled for resubmission. */
  networkError: string | null;
}

export interface NativeReceiptView {
  found: boolean;
  height: number;
  receipt: NativeReceipt | null;
}

/** Events that one submit/resubmit result means for a request. */
export function submitEvents(result: NativeSubmitResult): NativeRequestEvent[] {
  if (result.accepted) return [{ type: "submit_ok", txHash: result.txHash }];
  if (result.networkError !== null) return [{ type: "network_error", message: result.networkError }];
  return [{ type: "submit_refused", status: result.httpStatus ?? 0, body: result.body ?? "" }];
}

/** One journaled transaction, as `native_journal` returns it after a restart. */
export interface NativeJournalEntry {
  kind: "request" | "refund";
  requestId: string;
  txHash: string;
  nonce: number;
  expiresAt: number;
  createdAtMs: number;
  inputKind: string;
  promptPreview: string;
  executionPrice: number;
  reservedMaxPayment: number;
  refused: string | null;
  /** The signed bytes are still held and can be resubmitted. */
  resubmittable: boolean;
}

/**
 * Rebuild request states from the durable journal after a restart. Every
 * signed request comes back as "submitted" and its receipt or expiry decides
 * the rest - a refusal the journal remembers is shown as the reason, never
 * as a final state. A journaled refund still held for resubmission marks its
 * request "refund_submitted" until the receipt says otherwise. Unsigned
 * queued requests were never journaled and do not come back.
 */
export function restoreFromJournal(entries: readonly NativeJournalEntry[], account: string): NativeRequestState[] {
  const states = new Map<string, NativeRequestState>();
  for (const entry of entries) {
    if (entry.kind !== "request") continue;
    states.set(entry.requestId, {
      localId: `journal-${entry.requestId}`,
      account,
      // Every signed request comes back in flight: only its receipt or its
      // expiry decides, even one a node once refused (another copy may have
      // been admitted meanwhile).
      phase: "submitted",
      mayBeDelivered: true,
      promptPreview: entry.promptPreview,
      requestId: entry.requestId,
      txHash: entry.txHash,
      nonce: entry.nonce,
      expiresAt: entry.expiresAt,
      executionPrice: entry.executionPrice,
      reservedMaxPayment: entry.reservedMaxPayment,
      reason: entry.refused ?? "restored after restart; reading its receipt",
      attempts: 0,
      offline: false,
    });
  }
  for (const entry of entries) {
    if (entry.kind !== "refund") continue;
    const request = states.get(entry.requestId);
    if (request && !entry.refused && entry.resubmittable) {
      states.set(entry.requestId, { ...request, phase: "refund_submitted", refundTxHash: entry.txHash });
    }
  }
  return [...states.values()];
}

/** Plain-language label for a phase, as the Inference screen shows it. */
export function phaseLabel(state: NativeRequestState): string {
  const offline = state.offline ? " (node unreachable - showing last known state)" : "";
  const labels: Record<NativeRequestPhase, string> = {
    waiting: "Queued: not signed yet",
    withdrawn: "Withdrawn before signing: nothing was signed or sent",
    signing: "Signing",
    submitting: "Submitting",
    submitted: "Submitted: waiting to be admitted into a block",
    awaiting_certificate: "Admitted: validators are executing and voting",
    refund_due: "Expired without a certificate: claim the reservation back",
    refund_submitting: "Submitting refund",
    refund_submitted: "Refund submitted: waiting for a block",
    finalized: state.reconciled === false ? "Finalized - settlement does NOT reconcile" : "Finalized",
    refunded: "Refunded",
    rejected: "Rejected",
    dropped: "Dropped: never admitted, nothing charged",
  };
  return labels[state.phase] + offline;
}
