"use client";

// ---------------------------------------------------------------------------
// DataSourceBadge — inline pill showing live vs demo data source
// ---------------------------------------------------------------------------

interface DataSourceBadgeProps {
  source: "live" | "demo";
}

export default function DataSourceBadge({ source }: DataSourceBadgeProps) {
  const isLive = source === "live";

  return (
    <span
      className={`
        inline-flex items-center gap-1.5 px-2 py-0.5 rounded-full
        text-[11px] font-medium leading-none select-none
        ${
          isLive
            ? "bg-[var(--shield-green-bg)] text-[var(--shield-green)]"
            : "bg-[var(--shield-yellow-bg)] text-[var(--shield-yellow)]"
        }
      `}
    >
      {/* Status dot */}
      <span className="relative flex h-1.5 w-1.5 shrink-0">
        {isLive && (
          <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-[var(--shield-green)] opacity-60" />
        )}
        <span
          className={`relative inline-flex h-1.5 w-1.5 rounded-full ${
            isLive
              ? "bg-[var(--shield-green)]"
              : "bg-[var(--shield-yellow)]"
          }`}
        />
      </span>

      {isLive ? "Live" : "Demo"}
    </span>
  );
}
