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
// at most one request that is signed but not yet admitted. Later requests
// wait unsigned ("waiting") and are signed just in time with the nonce the
// node reports, so a dropped request never strands the ones behind it.

export type NativeRequestPhase =
  | "waiting" // queued behind an earlier request of the same account; not signed
  | "signing"
  | "submitting"
  | "submitted" // in a mempool; not yet admitted into a block
  | "awaiting_certificate" // admitted: funds reserved, validators executing and voting
  | "finalized"
  | "refunded" // admitted but expired without a certificate: reservation returned
  | "rejected" // refused as signed; it will never be admitted
  | "dropped"; // accepted by a node but never admitted before expiry; nothing reserved

export const TERMINAL_PHASES: ReadonlySet<NativeRequestPhase> = new Set([
  "finalized",
  "refunded",
  "rejected",
  "dropped",
]);

/** Phases in which the request's account nonce is taken but not yet consumed. */
const NONCE_IN_FLIGHT: ReadonlySet<NativeRequestPhase> = new Set([
  "signing",
  "submitting",
  "submitted",
]);

export interface NativeReceipt {
  request_id: string;
  observed_status: string;
  admission_height?: number | null;
  output_hash?: string | null;
  certificate_votes?: number | null;
  execution_price?: number;
  reserved_max_payment?: number;
  settlement_credits?: { payee: string; amount: number }[];
}

export interface NativeRequestState {
  localId: string;
  account: string;
  phase: NativeRequestPhase;
  requestId?: string;
  txHash?: string;
  nonce?: number;
  /** The request's signed expiry height. */
  expiresAt?: number;
  admissionHeight?: number;
  outputHash?: string;
  certificateVotes?: number;
  /** For a settled request: do its credits add up to exactly the reservation? */
  reconciled?: boolean;
  /** Why the request is rejected, dropped, or being retried. */
  reason?: string;
  /** Submissions attempted for this signed transaction. */
  attempts: number;
  /** Set while the node cannot be reached; the phase is the last one observed. */
  offline: boolean;
}

export type RefusalClass =
  | "wait_for_prior" // a nonce ahead of the account: an earlier request is still pending
  | "stale_nonce" // this nonce was consumed: look the request up before concluding anything
  | "retry_elsewhere" // this node cannot propose right now (rebuilding its DAG)
  | "retry_later" // service unavailable for now
  | "never"; // refused as signed: bad signature, unfunded, expired, over limits

export interface Refusal {
  kind: RefusalClass;
  reason: string;
}

export type NativeRequestEvent =
  | { type: "sign_started" }
  | { type: "signed"; requestId: string; nonce: number; expiresAt: number }
  | { type: "sign_failed"; reason: string }
  | { type: "submit_started" }
  | { type: "submit_ok"; txHash: string }
  | { type: "submit_refused"; status: number; body: string }
  | { type: "network_error"; message: string }
  | { type: "connection_restored" }
  /** A receipt read: `receipt` is null when the node answered 404. */
  | { type: "receipt"; receipt: NativeReceipt | null; height: number };

/** How long to back off before resubmitting after a transient refusal. */
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
  if (status === 400) {
    const nonce = NONCE.exec(text);
    if (nonce) {
      const expected = Number(nonce[1]);
      const got = Number(nonce[2]);
      if (got > expected) {
        return { kind: "wait_for_prior", reason: `an earlier request (nonce ${expected}) is not yet admitted` };
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

export function createNativeRequest(localId: string, account: string): NativeRequestState {
  return { localId, account, phase: "waiting", attempts: 0, offline: false };
}

function settle(state: NativeRequestState, receipt: NativeReceipt): NativeRequestState {
  const credits = receipt.settlement_credits ?? [];
  const credited = credits.reduce((sum, credit) => sum + Number(credit.amount ?? 0), 0);
  const reserved = Number(receipt.reserved_max_payment ?? Number.NaN);
  return {
    ...state,
    admissionHeight: receipt.admission_height ?? state.admissionHeight,
    outputHash: receipt.output_hash ?? undefined,
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
  switch (receipt.observed_status) {
    case "Pending":
      return {
        ...state,
        phase: "awaiting_certificate",
        admissionHeight: receipt.admission_height ?? state.admissionHeight,
        reason: undefined,
      };
    case "Finalized":
      return { ...settle(state, receipt), phase: "finalized", reason: undefined };
    case "Refunded":
      return { ...settle(state, receipt), phase: "refunded", reason: "no certificate before expiry; the reservation was returned" };
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
        attempts: 0,
      };
    case "sign_failed":
      return state.phase === "signing" ? { ...state, phase: "rejected", reason: event.reason } : state;
    case "submit_started":
      return state.phase === "submitting" ? { ...state, attempts: state.attempts + 1, offline: false } : state;
    case "submit_ok":
      return state.phase === "submitting"
        ? { ...state, phase: "submitted", txHash: event.txHash, reason: undefined }
        : state;
    case "submit_refused": {
      if (state.phase !== "submitting") return state;
      const refusal = classifyRefusal(event.status, event.body);
      switch (refusal.kind) {
        case "retry_elsewhere":
        case "retry_later":
          return state.attempts >= RETRY_ATTEMPT_LIMIT
            ? { ...state, phase: "rejected", reason: `${refusal.reason} (gave up after ${state.attempts} attempts)` }
            : { ...state, reason: refusal.reason };
        case "stale_nonce":
          // The nonce may have been consumed by this very request (an earlier
          // attempt that did get in). Only its receipt can tell; keep polling.
          return { ...state, phase: "submitted", reason: refusal.reason };
        case "wait_for_prior":
          // Signed too early: release the nonce and re-sign when this account's
          // earlier request is admitted.
          return {
            ...state,
            phase: "waiting",
            requestId: undefined,
            nonce: undefined,
            expiresAt: undefined,
            reason: refusal.reason,
          };
        case "never":
          return { ...state, phase: "rejected", reason: refusal.reason };
      }
      return state;
    }
    case "network_error":
      return { ...state, offline: true, reason: event.message };
    case "connection_restored":
      return { ...state, offline: false };
    case "receipt":
      return { ...applyReceipt(state, event.receipt, event.height), offline: false };
  }
  return state;
}

/**
 * Which waiting request of `account` may be signed now, if any: the oldest
 * one, and only when no request of that account holds an unconsumed nonce.
 */
export function nextToSign(states: readonly NativeRequestState[], account: string): string | undefined {
  const own = states.filter((state) => state.account === account);
  if (own.some((state) => NONCE_IN_FLIGHT.has(state.phase))) return undefined;
  return own.find((state) => state.phase === "waiting")?.localId;
}

/** Plain-language label for a phase, as the Inference screen shows it. */
export function phaseLabel(state: NativeRequestState): string {
  const offline = state.offline ? " (node unreachable - showing last known state)" : "";
  const labels: Record<NativeRequestPhase, string> = {
    waiting: "Queued: waiting for this account's earlier request",
    signing: "Signing",
    submitting: "Submitting",
    submitted: "Submitted: waiting to be admitted into a block",
    awaiting_certificate: "Admitted: validators are executing and voting",
    finalized: state.reconciled === false ? "Finalized - settlement does NOT reconcile" : "Finalized",
    refunded: "Refunded",
    rejected: "Rejected",
    dropped: "Dropped: never admitted, nothing charged",
  };
  return labels[state.phase] + offline;
}
