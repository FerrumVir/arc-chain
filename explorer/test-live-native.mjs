#!/usr/bin/env node
// The explorer's native receipt rules (native-receipts.js, shared by the
// production explorer and the local-dev view) against a REAL local chain.
// This is the explorer leg of P9: the same receipt, read by the explorer's
// own rules, must agree across replicas and with what the desktop and
// `python3 -m arc_ops.receipts` report for the same requests.
//
//   ARC_NATIVE_NODES=127.0.0.1:9960,127.0.0.1:9961,127.0.0.1:9962,127.0.0.1:9963 \
//   ARC_NATIVE_REQUESTS=<id>,<id> node explorer/test-live-native.mjs
//   (or ARC_NATIVE_JOURNAL=<desktop native-requests.json> instead of the ids)
//
// Skips (exit 0) unless ARC_NATIVE_NODES is set. Exit 1 on any disagreement
// or unreconciled settlement.

import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { readFile } from "node:fs/promises";

const nodesEnv = process.env.ARC_NATIVE_NODES;
if (!nodesEnv) {
  console.log("SKIP ARC explorer native live check: set ARC_NATIVE_NODES (and ARC_NATIVE_REQUESTS or ARC_NATIVE_JOURNAL)");
  process.exit(0);
}
const require = createRequire(import.meta.url);
const native = require("./native-receipts.js");

const nodes = nodesEnv.split(",").map((n) => n.trim()).filter(Boolean);
let ids = (process.env.ARC_NATIVE_REQUESTS ?? "").split(",").map((i) => i.trim()).filter(Boolean);
if (process.env.ARC_NATIVE_JOURNAL) {
  const journal = JSON.parse(await readFile(process.env.ARC_NATIVE_JOURNAL, "utf8"));
  for (const record of journal.records ?? []) {
    if (record.kind === "request" && !ids.includes(record.request_id)) ids.push(record.request_id);
  }
}
assert.ok(ids.length > 0, "no request ids: set ARC_NATIVE_REQUESTS or ARC_NATIVE_JOURNAL");

async function getJson(node, path) {
  try {
    const response = await fetch(`http://${node}${path}`, { cache: "no-store" });
    return response.ok ? await response.json() : null;
  } catch {
    return null;
  }
}

let failures = 0;
for (const id of ids) {
  const answers = [];
  const heights = [];
  for (const node of nodes) {
    const [receipt, health] = await Promise.all([
      getJson(node, `/native-inference/receipt/${id}`),
      getJson(node, "/health"),
    ]);
    answers.push({ source: node, receipt });
    heights.push(Number.isSafeInteger(health?.height) ? health.height : null);
  }
  const comparison = native.compareReplicas(answers);
  const problems = [];
  if (comparison.answered < Math.max(1, Math.ceil(nodes.length / 2))) problems.push("too few replicas answered");
  if (!comparison.agree) problems.push("replicas disagree");
  const rows = comparison.rows.map((row, index) => {
    const summary = row.summary;
    if (summary.known && ["Finalized", "Refunded"].includes(summary.status) && summary.reconciled === false) {
      problems.push(`${row.source}: settlement does not reconcile with the reservation`);
    }
    return {
      source: row.source,
      status: summary.known ? summary.status : "unknown",
      expiry: native.classifyExpiry(summary, heights[index]),
    };
  });
  if (problems.length) failures += 1;
  console.log(JSON.stringify({ request_id: id, ok: problems.length === 0, problems, rows }));
}
if (failures) {
  console.log(`FAIL ARC explorer native live check: ${failures}/${ids.length} requests`);
  process.exit(1);
}
console.log(`PASS ARC explorer native live check: ${ids.length} requests agree and reconcile on ${nodes.length} nodes`);
