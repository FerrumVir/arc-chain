import clsx from "clsx";
import {
  Activity,
  Coins,
  LayoutDashboard,
  ScrollText,
  Settings as SettingsIcon,
  Sparkles,
  Wallet,
} from "lucide-react";
import { useQuery } from "@tanstack/react-query";
import { useAppStore } from "../lib/store";
import { api } from "../lib/tauri";
import { describeConnection } from "../lib/connection";
import { PulseDot } from "./PulseDot";
import { Tagline, Wordmark } from "./Logo";
import { formatUptime } from "../lib/format";
import { PLAIN_COPY } from "../lib/plain-copy";
import type { Route } from "../lib/store";

// Home first: the chat answers whatever state the node is in.
const NAV: Array<{ id: Route; label: string; icon: typeof LayoutDashboard }> = [
  { id: "inference", label: PLAIN_COPY.home.navLabel, icon: Sparkles },
  { id: "dashboard", label: "Dashboard", icon: LayoutDashboard },
  { id: "wallet", label: "Wallet", icon: Wallet },
  { id: "earnings", label: "Earnings", icon: Coins },
  { id: "network", label: "Network", icon: Activity },
  { id: "logs", label: "Logs", icon: ScrollText },
  { id: "settings", label: "Settings", icon: SettingsIcon },
];

export function Sidebar() {
  const route = useAppStore((s) => s.route);
  const setRoute = useAppStore((s) => s.setRoute);

  const { data: status } = useQuery({
    queryKey: ["status"],
    queryFn: api.nodeStatus,
    refetchInterval: 2000,
  });

  // The connection in plain words (lib/connection.ts). Through v0.8.14 a
  // connected community node read "Running · 0 peers" here, which looked broken.
  const connection = describeConnection(status);
  const checking = status === undefined;   // no status yet: the node may well be running or starting

  return (
    <aside className="sidebar" data-testid="sidebar">
      <div className="sidebar-brand">
        <Wordmark height={28} />
        <Tagline size="sm" />
      </div>

      <nav className="sidebar-nav" aria-label="Primary">
        {NAV.map(({ id, label, icon: Icon }) => (
          <button
            key={id}
            className={clsx("nav-item", route === id && "active")}
            onClick={() => setRoute(id)}
            data-testid={`nav-${id}`}
            aria-current={route === id ? "page" : undefined}
          >
            <Icon />
            {label}
          </button>
        ))}
      </nav>

      <div className="sidebar-footer">
        <div className="node-status-chip" data-testid="sidebar-status">
          <PulseDot level={connection.level} ariaLabel={connection.label} />
          <div style={{ flex: 1, minWidth: 0 }}>
            <div style={{ fontSize: 13, fontWeight: 500, color: "var(--text)" }}>
              {checking
                ? "Checking…"
                : status?.running
                  ? "Running"
                  : status?.pid != null
                    ? "Starting"
                    : "Off"}
            </div>
            {status?.running && (
              <div style={{ fontSize: 11, color: "var(--text-muted)" }} data-testid="sidebar-connection">
                {formatUptime(status.uptimeSeconds)}
                {connection.via ? ` · ${connection.via}` : ""}
              </div>
            )}
            {!status?.running && status?.pid != null && (
              <div style={{ fontSize: 11, color: "var(--text-muted)" }}>
                loading the AI model…
              </div>
            )}
          </div>
        </div>
      </div>
    </aside>
  );
}
