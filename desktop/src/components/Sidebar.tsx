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
import { PulseDot } from "./PulseDot";
import { LogoMark, Tagline, Wordmark } from "./Logo";
import { formatUptime } from "../lib/format";
import type { Route } from "../lib/store";

const NAV: Array<{ id: Route; label: string; icon: typeof LayoutDashboard }> = [
  { id: "dashboard", label: "Dashboard", icon: LayoutDashboard },
  { id: "wallet", label: "Wallet", icon: Wallet },
  { id: "inference", label: "Inference", icon: Sparkles },
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

  const level = status?.health ?? "offline";

  return (
    <aside className="sidebar" data-testid="sidebar">
      <div className="sidebar-brand">
        <LogoMark size={28} />
        <div
          style={{
            display: "flex",
            flexDirection: "column",
            gap: 2,
            lineHeight: 1,
          }}
        >
          <Wordmark size={18} />
          <Tagline size="xs" />
        </div>
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
          <PulseDot level={level} />
          <div style={{ flex: 1, minWidth: 0 }}>
            <div style={{ fontSize: 13, fontWeight: 500, color: "var(--text)" }}>
              {status?.running ? "Running" : "Stopped"}
            </div>
            {status?.running && (
              <div style={{ fontSize: 11, color: "var(--text-muted)" }}>
                {formatUptime(status.uptimeSeconds)} · {status.peers} peers
              </div>
            )}
          </div>
        </div>
      </div>
    </aside>
  );
}
