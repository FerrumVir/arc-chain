import type { LucideIcon } from "lucide-react";

export function EmptyState({
  icon: Icon,
  title,
  description,
}: {
  icon: LucideIcon;
  title: string;
  description: string;
}) {
  return (
    <div className="empty">
      <Icon className="empty-icon" />
      <div className="empty-title">{title}</div>
      <div className="empty-description">{description}</div>
    </div>
  );
}
