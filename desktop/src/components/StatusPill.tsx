import clsx from "clsx";
import { PulseDot } from "./PulseDot";
import type { HealthLevel } from "../lib/types";

const labels: Record<HealthLevel | "info", string> = {
  live: "Live",
  syncing: "Syncing",
  offline: "Offline",
  info: "Info",
};

export function StatusPill({
  level,
  label,
  showDot = true,
}: {
  level: HealthLevel | "info";
  label?: string;
  showDot?: boolean;
}) {
  return (
    <span
      className={clsx("status-pill", level)}
      data-testid={`status-pill-${level}`}
    >
      {showDot && level !== "info" && (
        <PulseDot level={level as HealthLevel} />
      )}
      {label ?? labels[level]}
    </span>
  );
}
