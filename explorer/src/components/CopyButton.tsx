import { useState, useCallback } from 'react';
import { copyToClipboard } from '../utils';

interface CopyButtonProps {
  text: string;
  className?: string;
}

export default function CopyButton({ text, className = '' }: CopyButtonProps) {
  const [copied, setCopied] = useState(false);

  const handleCopy = useCallback(async () => {
    const ok = await copyToClipboard(text);
    if (ok) {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    }
  }, [text]);

  return (
    <span className="relative inline-flex">
      <button
        onClick={handleCopy}
        className={`
          inline-flex items-center justify-center w-7 h-7
          text-arc-grey-600 hover:text-arc-aquarius
          transition-all duration-150 cursor-pointer
          ${copied ? 'text-arc-success scale-110' : ''}
          ${className}
        `}
        title="Copy to clipboard"
        type="button"
      >
        {copied ? (
          <svg
            width="14"
            height="14"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2.5"
            strokeLinecap="round"
            strokeLinejoin="round"
            className="animate-toast"
          >
            <polyline points="20 6 9 17 4 12" />
          </svg>
        ) : (
          <svg
            width="14"
            height="14"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <rect x="9" y="9" width="13" height="13" rx="1" />
            <path d="M5 15H4a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h10a1 1 0 0 1 1 1v1" />
          </svg>
        )}
      </button>

      {/* Mini toast */}
      {copied && (
        <span className="absolute -top-7 left-1/2 -translate-x-1/2 px-2 py-0.5 text-[10px] text-arc-success bg-arc-surface-raised border border-arc-border whitespace-nowrap animate-toast z-50">
          Copied
        </span>
      )}
    </span>
  );
}
