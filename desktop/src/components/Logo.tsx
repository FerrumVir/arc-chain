// ARC brand marks. Per brandpad.io/arc:
//   - "arc" is the wordmark, always lowercase
//   - Logo icon = white "arc" housed in a solid-colour or gradient container
//     (primary use: profile images, app icons, avatars)
//   - Clear space ≥ the width of the lowercase "a"
//   - "ai for humans first" is the companion tagline
//
// If SVG assets are present in src/assets/brand/, they're used. Otherwise
// the component falls back to a CSS-rendered wordmark. See
// src/assets/brand/README.md for which files to drop.

import type { CSSProperties } from "react";

// Vite's ?url import resolves to the asset URL if the file exists, and
// throws at build time if it doesn't — so the catch + undefined pattern
// below gives a safe default. (Using { eager: false } import.meta.glob
// keeps it tree-shakable.)
const assetModules = import.meta.glob<{ default: string }>(
  "../assets/brand/arc-logo-white.svg",
  { eager: true, query: "?url", import: "default" },
);
const WHITE_WORDMARK_URL: string | undefined = Object.values(assetModules)[0]
  ?.default as string | undefined;

const gradientAssetModules = import.meta.glob<{ default: string }>(
  "../assets/brand/arc-logo-full-gradient.svg",
  { eager: true, query: "?url", import: "default" },
);
const GRADIENT_LOGO_URL: string | undefined = Object.values(
  gradientAssetModules,
)[0]?.default as string | undefined;

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
  /** solid (default, for app icons + sidebar) or gradient (hero / marketing). */
  variant?: "solid" | "gradient";
  className?: string;
}) {
  const r = radius ?? Math.round(size * 0.26);
  const showFullWord = size >= 40;
  const fontSize = Math.round(size * (showFullWord ? 0.38 : 0.55));

  // If the brand pack has been dropped into src/assets/brand/, use the
  // real SVG. Otherwise fall back to the CSS wordmark.
  const svgUrl =
    variant === "gradient" ? GRADIENT_LOGO_URL : WHITE_WORDMARK_URL;

  const containerStyle: CSSProperties = {
    width: size,
    height: size,
    borderRadius: r,
    background: variant === "gradient" ? "var(--arc-gradient)" : "var(--arc)",
    display: "grid",
    placeItems: "center",
    color: "white",
    fontFamily: "var(--font-display)",
    fontWeight: 500,
    fontSize,
    letterSpacing: "-0.045em",
    lineHeight: 1,
    paddingBottom: Math.max(1, Math.round(size * 0.04)),
    boxShadow: glow ? "var(--shadow-glow)" : undefined,
    overflow: "hidden",
  };

  return (
    <span
      aria-label="arc"
      role="img"
      className={className}
      style={containerStyle}
      data-testid="logo-mark"
      data-logo-source={svgUrl ? "svg" : "css"}
    >
      {svgUrl ? (
        <img
          src={svgUrl}
          alt=""
          width={size * 0.6}
          height={size * 0.6}
          style={{ filter: "brightness(0) invert(1)" }}
        />
      ) : showFullWord ? (
        "arc"
      ) : (
        "a"
      )}
    </span>
  );
}

export function Wordmark({
  size = 28,
  className,
  style,
}: {
  size?: number;
  className?: string;
  style?: CSSProperties;
}) {
  return (
    <span
      className={className}
      data-testid="wordmark"
      style={{
        fontFamily: "var(--font-display)",
        fontWeight: 500,
        fontSize: size,
        letterSpacing: "-0.045em",
        lineHeight: 1,
        color: "var(--text)",
        ...style,
      }}
    >
      arc
    </span>
  );
}

// The ARC-shape device. An abstract crop of a circle within a square.
// Used sparingly — as a secondary accent inside empty states, section
// dividers, and marketing hero areas.
export function ArcDevice({
  size = 24,
  stroke = 2.4,
  color = "currentColor",
  className,
}: {
  size?: number;
  stroke?: number;
  color?: string;
  className?: string;
}) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      className={className}
      aria-hidden
    >
      {/* the "arc": open at the bottom, a half-circle cap */}
      <path
        d="M 3 15 A 9 9 0 0 1 21 15"
        stroke={color}
        strokeWidth={stroke}
        strokeLinecap="round"
      />
      {/* the "square": a minimal hinting line along the bottom */}
      <path
        d="M 3 20 L 21 20"
        stroke={color}
        strokeWidth={stroke}
        strokeLinecap="round"
        opacity={0.5}
      />
    </svg>
  );
}

export function Tagline({
  size = "sm",
  className,
}: {
  size?: "xs" | "sm" | "md" | "lg";
  className?: string;
}) {
  const fontSize = {
    xs: "10px",
    sm: "12px",
    md: "14px",
    lg: "16px",
  }[size];
  return (
    <span
      className={className}
      data-testid="tagline"
      style={{
        fontSize,
        letterSpacing: "0.04em",
        color: "var(--text-muted)",
        textTransform: "lowercase",
        fontWeight: 400,
      }}
    >
      ai for humans first
    </span>
  );
}
