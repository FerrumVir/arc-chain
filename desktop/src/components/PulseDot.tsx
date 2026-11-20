import clsx from "clsx";
import type { HealthLevel } from "../lib/types";

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
