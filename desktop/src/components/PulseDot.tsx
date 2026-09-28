import clsx from "clsx";
import type { HealthLevel } from "../lib/types";

/** "checking" is the UI's own state: no status has arrived yet, so nothing is known about the node. */
export type DotLevel = HealthLevel | "checking";

export function PulseDot({
  level,
  className,
  ariaLabel,
}: {
  level: DotLevel;
  className?: string;
  ariaLabel?: string;
}) {
  return (
    <span
      role="status"
      aria-label={ariaLabel ?? level}
      data-testid={`pulse-${level}`}
      className={clsx("pulse-dot", level, className)}
    />
  );
}
