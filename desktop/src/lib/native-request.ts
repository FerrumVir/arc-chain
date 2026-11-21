export type NativeRequestPhase =
  | "waiting"
  | "withdrawn"
  | "signing"
  | "submitting"
  | "submitted"
  | "awaiting_certificate"
  | "refund_due"
  | "refund_submitting"
  | "refund_submitted"
  | "finalized"
  | "refunded"
  | "rejected"
  | "dropped";

export const TERMINAL_PHASES: ReadonlySet<NativeRequestPhase> = new Set([
  "withdrawn",
  "finalized",
  "refunded",
  "rejected",
  "dropped",
]);

const NONCE_IN_FLIGHT: ReadonlySet<NativeRequestPhase> = new Set([
  "signing",
  "submitting",
  "submitted",
  "refund_submitting",
  "refund_submitted",
]);

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
  promptPreview?: string;
  requestId?: string;
  txHash?: string;
  nonce?: number;
  expiresAt?: number;
  executionPrice?: number;
  reservedMaxPayment?: number;
  admissionHeight?: number;
  outputHash?: string;
  outputHex?: string;
  outputText?: string;
  certificateVotes?: number;
  reconciled?: boolean;
  refundTxHash?: string;
  reason?: string;
  attempts: number;
  mayBeDelivered?: boolean;
  offline: boolean;
}

export type RefusalClass =
  | "wait_for_prior"
  | "stale_nonce"
  | "already_pending"
  | "retry_elsewhere"
  | "retry_later"
  | "never";

export interface Refusal {
  kind: RefusalClass;
  reason: string;
}

export type NativeRequestEvent =
  | { type: "withdraw" }
  | { type: "sign_started" }
  | { type: "signed"; requestId: string; nonce: number; expiresAt: number; txHash?: string }
  | { type: "sign_failed"; reason: string }
  | { type: "submit_started" }
  | { type: "submit_ok"; txHash: string }
  | { type: "submit_refused"; status: number; body: string }
  | { type: "network_error"; message: string }
  | { type: "connection_restored" }
  | { type: "receipt"; receipt: NativeReceipt | null; height: number }
  | { type: "refund_started" }
  | { type: "refund_ok"; txHash: string }
  | { type: "refund_refused"; reason: string };

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
      return { ...state, reason: `unrecognised chain status ${receipt.observed_status}` };
  }
}

export function reduce(state: NativeRequestState, event: NativeRequestEvent): NativeRequestState {
  if (TERMINAL_PHASES.has(state.phase)) {
    if (event.type === "network_error") return { ...state, offline: true };
    if (event.type === "connection_restored") return { ...state, offline: false };
    return state;
  }
  switch (event.type) {
    case "withdraw":
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
          return { ...state, reason: refusal.reason };
        case "already_pending":
        case "stale_nonce":
          return { ...state, phase: "submitted", reason: refusal.reason, mayBeDelivered: true };
        case "wait_for_prior":
          if (state.phase === "submitted") return { ...state, reason: refusal.reason };
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
          if (state.phase === "submitted" || state.mayBeDelivered) {
            return { ...state, phase: "submitted", reason: refusal.reason };
          }
          return { ...state, phase: "rejected", reason: refusal.reason };
      }
      return state;
    }
    case "network_error":
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

export function nextToSign(states: readonly NativeRequestState[], account: string): string | undefined {
  const own = states.filter((state) => state.account === account);
  if (own.some((state) => NONCE_IN_FLIGHT.has(state.phase))) return undefined;
  return own.find((state) => state.phase === "waiting")?.localId;
}

export function canClaimRefund(states: readonly NativeRequestState[], localId: string): boolean {
  const state = states.find((candidate) => candidate.localId === localId);
  if (!state || state.phase !== "refund_due") return false;
  return !states.some((other) => other.account === state.account && NONCE_IN_FLIGHT.has(other.phase));
}

export function reservedInEscrow(states: readonly NativeRequestState[], account: string): number {
  return states
    .filter((state) => state.account === account && RESERVED.has(state.phase))
    .reduce((sum, state) => sum + (state.reservedMaxPayment ?? 0), 0);
}

export interface NativeServingView {
  executor: string;
  inputFormat: string;
  tokenizeEndpoint: boolean;
  tokenizerProfile: string | null;
  maxPositions: number | null;
}

export interface NativeContextView {
  host: string;
  compatible: boolean;
  reason: string | null;
  height: number | null;
  members: number | null;
  executions: number | null;
  maxTokens: number | null;
  serving: NativeServingView | null;
  inputKind: string | null;
  nodeVersion: string | null;
  appContractVersion: number;
  chainProtocol: number | null;
  nativeOnlyChain: boolean | null;
  requestAdmissionOpen: boolean;
  trackingAvailable: boolean;
}

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
  note: string;
}

export interface NativeSubmitResult {
  kind: "request" | "refund";
  requestId: string;
  txHash: string;
  nonce: number;
  expiresAt: number;
  accepted: boolean;
  httpStatus: number | null;
  body: string | null;
  networkError: string | null;
}

export interface NativeReceiptView {
  found: boolean;
  height: number;
  receipt: NativeReceipt | null;
}

export function submitEvents(result: NativeSubmitResult): NativeRequestEvent[] {
  if (result.accepted) return [{ type: "submit_ok", txHash: result.txHash }];
  if (result.networkError !== null) return [{ type: "network_error", message: result.networkError }];
  return [{ type: "submit_refused", status: result.httpStatus ?? 0, body: result.body ?? "" }];
}

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
  resubmittable: boolean;
}

export function restoreFromJournal(entries: readonly NativeJournalEntry[], account: string): NativeRequestState[] {
  const states = new Map<string, NativeRequestState>();
  for (const entry of entries) {
    if (entry.kind !== "request") continue;
    states.set(entry.requestId, {
      localId: `journal-${entry.requestId}`,
      account,
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
