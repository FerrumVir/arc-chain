import clsx from "clsx";
import type { ReactNode } from "react";

export function Card({
  children,
  className,
  featured = false,
  hoverable = false,
  ...props
}: {
  children: ReactNode;
  className?: string;
  featured?: boolean;
  hoverable?: boolean;
} & React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      className={clsx(
        "card",
        featured && "card-featured",
        hoverable && "card-hoverable",
        className,
      )}
      {...props}
    >
      {children}
    </div>
  );
}

export function CardHeader({
  title,
  action,
}: {
  title: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div className="card-header">
      <h3 className="card-title">{title}</h3>
      {action}
    </div>
  );
}
