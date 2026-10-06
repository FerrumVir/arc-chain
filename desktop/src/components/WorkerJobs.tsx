import { useQuery } from "@tanstack/react-query";
import { Card, CardHeader } from "./Card";
import { Skeleton } from "./Skeleton";
import { api } from "../lib/tauri";
import { formatInt, formatRelativeTime } from "../lib/format";
import { useAppStore } from "../lib/store";
import { computeContributionEnabled, type WorkerStatus } from "../lib/types";

const STATE_COPY: Record<string, string> = {
  polling: "Ready: waiting for a job",
  computing: "Computing a job now",
  reconnecting: "Reconnecting to coordinators",
};

/** One sentence for the worker's current state, from its own node. */
export function workerStateCopy(status: WorkerStatus): string {
  if (!status.running) return "Not taking jobs";
  return (status.state && STATE_COPY[status.state]) || "Running";
}

function count(value: number | null): string {
  return value === null ? "—" : formatInt(value);
}

/**
 * What this computer has done for the network, read from its own node.
 *
 * "Completed" counts results a coordinator accepted; "verified" counts the
 * accepted results the coordinator reported as passing its replica quorum.
 * Both cover the time since the node last started. They are local
 * observations, not payment: rewards appear only as mined receipts.
 */
export function WorkerJobsCard() {
  const config = useAppStore((s) => s.config);
  const contributing = computeContributionEnabled(config);
  const { data: status } = useQuery({
    queryKey: ["worker-status"],
    queryFn: api.fetchWorkerStatus,
    refetchInterval: 5000,
  });

  return (
    <Card style={{ marginBottom: "var(--space-6)" }} data-testid="worker-jobs">
      <CardHeader
        title="Jobs on this computer"
        action={
          status?.running ? (
            <span
              data-testid="worker-jobs-state"
              style={{ fontSize: "var(--text-sm)", color: "var(--text-muted)" }}
            >
              {workerStateCopy(status)}
            </span>
          ) : undefined
        }
      />
      {status === undefined ? (
        <Skeleton width="60%" />
      ) : status.running ? (
        <>
          <div
            style={{
              display: "grid",
              gridTemplateColumns: "repeat(auto-fit, minmax(140px, 1fr))",
              gap: "var(--space-4)",
            }}
          >
            <div>
              <div className="stat-label">Completed</div>
              <div className="stat-value mono" data-testid="worker-jobs-completed">
                {count(status.jobsCompleted)}
              </div>
            </div>
            <div>
              <div className="stat-label">Verified by the network</div>
              <div className="stat-value mono" data-testid="worker-jobs-verified">
                {count(status.jobsVerified)}
              </div>
            </div>
            <div>
              <div className="stat-label">Coordinators</div>
              <div className="stat-value mono" data-testid="worker-jobs-coordinators">
                {status.coordinatorsRegistered === null
                  ? "—"
                  : `${status.coordinatorsRegistered} of ${status.coordinatorsTotal ?? "?"}`}
              </div>
            </div>
          </div>
          <p
            style={{
              marginTop: "var(--space-3)",
              fontSize: "var(--text-sm)",
              color: "var(--text-muted)",
              lineHeight: 1.5,
            }}
            data-testid="worker-jobs-detail"
          >
            {status.lastJobCompletedUnixMs !== null
              ? `Last job ${formatRelativeTime(status.lastJobCompletedUnixMs)}. `
              : "No job completed since the node started. "}
            Counted since the node last started; a job is paid only through a
            mined reward receipt.
            {status.publicName ? ` Scoreboards show this computer as ${status.publicName}.` : ""}
          </p>
        </>
      ) : (
        <p
          style={{ fontSize: "var(--text-sm)", color: "var(--text-muted)", lineHeight: 1.5 }}
          data-testid={contributing ? "worker-jobs-unavailable" : "worker-jobs-off"}
        >
          {contributing
            ? (status.unavailable ?? "The worker is starting.")
            : "Compute contribution is off. Turn it on in Settings to take ARC jobs on this computer."}
        </p>
      )}
    </Card>
  );
}
