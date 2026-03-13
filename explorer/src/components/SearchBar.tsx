import { useState, useCallback, useEffect, useRef } from 'react';
import { useNavigate } from 'react-router-dom';
import { detectSearchType } from '../utils';

const MAX_RECENT = 5;
const LS_KEY = 'arc-recent-searches';

interface SearchBarProps {
  className?: string;
}

function getRecentSearches(): string[] {
  try {
    const raw = localStorage.getItem(LS_KEY);
    return raw ? JSON.parse(raw) : [];
  } catch {
    return [];
  }
}

function saveRecentSearch(query: string) {
  const recent = getRecentSearches().filter((s) => s !== query);
  recent.unshift(query);
  localStorage.setItem(LS_KEY, JSON.stringify(recent.slice(0, MAX_RECENT)));
}

export default function SearchBar({ className = '' }: SearchBarProps) {
  const [query, setQuery] = useState('');
  const [error, setError] = useState('');
  const [hint, setHint] = useState('');
  const [showRecent, setShowRecent] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const navigate = useNavigate();

  // ── Keyboard shortcut: "/" to focus ─────────────────────────────
  useEffect(() => {
    function handleKey(e: KeyboardEvent) {
      if (
        e.key === '/' &&
        !e.ctrlKey &&
        !e.metaKey &&
        document.activeElement?.tagName !== 'INPUT' &&
        document.activeElement?.tagName !== 'TEXTAREA'
      ) {
        e.preventDefault();
        inputRef.current?.focus();
      }
    }
    document.addEventListener('keydown', handleKey);
    return () => document.removeEventListener('keydown', handleKey);
  }, []);

  // ── Close recent dropdown on outside click ─────────────────────
  useEffect(() => {
    if (!showRecent) return;
    function handleClick(e: MouseEvent) {
      if (containerRef.current && !containerRef.current.contains(e.target as Node)) {
        setShowRecent(false);
      }
    }
    document.addEventListener('mousedown', handleClick);
    return () => document.removeEventListener('mousedown', handleClick);
  }, [showRecent]);

  // ── Live type hint ─────────────────────────────────────────────
  useEffect(() => {
    const trimmed = query.trim();
    if (!trimmed) {
      setHint('');
      return;
    }
    const type = detectSearchType(trimmed);
    switch (type) {
      case 'block':
        setHint('Block height');
        break;
      case 'tx':
        setHint('Transaction hash');
        break;
      case 'account':
        setHint('Account address');
        break;
      default:
        setHint('');
    }
  }, [query]);

  const handleSearch = useCallback(() => {
    const trimmed = query.trim();
    if (!trimmed) return;

    setError('');
    const type = detectSearchType(trimmed);

    switch (type) {
      case 'block':
        saveRecentSearch(trimmed);
        navigate(`/block/${trimmed}`);
        break;
      case 'tx': {
        const hash = trimmed.startsWith('0x') ? trimmed.slice(2) : trimmed;
        saveRecentSearch(trimmed);
        navigate(`/tx/${hash}`);
        break;
      }
      case 'account': {
        const addr = trimmed.startsWith('0x') ? trimmed.slice(2) : trimmed;
        saveRecentSearch(trimmed);
        navigate(`/account/${addr}`);
        break;
      }
      default:
        setError('Enter a block height, tx hash, or address');
        return;
    }

    setQuery('');
    setShowRecent(false);
  }, [query, navigate]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (e.key === 'Enter') {
        handleSearch();
      }
      if (e.key === 'Escape') {
        setShowRecent(false);
        inputRef.current?.blur();
      }
    },
    [handleSearch]
  );

  const recentSearches = getRecentSearches();

  return (
    <div ref={containerRef} className={`relative flex-1 max-w-xl ${className}`}>
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
          ref={inputRef}
          type="text"
          value={query}
          onChange={(e) => {
            setQuery(e.target.value);
            setError('');
          }}
          onFocus={() => setShowRecent(true)}
          onKeyDown={handleKeyDown}
          placeholder="Search by block height, tx hash, or address..."
          className="
            w-full pl-10 pr-16 py-2.5
            bg-arc-surface-raised border border-arc-border
            text-sm text-arc-white placeholder:text-arc-grey-700
            focus:outline-none focus:border-arc-blue/50
            transition-colors duration-150
          "
        />

        {/* Keyboard shortcut hint */}
        {!query && (
          <kbd className="absolute right-3 px-1.5 py-0.5 text-[10px] text-arc-grey-700 border border-arc-border bg-arc-surface rounded-sm pointer-events-none">
            /
          </kbd>
        )}

        {/* Type hint badge */}
        {hint && (
          <span className="absolute right-3 text-[10px] text-arc-aquarius pointer-events-none">
            {hint}
          </span>
        )}
      </div>

      {/* Error */}
      {error && (
        <p className="absolute top-full left-0 mt-1 text-xs text-arc-error z-10">
          {error}
        </p>
      )}

      {/* Recent searches dropdown */}
      {showRecent && recentSearches.length > 0 && !query && (
        <div className="absolute top-full left-0 right-0 mt-1 bg-arc-surface-raised border border-arc-border z-20 shadow-lg">
          <p className="px-3 py-2 text-[10px] uppercase tracking-widest text-arc-grey-700">
            Recent Searches
          </p>
          {recentSearches.map((s) => (
            <button
              key={s}
              className="w-full text-left px-3 py-2 text-sm text-arc-grey-500 hover:text-arc-white hover:bg-arc-surface-overlay transition-colors duration-100"
              onMouseDown={(e) => {
                e.preventDefault();
                setQuery(s);
                setShowRecent(false);
                // Auto-navigate
                const type = detectSearchType(s);
                const cleaned = s.startsWith('0x') ? s.slice(2) : s;
                if (type === 'block') navigate(`/block/${s}`);
                else if (type === 'tx') navigate(`/tx/${cleaned}`);
                else if (type === 'account') navigate(`/account/${cleaned}`);
              }}
            >
              <span className="font-mono text-xs">{s.length > 20 ? s.slice(0, 10) + '...' + s.slice(-8) : s}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
