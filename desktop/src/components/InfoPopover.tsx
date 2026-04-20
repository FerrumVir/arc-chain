import { Info, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { ReactNode } from "react";

// An accessible, click-to-open info popover. Used for in-context explainers
// (e.g. "what is an attestation?"). Keyboard accessible (Enter/Space to open,
// Esc to close). Click-outside dismisses.
export function InfoPopover({
  title,
  children,
  ariaLabel,
}: {
  title: string;
  children: ReactNode;
  ariaLabel?: string;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLSpanElement>(null);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  return (
    <span
      ref={ref}
      style={{ position: "relative", display: "inline-flex" }}
    >
      <button
        type="button"
        className="info-btn"
        aria-label={ariaLabel ?? `What is ${title.toLowerCase()}?`}
        aria-expanded={open}
        onClick={(e) => {
          e.stopPropagation();
          setOpen((v) => !v);
        }}
        data-testid="info-btn"
      >
        <Info size={12} strokeWidth={2.25} />
      </button>
      {open && (
        <div
          role="dialog"
          aria-label={title}
          className="info-popover"
          data-testid="info-popover"
          onMouseDown={(e) => e.stopPropagation()}
        >
          <div className="info-popover-header">
            <span>{title}</span>
            <button
              type="button"
              className="info-popover-close"
              onClick={() => setOpen(false)}
              aria-label="Close"
            >
              <X size={12} />
            </button>
          </div>
          <div className="info-popover-body">{children}</div>
        </div>
      )}
    </span>
  );
}
