// node explorer/test-localdev-native.mjs - the rules behind the local-dev
// native-inference view, without a browser.
import { createRequire } from "node:module";
import assert from "node:assert/strict";

const require = createRequire(import.meta.url);
const native = require("./localdev-native.js");

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

console.log(`\nARC local-dev native-inference view: ${passed}/${passed} checks passed`);
