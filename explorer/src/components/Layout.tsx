import { useState, useEffect, useRef, useCallback } from 'react';
import { Link, Outlet, useLocation } from 'react-router-dom';
import { getHealth } from '../api';
import SearchBar from './SearchBar';

/* ─── Types ─────────────────────────────────────────────────────── */

interface NetworkStatus {
  online: boolean;
  height: number;
  peers: number;
  version: string;
}

/* ─── Nav config ────────────────────────────────────────────────── */

const navLinks = [
  { to: '/', label: 'Home' },
  { to: '/blockchain', label: 'Blockchain' },
  { to: '/blocks', label: 'Blocks' },
  { to: '/validators', label: 'Validators' },
  { to: '/faucet', label: 'Faucet' },
];

const footerColumns = [
  {
    title: 'Explorer',
    links: [
      { label: 'Home', to: '/' },
      { label: 'Blocks', to: '/blocks' },
      { label: 'Validators', to: '/validators' },
      { label: 'Faucet', to: '/faucet' },
    ],
  },
  {
    title: 'Developers',
    links: [
      { label: 'GitHub', href: 'https://github.com/FerrumVir/arc-chain' },
      { label: 'Documentation', href: 'https://github.com/FerrumVir/arc-chain/blob/main/SPEC.md' },
      { label: 'Smart Contracts', to: '/blockchain' },
    ],
  },
  {
    title: 'Network',
    links: [
      { label: 'Blockchain', to: '/blockchain' },
      { label: 'Run a Node', href: 'https://github.com/FerrumVir/arc-chain/blob/main/testnet/README.md' },
    ],
  },
  {
    title: 'Community',
    links: [
      { label: 'Twitter', href: 'https://x.com/arcreactorai' },
      { label: 'Token', href: 'https://etherscan.io/token/0x672fdBA7055bddFa8fD6bD45B1455cE5eB97f499' },
    ],
  },
];

/* ─── Component ─────────────────────────────────────────────────── */

export default function Layout() {
  const location = useLocation();
  const [mobileOpen, setMobileOpen] = useState(false);
  const [network, setNetwork] = useState<NetworkStatus>({
    online: false,
    height: 0,
    peers: 0,
    version: '',
  });
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const mobileNavRef = useRef<HTMLDivElement>(null);

  // ── Network status polling ─────────────────────────────────────
  const pollNetwork = useCallback(async () => {
    try {
      const h = await getHealth();
      setNetwork({
        online: h.status === 'ok',
        height: h.height,
        peers: h.peers,
        version: h.version,
      });
    } catch {
      setNetwork((prev) => ({ ...prev, online: false }));
    }
  }, []);

  useEffect(() => {
    pollNetwork();
    intervalRef.current = setInterval(pollNetwork, 8000);
    return () => {
      if (intervalRef.current) clearInterval(intervalRef.current);
    };
  }, [pollNetwork]);

  // ── Close mobile nav on route change ───────────────────────────
  useEffect(() => {
    setMobileOpen(false);
  }, [location.pathname]);

  // ── Close mobile nav on outside click ──────────────────────────
  useEffect(() => {
    if (!mobileOpen) return;
    function handleClick(e: MouseEvent) {
      if (
        mobileNavRef.current &&
        !mobileNavRef.current.contains(e.target as Node)
      ) {
        setMobileOpen(false);
      }
    }
    document.addEventListener('mousedown', handleClick);
    return () => document.removeEventListener('mousedown', handleClick);
  }, [mobileOpen]);

  return (
    <div className="min-h-screen bg-arc-black flex flex-col">
      {/* ─── Network Status Bar ─────────────────────────────────── */}
      <div className="border-b border-arc-border-subtle bg-arc-surface">
        <div className="max-w-7xl mx-auto px-4 sm:px-6 flex items-center justify-between h-8 text-xs">
          <div className="flex items-center gap-2">
            <span
              className={`inline-block w-2 h-2 rounded-full ${
                network.online
                  ? 'bg-arc-success animate-pulse-dot'
                  : 'bg-arc-error'
              }`}
            />
            <span className={network.online ? 'text-arc-success' : 'text-arc-error'}>
              {network.online ? 'Network Active' : 'Disconnected'}
            </span>
          </div>
          <div className="hidden sm:flex items-center gap-4 text-arc-grey-600">
            {network.height > 0 && (
              <span>
                Block{' '}
                <Link to={`/block/${network.height}`} className="text-arc-aquarius hover:text-arc-blue transition-colors">
                  #{network.height.toLocaleString()}
                </Link>
              </span>
            )}
            {network.peers > 0 && <span>{network.peers} peers</span>}
            {network.version && <span>v{network.version}</span>}
          </div>
        </div>
      </div>

      {/* ─── Header ──────────────────────────────────────────── */}
      <header className="border-b border-arc-border bg-arc-black/80 backdrop-blur-md sticky top-0 z-50">
        <div className="max-w-7xl mx-auto px-4 sm:px-6">
          <div className="flex items-center justify-between h-16 gap-6">
            {/* Logo */}
            <Link to="/" className="flex items-center gap-1.5 shrink-0">
              <img
                src="/brand/arc-logo-white.png"
                alt="ARC"
                className="h-5"
              />
              <span
                className="text-lg tracking-tight text-arc-white"
                style={{ fontFamily: "'Favorit', sans-serif" }}
              >
                scan
              </span>
            </Link>

            {/* Search — desktop */}
            <SearchBar className="hidden sm:block" />

            {/* Nav — desktop */}
            <nav className="hidden md:flex items-center gap-1">
              {navLinks.map((link) => {
                const isActive =
                  link.to === '/'
                    ? location.pathname === '/'
                    : location.pathname.startsWith(link.to);

                return (
                  <Link
                    key={link.to}
                    to={link.to}
                    className={`
                      px-3 py-1.5 text-sm transition-colors duration-150
                      ${
                        isActive
                          ? 'text-arc-white'
                          : 'text-arc-grey-600 hover:text-arc-white'
                      }
                    `}
                  >
                    {link.label}
                  </Link>
                );
              })}
            </nav>

            {/* Hamburger — mobile */}
            <button
              onClick={() => setMobileOpen((v) => !v)}
              className="md:hidden p-2 text-arc-grey-500 hover:text-arc-white transition-colors"
              aria-label="Toggle navigation"
            >
              {mobileOpen ? (
                <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                  <line x1="18" y1="6" x2="6" y2="18" />
                  <line x1="6" y1="6" x2="18" y2="18" />
                </svg>
              ) : (
                <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                  <line x1="3" y1="6" x2="21" y2="6" />
                  <line x1="3" y1="12" x2="21" y2="12" />
                  <line x1="3" y1="18" x2="21" y2="18" />
                </svg>
              )}
            </button>
          </div>

          {/* Mobile search */}
          <div className="sm:hidden pb-3">
            <SearchBar />
          </div>
        </div>
      </header>

      {/* ─── Mobile Nav Slide-out ────────────────────────────── */}
      {mobileOpen && (
        <>
          {/* Backdrop */}
          <div className="fixed inset-0 bg-black/60 backdrop-blur-sm z-40 md:hidden" />

          {/* Panel */}
          <div
            ref={mobileNavRef}
            className="fixed top-0 right-0 bottom-0 w-72 bg-arc-surface border-l border-arc-border z-50 md:hidden animate-slide-in"
          >
            <div className="flex items-center justify-between p-4 border-b border-arc-border">
              <span className="text-sm font-medium text-arc-white">Menu</span>
              <button
                onClick={() => setMobileOpen(false)}
                className="p-1 text-arc-grey-500 hover:text-arc-white transition-colors"
                aria-label="Close menu"
              >
                <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                  <line x1="18" y1="6" x2="6" y2="18" />
                  <line x1="6" y1="6" x2="18" y2="18" />
                </svg>
              </button>
            </div>
            <nav className="p-4 space-y-1">
              {navLinks.map((link) => {
                const isActive =
                  link.to === '/'
                    ? location.pathname === '/'
                    : location.pathname.startsWith(link.to);

                return (
                  <Link
                    key={link.to}
                    to={link.to}
                    className={`
                      block px-3 py-2.5 text-sm transition-colors duration-150
                      ${
                        isActive
                          ? 'text-arc-white bg-arc-surface-raised'
                          : 'text-arc-grey-500 hover:text-arc-white hover:bg-arc-surface-raised'
                      }
                    `}
                  >
                    {link.label}
                  </Link>
                );
              })}
            </nav>

            {/* Mobile network info */}
            <div className="absolute bottom-0 left-0 right-0 p-4 border-t border-arc-border">
              <div className="flex items-center gap-2 text-xs">
                <span
                  className={`inline-block w-2 h-2 rounded-full ${
                    network.online ? 'bg-arc-success' : 'bg-arc-error'
                  }`}
                />
                <span className={network.online ? 'text-arc-success' : 'text-arc-error'}>
                  {network.online ? 'Network Active' : 'Disconnected'}
                </span>
              </div>
              {network.height > 0 && (
                <p className="text-xs text-arc-grey-600 mt-1">
                  Block #{network.height.toLocaleString()}
                </p>
              )}
            </div>
          </div>
        </>
      )}

      {/* ─── Content ─────────────────────────────────────────── */}
      <main className="max-w-7xl mx-auto px-4 sm:px-6 py-8 flex-1 w-full animate-fade-in">
        <Outlet />
      </main>

      {/* ─── Footer ──────────────────────────────────────────── */}
      <footer className="border-t border-arc-border mt-auto bg-arc-surface">
        <div className="max-w-7xl mx-auto px-4 sm:px-6">
          {/* Column links */}
          <div className="grid grid-cols-2 md:grid-cols-4 gap-8 py-12">
            {footerColumns.map((col) => (
              <div key={col.title}>
                <h4 className="text-xs uppercase tracking-widest text-arc-grey-600 font-medium mb-4">
                  {col.title}
                </h4>
                <ul className="space-y-2.5">
                  {col.links.map((link) => (
                    <li key={link.label}>
                      {'to' in link && link.to ? (
                        <Link
                          to={link.to}
                          className="text-sm text-arc-grey-500 hover:text-arc-white transition-colors duration-150"
                        >
                          {link.label}
                        </Link>
                      ) : (
                        <a
                          href={'href' in link ? link.href : '#'}
                          target="_blank"
                          rel="noopener noreferrer"
                          className="text-sm text-arc-grey-500 hover:text-arc-white transition-colors duration-150"
                        >
                          {link.label}
                          {('href' in link) && (
                            <svg className="inline-block ml-1 -mt-0.5" width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                              <path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" />
                              <polyline points="15 3 21 3 21 9" />
                              <line x1="10" y1="14" x2="21" y2="3" />
                            </svg>
                          )}
                        </a>
                      )}
                    </li>
                  ))}
                </ul>
              </div>
            ))}
          </div>

          {/* Bottom bar */}
          <div className="border-t border-arc-border-subtle py-6 flex flex-col sm:flex-row items-center justify-between gap-4">
            <div className="flex items-center gap-2">
              <img
                src="/brand/arc-logo-white.png"
                alt="ARC"
                className="h-4 opacity-60"
              />
              <span className="text-xs text-arc-grey-700">
                ai for Humans First
              </span>
            </div>
            <p className="text-xs text-arc-grey-700">
              &copy; {new Date().getFullYear()} ARC Chain. All rights reserved.
            </p>
          </div>
        </div>
      </footer>
    </div>
  );
}
