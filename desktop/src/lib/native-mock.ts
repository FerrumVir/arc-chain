// Browser-preview model of a native-inference chain for the native paid-request
// panel. A test seam only: `mockInvoke` consults it, never the Tauri app or a
// production bundle (both refuse to mock), and only after a test sets
// `window.__ARC_MOCK_NATIVE__`. Without that flag the mock host is not a
// protocol-4 chain and the panel stays hidden, as it would on a real one.
//
// It follows the chain's rules closely enough to exercise the screen: one
// transaction per account nonce, reservation at admission, settlement
// credits that add up to the reservation, refunds only at or past expiry.
// State lives in sessionStorage so a reload (the app "restarting") finds the
// same chain and journal, as the native journal would.
//
// Prompt markers choose a path: "[expire]" is admitted but never certified,
// "[drop]" is never admitted. Both jump the height to the expiry so a test
// does not wait for hundreds of blocks. "[hold]" stays in the mempool,
// neither admitted nor expired, for as long as the test needs.

import type {
  NativeContextView,
  NativeJournalEntry,
  NativeReceiptView,
  NativeSubmitResult,
  PreparedInput,
} from "./native-request";

const STORAGE_KEY = "__arc_mock_native_chain__";
const BASE_UNITS = 1_000_000_000;
const MOCK_HOST = "https://mock-native.arc.invalid";
const VALIDATORS = ["a1", "b2", "c3", "d4"].map((tag) => tag.repeat(32));

type MockStatus = "Pending" | "Finalized" | "Refunded";

interface MockRequest {
  requestId: string;
  txHash: string;
  requester: string;
  nonce: number;
  expiresAt: number;
  price: number;
  reserve: number;
  prompt: string;
  behaviour: "normal" | "expire" | "drop" | "hold";
  status: MockStatus | null;
  admittedAt: number | null;
  polls: number;
  refundTxHash: string | null;
}

interface MockChain {
  height: number;
  balance: number;
  nonce: number;
  requests: MockRequest[];
  journal: NativeJournalEntry[];
}

type Flag = true | "incompatible" | "closed-admission";

function flag(): Flag | null {
  if (typeof window === "undefined") return null;
  const value = (window as Window & { __ARC_MOCK_NATIVE__?: Flag }).__ARC_MOCK_NATIVE__;
  return value === true || value === "incompatible" || value === "closed-admission" ? value : null;
}

export function nativeMockEnabled(): boolean {
  return flag() !== null;
}

function fresh(): MockChain {
  return { height: 1_000, balance: 5 * BASE_UNITS, nonce: 0, requests: [], journal: [] };
}

function load(): MockChain {
  try {
    const raw = window.sessionStorage.getItem(STORAGE_KEY);
    if (raw) return JSON.parse(raw) as MockChain;
  } catch {
    // Storage unavailable: a chain that forgets on reload is still a chain.
  }
  return fresh();
}

function save(chain: MockChain): void {
  try {
    window.sessionStorage.setItem(STORAGE_KEY, JSON.stringify(chain));
  } catch {
    // See `load`.
  }
}

function randomHex(bytes: number): string {
  const values = new Uint8Array(bytes);
  crypto.getRandomValues(values);
  return Array.from(values, (value) => value.toString(16).padStart(2, "0")).join("");
}

/** Exact decimal ARC to base units, as wallet.rs::parse_arc_amount does. */
function parseArc(input: string): number {
  const value = input.trim();
  if (!/^\d*(\.\d{1,9})?$/.test(value) || value === "" || value === ".") {
    throw new Error("amount must be a plain positive decimal with at most 9 decimal places");
  }
  const [whole, fraction = ""] = value.split(".");
  return Number(whole || "0") * BASE_UNITS + Number(fraction.padEnd(9, "0"));
}

function formatArc(base: number): string {
  const whole = Math.floor(base / BASE_UNITS);
  const fraction = base % BASE_UNITS;
  return fraction === 0 ? `${whole}` : `${whole}.${`${fraction}`.padStart(9, "0").replace(/0+$/, "")}`;
}

function toHex(text: string): string {
  return Array.from(new TextEncoder().encode(text), (byte) => byte.toString(16).padStart(2, "0")).join("");
}

function fromHex(hex: string): string {
  const bytes = hex.match(/../g)?.map((pair) => parseInt(pair, 16)) ?? [];
  return new TextDecoder().decode(new Uint8Array(bytes));
}

function journalEntry(chain: MockChain, txHash: string): NativeJournalEntry | undefined {
  return chain.journal.find((entry) => entry.txHash === txHash);
}

/** Mirror of `Journal::settle`: bytes that can no longer be admitted are released. */
function settleJournal(chain: MockChain): void {
  for (const entry of chain.journal) {
    if (entry.resubmittable && (chain.nonce > entry.nonce || chain.height >= entry.expiresAt)) {
      entry.resubmittable = false;
    }
  }
}

function openEntry(chain: MockChain): NativeJournalEntry | undefined {
  settleJournal(chain);
  return chain.journal.find((entry) => entry.resubmittable);
}

function context(): NativeContextView {
  const chain = load();
  const incompatible = flag() === "incompatible";
  const admissionOpen = flag() !== "closed-admission" && !incompatible;
  return {
    host: MOCK_HOST,
    compatible: admissionOpen,
    reason: incompatible
      ? "the node speaks native contract v2 and this app speaks v1; update the app"
      : !admissionOpen
        ? "this chain is not admitting new native paid requests right now"
        : null,
    height: incompatible ? null : chain.height,
    members: incompatible ? null : VALIDATORS.length,
    executions: incompatible ? null : 1,
    maxTokens: incompatible ? null : 2048,
    serving: incompatible
      ? null
      : {
          executor: "deterministic_test",
          inputFormat: "opaque_bytes",
          tokenizeEndpoint: false,
          tokenizerProfile: null,
          maxPositions: null,
        },
    inputKind: incompatible ? null : "test_executor_bytes",
    nodeVersion: "0.8.11",
    appContractVersion: 1,
    chainProtocol: 4,
    nativeOnlyChain: true,
    requestAdmissionOpen: admissionOpen,
    trackingAvailable: !incompatible,
  };
}

function prepare(prompt: string): PreparedInput {
  if (!prompt.trim()) throw new Error("enter a prompt");
  const inputHex = toHex(prompt);
  return {
    inputHex,
    inputHash: randomHex(32),
    inputKind: "test_executor_bytes",
    tokenCount: null,
    byteLen: inputHex.length / 2,
    note:
      "This chain runs the deterministic TEST executor. It ignores the prompt and returns protocol-test tokens, not an answer. Payment and settlement on this chain are real.",
  };
}

interface SubmitArgs {
  inputHex: string;
  inputKind: string;
  promptPreview: string;
  maxTokens: number;
  priceArc: string;
  reserveArc: string;
  expiryBlocks: number;
}

function submit(args: SubmitArgs): NativeSubmitResult {
  const chain = load();
  const price = parseArc(args.priceArc);
  const reserve = parseArc(args.reserveArc);
  if (args.maxTokens < 2 || args.maxTokens > 2048) {
    throw new Error("the test executor returns two tokens; allow at least 2");
  }
  if (price === 0) throw new Error("the price must be greater than zero");
  if (reserve < price) throw new Error("the reservation must cover the price");
  if (args.expiryBlocks < 30 || args.expiryBlocks > 20_000) {
    throw new Error("expiry must be between 30 and 20000 blocks");
  }
  const open = openEntry(chain);
  if (open) {
    save(chain);
    throw new Error(
      `an earlier transaction from this wallet (nonce ${open.nonce}) is not in a block yet; it is admitted or expires by block ${open.expiresAt}, and then this request can be signed`,
    );
  }
  if (chain.balance < reserve) {
    throw new Error(
      `insufficient balance: available ${formatArc(chain.balance)} ARC; this request reserves ${formatArc(reserve)} ARC until it settles`,
    );
  }
  const prompt = fromHex(args.inputHex);
  const request: MockRequest = {
    requestId: randomHex(32),
    txHash: randomHex(32),
    requester: "99".repeat(32),
    nonce: chain.nonce,
    expiresAt: chain.height + args.expiryBlocks,
    price,
    reserve,
    prompt,
    behaviour: prompt.includes("[expire]")
      ? "expire"
      : prompt.includes("[drop]")
        ? "drop"
        : prompt.includes("[hold]")
          ? "hold"
          : "normal",
    status: null,
    admittedAt: null,
    polls: 0,
    refundTxHash: null,
  };
  chain.requests.push(request);
  chain.journal.push({
    kind: "request",
    requestId: request.requestId,
    txHash: request.txHash,
    nonce: request.nonce,
    expiresAt: request.expiresAt,
    createdAtMs: Date.now(),
    inputKind: args.inputKind,
    promptPreview: args.promptPreview.slice(0, 120),
    executionPrice: price,
    reservedMaxPayment: reserve,
    refused: null,
    resubmittable: true,
  });
  save(chain);
  return {
    kind: "request",
    requestId: request.requestId,
    txHash: request.txHash,
    nonce: request.nonce,
    expiresAt: request.expiresAt,
    accepted: true,
    httpStatus: null,
    body: null,
    networkError: null,
  };
}

/** One receipt read advances the mock chain by a block. */
function receipt(requestId: string): NativeReceiptView {
  const chain = load();
  chain.height += 1;
  const request = chain.requests.find((candidate) => candidate.requestId === requestId);
  if (!request) {
    save(chain);
    return { found: false, height: chain.height, receipt: null };
  }
  request.polls += 1;
  if (request.status === null) {
    if (request.behaviour === "drop") {
      if (request.polls >= 2) chain.height = Math.max(chain.height, request.expiresAt);
    } else if (request.behaviour === "hold") {
      // Still in a mempool.
    } else if (chain.height < request.expiresAt) {
      // Admission: the reservation leaves the balance, the nonce is used.
      request.status = "Pending";
      request.admittedAt = chain.height;
      chain.balance -= request.reserve;
      chain.nonce += 1;
    }
  } else if (request.status === "Pending") {
    if (request.refundTxHash) {
      request.status = "Refunded";
      chain.balance += request.reserve;
      chain.nonce += 1;
    } else if (request.behaviour === "normal" && request.polls >= 3) {
      request.status = "Finalized";
      chain.balance += request.reserve - request.price;
    } else if (request.behaviour === "expire" && request.polls >= 2) {
      chain.height = Math.max(chain.height, request.expiresAt - 1);
    }
  }
  settleJournal(chain);
  save(chain);
  if (request.status === null) return { found: false, height: chain.height, receipt: null };
  const share = Math.floor(request.price / VALIDATORS.length);
  const credits =
    request.status === "Finalized"
      ? [
          ...VALIDATORS.map((payee, index) => ({
            payee,
            amount: share + (index === 0 ? request.price - share * VALIDATORS.length : 0),
          })),
          { payee: request.requester, amount: request.reserve - request.price },
        ]
      : request.status === "Refunded"
        ? [{ payee: request.requester, amount: request.reserve }]
        : [];
  return {
    found: true,
    height: chain.height,
    receipt: {
      request_id: request.requestId,
      observed_status: request.status,
      admission_height: request.admittedAt,
      expires_at: request.expiresAt,
      output_hash: request.status === "Finalized" ? randomHex(32) : null,
      output_hex: request.status === "Finalized" ? "0d0000002a000000" : "",
      output_text: null,
      certificate_votes: request.status === "Finalized" ? 3 : null,
      execution_price: request.price,
      reserved_max_payment: request.reserve,
      settlement_credits: credits,
    },
  };
}

function refund(requestId: string): NativeSubmitResult {
  const chain = load();
  const request = chain.requests.find((candidate) => candidate.requestId === requestId);
  if (!request || request.status === null) {
    throw new Error("the chain has no admitted request with this id, so nothing is reserved");
  }
  if (request.status === "Refunded") throw new Error("this request is already refunded");
  if (request.status === "Finalized") throw new Error("this request finalized; there is nothing to refund");
  if (chain.height + 1 < request.expiresAt) {
    throw new Error(`not refundable yet: the request expires at block ${request.expiresAt} (now ${chain.height})`);
  }
  const open = openEntry(chain);
  if (open) {
    save(chain);
    throw new Error(`an earlier transaction from this wallet (nonce ${open.nonce}) is not in a block yet; claim the refund once it is`);
  }
  request.refundTxHash = randomHex(32);
  chain.journal.push({
    kind: "refund",
    requestId,
    txHash: request.refundTxHash,
    nonce: chain.nonce,
    expiresAt: chain.height + 600,
    createdAtMs: Date.now(),
    inputKind: "",
    promptPreview: "",
    executionPrice: 0,
    reservedMaxPayment: request.reserve,
    refused: null,
    resubmittable: true,
  });
  save(chain);
  return {
    kind: "refund",
    requestId,
    txHash: request.refundTxHash,
    nonce: chain.nonce,
    expiresAt: chain.height + 600,
    accepted: true,
    httpStatus: null,
    body: null,
    networkError: null,
  };
}

function resubmit(txHash: string): NativeSubmitResult {
  const chain = load();
  settleJournal(chain);
  save(chain);
  const entry = journalEntry(chain, txHash);
  if (!entry) throw new Error("no journaled transaction with this hash on this host");
  if (!entry.resubmittable) {
    throw new Error(
      "this transaction can no longer be admitted (its nonce was used or it expired); its receipt decides the outcome",
    );
  }
  // The mock node still holds it.
  return {
    kind: entry.kind,
    requestId: entry.requestId,
    txHash,
    nonce: entry.nonce,
    expiresAt: entry.expiresAt,
    accepted: false,
    httpStatus: 409,
    body: "",
    networkError: null,
  };
}

/** The native mock's wallet balance, in the `fetch_balance` shape. */
export function nativeMockBalance(): Record<string, unknown> {
  const chain = load();
  return {
    address: "99".repeat(32),
    balanceBase: `${chain.balance}`,
    balanceArc: formatArc(chain.balance),
    nonce: chain.nonce,
    stakedBalanceBase: "0",
    stakedBalanceArc: "0",
  };
}

/**
 * A wallet transfer on the browser chain. Like a real protocol-4 node, which
 * refuses any non-native transaction (decision D13), it can never be
 * included, and the Rust transfer path refuses before signing.
 */
export function mockNativeTransfer(): never {
  throw new Error(
    "this chain carries only native paid-inference transactions, so a transfer can never be included; nothing was signed",
  );
}

/**
 * Answer a native command, or `undefined` when `cmd` is not one. With the
 * seam off, the host is simply not a protocol-4 chain.
 */
export function mockNativeInvoke(cmd: string, args: unknown): unknown {
  if (!cmd.startsWith("native_")) return undefined;
  if (!nativeMockEnabled()) {
    if (cmd === "native_context") return null;
    if (cmd === "native_journal") return [];
    throw new Error("the selected host is not a protocol-4 chain");
  }
  const input = (args ?? {}) as Record<string, unknown>;
  switch (cmd) {
    case "native_context":
      return context();
    case "native_prepare":
      if (flag() === "closed-admission") throw new Error("this chain is not admitting new native paid requests right now");
      return prepare(String(input.prompt ?? ""));
    case "native_submit":
      if (flag() === "closed-admission") throw new Error("this chain is not admitting new native paid requests right now");
      return submit(input as unknown as SubmitArgs);
    case "native_receipt":
      return receipt(String(input.requestId ?? ""));
    case "native_refund":
      return refund(String(input.requestId ?? ""));
    case "native_resubmit":
      return resubmit(String(input.txHash ?? ""));
    case "native_journal": {
      const chain = load();
      settleJournal(chain);
      save(chain);
      return chain.journal;
    }
    default:
      throw new Error(`Unmocked native command: ${cmd}`);
  }
}
