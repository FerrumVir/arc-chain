import clsx from "clsx";
import type { HealthLevel } from "../lib/types";

/**
 * "checking" is the UI's own state: no status has arrived yet, so nothing is
 * known about the node. "off" is a node that is not running by choice, which
 * is not a fault and takes no colour. Callers pass the plain label they show
 * next to the dot (lib/connection.ts), so a screen reader never hears "lite".
 */
export type DotLevel = HealthLevel | "checking" | "off";

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
