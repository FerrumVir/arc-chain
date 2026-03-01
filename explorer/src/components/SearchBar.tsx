import { useState, useCallback } from 'react';
import { useNavigate } from 'react-router-dom';
import { detectSearchType } from '../utils';

interface SearchBarProps {
  className?: string;
}

export default function SearchBar({ className = '' }: SearchBarProps) {
  const [query, setQuery] = useState('');
  const [error, setError] = useState('');
  const navigate = useNavigate();

  const handleSearch = useCallback(() => {
    const trimmed = query.trim();
    if (!trimmed) return;

    setError('');
    const type = detectSearchType(trimmed);

    switch (type) {
      case 'block':
        navigate(`/block/${trimmed}`);
        break;
      case 'tx': {
        const hash = trimmed.startsWith('0x') ? trimmed.slice(2) : trimmed;
        navigate(`/tx/${hash}`);
        break;
      }
      case 'account': {
        const addr = trimmed.startsWith('0x') ? trimmed.slice(2) : trimmed;
        navigate(`/account/${addr}`);
        break;
      }
      default:
        setError('Enter a block height, tx hash, or address');
        return;
    }

    setQuery('');
  }, [query, navigate]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (e.key === 'Enter') {
        handleSearch();
      }
    },
    [handleSearch]
  );

  return (
    <div className={`relative flex-1 max-w-xl ${className}`}>
      <div className="relative flex items-center">
        {/* Search icon */}
        <svg
          className="absolute left-3.5 text-arc-grey-600 pointer-events-none"
          width="16"
          height="16"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          strokeLinejoin="round"
        >
          <circle cx="11" cy="11" r="8" />
          <line x1="21" y1="21" x2="16.65" y2="16.65" />
        </svg>

        <input
          type="text"
          value={query}
          onChange={(e) => {
            setQuery(e.target.value);
            setError('');
          }}
          onKeyDown={handleKeyDown}
          placeholder="Search by block height, tx hash, or address..."
          className="
            w-full pl-10 pr-4 py-2.5
            bg-arc-surface-raised border border-arc-border
            text-sm text-arc-white placeholder:text-arc-grey-700
            focus:outline-none focus:border-arc-blue/50
            transition-colors duration-150
          "
        />
      </div>

      {error && (
        <p className="absolute top-full left-0 mt-1 text-xs text-arc-error">
          {error}
        </p>
      )}
    </div>
  );
}
