// "Measured records": one-time measurements with public receipts, kept apart
// from the live numbers. The list itself is static JSON
// (measured-records.json, schema `arc.measured-records.v1`) so it can be
// reviewed line by line and reused by the arc.ai website. Every record must
// carry a date, the pull request it came from and at least one receipt link to
// a public CI run in this repository.

export const MEASURED_RECORDS_SCHEMA = "arc.measured-records.v1";

const RECEIPT_PREFIX = "https://github.com/FerrumVir/arc-chain/actions/runs/";
const PR_PREFIX = "https://github.com/FerrumVir/arc-chain/pull/";

export interface MeasuredReceipt {
  label: string;
  url: string;
}

export interface MeasuredRecord {
  id: string;
  /** The figure or claim, in plain words. */
  headline: string;
  /** What exactly was measured, where, and what it does not cover. */
  detail: string;
  /** Where it was measured: "lab" (test machines) or "ci" (public CI runners). Never the live network. */
  setting: "lab" | "ci";
  /** ISO date, YYYY-MM-DD. */
  measured_on: string;
  prs: number[];
  receipts: MeasuredReceipt[];
}

export interface MeasuredRecords {
  schema: typeof MEASURED_RECORDS_SCHEMA;
  records: MeasuredRecord[];
}

function isText(value: unknown): value is string {
  return typeof value === "string" && value.trim() !== "";
}

/** Check the static list. Returns the problems found; an empty list means it is valid. */
export function measuredRecordsProblems(raw: unknown): string[] {
  const problems: string[] = [];
  const doc = raw !== null && typeof raw === "object" ? (raw as Record<string, unknown>) : {};
  if (doc.schema !== MEASURED_RECORDS_SCHEMA) problems.push(`schema must be ${MEASURED_RECORDS_SCHEMA}`);
  if (!Array.isArray(doc.records) || doc.records.length === 0) {
    problems.push("records must be a non-empty list");
    return problems;
  }
  const ids = new Set<string>();
  doc.records.forEach((value: unknown, index: number) => {
    const r = value !== null && typeof value === "object" ? (value as Record<string, unknown>) : {};
    const where = isText(r.id) ? r.id : `record ${index}`;
    if (!isText(r.id)) problems.push(`${where}: id is missing`);
    else if (ids.has(r.id)) problems.push(`${where}: id is repeated`);
    else ids.add(r.id);
    if (!isText(r.headline)) problems.push(`${where}: headline is missing`);
    if (!isText(r.detail)) problems.push(`${where}: detail is missing`);
    if (r.setting !== "lab" && r.setting !== "ci") problems.push(`${where}: setting must be "lab" or "ci"`);
    if (!isText(r.measured_on) || !/^\d{4}-\d{2}-\d{2}$/.test(r.measured_on)) {
      problems.push(`${where}: measured_on must be a YYYY-MM-DD date`);
    }
    if (
      !Array.isArray(r.prs) ||
      r.prs.length === 0 ||
      !r.prs.every((pr: unknown) => typeof pr === "number" && Number.isSafeInteger(pr) && pr > 0)
    ) {
      problems.push(`${where}: prs must list the pull requests it came from`);
    }
    if (!Array.isArray(r.receipts) || r.receipts.length === 0) {
      problems.push(`${where}: at least one receipt is required`);
    } else {
      r.receipts.forEach((receipt: unknown, n: number) => {
        const rc =
          receipt !== null && typeof receipt === "object" ? (receipt as Record<string, unknown>) : {};
        if (!isText(rc.label)) problems.push(`${where}: receipt ${n} has no label`);
        if (!isText(rc.url) || !/^\d+$/.test(rc.url.slice(RECEIPT_PREFIX.length)) || !rc.url.startsWith(RECEIPT_PREFIX)) {
          problems.push(`${where}: receipt ${n} must link a public CI run (${RECEIPT_PREFIX}<id>)`);
        }
      });
    }
  });
  return problems;
}

export function pullRequestUrl(pr: number): string {
  return `${PR_PREFIX}${pr}`;
}

/** "6 Oct 2026" from "2026-10-06", without time-zone drift. */
export function formatRecordDate(isoDate: string): string {
  const [year, month, day] = isoDate.split("-").map(Number);
  const months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
  if (!year || !month || !day || month < 1 || month > 12) return isoDate;
  return `${day} ${months[month - 1]} ${year}`;
}
