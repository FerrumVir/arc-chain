"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import {
  Blocks,
  Activity,
  Users,
  ShieldCheck,
  Search,
  Menu,
  X,
} from "lucide-react";
import { useState, useEffect } from "react";
import { useRouter } from "next/navigation";
import DataSourceBadge from "@/components/DataSourceBadge";
import type { DataSource } from "@/lib/chain-client";

interface HeaderProps {
  dataSource?: DataSource;
}

const NAV_ITEMS = [
  { href: "/", label: "Home", icon: Activity },
  { href: "/blocks", label: "Blocks", icon: Blocks },
  { href: "/accounts", label: "Accounts", icon: Users },
  { href: "/verify", label: "Verify", icon: ShieldCheck, accent: true },
];

export default function Header({ dataSource: dataSourceProp }: HeaderProps = {}) {
  const pathname = usePathname();
  const router = useRouter();
  const [search, setSearch] = useState("");
  const [mobileOpen, setMobileOpen] = useState(false);

  // Self-detect data source when no prop provided
  const [detectedSource, setDetectedSource] = useState<DataSource>("demo");
  useEffect(() => {
    if (dataSourceProp) return;
    import("@/lib/chain-client").then(({ checkDataSource }) => {
      checkDataSource().then(setDetectedSource).catch(() => setDetectedSource("demo"));
    });
  }, [dataSourceProp]);

  const source = dataSourceProp ?? detectedSource;

  function handleSearch(e: React.FormEvent) {
    e.preventDefault();
    const q = search.trim();
    if (!q) return;
    if (q.startsWith("0x") && q.length === 66) {
      router.push(`/tx/${q}`);
    } else if (q.startsWith("0x") && q.length === 42) {
      router.push(`/account/${q}`);
    } else if (/^\d+$/.test(q)) {
      router.push(`/block/${q}`);
    }
    setSearch("");
  }

  return (
    <header className="sticky top-0 z-50 border-b border-[var(--border)] glass-nav">
      <div className="mx-auto max-w-[1400px] px-4 sm:px-6 lg:px-8">
        <div className="flex h-14 items-center justify-between gap-4">
          {/* Logo */}
          <Link href="/" className="flex items-center gap-2.5 shrink-0 group">
            <div
              className="w-8 h-8 rounded-lg flex items-center justify-center overflow-hidden shadow-sm transition-all duration-300 group-hover:shadow-md group-hover:scale-105"
              style={{ background: 'var(--gradient-arc)' }}
            >
              {/* eslint-disable-next-line @next/next/no-img-element */}
              <img
                src="/brand/arc-logo-white.png"
                alt="ARC"
                width={18}
                height={18}
                className="object-contain"
              />
            </div>
            <div className="flex items-baseline gap-1.5">
              <span className="text-[15px] font-semibold tracking-tight">
                ARC
              </span>
              <span className="text-[12px] text-[var(--text-tertiary)] font-medium">
                scan
              </span>
            </div>
          </Link>

          {/* Desktop Nav */}
          <nav className="hidden md:flex items-center gap-0.5">
            {NAV_ITEMS.map(({ href, label, icon: Icon, accent }) => {
              const active =
                href === "/"
                  ? pathname === "/"
                  : pathname.startsWith(href);
              return (
                <Link
                  key={href}
                  href={href}
                  className={`flex items-center gap-1.5 px-3.5 py-2 rounded-lg text-[13px] font-medium transition-all duration-200 ${
                    active
                      ? "bg-[var(--accent-light)] text-[var(--accent)]"
                      : accent
                      ? "text-[var(--accent)] hover:bg-[var(--accent-light)]"
                      : "text-[var(--text-secondary)] hover:text-[var(--text)] hover:bg-[var(--bg-secondary)]"
                  }`}
                >
                  <Icon className="w-3.5 h-3.5" />
                  {label}
                </Link>
              );
            })}
          </nav>

          {/* Search */}
          <form onSubmit={handleSearch} className="flex-1 max-w-[380px] hidden sm:block">
            <div className="relative group">
              <Search className="absolute left-3.5 top-1/2 -translate-y-1/2 w-3.5 h-3.5 text-[var(--text-tertiary)] transition-colors group-focus-within:text-[var(--accent)]" />
              <input
                type="text"
                value={search}
                onChange={(e) => setSearch(e.target.value)}
                placeholder="Search block, tx hash, address..."
                className="w-full h-9 pl-10 pr-3 rounded-xl bg-[var(--bg-secondary)] border border-[var(--border)] text-[12px] placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[var(--accent)] focus:bg-[var(--bg-input)] focus:shadow-[0_0_0_3px_var(--accent-glow)] transition-all duration-200"
              />
            </div>
          </form>

          {/* Badges */}
          <div className="hidden sm:flex items-center gap-2.5">
            <div className="flex items-center gap-1.5 px-2.5 py-1 rounded-full bg-[var(--bg-secondary)] border border-[var(--border)]">
              <div className="w-1.5 h-1.5 rounded-full bg-[var(--shield-green)]" />
              <span className="text-[11px] font-medium text-[var(--text-secondary)]">
                Testnet
              </span>
            </div>
            <DataSourceBadge source={source} />
          </div>

          {/* Mobile menu */}
          <button
            onClick={() => setMobileOpen(!mobileOpen)}
            className="md:hidden p-2 rounded-lg hover:bg-[var(--bg-secondary)] transition-colors"
          >
            {mobileOpen ? (
              <X className="w-5 h-5 text-[var(--text-secondary)]" />
            ) : (
              <Menu className="w-5 h-5 text-[var(--text-secondary)]" />
            )}
          </button>
        </div>
      </div>

      {/* Mobile dropdown */}
      {mobileOpen && (
        <div className="md:hidden border-t border-[var(--border)] bg-[var(--bg)] px-4 py-3 animate-slide-up">
          <form onSubmit={(e) => { handleSearch(e); setMobileOpen(false); }} className="mb-3">
            <div className="relative">
              <Search className="absolute left-3 top-1/2 -translate-y-1/2 w-3.5 h-3.5 text-[var(--text-tertiary)]" />
              <input
                type="text"
                value={search}
                onChange={(e) => setSearch(e.target.value)}
                placeholder="Search block, tx hash, address..."
                className="w-full h-10 pl-9 pr-3 rounded-xl bg-[var(--bg-secondary)] border border-[var(--border)] text-[13px] placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[var(--accent)] transition-colors"
              />
            </div>
          </form>

          <nav className="flex flex-col gap-1">
            {NAV_ITEMS.map(({ href, label, icon: Icon }) => {
              const active =
                href === "/" ? pathname === "/" : pathname.startsWith(href);
              return (
                <Link
                  key={href}
                  href={href}
                  onClick={() => setMobileOpen(false)}
                  className={`flex items-center gap-2.5 px-3 py-2.5 rounded-xl text-[14px] font-medium transition-colors ${
                    active
                      ? "bg-[var(--accent-light)] text-[var(--accent)]"
                      : "text-[var(--text-secondary)] hover:text-[var(--text)] hover:bg-[var(--bg-secondary)]"
                  }`}
                >
                  <Icon className="w-4 h-4" />
                  {label}
                </Link>
              );
            })}
          </nav>

          <div className="flex items-center gap-2.5 px-3 py-2 mt-2">
            <div className="flex items-center gap-1.5">
              <div className="w-1.5 h-1.5 rounded-full bg-[var(--shield-green)]" />
              <span className="text-[12px] font-medium text-[var(--text-secondary)]">
                Testnet
              </span>
            </div>
            <DataSourceBadge source={source} />
          </div>
        </div>
      )}
    </header>
  );
}
