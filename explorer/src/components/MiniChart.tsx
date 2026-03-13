interface MiniChartProps {
  data: number[];
  color?: string;
  width?: number;
  height?: number;
  className?: string;
}

export default function MiniChart({
  data,
  color = '#6F7CF4',
  width = 80,
  height = 28,
  className = '',
}: MiniChartProps) {
  if (data.length < 2) return null;

  const min = Math.min(...data);
  const max = Math.max(...data);
  const range = max - min || 1;
  const padding = 2;

  const points = data.map((v, i) => {
    const x = (i / (data.length - 1)) * (width - padding * 2) + padding;
    const y = height - padding - ((v - min) / range) * (height - padding * 2);
    return `${x},${y}`;
  });

  const pathD = `M ${points.join(' L ')}`;

  // Area fill path
  const areaD = `${pathD} L ${width - padding},${height - padding} L ${padding},${height - padding} Z`;

  return (
    <svg
      width={width}
      height={height}
      viewBox={`0 0 ${width} ${height}`}
      className={className}
    >
      <defs>
        <linearGradient id={`sparkGrad-${color.replace('#', '')}`} x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor={color} stopOpacity="0.2" />
          <stop offset="100%" stopColor={color} stopOpacity="0" />
        </linearGradient>
      </defs>
      <path
        d={areaD}
        fill={`url(#sparkGrad-${color.replace('#', '')})`}
      />
      <path
        d={pathD}
        className="sparkline-path"
        stroke={color}
      />
    </svg>
  );
}
