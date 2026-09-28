import clsx from "clsx";
import type { CSSProperties } from "react";

// A placeholder for content that is still loading, the same size as what will replace it, so nothing jumps when the
// data arrives. It is decoration (aria-hidden): the region that is loading should carry aria-busy, and the app never
// shows a number, a verdict or an empty message in place of data it has not read yet (see NotAvailable).
export function Skeleton({
  width = "100%",
  height = "1em",
  radius,
  block = false,
  className,
  style,
}: {
  width?: CSSProperties["width"];
  height?: CSSProperties["height"];
  radius?: CSSProperties["borderRadius"];
  /** Take a whole line instead of sitting inline with text. */
  block?: boolean;
  className?: string;
  style?: CSSProperties;
}) {
  return (
    <span
      aria-hidden="true"
      data-testid="skeleton"
      className={clsx("skeleton shimmer", block && "skeleton-block", className)}
      style={{ width, height, borderRadius: radius, ...style }}
    />
  );
}

/** A few lines of text still loading: full-width lines and a shorter last one. */
export function SkeletonLines({ lines = 2, gap = 8 }: { lines?: number; gap?: number }) {
  return (
    <span className="skeleton-lines" style={{ gap }} aria-hidden="true">
      {Array.from({ length: lines }, (_, i) => (
        <Skeleton key={i} block height="0.8em" width={i === lines - 1 && lines > 1 ? "62%" : "100%"} />
      ))}
    </span>
  );
}
