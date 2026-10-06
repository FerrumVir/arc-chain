// "Measured records": one-time measurements with public receipts, kept apart
// from the live numbers. The list itself is static JSON
// (measured-records.json, schema `arc.measured-records.v1`) so it can be
// reviewed line by line and reused by the arc.ai website.
//
// Every record carries a date, the pull requests it came from, where it was
// measured, and at least one receipt: a public CI run in this repository, or
// an evidence file pinned to a commit. A model-speed record (for example a
// Kimi-class model at some tokens per second) also names the exact model, the
// hardware, the metric, the measured value and its unit. Projections are not
// records.

export const MEASURED_RECORDS_SCHEMA = "arc.measured-records.v1";

const REPO = "https://github.com/FerrumVir/arc-chain";
const PR_PREFIX = `${REPO}/pull/`;
/** A CI run (optionally one job) or a file at an exact commit. */
const RECEIPT_URL =
  /^https:\/\/github\.com\/FerrumVir\/arc-chain\/(actions\/runs\/\d+(\/job\/\d+)?|blob\/[0-9a-f]{40}\/\S+)$/;

/**
 * Where a record was measured. Never a projection.
 * - `lab`: a staged test setup, such as several validators on one machine;
 * - `ci`: public CI runners;
 * - `testnet`: once, on the public testnet, at the stated date.
 */
export type RecordSetting = "lab" | "ci" | "testnet";

export const RECORD_SETTINGS: readonly RecordSetting[] = ["lab", "ci", "testnet"];

export const RECORD_SETTING_LABELS: Record<RecordSetting, string> = {
  lab: "Lab test",
  ci: "CI runners",
  testnet: "Public testnet, one-time",
};

/** What a record's `value` measures. */
export type RecordMetric =
  | "transfers_per_second"
  | "answer_tokens_per_second"
  | "served_tokens_per_second"
  | "time_to_first_token_seconds";

export const RECORD_METRICS: readonly RecordMetric[] = [
  "transfers_per_second",
  "answer_tokens_per_second",
  "served_tokens_per_second",
  "time_to_first_token_seconds",
];

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
  setting: RecordSetting;
  /** ISO date, YYYY-MM-DD. */
  measured_on: string;
  prs: number[];
  receipts: MeasuredReceipt[];
  /** The exact model, for model records (e.g. a Kimi-class model's full name and revision). */
  model?: string;
  /** The machines it ran on. */
  hardware?: string;
  /** The measured figure, when the record has one: all three together. */
  metric?: RecordMetric;
  value?: number;
  unit?: string;
}

export interface MeasuredRecords {
  schema: typeof MEASURED_RECORDS_SCHEMA;
  records: MeasuredRecord[];
}

function isText(value: unknown): value is string {
  return typeof value === "string" && value.trim() !== "";
}

/** Check the list. Returns the problems found; an empty list means it is valid. */
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
    if (!RECORD_SETTINGS.includes(r.setting as RecordSetting)) {
      problems.push(`${where}: setting must be one of ${RECORD_SETTINGS.join(", ")} (a projection is not a record)`);
    }
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
        if (!isText(rc.url) || !RECEIPT_URL.test(rc.url)) {
          problems.push(
            `${where}: receipt ${n} must be a public CI run or a commit-pinned file in ${REPO}`,
          );
        }
      });
    }
    for (const key of ["model", "hardware"] as const) {
      if (r[key] !== undefined && !isText(r[key])) problems.push(`${where}: ${key} must be text`);
    }
    const figure = [r.metric, r.value, r.unit];
    if (figure.some((part) => part !== undefined)) {
      if (!RECORD_METRICS.includes(r.metric as RecordMetric)) {
        problems.push(`${where}: metric must be one of ${RECORD_METRICS.join(", ")}`);
      }
      if (typeof r.value !== "number" || !Number.isFinite(r.value) || r.value <= 0) {
        problems.push(`${where}: value must be the measured number`);
      }
      if (!isText(r.unit)) problems.push(`${where}: unit is missing`);
    }
    if (
      (r.metric === "answer_tokens_per_second" || r.metric === "served_tokens_per_second") &&
      (!isText(r.model) || !isText(r.hardware))
    ) {
      problems.push(`${where}: a model-speed record must name the exact model and the hardware`);
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
