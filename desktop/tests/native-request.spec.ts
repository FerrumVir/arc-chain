import { expect, test } from "@playwright/test";
import {
  classifyRefusal,
  createNativeRequest,
  nextToSign,
  phaseLabel,
  reduce,
  RETRY_ATTEMPT_LIMIT,
  type NativeReceipt,
  type NativeRequestEvent,
  type NativeRequestState,
} from "../src/lib/native-request";

const REQUEST = "ab".repeat(32);

function run(state: NativeRequestState, ...events: NativeRequestEvent[]): NativeRequestState {
  return events.reduce(reduce, state);
}

function submitted(expiresAt = 100): NativeRequestState {
  return run(
    createNativeRequest("r1", "alice"),
    { type: "sign_started" },
    { type: "signed", requestId: REQUEST, nonce: 7, expiresAt },
    { type: "submit_started" },
    { type: "submit_ok", txHash: "cd".repeat(32) },
  );
}

function receipt(status: string, extra: Partial<NativeReceipt> = {}): NativeReceipt {
  return { request_id: `0x${REQUEST}`, observed_status: status, admission_height: 42, ...extra };
}

test.describe("native request lifecycle", () => {
  test("only the chain's receipt moves a request from submitted to finalized", () => {
    let state = submitted();
    expect(state.phase).toBe("submitted");
    state = reduce(state, { type: "receipt", receipt: null, height: 50 });
    expect(state.phase).toBe("submitted");
    state = reduce(state, { type: "receipt", receipt: receipt("Pending"), height: 51 });
    expect(state.phase).toBe("awaiting_certificate");
    expect(state.admissionHeight).toBe(42);
    state = reduce(state, {
      type: "receipt",
      height: 53,
      receipt: receipt("Finalized", {
        output_hash: "ee".repeat(32),
        certificate_votes: 3,
        reserved_max_payment: 1000,
        settlement_credits: [
          { payee: "v1", amount: 400 },
          { payee: "v2", amount: 300 },
          { payee: "alice", amount: 300 },
        ],
      }),
    });
    expect(state.phase).toBe("finalized");
    expect(state.reconciled).toBe(true);
    expect(state.certificateVotes).toBe(3);
    expect(phaseLabel(state)).toBe("Finalized");
  });

  test("a settlement whose credits do not add up to the reservation is flagged", () => {
    const state = reduce(submitted(), {
      type: "receipt",
      height: 53,
      receipt: receipt("Finalized", {
        reserved_max_payment: 1000,
        settlement_credits: [{ payee: "v1", amount: 999 }],
      }),
    });
    expect(state.reconciled).toBe(false);
    expect(phaseLabel(state)).toContain("does NOT reconcile");
  });

  test("an admitted request that expires is refunded; one never admitted is dropped", () => {
    const refunded = reduce(submitted(), {
      type: "receipt",
      height: 120,
      receipt: receipt("Refunded", { reserved_max_payment: 1000, settlement_credits: [{ payee: "alice", amount: 1000 }] }),
    });
    expect(refunded.phase).toBe("refunded");
    expect(refunded.reconciled).toBe(true);

    const early = reduce(submitted(100), { type: "receipt", receipt: null, height: 99 });
    expect(early.phase).toBe("submitted");
    const dropped = reduce(submitted(100), { type: "receipt", receipt: null, height: 100 });
    expect(dropped.phase).toBe("dropped");
    expect(phaseLabel(dropped)).toContain("nothing charged");
  });

  test("terminal states are final", () => {
    const finalized = reduce(submitted(), { type: "receipt", height: 53, receipt: receipt("Finalized") });
    const later = run(
      finalized,
      { type: "receipt", height: 54, receipt: receipt("Pending") },
      { type: "submit_refused", status: 400, body: "request is expired" },
      { type: "sign_started" },
    );
    expect(later.phase).toBe("finalized");
  });

  test("a receipt for a different request never moves this one", () => {
    const state = reduce(submitted(), {
      type: "receipt",
      height: 53,
      receipt: { request_id: "ff".repeat(32), observed_status: "Finalized" },
    });
    expect(state.phase).toBe("submitted");
  });

  test("an unrecognised chain status is reported, not guessed", () => {
    const state = reduce(submitted(), { type: "receipt", height: 53, receipt: receipt("Quarantined") });
    expect(state.phase).toBe("submitted");
    expect(state.reason).toContain("Quarantined");
  });
});

test.describe("submit refusals", () => {
  const signing = run(createNativeRequest("r1", "alice"), { type: "sign_started" }, {
    type: "signed",
    requestId: REQUEST,
    nonce: 9,
    expiresAt: 100,
  }, { type: "submit_started" });

  test("a nonce ahead of the account releases the nonce and waits for the earlier request", () => {
    const state = reduce(signing, { type: "submit_refused", status: 400, body: "execution error: invalid nonce: expected 7, got 9" });
    expect(state.phase).toBe("waiting");
    expect(state.nonce).toBeUndefined();
    expect(state.requestId).toBeUndefined();
  });

  test("a consumed nonce keeps polling: the request itself may already be in", () => {
    const state = reduce(signing, { type: "submit_refused", status: 400, body: "invalid nonce: expected 10, got 9" });
    expect(state.phase).toBe("submitted");
    const admitted = reduce(state, { type: "receipt", height: 60, receipt: receipt("Pending") });
    expect(admitted.phase).toBe("awaiting_certificate");
  });

  test("refusals that can never succeed reject the request with the node's reason", () => {
    for (const body of [
      "account not found: Address(..)",
      "insufficient balance: have 5, need 1000",
      "request is expired",
      "native signature: bad signature",
    ]) {
      const state = reduce(signing, { type: "submit_refused", status: 400, body });
      expect(state.phase, body).toBe("rejected");
      expect(state.reason, body).toBeTruthy();
    }
  });

  test("a node rebuilding its DAG is retried, then given up on", () => {
    const body = "this node is rebuilding its DAG from its peers and cannot propose yet; submit to another validator";
    expect(classifyRefusal(503, body).kind).toBe("retry_elsewhere");
    expect(classifyRefusal(503, "").kind).toBe("retry_later");
    let state = reduce(signing, { type: "submit_refused", status: 503, body });
    expect(state.phase).toBe("submitting");
    for (let attempt = state.attempts; attempt < RETRY_ATTEMPT_LIMIT; attempt += 1) {
      state = run(state, { type: "submit_started" }, { type: "submit_refused", status: 503, body });
    }
    expect(state.phase).toBe("rejected");
    expect(state.reason).toContain(`gave up after ${RETRY_ATTEMPT_LIMIT} attempts`);
  });
});

test.describe("connection and concurrency", () => {
  test("losing the node keeps the last known state and says so; a receipt restores it", () => {
    let state = reduce(submitted(), { type: "receipt", height: 51, receipt: receipt("Pending") });
    state = reduce(state, { type: "network_error", message: "connection refused" });
    expect(state.phase).toBe("awaiting_certificate");
    expect(state.offline).toBe(true);
    expect(phaseLabel(state)).toContain("unreachable");
    state = reduce(state, { type: "receipt", height: 55, receipt: receipt("Finalized") });
    expect(state.offline).toBe(false);
    expect(state.phase).toBe("finalized");
  });

  test("an account signs one request at a time; accounts are independent", () => {
    const a1 = createNativeRequest("a1", "alice");
    const a2 = createNativeRequest("a2", "alice");
    const b1 = createNativeRequest("b1", "bob");
    let states = [a1, a2, b1];
    expect(nextToSign(states, "alice")).toBe("a1");
    expect(nextToSign(states, "bob")).toBe("b1");

    const a1Submitted = run(a1, { type: "sign_started" }, {
      type: "signed",
      requestId: REQUEST,
      nonce: 0,
      expiresAt: 100,
    }, { type: "submit_started" }, { type: "submit_ok", txHash: "00" });
    states = [a1Submitted, a2, b1];
    expect(nextToSign(states, "alice")).toBeUndefined();
    expect(nextToSign(states, "bob")).toBe("b1");

    const a1Admitted = reduce(a1Submitted, { type: "receipt", height: 10, receipt: receipt("Pending") });
    states = [a1Admitted, a2, b1];
    expect(nextToSign(states, "alice")).toBe("a2");

    const a1Dropped = reduce(a1Submitted, { type: "receipt", receipt: null, height: 100 });
    expect(nextToSign([a1Dropped, a2, b1], "alice")).toBe("a2");
  });
});
