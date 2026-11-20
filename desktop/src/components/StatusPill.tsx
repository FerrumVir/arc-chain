import clsx from "clsx";
import { PLAIN_COPY } from "../lib/plain-copy";
import { PulseDot, type DotLevel } from "./PulseDot";

const labels: Record<DotLevel | "info", string> = {
  checking: PLAIN_COPY.connection.labels.checking,
  live: PLAIN_COPY.connection.labels.connected,
  lite: PLAIN_COPY.connection.labels.connected,
  syncing: PLAIN_COPY.connection.labels.connecting,
  offline: PLAIN_COPY.connection.labels.unreachable,
  off: PLAIN_COPY.connection.labels.off,
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
  const text = label ?? labels[level];
  return (
    <span
      className={clsx("status-pill", level)}
      data-testid={`status-pill-${level}`}
    >
      {showDot && level !== "info" && (
        <PulseDot level={level as DotLevel} ariaLabel={text} />
      )}
      {text}
    </span>
  );
}
