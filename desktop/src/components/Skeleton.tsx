import clsx from "clsx";
import type { CSSProperties } from "react";

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

export function SkeletonLines({ lines = 2, gap = 8 }: { lines?: number; gap?: number }) {
  return (
    <span className="skeleton-lines" style={{ gap }} aria-hidden="true">
      {Array.from({ length: lines }, (_, i) => (
        <Skeleton key={i} block height="0.8em" width={i === lines - 1 && lines > 1 ? "62%" : "100%"} />
      ))}
    </span>
  );
}
