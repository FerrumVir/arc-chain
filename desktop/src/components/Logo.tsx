// ARC brand marks, drawn only from the real arc.ai wordmark: the same file the
// website uses, copied to src/assets/brand/arc-logo-white.svg (white on
// transparent, viewBox 0 0 241.87 94.04). Per the brand:
//   - "arc" is the wordmark, always lowercase, and is never typed in a font
//   - the logo icon is that white wordmark housed in a solid or gradient container
//   - "Own your AI" is the tagline
//
// The asset is imported directly rather than looked up, so a build without it
// fails instead of quietly falling back to a placeholder. See
// src/assets/brand/README.md.

import type { CSSProperties } from "react";
import wordmarkUrl from "../assets/brand/arc-logo-white.svg";

/** Width over height of the wordmark's viewBox (241.87 x 94.04). */
const WORDMARK_ASPECT = 241.87 / 94.04;

/** The arc wordmark, white, at a given height in CSS pixels. */
export function Wordmark({
  height = 24,
  decorative = false,
  className,
  style,
}: {
  height?: number;
  /** True when a surrounding element already names the brand. */
  decorative?: boolean;
  className?: string;
  style?: CSSProperties;
}) {
  return (
    <img
      src={wordmarkUrl}
      alt={decorative ? "" : "arc"}
      aria-hidden={decorative || undefined}
      width={Math.round(height * WORDMARK_ASPECT)}
      height={height}
      draggable={false}
      className={className}
      data-testid={decorative ? undefined : "wordmark"}
      style={{ display: "block", flexShrink: 0, ...style }}
    />
  );
}

/**
 * The logo icon: the white wordmark in a solid Arc-blue or brand-gradient
 * container, as on the app icon and the website's favicon.
 */
export function LogoMark({
  size = 28,
  radius,
  glow = true,
  variant = "solid",
  className,
}: {
  size?: number;
  radius?: number;
  glow?: boolean;
  /** solid (default) or gradient (the onboarding hero). */
  variant?: "solid" | "gradient";
  className?: string;
}) {
  const containerStyle: CSSProperties = {
    width: size,
    height: size,
    borderRadius: radius ?? Math.round(size * 0.24),
    background: variant === "gradient" ? "var(--arc-gradient)" : "var(--arc)",
    display: "grid",
    placeItems: "center",
    flexShrink: 0,
    boxShadow: glow ? "var(--shadow-glow)" : undefined,
  };
  // The wordmark fills about 70% of the container's width, as in the favicon.
  return (
    <span
      role="img"
      aria-label="arc"
      className={className}
      style={containerStyle}
      data-testid="logo-mark"
    >
      <Wordmark height={Math.max(8, Math.round((size * 0.7) / WORDMARK_ASPECT))} decorative />
    </span>
  );
}

export function Tagline({
  size = "sm",
  className,
  style,
}: {
  size?: "xs" | "sm" | "md" | "lg";
  className?: string;
  style?: CSSProperties;
}) {
  const fontSize = {
    xs: "11px",
    sm: "13px",
    md: "14px",
    lg: "16px",
  }[size];
  return (
    <span
      className={className}
      data-testid="tagline"
      style={{
        fontSize,
        lineHeight: 1.3,
        letterSpacing: "0.01em",
        color: "var(--text-muted)",
        fontWeight: 400,
        ...style,
      }}
    >
      Own your AI
    </span>
  );
}
