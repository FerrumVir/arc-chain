import type { LucideIcon } from "lucide-react";
import { useTheme } from "../lib/theme";
import { Vignette, type VignetteKind } from "./Vignette";

// In the engraved theme an empty state shows a small engraving of what is
// missing instead of a stock icon; call sites stay the same.
const ENGRAVED: Record<string, VignetteKind> = {
  FileSignature: "tablet",
  ScrollText: "leaves",
  Blocks: "stones",
};

export function EmptyState({
  icon: Icon,
  title,
  description,
}: {
  icon: LucideIcon;
  title: string;
  description: string;
}) {
  const theme = useTheme();
  const kind = theme === "engraved" ? ENGRAVED[Icon.displayName ?? ""] : undefined;
  return (
    <div className="empty">
      {kind ? <Vignette kind={kind} /> : <Icon className="empty-icon" />}
      <div className="empty-title">{title}</div>
      <div className="empty-description">{description}</div>
    </div>
  );
}
