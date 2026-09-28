import clsx from "clsx";
import { PulseDot, type DotLevel } from "./PulseDot";

const labels: Record<DotLevel | "info", string> = {
  checking: "Checking",
  live: "Live",
  lite: "Lite",
  syncing: "Syncing",
  offline: "Offline",
  info: "Info",
};

export function StatusPill({
  level,
  label,
  showDot = true,
}: {
  level: DotLevel | "info";
  label?: string;
  showDot?: boolean;
}) {
  return (
    <span
      className={clsx("status-pill", level)}
      data-testid={`status-pill-${level}`}
    >
      {showDot && level !== "info" && (
        <PulseDot level={level as DotLevel} />
      )}
      {label ?? labels[level]}
    </span>
  );
}
