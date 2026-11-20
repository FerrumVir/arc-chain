import type { CSSProperties } from "react";
import wordmarkUrl from "../assets/brand/arc-logo-white.svg";

const WORDMARK_ASPECT = 241.87 / 94.04;

export function Wordmark({
  height = 24,
  decorative = false,
  className,
  style,
}: {
  height?: number;
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
