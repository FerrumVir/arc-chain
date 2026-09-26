// node explorer/test-localdev-native.mjs - the rules behind the local-dev
// native-inference view, without a browser.
import { createRequire } from "node:module";
import assert from "node:assert/strict";

const require = createRequire(import.meta.url);
const native = require("./native-receipts.js");

let passed = 0;
const check = (name, fn) => {
  fn();
  passed += 1;
  console.log(`ok ${passed} - ${name}`);
};

// A real receipt shape from a four-validator protocol-4 chain.
const finalized = {
  request_id: "20".repeat(32),
  observed_status: "Finalized",
  execution_price: 10,
  reserved_max_payment: 100,
  output_hash: "87".repeat(32),
  certificate_votes: 3,
  admission_transaction: { block_height: 15 },
  terminal_transaction: { block_height: 21 },
  settlement_credits: [
    { payee: "aa".repeat(32), amount: 4 },
    { payee: "bb".repeat(32), amount: 3 },
    { payee: "cc".repeat(32), amount: 3 },
    { payee: "dd".repeat(32), amount: 90 },
  ],
};

check("a settled receipt reconciles when credits equal the reservation", () => {
  const s = native.summarize(finalized);
  assert.equal(s.status, "Finalized");
  assert.equal(s.credited, 100);
  assert.equal(s.reconciled, true);
  assert.equal(s.admissionHeight, 15);
  assert.equal(s.terminalHeight, 21);
});

// A migrated chain settles paid work too, and its receipts are the same shape
// whatever binding settled them. On the node side, reading a receipt used to
// require the binding its metadata named to still be one the chain held, so a
// configuration change made every past settlement unreadable. The view must
// not reintroduce that from the other end: what a receipt says is decided by
// the receipt, not by which binding is in force now.
check("a settlement made under a superseded binding still reads and reconciles", () => {
  const underOldBinding = {
    ...finalized,
    request_id: "21".repeat(32),
    // The binding that settled it, which the chain has since replaced.
    context_commitment: "11".repeat(32),
  };
  const s = native.summarize(underOldBinding);
  assert.equal(s.known, true);
  assert.equal(s.status, "Finalized");
  assert.equal(s.credited, 100);
  assert.equal(s.reconciled, true, "history does not stop reconciling because the binding moved");
  assert.equal(s.terminal, true);
});

// A migrated chain reports protocol 3 and carries native receipts at the same
// time - a combination neither a recovered chain nor a private protocol-4
// chain has ever produced. These rules read receipts, not protocol versions,
// and that is what makes them work across the migration.
check("a receipt is read the same way whatever protocol version the chain reports", () => {
  const three = { ...finalized, request_id: "22".repeat(32), protocol_version: "3.0.0" };
  const four = { ...finalized, request_id: "22".repeat(32), protocol_version: "4.0.0" };
  const a = native.summarize(three);
  const b = native.summarize(four);
  assert.deepEqual(a, b, "the protocol version must not change what a receipt says");
  assert.equal(a.status, "Finalized");
  assert.equal(a.reconciled, true);
});

check("credits that do not add up are reported, not hidden", () => {
  const broken = { ...finalized, settlement_credits: [{ payee: "aa".repeat(32), amount: 50 }] };
  assert.equal(native.summarize(broken).reconciled, false);
});

check("a pending request has nothing to reconcile yet", () => {
  const pending = { ...finalized, observed_status: "Pending", settlement_credits: [], terminal_transaction: null };
  const s = native.summarize(pending);
  assert.equal(s.terminal, false);
  assert.equal(s.reconciled, null);
});

check("no receipt is unknown, never a status", () => {
  assert.equal(native.summarize(null).known, false);
});

check("replicas agree when they record the same settlement in any order", () => {
  const reordered = { ...finalized, settlement_credits: [...finalized.settlement_credits].reverse() };
  const r = native.compareReplicas([
    { source: "a", receipt: finalized },
    { source: "b", receipt: reordered },
    { source: "c", receipt: null },
  ]);
  assert.equal(r.asked, 3);
  assert.equal(r.answered, 2);
  assert.equal(r.agree, true);
});

check("a replica recording a different settlement is a disagreement", () => {
  const other = { ...finalized, settlement_credits: [{ payee: "dd".repeat(32), amount: 100 }] };
  const r = native.compareReplicas([
    { source: "a", receipt: finalized },
    { source: "b", receipt: other },
  ]);
  assert.equal(r.agree, false);
});

check("native transactions link to their request; others do not", () => {
  const tx = native.describeTransaction({
    tx_hash: "20".repeat(32), tx_type: "NativeInferenceFinalize", success: true,
    block_height: 21, body: { request_id: "20".repeat(32) },
  });
  assert.equal(tx.native, true);
  assert.equal(tx.requestId, "20".repeat(32));
  const transfer = native.describeTransaction({ tx_hash: "ab".repeat(32), tx_type: "Transfer", success: true });
  assert.equal(transfer.native, false);
  assert.equal(transfer.requestId, null);
});

// The node source for expires_at/requester/output_hex/output_text was added
// but is not yet compiled anywhere; every receipt above is the shape a
// running node actually serves today. These fields must read as
// "unavailable", never as fabricated zeros/empties, until a node upgrades.
check("expiry, requester, and certified output are absent-safe on an un-upgraded receipt", () => {
  const s = native.summarize(finalized);
  assert.equal(s.expiresAt, null);
  assert.equal(s.requester, null);
  assert.equal(s.outputHex, "");
  assert.equal(s.outputCertified, false);
  assert.equal(s.outputText, null);
});

check("an upgraded receipt exposes expiry, requester, and certified output separately from display text", () => {
  const upgraded = {
    ...finalized,
    expires_at: 40,
    requester: "cc".repeat(32),
    output_hex: "deadbeef",
    output_text: "hello world",
  };
  const s = native.summarize(upgraded);
  assert.equal(s.expiresAt, 40);
  assert.equal(s.requester, "cc".repeat(32));
  assert.equal(s.outputHex, "deadbeef");
  assert.equal(s.outputCertified, true);
  assert.equal(s.outputText, "hello world");
});

check("output_text absent from an otherwise-upgraded receipt reads as unavailable, not empty certified output", () => {
  const noText = { ...finalized, expires_at: 40, requester: "cc".repeat(32), output_hex: "", output_text: null };
  const s = native.summarize(noText);
  assert.equal(s.outputCertified, false);
  assert.equal(s.outputText, null);
});

check("a Pending request past its expiry height is expired without a certificate", () => {
  const pending = { ...finalized, observed_status: "Pending", settlement_credits: [], terminal_transaction: null, expires_at: 40 };
  const s = native.summarize(pending);
  const atBoundary = native.classifyExpiry(s, 39); // 39 + 1 >= 40
  assert.equal(atBoundary.applicable, true);
  assert.equal(atBoundary.expired, true);
  assert.match(atBoundary.message, /refundable by a refund transaction/);
  assert.doesNotMatch(atBoundary.message, /has been refunded|refund(ed)? successfully/i);
});

check("a Pending request before its expiry height is not expired", () => {
  const pending = { ...finalized, observed_status: "Pending", settlement_credits: [], terminal_transaction: null, expires_at: 40 };
  const s = native.summarize(pending);
  const early = native.classifyExpiry(s, 10); // 10 + 1 < 40
  assert.equal(early.applicable, true);
  assert.equal(early.expired, false);
  assert.equal(early.message, null);
});

check("expiry does not apply once a request is settled or when a node reports no expiry field", () => {
  const settledWithExpiry = { ...finalized, expires_at: 40 };
  assert.equal(native.classifyExpiry(native.summarize(settledWithExpiry), 1000).applicable, false);
  const pendingNoExpiry = { ...finalized, observed_status: "Pending", settlement_credits: [], terminal_transaction: null };
  assert.equal(native.classifyExpiry(native.summarize(pendingNoExpiry), 1000).applicable, false);
  assert.equal(native.classifyExpiry({ known: false }, 1000).applicable, false);
});

check("expiry is undecided, never assumed, when the chain height is unavailable", () => {
  const pending = { ...finalized, observed_status: "Pending", settlement_credits: [], terminal_transaction: null, expires_at: 40 };
  const result = native.classifyExpiry(native.summarize(pending), null);
  assert.equal(result.applicable, true);
  assert.equal(result.expired, null);
});

check("replicas that differ only in requester, expiry, or certified output are still a disagreement", () => {
  const base = { ...finalized, expires_at: 40, requester: "cc".repeat(32), output_hex: "deadbeef" };
  const differentRequester = { ...base, requester: "ee".repeat(32) };
  const r = native.compareReplicas([
    { source: "a", receipt: base },
    { source: "b", receipt: differentRequester },
  ]);
  assert.equal(r.agree, false);
});

check("a replica that does not report the newer fields yet is not a disagreement", () => {
  // During a rolling upgrade one node reports expiry, requester and output
  // bytes and another does not: missing detail, not a different settlement.
  const upgraded = { ...finalized, expires_at: 40, requester: "cc".repeat(32), output_hex: "deadbeef" };
  const mixed = native.compareReplicas([
    { source: "new", receipt: upgraded },
    { source: "old", receipt: finalized },
  ]);
  assert.equal(mixed.agree, true);
  // Settlement itself is still compared strictly across both.
  const differentCredits = {
    ...finalized,
    settlement_credits: [{ payee: "ff".repeat(32), amount: 1 }],
  };
  assert.equal(
    native.compareReplicas([
      { source: "new", receipt: upgraded },
      { source: "old", receipt: differentCredits },
    ]).agree,
    false,
  );
  // Two upgraded replicas with different certified output bytes disagree.
  assert.equal(
    native.compareReplicas([
      { source: "a", receipt: upgraded },
      { source: "b", receipt: { ...upgraded, output_hex: "00" } },
    ]).agree,
    false,
  );
});

console.log(`\nARC local-dev native-inference view: ${passed}/${passed} checks passed`);
