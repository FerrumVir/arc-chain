import { useState, useEffect } from 'react';
import { timeAgo, formatTimestamp } from '../utils';

interface TimeAgoProps {
  timestamp: number;
  className?: string;
}

export default function TimeAgo({ timestamp, className = '' }: TimeAgoProps) {
  const [display, setDisplay] = useState(() => timeAgo(timestamp));

  useEffect(() => {
    setDisplay(timeAgo(timestamp));
    const interval = setInterval(() => {
      setDisplay(timeAgo(timestamp));
    }, 10000);
    return () => clearInterval(interval);
  }, [timestamp]);

  return (
    <span
      className={`text-arc-grey-600 ${className}`}
      title={formatTimestamp(timestamp)}
    >
      {display}
    </span>
  );
}
