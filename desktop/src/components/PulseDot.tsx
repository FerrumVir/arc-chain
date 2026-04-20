import clsx from "clsx";
import type { HealthLevel } from "../lib/types";

export function PulseDot({
  level,
  className,
  ariaLabel,
}: {
  level: HealthLevel;
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
