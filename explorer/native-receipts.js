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
    const outputHex = typeof receipt.output_hex === "string" ? receipt.output_hex : "";
    return {
      known: true,
      requestId: String(receipt.request_id ?? ""),
      status,
      terminal,
      price: Number(receipt.execution_price ?? NaN),
      reserved,
      credited,
      reconciled: terminal ? Number.isFinite(reserved) && credited === reserved : null,
      credits: credits.map((c) => ({ payee: String(c?.payee ?? ""), amount: Number(c?.amount ?? 0) })),
      outputHash: receipt.output_hash ?? null,
      votes: receipt.certificate_votes ?? null,
      admissionHeight: receipt.admission_transaction?.block_height ?? receipt.admission_height ?? null,
      terminalHeight: receipt.terminal_transaction?.block_height ?? null,
      expiresAt: typeof receipt.expires_at === "number" ? receipt.expires_at : null,
      requester: typeof receipt.requester === "string" && receipt.requester ? receipt.requester : null,
      outputHex,
      outputCertified: outputHex.length > 0,
      outputText: typeof receipt.output_text === "string" ? receipt.output_text : null,
    };
  }

  function fingerprint(summary) {
    if (!summary.known) return null;
    const credits = summary.credits
      .map((c) => `${c.payee}:${c.amount}`)
      .sort()
      .join(",");
    return [summary.status, summary.outputHash ?? "", summary.terminalHeight ?? "", credits].join("|");
  }

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
  root.ArcNativeReceipts = api;
})(typeof globalThis !== "undefined" ? globalThis : this);
