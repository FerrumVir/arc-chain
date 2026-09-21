// ARC explorer: native paid inference (protocol 4) receipt rules, shared by
// the production explorer (app.js) and the local-development view.
//
// Pure functions only - no DOM, no fetch - so the rules that decide what a
// receipt SAYS are tested without a browser (test-localdev-native.mjs) and
// the page just renders their output.
//
// A native request's canonical record is its receipt at
// /native-inference/receipt/{request_id}. What the page claims from it:
// - the status the chain recorded (Pending, Finalized, Refunded), never a
//   status inferred from a transaction hash or a local output;
// - for a settled request, whether its credits reconcile: they must add up to
//   exactly the reservation (price to the certificate's signers, the rest back
//   to the requester), otherwise the page says so;
// - across replicas, whether every node that answered records the same
//   settlement - a disagreement is shown, never averaged away.
(function (root) {
  "use strict";

  const TERMINAL = new Set(["Finalized", "Refunded"]);

  function summarize(receipt) {
    if (!receipt || typeof receipt !== "object") {
      return { known: false };
    }
    const credits = Array.isArray(receipt.settlement_credits) ? receipt.settlement_credits : [];
    const credited = credits.reduce((sum, c) => sum + Number(c?.amount ?? 0), 0);
    const reserved = Number(receipt.reserved_max_payment ?? NaN);
    const status = String(receipt.observed_status ?? "Unknown");
    const terminal = TERMINAL.has(status);
    return {
      known: true,
      requestId: String(receipt.request_id ?? ""),
      status,
      terminal,
      price: Number(receipt.execution_price ?? NaN),
      reserved,
      credited,
      // Only a settled request has credits to reconcile.
      reconciled: terminal ? Number.isFinite(reserved) && credited === reserved : null,
      credits: credits.map((c) => ({ payee: String(c?.payee ?? ""), amount: Number(c?.amount ?? 0) })),
      outputHash: receipt.output_hash ?? null,
      votes: receipt.certificate_votes ?? null,
      admissionHeight: receipt.admission_transaction?.block_height ?? receipt.admission_height ?? null,
      terminalHeight: receipt.terminal_transaction?.block_height ?? null,
    };
  }

  // The fields every replica must agree on for one request.
  function fingerprint(summary) {
    if (!summary.known) return null;
    const credits = summary.credits
      .map((c) => `${c.payee}:${c.amount}`)
      .sort()
      .join(",");
    return [summary.status, summary.outputHash ?? "", summary.terminalHeight ?? "", credits].join("|");
  }

  // `answers`: [{ source, receipt }] - one per replica that was asked.
  function compareReplicas(answers) {
    const rows = answers.map((a) => ({ source: a.source, summary: summarize(a.receipt) }));
    const known = rows.filter((r) => r.summary.known);
    const prints = new Set(known.map((r) => fingerprint(r.summary)));
    return {
      asked: rows.length,
      answered: known.length,
      agree: known.length > 0 && prints.size === 1,
      rows,
    };
  }

  const NATIVE_TYPES = new Set([
    "NativeInferenceRequest",
    "NativeInferenceFinalize",
    "NativeInferenceRefund",
  ]);

  // A /tx/{hash}/full response: its type, outcome and - for a native
  // transaction - the request it belongs to.
  function describeTransaction(full) {
    if (!full || typeof full !== "object") return { known: false };
    const type = String(full.tx_type ?? "Unknown");
    return {
      known: true,
      hash: String(full.tx_hash ?? ""),
      type,
      success: full.success === true,
      height: full.block_height ?? null,
      native: NATIVE_TYPES.has(type),
      requestId: NATIVE_TYPES.has(type) ? String(full.body?.request_id ?? "") || null : null,
    };
  }

  const api = { summarize, compareReplicas, describeTransaction, fingerprint };
  if (typeof module === "object" && module && module.exports) module.exports = api;
  root.ArcLocalDevNative = api;
  // The production explorer reads the same receipt rules under this name.
  root.ArcNativeReceipts = api;
})(typeof globalThis !== "undefined" ? globalThis : this);
