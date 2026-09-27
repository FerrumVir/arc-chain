// ARC explorer: native paid-inference receipt rules, shared by
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
//   settlement - a disagreement is shown, never averaged away;
// - a Pending request past its expiry height is flagged as uncertifiable, but
//   never as refunded: the chain only refunds once a refund transaction is
//   mined, and this module never claims that happened on its own.
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
    // `expires_at`, `requester`, `output_hex`, and `output_text` are additive
    // fields: a node that has not yet upgraded simply omits them, and that
    // must read as "unavailable", never as zero/empty-by-construction.
    const outputHex = typeof receipt.output_hex === "string" ? receipt.output_hex : "";
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
      // The block height after which no certificate can still land for this
      // request (u64 on the wire; a bare, un-coerced pass-through like the
      // other heights on this object).
      expiresAt: typeof receipt.expires_at === "number" ? receipt.expires_at : null,
      requester: typeof receipt.requester === "string" && receipt.requester ? receipt.requester : null,
      // The certified output bytes. Empty means "not finalized yet", never
      // "certified as empty" - callers must check `outputCertified`, not the
      // string length on its own.
      outputHex,
      outputCertified: outputHex.length > 0,
      // Display only, from this node's own tokenizer: NOT part of the
      // certificate, which commits only to `outputHex`/`outputHash`.
      outputText: typeof receipt.output_text === "string" ? receipt.output_text : null,
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

  // Fields a node reports only once it is upgraded. Each is fixed by the
  // request or by the certified bytes, so two replicas that both report one
  // must agree on it; a replica that omits it (not yet upgraded) is not a
  // disagreement, only missing detail.
  const OPTIONAL_FIELDS = ["expiresAt", "requester", "outputHex"];

  function optionalFieldsAgree(summaries) {
    return OPTIONAL_FIELDS.every((field) => {
      const reported = new Set(
        summaries
          .map((summary) => summary[field])
          .filter((value) => value !== null && value !== undefined && value !== ""),
      );
      return reported.size <= 1;
    });
  }

  // A Pending request becomes uncertifiable once the chain is past its expiry
  // height - the node's own admission rule is `height + 1 >= expires_at`.
  // This is a display classification only: the chain refunds a request only
  // once a refund transaction is mined, and this function never asserts that
  // a refund happened, only that one is now possible.
  function classifyExpiry(summary, currentHeight) {
    if (!summary || !summary.known) return { applicable: false, reason: "receipt-unknown" };
    if (summary.expiresAt === null) return { applicable: false, reason: "no-expiry-field" };
    if (summary.status !== "Pending") return { applicable: false, reason: "not-pending" };
    const height = Number.isSafeInteger(currentHeight) ? currentHeight : null;
    if (height === null) return { applicable: true, expired: null, reason: "chain-height-unavailable" };
    const expired = height + 1 >= summary.expiresAt;
    return {
      applicable: true,
      expired,
      height,
      message: expired
        ? "Expired without a certificate: refundable by a refund transaction. The chain refunds this only once that transaction is mined; no refund has been observed."
        : null,
    };
  }

  // `answers`: [{ source, receipt }] - one per replica that was asked.
  function compareReplicas(answers) {
    const rows = answers.map((a) => ({ source: a.source, summary: summarize(a.receipt) }));
    const known = rows.filter((r) => r.summary.known);
    const prints = new Set(known.map((r) => fingerprint(r.summary)));
    return {
      asked: rows.length,
      answered: known.length,
      agree:
        known.length > 0 &&
        prints.size === 1 &&
        optionalFieldsAgree(known.map((r) => r.summary)),
      rows,
    };
  }

  const NATIVE_TYPES = new Set([
    "NativeInferenceRequest",
    "NativeInferenceFinalize",
    "NativeInferenceRefund",
  ]);
  const NATIVE_TYPE_CODES = new Map([[0x26, "request"], [0x27, "finalize"], [0x28, "refund"]]);

  function nativeEvidenceLabels(classification, canonical) {
    const stage = classification?.nativeInferenceStage;
    if (!stage) return null;
    const stageLabel = ({ request: "Request", finalize: "Finalize", refund: "Refund" })[stage] || "Unknown";
    let outcome;
    if (classification.transactionHashMatches !== true) outcome = "Receipt identity does not match the requested transaction hash";
    else if (!classification.receiptBacked) outcome = "No transaction receipt observed";
    else if (!classification.mined) outcome = "Receipt does not prove mined inclusion";
    else if (!classification.success) outcome = "Mined transaction failed";
    else if (!canonical) outcome = "Mined, but canonical inclusion is not verified";
    else outcome = "Successful canonical mined transaction";
    const inference = stage === "finalize" && classification.transactionHashMatches === true
      && classification.inferenceConfirmed && canonical
      ? "Confirmed canonical finalization"
      : stage === "request"
        ? "Request admission is not inference completion"
        : stage === "refund"
          ? "Refund transaction; not an inference completion"
          : "Finalization is not confirmed";
    return { stage: stageLabel, outcome, inference };
  }

  // A /tx/{hash}/full response: its type, outcome and - for a native
  // transaction - the request it belongs to.
  function describeTransaction(full) {
    if (!full || typeof full !== "object") return { known: false };
    const tx = full.transaction || full.tx || full;
    const type = String(tx.tx_type ?? tx.type ?? full.tx_type ?? "Unknown");
    const normalizedType = type.trim().toLowerCase();
    const rawCode = tx.tx_type_code ?? full.tx_type_code;
    const code = typeof rawCode === "number" ? rawCode
      : /^0x[0-9a-f]+$/i.test(String(rawCode ?? "")) ? Number.parseInt(String(rawCode), 16)
        : /^0x[0-9a-f]+$/i.test(type) ? Number.parseInt(type, 16) : null;
    const stage = NATIVE_TYPE_CODES.get(code) ?? ({
      nativeinferencerequest: "request",
      nativeinferencefinalize: "finalize",
      nativeinferencerefund: "refund",
    })[normalizedType] ?? null;
    const body = tx.body ?? full.body ?? {};
    return {
      known: true,
      hash: String(tx.tx_hash ?? tx.hash ?? full.tx_hash ?? ""),
      type,
      success: (tx.success ?? full.success) === true,
      height: tx.block_height ?? full.block_height ?? null,
      native: stage !== null || NATIVE_TYPES.has(type),
      nativeStage: stage,
      requestId: stage !== null
        ? String(body.request_id ?? body.request?.request_id ?? "") || null
        : null,
    };
  }

  const api = { summarize, compareReplicas, describeTransaction, nativeEvidenceLabels, fingerprint, classifyExpiry };
  if (typeof module === "object" && module && module.exports) module.exports = api;
  root.ArcLocalDevNative = api;
  // The production explorer reads the same receipt rules under this name.
  root.ArcNativeReceipts = api;
})(typeof globalThis !== "undefined" ? globalThis : this);
