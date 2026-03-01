interface BadgeProps {
  variant: 'success' | 'error' | 'warning' | 'info' | 'neutral';
  children: React.ReactNode;
  size?: 'sm' | 'md';
}

const variantStyles: Record<BadgeProps['variant'], string> = {
  success: 'bg-arc-success/10 text-arc-success border-arc-success/20',
  error: 'bg-arc-error/10 text-arc-error border-arc-error/20',
  warning: 'bg-arc-warning/10 text-arc-warning border-arc-warning/20',
  info: 'bg-arc-info/10 text-arc-info border-arc-info/20',
  neutral: 'bg-arc-grey-700/10 text-arc-grey-500 border-arc-grey-700/20',
};

const sizeStyles: Record<NonNullable<BadgeProps['size']>, string> = {
  sm: 'px-2 py-0.5 text-[11px]',
  md: 'px-2.5 py-1 text-xs',
};

export default function Badge({
  variant,
  children,
  size = 'sm',
}: BadgeProps) {
  return (
    <span
      className={`
        inline-flex items-center font-medium tracking-wide uppercase border
        ${variantStyles[variant]}
        ${sizeStyles[size]}
      `}
    >
      {children}
    </span>
  );
}
