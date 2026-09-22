import { expect, test } from "@playwright/test";
import {
  canClaimRefund,
  classifyRefusal,
  createNativeRequest,
  nextToSign,
  phaseLabel,
  reduce,
  reservedInEscrow,
  restoreFromJournal,
  RETRY_ATTEMPT_LIMIT,
  type NativeJournalEntry,
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

  test("a node rebuilding its DAG is retried until the request expires, never shown as rejected", () => {
    const body = "this node is rebuilding its DAG from its peers and cannot propose yet; submit to another validator";
    expect(classifyRefusal(503, body).kind).toBe("retry_elsewhere");
    expect(classifyRefusal(503, "").kind).toBe("retry_later");
    let state = reduce(signing, { type: "submit_refused", status: 503, body });
    expect(state.phase).toBe("submitting");
    for (let attempt = 0; attempt < RETRY_ATTEMPT_LIMIT * 3; attempt += 1) {
      state = run(state, { type: "submit_started" }, { type: "submit_refused", status: 503, body });
    }
    // The signed bytes can still be admitted: still in flight, with the reason.
    expect(state.phase).toBe("submitting");
    expect(state.reason).toContain("rebuilding its DAG");
    // Once the chain is past its expiry it can never be admitted: dropped.
    state = reduce(state, { type: "receipt", receipt: null, height: 100 });
    expect(state.phase).toBe("dropped");
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

test.describe("cancellation follows finality", () => {
  test("only an unsigned request can be withdrawn", () => {
    const queued = createNativeRequest("q1", "alice", "hello");
    const withdrawn = reduce(queued, { type: "withdraw" });
    expect(withdrawn.phase).toBe("withdrawn");
    expect(phaseLabel(withdrawn)).toContain("nothing was signed or sent");
    // Withdrawn is final: it cannot be signed later.
    expect(reduce(withdrawn, { type: "sign_started" }).phase).toBe("withdrawn");

    // Once signed there is nothing to withdraw: a button that hid it would lie.
    for (const state of [
      reduce(queued, { type: "sign_started" }),
      submitted(),
      reduce(submitted(), { type: "receipt", height: 51, receipt: receipt("Pending") }),
    ]) {
      expect(reduce(state, { type: "withdraw" }).phase, state.phase).toBe(state.phase);
    }
  });

  test("an admitted request becomes refundable exactly when the next block may refund it", () => {
    const admitted = reduce(submitted(100), { type: "receipt", height: 90, receipt: receipt("Pending") });
    expect(admitted.phase).toBe("awaiting_certificate");
    // The next block is height + 1; the chain refunds at or past the expiry.
    const early = reduce(admitted, { type: "receipt", height: 98, receipt: receipt("Pending") });
    expect(early.phase).toBe("awaiting_certificate");
    const due = reduce(admitted, { type: "receipt", height: 99, receipt: receipt("Pending") });
    expect(due.phase).toBe("refund_due");
    expect(phaseLabel(due)).toContain("claim the reservation back");
    // The receipt's own expiry wins over what the app remembered.
    const byReceipt = reduce(submitted(1_000), {
      type: "receipt",
      height: 99,
      receipt: receipt("Pending", { expires_at: 100 }),
    });
    expect(byReceipt.phase).toBe("refund_due");
    expect(byReceipt.expiresAt).toBe(100);
  });

  test("a refund is claimed, followed on chain, and retried when it does not land", () => {
    let state = reduce(submitted(100), {
      type: "receipt",
      height: 99,
      receipt: receipt("Pending", { reserved_max_payment: 1000 }),
    });
    expect(state.phase).toBe("refund_due");
    expect(canClaimRefund([state], state.localId)).toBe(true);
    state = run(state, { type: "refund_started" }, { type: "refund_ok", txHash: "ef".repeat(32) });
    expect(state.phase).toBe("refund_submitted");
    expect(state.refundTxHash).toBe("ef".repeat(32));
    // Still pending on chain: the refund is not in a block yet.
    state = reduce(state, { type: "receipt", height: 101, receipt: receipt("Pending") });
    expect(state.phase).toBe("refund_submitted");
    // A refused refund goes back to claimable with the reason.
    const refused = reduce(state, { type: "refund_refused", reason: "rate limited" });
    expect(refused.phase).toBe("refund_due");
    expect(refused.reason).toBe("rate limited");
    // The chain returns the reservation.
    state = reduce(state, {
      type: "receipt",
      height: 102,
      receipt: receipt("Refunded", {
        reserved_max_payment: 1000,
        settlement_credits: [{ payee: "alice", amount: 1000 }],
      }),
    });
    expect(state.phase).toBe("refunded");
    expect(state.reconciled).toBe(true);
  });

  test("a refund takes the account's nonce in turn, like a request", () => {
    const due = reduce(submitted(100), { type: "receipt", height: 99, receipt: receipt("Pending") });
    const queued = createNativeRequest("q2", "alice");
    expect(canClaimRefund([due, queued], due.localId)).toBe(true);
    const claiming = reduce(due, { type: "refund_started" });
    expect(nextToSign([claiming, queued], "alice")).toBeUndefined();
    const other = run(queued, { type: "sign_started" });
    expect(canClaimRefund([due, other], due.localId)).toBe(false);
  });

  test("reservations are counted only while they sit in escrow", () => {
    const admitted = reduce(submitted(100), {
      type: "receipt",
      height: 50,
      receipt: receipt("Pending", { reserved_max_payment: 700 }),
    });
    const due = reduce(admitted, { type: "receipt", height: 99, receipt: receipt("Pending") });
    const finalized = reduce(admitted, {
      type: "receipt",
      height: 60,
      receipt: receipt("Finalized", { reserved_max_payment: 700 }),
    });
    expect(reservedInEscrow([admitted], "alice")).toBe(700);
    expect(reservedInEscrow([due], "alice")).toBe(700);
    expect(reservedInEscrow([finalized, submitted()], "alice")).toBe(0);
    expect(reservedInEscrow([admitted], "bob")).toBe(0);
  });
});

test.describe("resubmission and restart", () => {
  test("a node that already holds the transaction is not a refusal", () => {
    expect(classifyRefusal(409, "").kind).toBe("already_pending");
    expect(classifyRefusal(429, "").kind).toBe("retry_later");
    const signing = run(createNativeRequest("r1", "alice"), { type: "sign_started" }, {
      type: "signed",
      requestId: REQUEST,
      nonce: 1,
      expiresAt: 100,
    }, { type: "submit_started" });
    expect(reduce(signing, { type: "submit_refused", status: 409, body: "" }).phase).toBe("submitted");
  });

  test("a resubmission refused for good leaves the copy already in a mempool to its receipt", () => {
    const state = reduce(submitted(), { type: "submit_refused", status: 400, body: "native signature: bad" });
    expect(state.phase).toBe("submitted");
    expect(state.reason).toContain("bad");
  });

  test("a refusal after a delivery that may have happened is never final", () => {
    const signing = run(createNativeRequest("r1", "alice"), { type: "sign_started" }, {
      type: "signed",
      requestId: REQUEST,
      nonce: 1,
      expiresAt: 100,
    }, { type: "submit_started" });
    // The first attempt timed out: delivery unknown.
    let state = reduce(signing, { type: "network_error", message: "timed out" });
    expect(state.mayBeDelivered).toBe(true);
    state = run(state, { type: "submit_started" }, { type: "submit_refused", status: 400, body: "request is expired" });
    expect(state.phase).toBe("submitted");
    // The copy that got through was admitted: the receipt moves it on.
    state = reduce(state, { type: "receipt", height: 60, receipt: receipt("Pending") });
    expect(state.phase).toBe("awaiting_certificate");
    // A clean first refusal is still final.
    expect(reduce(signing, { type: "submit_refused", status: 400, body: "insufficient balance: have 1, need 2" }).phase).toBe("rejected");
  });

  test("the journal brings back signed requests and pending refunds, never unsigned drafts", () => {
    const entries: NativeJournalEntry[] = [
      {
        kind: "request",
        requestId: REQUEST,
        txHash: "01".repeat(32),
        nonce: 3,
        expiresAt: 900,
        createdAtMs: 1,
        inputKind: "test_executor_bytes",
        promptPreview: "first",
        executionPrice: 10,
        reservedMaxPayment: 100,
        refused: null,
        resubmittable: false,
      },
      {
        kind: "request",
        requestId: "cd".repeat(32),
        txHash: "02".repeat(32),
        nonce: 4,
        expiresAt: 950,
        createdAtMs: 2,
        inputKind: "test_executor_bytes",
        promptPreview: "second",
        executionPrice: 10,
        reservedMaxPayment: 100,
        refused: "insufficient balance: have 5, need 100",
        resubmittable: false,
      },
      {
        kind: "refund",
        requestId: REQUEST,
        txHash: "03".repeat(32),
        nonce: 5,
        expiresAt: 1_500,
        createdAtMs: 3,
        inputKind: "",
        promptPreview: "",
        executionPrice: 0,
        reservedMaxPayment: 100,
        refused: null,
        resubmittable: true,
      },
    ];
    const states = restoreFromJournal(entries, "alice");
    expect(states).toHaveLength(2);
    const first = states.find((state) => state.requestId === REQUEST)!;
    expect(first.phase).toBe("refund_submitted");
    expect(first.refundTxHash).toBe("03".repeat(32));
    expect(first.promptPreview).toBe("first");
    const second = states.find((state) => state.requestId === "cd".repeat(32))!;
    // A remembered refusal is the reason, not a verdict: another copy may
    // have been admitted, so the receipt or the expiry decides.
    expect(second.phase).toBe("submitted");
    expect(second.reason).toContain("insufficient balance");
    // The receipt still decides: a restored request that the chain finalized is finalized.
    const settled = reduce(first, { type: "receipt", height: 1_000, receipt: receipt("Refunded") });
    expect(settled.phase).toBe("refunded");
  });
});
