import { Link, Outlet, useLocation } from 'react-router-dom';
import SearchBar from './SearchBar';

const navLinks = [
  { to: '/', label: 'Home' },
  { to: '/blocks', label: 'Blocks' },
];

export default function Layout() {
  const location = useLocation();

  return (
    <div className="min-h-screen bg-arc-black">
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

            {/* Search */}
            <SearchBar className="hidden sm:block" />

            {/* Nav */}
            <nav className="flex items-center gap-1">
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
          </div>

          {/* Mobile search */}
          <div className="sm:hidden pb-3">
            <SearchBar />
          </div>
        </div>
      </header>

      {/* ─── Content ─────────────────────────────────────────── */}
      <main className="max-w-7xl mx-auto px-4 sm:px-6 py-8">
        <Outlet />
      </main>

      {/* ─── Footer ──────────────────────────────────────────── */}
      <footer className="border-t border-arc-border mt-auto">
        <div className="max-w-7xl mx-auto px-4 sm:px-6 py-6 flex items-center justify-between">
          <p className="text-xs text-arc-grey-700">
            ARC scan
          </p>
          <p className="text-xs text-arc-grey-700">
            ai for Humans First
          </p>
        </div>
      </footer>
    </div>
  );
}
